//! Native registration is the only permission to release a Provision. Storage work stays outside
//! native transition locks, and the release callback touches only the bounded transport queue.

use std::sync::Arc;

use bitcoin::hashes::{sha256, Hash};
use bitcoin::secp256k1::PublicKey;
use lightning::ln::ffor::{FFORReceiverRecoveryContext, FFORReceiverWitnessRegistration};
use lightning_ffor::witness::{SignedManifest, WitnessConnection};

use super::pending::Epoch;
use super::{WitnessOwner, WitnessOwnerError};
use crate::ffor::witness_store::{WitnessPolicy, WitnessStorageBinding};
use crate::message_handler::ffor::OutboundError;

/// Observations of registration persistence, not invoice or receiving authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RegistrationProgress {
	AwaitingPersistence,
	Retained,
}

/// One bounded release attempt. Queued means accepted locally, never delivered or acknowledged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProvisioningProgress {
	AwaitingPersistence,
	Backpressured,
	Queued,
	AwaitingAcknowledgement,
}

enum Attempt {
	Advance,
	Retry,
}

type Manifests = Vec<(PublicKey, SignedManifest)>;

impl WitnessOwner {
	/// Create protected material only when native has no immutable registration. An existing native
	/// selection always requires load-only recovery, including when the caller changes its policy.
	pub(crate) fn register(
		&mut self, context: &FFORReceiverRecoveryContext, policies: &[WitnessPolicy],
	) -> Result<RegistrationProgress, WitnessOwnerError> {
		self.synchronize_connections();
		let registration = self
			.manager
			.ffor_receiver_witness_registration(context)
			.map_err(WitnessOwnerError::Native)?;
		let binding = WitnessStorageBinding::from_native_context(context)
			.map_err(WitnessOwnerError::Storage)?;
		if registration.is_none() {
			self.store.create(&binding, policies).map_err(WitnessOwnerError::Storage)?;
		}
		let manifests =
			self.store.provisioning_manifests(&binding).map_err(WitnessOwnerError::Storage)?;
		check_policies(policies, &manifests)?;
		if let Some(registration) = registration {
			check_registration(context, &registration, &manifests)?;
		}
		let requirement = self
			.manager
			.register_ffor_receiver_witnesses(context, &manifests)
			.map_err(WitnessOwnerError::Native)?;
		Ok(if self.manager.is_ffor_state_persisted(&requirement) {
			RegistrationProgress::Retained
		} else {
			RegistrationProgress::AwaitingPersistence
		})
	}

	/// Reconfirm the sidecar, require durable native selection, then stage exact response correlation
	/// before the native callback can enqueue. Missing protected data never allocates replacement keys.
	pub(crate) fn provision(
		&mut self, context: &FFORReceiverRecoveryContext, witness: PublicKey,
	) -> Result<ProvisioningProgress, WitnessOwnerError> {
		self.provision_inner(context, witness, Attempt::Advance)
	}

	/// Explicit timeout or ambiguous-delivery retry. Retain exact manifest/keys but use a fresh ID.
	pub(crate) fn retry_provision(
		&mut self, context: &FFORReceiverRecoveryContext, witness: PublicKey,
	) -> Result<ProvisioningProgress, WitnessOwnerError> {
		self.provision_inner(context, witness, Attempt::Retry)
	}

	fn provision_inner(
		&mut self, context: &FFORReceiverRecoveryContext, witness: PublicKey, attempt: Attempt,
	) -> Result<ProvisioningProgress, WitnessOwnerError> {
		self.synchronize_connections();
		let manifests = self.retained_manifests(context)?;
		let requirement = self
			.manager
			.register_ffor_receiver_witnesses(context, &manifests)
			.map_err(WitnessOwnerError::Native)?;
		if !self.manager.is_ffor_state_persisted(&requirement) {
			return Ok(ProvisioningProgress::AwaitingPersistence);
		}
		let active = self
			.manager
			.capture_ffor_receiver_active_context(
				&context.channel_id(),
				&context.settlement_node_id(),
				context.epoch_id(),
			)
			.map_err(WitnessOwnerError::Native)?;
		let manifest = manifests
			.into_iter()
			.find_map(|(selected, manifest)| (selected == witness).then_some(manifest))
			.ok_or(WitnessOwnerError::UnknownWitness)?;
		let connection =
			self.transport.connection(witness).ok_or(WitnessOwnerError::StaleConnection)?;
		let epoch = Epoch {
			channel: context.channel_id(),
			epoch: context.epoch_id(),
			context_digest: context.context_digest(),
		};
		let source = WitnessConnection { node_id: witness, identity: connection.clone() };
		let (other_count, other_bytes) = self.fetches.usage();
		self.pending.set_other_usage(other_count, other_bytes)?;
		let fetches = &self.fetches;
		let pending = match attempt {
			Attempt::Advance => self
				.pending
				.stage_excluding(epoch, source, manifest, |id| fetches.contains_request_id(id))?,
			Attempt::Retry => self
				.pending
				.restart_excluding(epoch, source, manifest, |id| fetches.contains_request_id(id))?,
		};
		if self.pending.is_queued(&pending) {
			return Ok(ProvisioningProgress::AwaitingAcknowledgement);
		}
		let transport = Arc::clone(&self.transport);
		let mut refusal = None;
		let queued = self
			.manager
			.release_ffor_receiver_witness_provision(
				&active,
				&witness,
				pending.provision(),
				|provision| {
					// This is the sole nested native -> transport lock acquisition. No owner/store reentry.
					transport.enqueue_provision(witness, &connection, provision).map_err(|error| {
						refusal = Some(error);
					})
				},
			)
			.map_err(WitnessOwnerError::Native)?;
		if queued {
			self.pending.mark_queued(&pending);
			return Ok(ProvisioningProgress::Queued);
		}
		match refusal {
			Some(OutboundError::Capacity) => Ok(ProvisioningProgress::Backpressured),
			Some(OutboundError::StaleConnection) => {
				self.pending.discard_disconnected(&self.transport);
				Err(WitnessOwnerError::StaleConnection)
			},
			Some(OutboundError::InvalidMessage) => Err(WitnessOwnerError::Conflict),
			None => Ok(ProvisioningProgress::AwaitingPersistence),
		}
	}

	pub(super) fn retained_manifests(
		&self, context: &FFORReceiverRecoveryContext,
	) -> Result<Manifests, WitnessOwnerError> {
		let registration = self
			.manager
			.ffor_receiver_witness_registration(context)
			.map_err(WitnessOwnerError::Native)?
			.ok_or(WitnessOwnerError::Unregistered)?;
		let binding = WitnessStorageBinding::from_native_context(context)
			.map_err(WitnessOwnerError::Storage)?;
		let manifests =
			self.store.provisioning_manifests(&binding).map_err(WitnessOwnerError::Storage)?;
		check_registration(context, &registration, &manifests)?;
		Ok(manifests)
	}
}

fn check_policies(
	policies: &[WitnessPolicy], manifests: &Manifests,
) -> Result<(), WitnessOwnerError> {
	if policies.len() != manifests.len() || policies.is_empty() || policies.len() > 4 {
		return Err(WitnessOwnerError::Conflict);
	}
	let mut policies = policies.to_vec();
	policies.sort_unstable_by_key(|policy| policy.witness);
	for (policy, (witness, manifest)) in policies.iter().zip(manifests) {
		let params = manifest.unsigned().parameters();
		if policy.witness != *witness
			|| policy.retention_until != params.retention_until
			|| policy.minimum_receipts != params.minimum_receipts
		{
			return Err(WitnessOwnerError::Conflict);
		}
	}
	Ok(())
}

fn check_registration(
	context: &FFORReceiverRecoveryContext, registration: &FFORReceiverWitnessRegistration,
	manifests: &Manifests,
) -> Result<(), WitnessOwnerError> {
	if registration.context_digest() != context.context_digest()
		|| registration.witnesses().len() != manifests.len()
	{
		return Err(WitnessOwnerError::Conflict);
	}
	for (selected, (witness, manifest)) in registration.witnesses().iter().zip(manifests) {
		let params = manifest.unsigned().parameters();
		if selected.witness_node_id() != *witness
			|| selected.manifest_digest() != sha256::Hash::hash(&manifest.encode()).to_byte_array()
			|| selected.mailbox_id() != params.mailbox_id
			|| selected.fetch_public_key() != params.fetch_public_key
			|| selected.encryption_public_key() != params.encryption_public_key
			|| selected.retention_until() != params.retention_until
			|| selected.minimum_receipts() != params.minimum_receipts
		{
			return Err(WitnessOwnerError::Conflict);
		}
	}
	Ok(())
}

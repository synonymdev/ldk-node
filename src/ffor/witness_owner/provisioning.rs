//! Native registration is the only permission to release a Provision. Storage work stays outside
//! native transition locks, and the release callback touches only the bounded transport queue.

use std::sync::Arc;

use bitcoin::hashes::{sha256, Hash};
use bitcoin::secp256k1::PublicKey;
use lightning::ln::ffor::{FFORReceiverRecoveryContext, FFORReceiverWitnessRegistration};
use lightning_ffor::witness::{SignedManifest, WitnessConnection};

use super::pending::{Epoch, NativeAttempt};
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
	/// Both historical promises are retained and the current native requirement is complete.
	AcknowledgementRetained,
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
		let manifest = manifests
			.into_iter()
			.find_map(|(selected, manifest)| (selected == witness).then_some(manifest))
			.ok_or(WitnessOwnerError::UnknownWitness)?;
		let acknowledged = self.acknowledgement_retained(context, witness, &manifest)?;
		// Storage and historical ACK reads may race a newer native promise. Capture after the
		// join so its latest durability barrier defines the observation point for this result.
		let active = self
			.manager
			.capture_ffor_receiver_active_context(
				&context.channel_id(),
				&context.settlement_node_id(),
				context.epoch_id(),
			)
			.map_err(WitnessOwnerError::Native)?;
		if acknowledged {
			self.pending.complete_witness(
				Epoch {
					channel: context.channel_id(),
					epoch: context.epoch_id(),
					context_digest: context.context_digest(),
				},
				witness,
			);
			return Ok(ProvisioningProgress::AcknowledgementRetained);
		}
		let connection =
			self.transport.native_connection(witness).ok_or(WitnessOwnerError::StaleConnection)?;
		let epoch = Epoch {
			channel: context.channel_id(),
			epoch: context.epoch_id(),
			context_digest: context.context_digest(),
		};
		let source =
			WitnessConnection { node_id: witness, identity: connection.transport().clone() };
		let (other_count, other_bytes) = self.fetches.usage();
		self.pending.set_other_usage(other_count, other_bytes)?;
		let fetches = &self.fetches;
		// Preserve the previous request and accounting if native staging refuses the replacement.
		let mut candidate = self.pending.clone();
		let pending = match attempt {
			Attempt::Advance => candidate
				.stage_excluding(epoch, source, manifest, |id| fetches.contains_request_id(id))?,
			Attempt::Retry => candidate
				.restart_excluding(epoch, source, manifest, |id| fetches.contains_request_id(id))?,
		};
		if candidate.is_queued(&pending) {
			return Ok(ProvisioningProgress::AwaitingAcknowledgement);
		}
		let native_attempt = match candidate.native_attempt(&pending) {
			Some(retained) => retained.attempt.clone(),
			None => self
				.manager
				.stage_ffor_receiver_witness_provision(
					context,
					connection.native(),
					pending.provision(),
				)
				.map_err(WitnessOwnerError::Native)?,
		};
		candidate.bind_native(
			&pending,
			NativeAttempt { connection: connection.clone(), attempt: native_attempt.clone() },
		);
		self.pending = candidate;
		let transport = Arc::clone(&self.transport);
		let mut refusal = None;
		let queued = self
			.manager
			.release_ffor_receiver_witness_attempt(
				&active,
				&native_attempt,
				pending.provision(),
				|provision| {
					// This is the sole nested native -> transport lock acquisition. No owner/store reentry.
					transport.enqueue_provision(witness, connection.transport(), provision).map_err(
						|error| {
							refusal = Some(error);
						},
					)
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

	fn acknowledgement_retained(
		&self, context: &FFORReceiverRecoveryContext, witness: PublicKey, manifest: &SignedManifest,
	) -> Result<bool, WitnessOwnerError> {
		let binding = WitnessStorageBinding::from_native_context(context)
			.map_err(WitnessOwnerError::Storage)?;
		let stored = self.store.load(&binding).map_err(WitnessOwnerError::Storage)?;
		let required = manifest.unsigned().parameters().retention_until;
		if stored.acknowledgement_retention(witness).filter(|until| *until >= required).is_none() {
			return Ok(false);
		}
		let native = self
			.manager
			.ffor_receiver_witness_acknowledgements(context)
			.map_err(WitnessOwnerError::Native)?;
		let Some(native) = native else {
			return Ok(false);
		};
		if native.context_digest() != context.context_digest() {
			return Err(WitnessOwnerError::Conflict);
		}
		let digest = sha256::Hash::hash(&manifest.encode()).to_byte_array();
		// Independent first acknowledgements may have different request IDs after a crash.
		Ok(native.acknowledgements().iter().any(|ack| {
			ack.witness_node_id() == witness
				&& ack.manifest_digest() == digest
				&& ack.retention_until() >= required
		}))
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

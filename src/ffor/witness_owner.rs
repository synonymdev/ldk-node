//! Private orchestration of native-authorized witness provisioning and durable acknowledgement.
//!
//! Only the opt-in runtime constructs this owner. Methods require exclusive access, while the protected store
//! remains the single persistence owner. Native registration and release own phase authority;
//! this module retains only bounded transient response correlation. Historical acknowledgements
//! authorize neither an invoice nor a native transition. Transport callbacks never call the store.

use std::sync::Arc;

use fetch::FetchRequests;
use lightning::ln::ffor::FFORReceiverError;
use lightning_ffor::witness::{Acknowledgement, AcknowledgementResult, WitnessError};
use pending::PendingProvisions;

use super::witness_store::{
	WitnessKeyUseError, WitnessSecretStore, WitnessStorageBinding, WitnessStoreError,
};
use crate::message_handler::ffor::{FforReceiverTransport, ReceivedFforMessage};
use crate::types::{ChainMonitor, ChannelManager};

mod fetch;
mod pending;
mod provisioning;
mod recovery;
pub(crate) use fetch::FetchProgress;
pub(crate) use provisioning::{ProvisioningProgress, RegistrationProgress};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WitnessOwnerError {
	Native(FFORReceiverError),
	Storage(WitnessStoreError),
	Protocol(WitnessError),
	KeyUse(WitnessKeyUseError),
	Capacity,
	Conflict,
	Entropy,
	StaleConnection,
	UnknownRequest,
	UnknownWitness,
	Unregistered,
}

/// Persistence observations only, never invoice readiness or current receiving authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AcknowledgementProgress {
	AwaitingPersistence,
	Retained,
	Refused,
}

pub(crate) struct WitnessOwner {
	manager: Arc<ChannelManager>,
	monitor: Arc<ChainMonitor>,
	store: Arc<WitnessSecretStore>,
	transport: Arc<FforReceiverTransport>,
	pending: PendingProvisions,
	fetches: FetchRequests,
}

impl WitnessOwner {
	pub(crate) fn new(
		manager: Arc<ChannelManager>, monitor: Arc<ChainMonitor>, store: Arc<WitnessSecretStore>,
		transport: Arc<FforReceiverTransport>,
	) -> Self {
		Self {
			manager,
			monitor,
			store,
			transport,
			pending: PendingProvisions::default(),
			fetches: FetchRequests::default(),
		}
	}

	/// Only the actual transport message supplies W and connection identity. An invalid or failed
	/// write leaves pending correlation intact; persistence recovery must finish before retry.
	pub(crate) fn acknowledge(
		&mut self, message: &ReceivedFforMessage,
	) -> Result<AcknowledgementProgress, WitnessOwnerError> {
		self.synchronize_connections();
		if !self.transport.is_current(message) {
			return Err(WitnessOwnerError::StaleConnection);
		}
		let ack =
			Acknowledgement::decode(message.frame().wire()).map_err(WitnessOwnerError::Protocol)?;
		let (epoch, native) = self.pending.native_for(message, &ack)?;
		if matches!(ack.result(), AcknowledgementResult::Refused(_)) {
			return match self
				.manager
				.retain_ffor_receiver_witness_ack(native.connection.native(), &ack)
			{
				Err(FFORReceiverError::InvalidWitnessRegistration) => {
					self.pending.complete(ack.request_id());
					Ok(AcknowledgementProgress::Refused)
				},
				Err(error) => Err(WitnessOwnerError::Native(error)),
				Ok(_) => Err(WitnessOwnerError::Conflict),
			};
		}
		let (_, checked) = self.pending.check(message, &ack)?;
		let context = self
			.manager
			.ffor_receiver_recovery_context(&epoch.channel, epoch.epoch)
			.map_err(WitnessOwnerError::Native)?;
		if context.context_digest() != epoch.context_digest {
			return Err(WitnessOwnerError::Conflict);
		}
		self.retained_manifests(&context)?;
		let binding = WitnessStorageBinding::from_native_context(&context)
			.map_err(WitnessOwnerError::Storage)?;
		self.store.acknowledge(&binding, &checked).map_err(WitnessOwnerError::Storage)?;
		// The protected store lock is released before native checks its genuine W generation.
		// If disconnect intervenes, keep the first stored promise and recover with a fresh attempt.
		let requirement = self
			.manager
			.retain_ffor_receiver_witness_ack(native.connection.native(), &ack)
			.map_err(WitnessOwnerError::Native)?;
		if !self.manager.is_ffor_state_persisted(&requirement) {
			return Ok(AcknowledgementProgress::AwaitingPersistence);
		}
		self.pending.complete(ack.request_id());
		Ok(AcknowledgementProgress::Retained)
	}

	/// Retry only the protected store's exact uncertain write; this grants no release authority.
	pub(crate) fn recover_storage(&mut self) -> Result<(), WitnessOwnerError> {
		self.store.recover_write().map_err(WitnessOwnerError::Storage)
	}

	/// Drop only transient correlation. Exact manifests, keys and historical promises remain stored.
	pub(crate) fn synchronize_connections(&mut self) {
		self.pending.discard_disconnected(&self.transport);
		self.fetches.discard_disconnected(&self.transport);
	}
}

#[cfg(test)]
mod tests;

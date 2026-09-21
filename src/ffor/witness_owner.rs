//! Private orchestration of native-authorized witness provisioning and durable acknowledgement.
//!
//! No builder constructs this owner. Methods require exclusive access, while the protected store
//! remains the single persistence owner. Native registration and release own phase authority;
//! this module retains only bounded transient response correlation. Historical acknowledgements
//! authorize neither an invoice nor a native transition. Transport callbacks never call the store.

use std::sync::Arc;

use lightning::ln::ffor::FFORReceiverError;
use lightning_ffor::witness::{Acknowledgement, WitnessError};

use super::witness_store::{
	WitnessKeyUseError, WitnessSecretStore, WitnessStorageBinding, WitnessStoreError,
};
use crate::message_handler::ffor::{FforReceiverTransport, ReceivedFforMessage};
use crate::types::ChannelManager;
use fetch::FetchRequests;
use pending::PendingProvisions;

mod fetch;
mod pending;
mod provisioning;

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

/// Storage completion only. This is deliberately not a provisioning or invoice readiness value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AcknowledgementProgress {
	Retained,
}

pub(crate) struct WitnessOwner {
	manager: Arc<ChannelManager>,
	store: Arc<WitnessSecretStore>,
	transport: Arc<FforReceiverTransport>,
	pending: PendingProvisions,
	fetches: FetchRequests,
}

impl WitnessOwner {
	pub(crate) fn new(
		manager: Arc<ChannelManager>, store: Arc<WitnessSecretStore>,
		transport: Arc<FforReceiverTransport>,
	) -> Self {
		Self {
			manager,
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
		let (epoch, checked) = self.pending.check(message, &ack)?;
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

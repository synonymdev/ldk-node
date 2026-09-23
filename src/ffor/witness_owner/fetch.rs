//! Historical witness retrieval. A completed traversal is not evidence that missing slots are unpaid.
//! Checked pages remain bounded and survive storage backpressure until their evidence is retained.

use bitcoin::secp256k1::PublicKey;
use lightning::ln::ffor::FFORReceiverRecoveryContext;
use lightning_ffor::witness::{
	CheckedFetchPage, FetchResponse, PendingFetch, SignedFetch, WitnessConnection,
};

use super::pending::Epoch;
use super::{WitnessOwner, WitnessOwnerError};
use crate::ffor::witness_store::{ReceiptPageRetention, WitnessStorageBinding};
use crate::message_handler::ffor::{
	ConnectionToken, FforReceiverTransport, OutboundError, ReceivedFforMessage,
};

const MAX_WORK_COUNT: usize = 64;
const MAX_WORK_BYTES: usize = 8 * 1024 * 1024;

/// Retrieval progress only. No variant establishes settlement, payment credit or invoice authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FetchProgress {
	Queued,
	AwaitingResponse,
	Backpressured,
	Complete,
	RestartRequired,
	/// Invalid or conflicting candidates were rejected, after all other page evidence was retained.
	RejectedPage,
}

enum FetchState {
	Request { pending: PendingFetch<ConnectionToken>, queued: bool },
	Page { checked: CheckedFetchPage<ConnectionToken> },
}

struct FetchEntry {
	epoch: Epoch,
	source: WitnessConnection<ConnectionToken>,
	request_id: [u8; 16],
	nonce: [u8; 32],
	reserved_bytes: usize,
	state: FetchState,
}

#[derive(Default)]
pub(super) struct FetchRequests {
	entries: Vec<FetchEntry>,
}

impl FetchRequests {
	pub(super) fn usage(&self) -> (usize, usize) {
		(self.entries.len(), self.entries.iter().map(|entry| entry.reserved_bytes).sum())
	}

	pub(super) fn contains_request_id(&self, id: [u8; 16]) -> bool {
		self.entries.iter().any(|entry| entry.request_id == id)
	}

	pub(super) fn discard_disconnected(&mut self, transport: &FforReceiverTransport) {
		self.entries.retain(|entry| {
			// An authenticated page may contain valuable evidence even when its connection is gone.
			matches!(entry.state, FetchState::Page { .. })
				|| transport.connection(entry.source.node_id).as_ref()
					== Some(&entry.source.identity)
		});
	}

	fn find(&self, epoch: Epoch, witness: PublicKey) -> Result<Option<usize>, WitnessOwnerError> {
		let index = self.entries.iter().position(|entry| {
			entry.epoch.channel == epoch.channel
				&& entry.epoch.epoch == epoch.epoch
				&& entry.source.node_id == witness
		});
		if index.is_some_and(|index| self.entries[index].epoch != epoch) {
			return Err(WitnessOwnerError::Conflict);
		}
		Ok(index)
	}

	fn install(
		&mut self, epoch: Epoch, pending: PendingFetch<ConnectionToken>,
		other_usage: (usize, usize),
	) -> Result<usize, WitnessOwnerError> {
		let source = pending.connection().clone();
		let previous = self.find(epoch, source.node_id)?;
		if previous
			.is_some_and(|index| matches!(self.entries[index].state, FetchState::Page { .. }))
		{
			return Err(WitnessOwnerError::Conflict);
		}
		let parameters = pending.request().unsigned().parameters();
		if self.contains_request_id(parameters.request_id)
			|| self.entries.iter().any(|entry| {
				entry.source.node_id == source.node_id && entry.nonce == parameters.nonce
			}) {
			return Err(WitnessOwnerError::Entropy);
		}
		let manifest = pending.manifest();
		// Reserve a complete response and all traversal history before the first request leaves.
		// A second full frame covers a request and any future signed paging extensions.
		// Encoded payload accounting excludes fixed metadata, whose count is bounded separately.
		let reserved_bytes = manifest.encode().len()
			+ 2 * lightning_ffor::wire::MAX_MESSAGE_LEN
			+ lightning_ffor::book::MAX_VOUCHERS * 48;
		let (count, bytes) = self.usage();
		let old_bytes = previous.map_or(0, |index| self.entries[index].reserved_bytes);
		let new_count = count + usize::from(previous.is_none());
		if new_count + other_usage.0 > MAX_WORK_COUNT
			|| (bytes - old_bytes)
				.checked_add(reserved_bytes)
				.and_then(|bytes| bytes.checked_add(other_usage.1))
				.filter(|bytes| *bytes <= MAX_WORK_BYTES)
				.is_none()
		{
			return Err(WitnessOwnerError::Capacity);
		}
		let entry = FetchEntry {
			epoch,
			source,
			request_id: parameters.request_id,
			nonce: parameters.nonce,
			reserved_bytes,
			state: FetchState::Request { pending, queued: false },
		};
		Ok(if let Some(index) = previous {
			self.entries[index] = entry;
			index
		} else {
			self.entries.push(entry);
			self.entries.len() - 1
		})
	}

	fn accept(&mut self, message: &ReceivedFforMessage) -> Result<usize, WitnessOwnerError> {
		let response =
			FetchResponse::decode(message.frame().wire()).map_err(WitnessOwnerError::Protocol)?;
		let index = self
			.entries
			.iter()
			.position(|entry| entry.request_id == response.request_id())
			.ok_or(WitnessOwnerError::UnknownRequest)?;
		let entry = &mut self.entries[index];
		let pending = match &entry.state {
			FetchState::Request { pending, queued: true } => pending,
			_ => return Err(WitnessOwnerError::UnknownRequest),
		};
		let source =
			WitnessConnection { node_id: message.peer(), identity: message.connection().clone() };
		// Validate the entire response's identity, manifest, signatures and pagination first.
		let checked =
			pending.check_response(&response, &source).map_err(WitnessOwnerError::Protocol)?;
		entry.state = FetchState::Page { checked };
		Ok(index)
	}
}

impl WitnessOwner {
	/// Begin or advance historical recovery. Current Active state and S connectivity are unnecessary.
	/// A queued request is never resent. Continue a failed storage write before invoking this again.
	pub(crate) fn fetch(
		&mut self, context: &FFORReceiverRecoveryContext, witness: PublicKey,
	) -> Result<FetchProgress, WitnessOwnerError> {
		self.fetch_inner(context, witness, false)
	}

	/// Restart an ambiguous traversal from slot zero with fresh signed request identity and nonce.
	/// Already retained evidence remains. A received page must finish persistence before replacement.
	pub(crate) fn retry_fetch(
		&mut self, context: &FFORReceiverRecoveryContext, witness: PublicKey,
	) -> Result<FetchProgress, WitnessOwnerError> {
		self.fetch_inner(context, witness, true)
	}

	fn fetch_inner(
		&mut self, context: &FFORReceiverRecoveryContext, witness: PublicKey, restart: bool,
	) -> Result<FetchProgress, WitnessOwnerError> {
		self.pending.discard_disconnected(&self.transport);
		self.fetches.discard_disconnected(&self.transport);
		let manifest = self
			.retained_manifests(context)?
			.into_iter()
			.find_map(|(selected, manifest)| (selected == witness).then_some(manifest))
			.ok_or(WitnessOwnerError::UnknownWitness)?;
		let binding = WitnessStorageBinding::from_native_context(context)
			.map_err(WitnessOwnerError::Storage)?;
		let epoch = Epoch {
			channel: context.channel_id(),
			epoch: context.epoch_id(),
			context_digest: context.context_digest(),
		};
		let previous = self.fetches.find(epoch, witness)?;
		if let Some(index) = previous {
			if matches!(self.fetches.entries[index].state, FetchState::Page { .. }) {
				// Protect the received evidence even if restart was requested during storage failure.
				return self.advance_fetch(index, &binding);
			}
			if !restart {
				return self.advance_fetch(index, &binding);
			}
		}
		let connection =
			self.transport.connection(witness).ok_or(WitnessOwnerError::StaleConnection)?;
		let request = self.fresh_fetch(&binding, witness, None)?;
		let pending = PendingFetch::first(
			request,
			manifest,
			WitnessConnection { node_id: witness, identity: connection },
		)
		.map_err(WitnessOwnerError::Protocol)?;
		let index = self.fetches.install(epoch, pending, self.pending.usage())?;
		self.advance_fetch(index, &binding)
	}

	/// Retain an exactly correlated page. Encrypted evidence is persisted before any cursor advances.
	/// Malformed input or a later failure cannot erase records retained from an earlier response.
	pub(crate) fn accept_fetch_page(
		&mut self, message: &ReceivedFforMessage,
	) -> Result<FetchProgress, WitnessOwnerError> {
		if !self.transport.is_current(message) {
			return Err(WitnessOwnerError::StaleConnection);
		}
		let index = self.fetches.accept(message)?;
		let epoch = self.fetches.entries[index].epoch;
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
		self.advance_fetch(index, &binding)
	}

	fn fresh_fetch(
		&self, binding: &WitnessStorageBinding, witness: PublicKey, after_slot: Option<u16>,
	) -> Result<SignedFetch, WitnessOwnerError> {
		let secrets = self.store.load(binding).map_err(WitnessOwnerError::Storage)?;
		for _ in 0..16 {
			let request =
				secrets.prepare_fetch(witness, after_slot).map_err(WitnessOwnerError::KeyUse)?;
			let parameters = request.unsigned().parameters();
			if !self.pending.contains_request_id(parameters.request_id)
				&& !self.fetches.contains_request_id(parameters.request_id)
				&& self
					.fetches
					.entries
					.iter()
					.all(|entry| entry.source.node_id != witness || entry.nonce != parameters.nonce)
			{
				return Ok(request);
			}
		}
		Err(WitnessOwnerError::Entropy)
	}

	fn advance_fetch(
		&mut self, index: usize, binding: &WitnessStorageBinding,
	) -> Result<FetchProgress, WitnessOwnerError> {
		let entry = &mut self.fetches.entries[index];
		if let FetchState::Page { checked } = &mut entry.state {
			// One bounded page write retains every valid core before any cursor advances.
			let retained = self
				.store
				.retain_receipt_page(binding, entry.source.node_id, checked)
				.map_err(WitnessOwnerError::Storage)?;
			if retained == ReceiptPageRetention::Rejected {
				self.fetches.entries.remove(index);
				return Ok(FetchProgress::RejectedPage);
			}
			if checked.next_after_slot().is_none() {
				self.fetches.entries.remove(index);
				return Ok(FetchProgress::Complete);
			}
			if self.transport.connection(entry.source.node_id).as_ref()
				!= Some(&entry.source.identity)
			{
				self.fetches.entries.remove(index);
				return Ok(FetchProgress::RestartRequired);
			}
			let witness = entry.source.node_id;
			let after = checked.next_after_slot();
			let request = self.fresh_fetch(binding, witness, after)?;
			let entry = &mut self.fetches.entries[index];
			let FetchState::Page { checked, .. } = &entry.state else { unreachable!() };
			let pending = checked.next(request).map_err(WitnessOwnerError::Protocol)?;
			let parameters = pending.request().unsigned().parameters();
			entry.request_id = parameters.request_id;
			entry.nonce = parameters.nonce;
			entry.state = FetchState::Request { pending, queued: false };
		}
		let entry = &mut self.fetches.entries[index];
		let FetchState::Request { pending, queued } = &mut entry.state else { unreachable!() };
		if *queued {
			return Ok(FetchProgress::AwaitingResponse);
		}
		match self.transport.enqueue_fetch(
			entry.source.node_id,
			&entry.source.identity,
			pending.request(),
		) {
			Ok(()) => {
				*queued = true;
				Ok(FetchProgress::Queued)
			},
			Err(OutboundError::Capacity) => Ok(FetchProgress::Backpressured),
			Err(OutboundError::StaleConnection) => {
				self.fetches.entries.remove(index);
				Err(WitnessOwnerError::StaleConnection)
			},
			Err(OutboundError::InvalidMessage) => Err(WitnessOwnerError::Conflict),
		}
	}
}

#[cfg(test)]
pub(super) mod tests;

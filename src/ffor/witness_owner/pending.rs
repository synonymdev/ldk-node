//! Bounded transient correlation. This table carries no native phase or persistence authority.

use std::sync::Arc;

use lightning::ln::types::ChannelId;
use lightning_ffor::witness::{
	Acknowledgement, CheckedAcknowledgement, PendingProvision, Provision, SignedManifest,
	WitnessConnection,
};
use rand::{rngs::OsRng, TryRngCore};

use crate::message_handler::ffor::{ConnectionToken, FforReceiverTransport, ReceivedFforMessage};

use super::WitnessOwnerError;

const MAX_PENDING: usize = 64;
const MAX_PENDING_BYTES: usize = 8 * 1024 * 1024;

/// Historical lookup identity, checked again against the native archive before using the sidecar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Epoch {
	pub(super) channel: ChannelId,
	pub(super) epoch: [u8; 32],
	pub(super) context_digest: [u8; 32],
}

struct Entry {
	epoch: Epoch,
	request: Arc<PendingProvision<ConnectionToken>>,
	encoded_bytes: usize,
	queued: bool,
}

pub(super) struct PendingProvisions {
	entries: Vec<Entry>,
	encoded_bytes: usize,
	available_count: usize,
	available_bytes: usize,
}

impl Default for PendingProvisions {
	fn default() -> Self {
		Self {
			entries: Vec::new(),
			encoded_bytes: 0,
			available_count: MAX_PENDING,
			available_bytes: MAX_PENDING_BYTES,
		}
	}
}

impl PendingProvisions {
	pub(super) fn usage(&self) -> (usize, usize) {
		(self.entries.len(), self.encoded_bytes)
	}

	pub(super) fn contains_request_id(&self, id: [u8; 16]) -> bool {
		self.entries.iter().any(|entry| entry.request.provision().request_id() == id)
	}

	/// Called under the exclusive owner before admission; existing work is never evicted.
	pub(super) fn set_other_usage(
		&mut self, count: usize, bytes: usize,
	) -> Result<(), WitnessOwnerError> {
		let available_count = MAX_PENDING.checked_sub(count).ok_or(WitnessOwnerError::Capacity)?;
		let available_bytes =
			MAX_PENDING_BYTES.checked_sub(bytes).ok_or(WitnessOwnerError::Capacity)?;
		if self.entries.len() > available_count || self.encoded_bytes > available_bytes {
			return Err(WitnessOwnerError::Capacity);
		}
		self.available_count = available_count;
		self.available_bytes = available_bytes;
		Ok(())
	}
	pub(super) fn stage(
		&mut self, epoch: Epoch, source: WitnessConnection<ConnectionToken>,
		manifest: SignedManifest,
	) -> Result<Arc<PendingProvision<ConnectionToken>>, WitnessOwnerError> {
		self.stage_with(epoch, source, manifest, random_id)
	}

	pub(super) fn stage_excluding(
		&mut self, epoch: Epoch, source: WitnessConnection<ConnectionToken>,
		manifest: SignedManifest, excluded: impl Fn([u8; 16]) -> bool,
	) -> Result<Arc<PendingProvision<ConnectionToken>>, WitnessOwnerError> {
		self.stage_with(epoch, source, manifest, || fresh_external_id(&excluded))
	}

	fn stage_with(
		&mut self, epoch: Epoch, source: WitnessConnection<ConnectionToken>,
		manifest: SignedManifest, draw: impl FnMut() -> Result<[u8; 16], WitnessOwnerError>,
	) -> Result<Arc<PendingProvision<ConnectionToken>>, WitnessOwnerError> {
		let previous = self.existing(epoch, &source, &manifest)?;
		if let Some(index) = previous {
			if self.entries[index].request.connection() == &source {
				return Ok(Arc::clone(&self.entries[index].request));
			}
		}
		self.install_new(epoch, source, manifest, previous, draw)
	}

	pub(super) fn restart(
		&mut self, epoch: Epoch, source: WitnessConnection<ConnectionToken>,
		manifest: SignedManifest,
	) -> Result<Arc<PendingProvision<ConnectionToken>>, WitnessOwnerError> {
		let previous = self.existing(epoch, &source, &manifest)?;
		self.install_new(epoch, source, manifest, previous, random_id)
	}

	pub(super) fn restart_excluding(
		&mut self, epoch: Epoch, source: WitnessConnection<ConnectionToken>,
		manifest: SignedManifest, excluded: impl Fn([u8; 16]) -> bool,
	) -> Result<Arc<PendingProvision<ConnectionToken>>, WitnessOwnerError> {
		let previous = self.existing(epoch, &source, &manifest)?;
		self.install_new(epoch, source, manifest, previous, || fresh_external_id(&excluded))
	}

	fn existing(
		&self, epoch: Epoch, source: &WitnessConnection<ConnectionToken>, manifest: &SignedManifest,
	) -> Result<Option<usize>, WitnessOwnerError> {
		let previous = self.entries.iter().position(|entry| {
			entry.epoch.channel == epoch.channel
				&& entry.epoch.epoch == epoch.epoch
				&& entry.request.connection().node_id == source.node_id
		});
		if let Some(index) = previous {
			let entry = &self.entries[index];
			if entry.epoch.context_digest != epoch.context_digest
				|| entry.request.provision().manifest() != manifest
			{
				return Err(WitnessOwnerError::Conflict);
			}
		}
		Ok(previous)
	}

	fn install_new(
		&mut self, epoch: Epoch, source: WitnessConnection<ConnectionToken>,
		manifest: SignedManifest, previous: Option<usize>,
		mut draw: impl FnMut() -> Result<[u8; 16], WitnessOwnerError>,
	) -> Result<Arc<PendingProvision<ConnectionToken>>, WitnessOwnerError> {
		// Keep the predecessor while drawing: even an explicit retry cannot reuse its request ID.
		let id = self.fresh_id(&mut draw)?;
		let provision = Provision::new(id, manifest);
		let bytes = provision.encode().len();
		let previous_bytes = previous.map_or(0, |index| self.entries[index].encoded_bytes);
		let total = self.encoded_bytes - previous_bytes;
		if (previous.is_none() && self.entries.len() >= self.available_count)
			|| total.checked_add(bytes).filter(|size| *size <= self.available_bytes).is_none()
		{
			return Err(WitnessOwnerError::Capacity);
		}
		let request = Arc::new(PendingProvision::new(provision, source));
		let entry =
			Entry { epoch, request: Arc::clone(&request), encoded_bytes: bytes, queued: false };
		if let Some(index) = previous {
			self.entries[index] = entry;
		} else {
			self.entries.push(entry);
		}
		self.encoded_bytes = total + bytes;
		Ok(request)
	}

	pub(super) fn is_queued(&self, request: &Arc<PendingProvision<ConnectionToken>>) -> bool {
		self.entries.iter().any(|entry| Arc::ptr_eq(&entry.request, request) && entry.queued)
	}

	fn fresh_id(
		&self, draw: &mut impl FnMut() -> Result<[u8; 16], WitnessOwnerError>,
	) -> Result<[u8; 16], WitnessOwnerError> {
		for _ in 0..16 {
			let id = draw()?;
			if self.entries.iter().all(|entry| entry.request.provision().request_id() != id) {
				return Ok(id);
			}
		}
		Err(WitnessOwnerError::Entropy)
	}

	pub(super) fn mark_queued(&mut self, request: &Arc<PendingProvision<ConnectionToken>>) {
		if let Some(entry) =
			self.entries.iter_mut().find(|entry| Arc::ptr_eq(&entry.request, request))
		{
			entry.queued = true;
		}
	}

	pub(super) fn check(
		&self, message: &ReceivedFforMessage, ack: &Acknowledgement,
	) -> Result<(Epoch, CheckedAcknowledgement<ConnectionToken>), WitnessOwnerError> {
		let entry = self
			.entries
			.iter()
			.find(|entry| {
				entry.queued && entry.request.provision().request_id() == ack.request_id()
			})
			.ok_or(WitnessOwnerError::UnknownRequest)?;
		let source =
			WitnessConnection { node_id: message.peer(), identity: message.connection().clone() };
		let checked = entry
			.request
			.check_acknowledgement(ack, &source)
			.map_err(WitnessOwnerError::Protocol)?;
		Ok((entry.epoch, checked))
	}

	pub(super) fn complete(&mut self, request_id: [u8; 16]) {
		if let Some(index) = self
			.entries
			.iter()
			.position(|entry| entry.request.provision().request_id() == request_id)
		{
			self.encoded_bytes -= self.entries.remove(index).encoded_bytes;
		}
	}

	pub(super) fn discard_disconnected(&mut self, transport: &FforReceiverTransport) {
		self.entries.retain(|entry| {
			transport.connection(entry.request.connection().node_id).as_ref()
				== Some(&entry.request.connection().identity)
		});
		self.encoded_bytes = self.entries.iter().map(|entry| entry.encoded_bytes).sum();
	}
}

fn random_id() -> Result<[u8; 16], WitnessOwnerError> {
	let mut id = [0; 16];
	OsRng.try_fill_bytes(&mut id).map_err(|_| WitnessOwnerError::Entropy)?;
	Ok(id)
}

fn fresh_external_id(excluded: &impl Fn([u8; 16]) -> bool) -> Result<[u8; 16], WitnessOwnerError> {
	for _ in 0..16 {
		let id = random_id()?;
		if !excluded(id) {
			return Ok(id);
		}
	}
	Err(WitnessOwnerError::Entropy)
}

#[cfg(test)]
mod tests;

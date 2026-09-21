//! Concrete native join. Historical correlation never substitutes for exact native intent checks.

use std::sync::Arc;

use bitcoin::blockdata::constants::ChainHash;
use lightning::ln::ffor::{FFORPeerConnection, FFORReceiverId};

use crate::types::DynStore;
use crate::Node;

use super::{RequestIntent, RequestPlan, RequestStore, RequestStoreError, StoredRequest};

impl RequestStore {
	/// Capture actual Node identity and its concrete manager, without retaining the Node itself.
	/// The internal caller supplies the same wallet seed and backing store used for that Node.
	pub(in crate::ffor) fn open(
		seed: &[u8; 64], node: &Node, storage: Arc<DynStore>,
	) -> Result<Self, RequestStoreError> {
		Self::open_bound(
			seed,
			ChainHash::using_genesis_block(node.config.network).to_bytes(),
			node.node_id(),
			Arc::clone(&node.channel_manager),
			storage,
		)
	}

	/// Persist immutable arguments before native preparation. Existing native allocation without
	/// its application record is a refusal, not permission to reconstruct a missing description.
	pub(in crate::ffor) fn begin(
		&mut self, intent: RequestIntent, plan: RequestPlan,
	) -> Result<StoredRequest, RequestStoreError> {
		let previous = self.lookup(intent.client_id())?;
		let candidate = StoredRequest::new(intent, plan)?;
		if candidate.local_request_id() != self.local_request_id(candidate.intent().client_id())?
			|| candidate.plan().settlement == self.node
		{
			return Err(RequestStoreError::InvalidIntent);
		}
		if let Some(previous) = previous {
			if !previous.same_request(&candidate) {
				return Err(RequestStoreError::Conflict);
			}
			return Ok(previous);
		}
		if self
			.manager
			.find_ffor_receiver_request(candidate.local_request_id())
			.map_err(RequestStoreError::Native)?
			.is_some()
		{
			return Err(RequestStoreError::Missing);
		}
		self.write_record(&candidate)?;
		Ok(candidate)
	}

	/// Retry native preparation with the exact stored terms on a genuine current S connection.
	/// Native revalidates intent even when recovering an earlier allocation. No Init is released.
	/// The caller must not advance native until this binding write succeeds. A coroutine detaching
	/// does not cancel native work; its durable intent remains recoverable through this same method.
	pub(in crate::ffor) fn prepare_native(
		&mut self, client_id: &str, connection: &FFORPeerConnection,
	) -> Result<FFORReceiverId, RequestStoreError> {
		let before = self.lookup(client_id)?.ok_or(RequestStoreError::Missing)?;
		if connection.peer_node_id() != before.plan().settlement {
			return Err(RequestStoreError::Identity);
		}
		let found = self
			.manager
			.find_ffor_receiver_request(before.local_request_id())
			.map_err(RequestStoreError::Native)?;
		if let Some(retained) = before.selector() {
			let actual = found.as_ref().ok_or(RequestStoreError::MissingNative)?;
			if !retained.matches(actual) {
				return Err(RequestStoreError::Conflict);
			}
		}
		if found.as_ref().is_some_and(|id| id.channel_id() != before.plan().channel) {
			return Err(RequestStoreError::Conflict);
		}
		// There is no store mutex or I/O under native locks. &mut self excludes another store
		// operation, and the re-read below also catches unsupported external namespace replacement.
		let actual = self
			.manager
			.prepare_ffor_receiver(
				&before.plan().channel,
				connection,
				before.plan().parameters.clone(),
			)
			.map_err(RequestStoreError::Native)?;
		if found.is_some_and(|previous| previous != actual)
			|| self
				.manager
				.find_ffor_receiver_request(before.local_request_id())
				.map_err(RequestStoreError::Native)?
				!= Some(actual)
		{
			return Err(RequestStoreError::Conflict);
		}
		let mut current = self.lookup(client_id)?.ok_or(RequestStoreError::Missing)?;
		if current.encode() != before.encode() {
			return Err(RequestStoreError::Conflict);
		}
		if current.bind(&actual)? {
			self.write_record(&current)?;
		}
		Ok(actual)
	}
}

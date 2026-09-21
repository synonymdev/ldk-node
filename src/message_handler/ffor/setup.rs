//! Private setup bridge. The concrete native manager owns admission, signatures and persistence.
//! No production builder constructs this bridge, and setup progress grants no invoice authority.

use std::sync::Arc;

use bitcoin::secp256k1::PublicKey;
use lightning::ln::ffor::{
	FFORCommitmentError, FFORReceiverError, FFORReceiverId, FFORReceiverParameters,
	FFORReceiverProgress,
};
use lightning::ln::types::ChannelId;

use super::{FforFrame, FforReceiverTransport, NativeConnection};
use crate::types::ChannelManager;

/// Holds no duplicate epoch or wire state. Connection pairs live in the bounded transport map.
pub(crate) struct FforSetupAdapter {
	manager: Arc<ChannelManager>,
	transport: Arc<FforReceiverTransport>,
}

// The production builder deliberately does not install this experimental adapter yet.
#[allow(dead_code)]
impl FforSetupAdapter {
	pub(crate) fn new(manager: Arc<ChannelManager>, transport: Arc<FforReceiverTransport>) -> Self {
		Self { manager, transport }
	}

	pub(in crate::message_handler) fn transport(&self) -> &Arc<FforReceiverTransport> {
		&self.transport
	}

	pub(in crate::message_handler) fn peer_connected(&self, peer: PublicKey) {
		// PeerManager calls native peer_connected first. Capture with no transport lock held.
		// Failure leaves ordinary LSPS available but installs no FFOR connection or authority.
		if let Ok(native) = self.manager.ffor_peer_connection(&peer) {
			self.transport.connect(native.peer_node_id(), Some(native));
		}
	}

	fn connection(&self, peer: PublicKey) -> Result<NativeConnection, FFORReceiverError> {
		self.transport
			.native_connection(peer)
			.ok_or_else(|| FFORCommitmentError::ChannelUnavailable.into())
	}

	pub(in crate::message_handler) fn handle(
		&self, peer: PublicKey, frame: &FforFrame,
	) -> Result<FFORReceiverProgress, FFORReceiverError> {
		let connection = self.connection(peer)?;
		// Synchronous completion precedes PeerManager's next ordinary HTLC frame. Cloning the
		// pair released the transport mutex; only native can accept or reject its generation.
		self.manager.handle_ffor_receiver_message(&connection.native, frame.wire())
	}

	pub(crate) fn prepare(
		&self, peer: PublicKey, channel: &ChannelId, parameters: FFORReceiverParameters,
	) -> Result<FFORReceiverId, FFORReceiverError> {
		let connection = self.connection(peer)?;
		self.manager.prepare_ffor_receiver(channel, &connection.native, parameters)
	}

	pub(crate) fn find(
		&self, local_request_id: [u8; 32],
	) -> Result<Option<FFORReceiverId>, FFORReceiverError> {
		self.manager.find_ffor_receiver_request(local_request_id)
	}

	pub(crate) fn advance(
		&self, peer: PublicKey, id: &FFORReceiverId,
	) -> Result<FFORReceiverProgress, FFORReceiverError> {
		let connection = self.connection(peer)?;
		self.manager.advance_ffor_receiver(id, &connection.native, |wire| {
			// Native -> transport is the only nested lock order. No I/O or manager reentry.
			// Refusal leaves the exact Init and one-shot release permission owned natively.
			self.transport.enqueue(peer, &connection.transport, wire).map_err(|_| ())
		})
	}

	pub(crate) fn cancel(
		&self, peer: PublicKey, id: &FFORReceiverId,
	) -> Result<FFORReceiverProgress, FFORReceiverError> {
		let connection = self.connection(peer)?;
		self.manager.cancel_ffor_receiver_setup(id, &connection.native)
	}
}

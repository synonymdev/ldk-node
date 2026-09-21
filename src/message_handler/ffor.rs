//! Bounded connection-scoped transport state. Successful parsing grants no protocol authority.
//!
//! Only PeerManager's authenticated callbacks supply peer identity. A connection token identifies
//! one successful init callback, including reconnects to the same key. Consumers must recheck that
//! token under the eventual native channel authority before mutation; a queue pop is not a lock on
//! the connection. An outbound enqueue callback checks its token and queue capacity atomically;
//! the native caller must first authorize and durably retain the exact bytes. No protocol transition
//! or signature generation exists here, and the production builder leaves this transport disabled.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex};

use bitcoin::secp256k1::PublicKey;
use lightning::io;
use lightning::ln::msgs::{DecodeError, ErrorAction, LightningError};
use lightning::ln::wire::Type;
use lightning::util::logger::Level;
use lightning::util::ser::{LengthLimitedRead, Writeable, Writer};
use lightning_ffor::wire::{Message, MAX_MESSAGE_LEN};
use lightning_ffor::witness::Acknowledgement;

const MAX_PEERS: usize = 64;
const MAX_PEER_MESSAGES: usize = 8;
const MAX_PEER_BYTES: usize = 256 * 1024;
const MAX_QUEUED_MESSAGES: usize = 128;
const MAX_QUEUED_BYTES: usize = 1024 * 1024;

/// Exact canonical input, including the message type. Authentication remains the consumer's job.
#[derive(PartialEq, Eq)]
pub(crate) struct FforFrame(Vec<u8>);

// PeerManager traces received custom messages through Debug. Lifecycle bodies may contain
// preimages, so even a transport-only representation must omit the entire wire payload.
impl fmt::Debug for FforFrame {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("FforFrame")
			.field("message_type", &self.type_id())
			.field("length", &self.0.len())
			.finish()
	}
}

impl FforFrame {
	pub(super) fn handles_type(message_type: u16) -> bool {
		matches!(message_type, 55001 | 55003 | 55045 | 55047 | 55049 | 55051 | 55053 | 55057)
	}

	pub(super) fn read<R: LengthLimitedRead>(
		message_type: u16, reader: &mut R,
	) -> Result<Self, DecodeError> {
		let body_len = reader.remaining_bytes();
		if !Self::handles_type(message_type) || body_len > (MAX_MESSAGE_LEN - 2) as u64 {
			return Err(DecodeError::InvalidValue);
		}
		let mut bytes = vec![0; body_len as usize + 2];
		bytes[..2].copy_from_slice(&message_type.to_be_bytes());
		reader.read_exact(&mut bytes[2..])?;
		Self::validate_wire(&bytes)?;
		Ok(Self(bytes))
	}

	fn validate_wire(bytes: &[u8]) -> Result<(), DecodeError> {
		if bytes.len() < 2 || bytes.len() > MAX_MESSAGE_LEN {
			return Err(DecodeError::InvalidValue);
		}
		let message_type = u16::from_be_bytes([bytes[0], bytes[1]]);
		if !Self::handles_type(message_type) {
			return Err(DecodeError::InvalidValue);
		}
		if message_type == 55057 {
			Acknowledgement::decode(bytes).map_err(|_| DecodeError::InvalidValue)?;
		} else {
			Message::decode(bytes).map_err(|_| DecodeError::InvalidValue)?;
		}
		Ok(())
	}

	// The eventual receiver consumes exact bytes for native authentication and transcript binding.
	#[allow(dead_code)]
	pub(crate) fn wire(&self) -> &[u8] {
		&self.0
	}
}

impl Type for FforFrame {
	fn type_id(&self) -> u16 {
		u16::from_be_bytes([self.0[0], self.0[1]])
	}
}

impl Writeable for FforFrame {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), io::Error> {
		writer.write_all(&self.0[2..])
	}
}

/// Unforgeable outside this module and never numerically reused or wrapped.
///
/// Pointer equality is intentional. Retaining an old token also keeps its allocation alive, so a
/// reconnect cannot acquire the same identity even if both connections use the same node key.
#[derive(Clone, Debug)]
pub(crate) struct ConnectionToken(Arc<()>);

impl PartialEq for ConnectionToken {
	fn eq(&self, other: &Self) -> bool {
		Arc::ptr_eq(&self.0, &other.0)
	}
}

impl Eq for ConnectionToken {}

/// A bounded input carrying only transport-authenticated identity, never claimed wire identity.
#[derive(Debug)]
pub(crate) struct ReceivedFforMessage {
	peer: PublicKey,
	connection: ConnectionToken,
	frame: FforFrame,
}

// These accessors are the private receiver seam, deliberately unused by production code for now.
#[allow(dead_code)]
impl ReceivedFforMessage {
	pub(crate) fn peer(&self) -> PublicKey {
		self.peer
	}
	pub(crate) fn connection(&self) -> &ConnectionToken {
		&self.connection
	}
	pub(crate) fn frame(&self) -> &FforFrame {
		&self.frame
	}
}

struct PeerState {
	connection: ConnectionToken,
	messages: usize,
	bytes: usize,
}

#[derive(Default)]
struct QueueState {
	peers: HashMap<PublicKey, PeerState>,
	queue: VecDeque<ReceivedFforMessage>,
	outbound: VecDeque<ReceivedFforMessage>,
	bytes: usize,
}

impl QueueState {
	// Remove accounting and payloads together while holding the only state lock.
	fn disconnect(&mut self, peer: PublicKey) {
		if let Some(removed) = self.peers.remove(&peer) {
			self.bytes -= removed.bytes;
			self.queue.retain(|message| message.peer != peer);
			self.outbound.retain(|message| message.peer != peer);
		}
	}

	fn at_capacity(&self, peer: &PeerState, bytes: usize) -> bool {
		peer.messages >= MAX_PEER_MESSAGES
			|| peer.bytes + bytes > MAX_PEER_BYTES
			|| self.queue.len() + self.outbound.len() >= MAX_QUEUED_MESSAGES
			|| self.bytes + bytes > MAX_QUEUED_BYTES
	}

	fn remove_accounting(&mut self, message: &ReceivedFforMessage) {
		let bytes = message.frame.0.len();
		let peer = self.peers.get_mut(&message.peer).unwrap();
		peer.messages -= 1;
		peer.bytes -= bytes;
		self.bytes -= bytes;
	}
}

/// Optional inbound mailbox and outbound queue sharing one bounded capacity budget.
/// It is never constructed by the production builder.
#[derive(Default)]
pub(crate) struct FforReceiverTransport {
	state: Mutex<QueueState>,
}

impl FforReceiverTransport {
	pub(super) fn peer_connected(&self, peer: PublicKey) {
		let mut state = self.state.lock().unwrap();
		// Defensive replacement also clears old work if callbacks are repeated without disconnect.
		state.disconnect(peer);
		if state.peers.len() < MAX_PEERS {
			state.peers.insert(
				peer,
				PeerState { connection: ConnectionToken(Arc::new(())), messages: 0, bytes: 0 },
			);
		}
	}

	pub(super) fn peer_disconnected(&self, peer: PublicKey) {
		self.state.lock().unwrap().disconnect(peer);
	}

	pub(super) fn receive(&self, peer: PublicKey, frame: FforFrame) -> Result<(), LightningError> {
		let mut state = self.state.lock().unwrap();
		let peer_state = state.peers.get(&peer).ok_or_else(refused)?;
		let bytes = frame.0.len();
		if state.at_capacity(peer_state, bytes) {
			return Err(refused());
		}
		let connection = peer_state.connection.clone();
		let peer_state = state.peers.get_mut(&peer).unwrap();
		peer_state.messages += 1;
		peer_state.bytes += bytes;
		state.bytes += bytes;
		state.queue.push_back(ReceivedFforMessage { peer, connection, frame });
		Ok(())
	}

	/// Captures only a transport generation, not native protocol or persistence authority.
	#[allow(dead_code)]
	pub(crate) fn connection(&self, peer: PublicKey) -> Option<ConnectionToken> {
		self.state.lock().unwrap().peers.get(&peer).map(|entry| entry.connection.clone())
	}

	/// Enqueues exact bytes inside a native-authorized release callback, using native -> transport
	/// lock order. This method performs no I/O or manager calls. An error preserves all queued work
	/// and must be returned to native so it retains retry ownership. Success means queue acceptance,
	/// never network delivery. Disconnect discards accepted connection-scoped work.
	#[allow(dead_code)]
	pub(crate) fn enqueue(
		&self, peer: PublicKey, connection: &ConnectionToken, wire: &[u8],
	) -> Result<(), OutboundError> {
		FforFrame::validate_wire(wire).map_err(|_| OutboundError::InvalidMessage)?;
		let mut state = self.state.lock().unwrap();
		let peer_state = state.peers.get(&peer).ok_or(OutboundError::StaleConnection)?;
		if peer_state.connection != *connection {
			return Err(OutboundError::StaleConnection);
		}
		// A retry already retained on this connection needs neither capacity nor another copy.
		if state.outbound.iter().any(|message| message.peer == peer && message.frame.wire() == wire)
		{
			return Ok(());
		}
		if state.at_capacity(peer_state, wire.len()) {
			return Err(OutboundError::Capacity);
		}
		let frame = FforFrame(wire.to_vec());
		let peer_state = state.peers.get_mut(&peer).unwrap();
		peer_state.messages += 1;
		peer_state.bytes += wire.len();
		state.bytes += wire.len();
		state.outbound.push_back(ReceivedFforMessage {
			peer,
			connection: connection.clone(),
			frame,
		});
		Ok(())
	}

	/// Called only from CustomMessageHandler's drain. The pinned PeerManager holds peers.read from
	/// that callback through selection/encryption into the same peer's socket queue. Disconnect and
	/// replacement require peers.write, and Noise refuses duplicate node-id mappings.
	pub(super) fn drain_outbound(&self) -> Vec<(PublicKey, FforFrame)> {
		let mut state = self.state.lock().unwrap();
		let mut messages = Vec::with_capacity(state.outbound.len());
		while let Some(message) = state.outbound.pop_front() {
			state.remove_accounting(&message);
			messages.push((message.peer, message.frame));
		}
		messages
	}

	// Popping does not authorize any protocol action or retain the connection lock for the caller.
	#[allow(dead_code)]
	pub(crate) fn pop(&self) -> Option<ReceivedFforMessage> {
		let mut state = self.state.lock().unwrap();
		let message = state.queue.pop_front()?;
		state.remove_accounting(&message);
		Some(message)
	}

	#[allow(dead_code)]
	pub(crate) fn is_current(&self, message: &ReceivedFforMessage) -> bool {
		self.state
			.lock()
			.unwrap()
			.peers
			.get(&message.peer)
			.is_some_and(|peer| peer.connection == message.connection)
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutboundError {
	InvalidMessage,
	StaleConnection,
	Capacity,
}

fn refused() -> LightningError {
	LightningError {
		err: "FFOR receive transport unavailable or at capacity".to_owned(),
		action: ErrorAction::IgnoreAndLog(Level::Debug),
	}
}

#[cfg(test)]
mod tests;

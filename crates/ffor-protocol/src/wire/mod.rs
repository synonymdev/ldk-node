//! Bounded canonical encodings for the signed Variant D setup and lifecycle messages.
//!
//! Peer bytes are untrusted. Decoding checks structure, low-S signature encoding and
//! message-local invariants, but does not authenticate the sender or establish channel
//! readiness. Call [`Message::verify_signature`] with the expected channel peer before
//! acting, then validate the transcript and live channel state. Unknown odd TLVs are
//! retained byte-for-byte; unknown even TLVs are rejected. No signing keys are accepted.
//!
//! ```
//! use ffor_protocol::wire::{Message, WireError};
//! assert_eq!(Message::decode(&[]), Err(WireError::Truncated));
//! ```

mod codec;
mod lifecycle;
mod setup;
mod tlv;
mod types;

use std::fmt;

use bitcoin::secp256k1::PublicKey;
pub use types::{Abort, Accept, Activate, CloseAck, Header, Init, Payload, Preimage, Tlv};

use crate::transcript::{self, Digest};

/// BOLT 8 maximum plaintext peer message, including its two-byte type.
pub const MAX_MESSAGE_LEN: usize = 65_535;
/// Maximum number of TLVs that can fit a peer message even with empty values.
pub const MAX_TLV_COUNT: usize = MAX_MESSAGE_LEN / 2;

/// Structural or authentication failure. No error authorizes a channel transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireError {
	/// The message, field, or BigSize value ended early.
	Truncated,
	/// A message or bounded collection exceeds its protocol limit.
	SizeLimit,
	/// This message type is not implemented by this Variant D codec.
	UnsupportedMessage(u16),
	/// The init selected a variant other than D.
	UnsupportedVariant(u8),
	/// A BigSize value used a longer-than-minimal representation.
	NonCanonicalBigSize,
	/// TLV types were duplicated or not strictly increasing.
	TlvOrder,
	/// An unknown mandatory even TLV appeared.
	UnknownEvenTlv(u64),
	/// A required known TLV is absent.
	MissingTlv(u64),
	/// A known TLV has an invalid value length or is forbidden for this message.
	InvalidTlv(u64),
	/// A fixed field or count violates the message's local invariants.
	InvalidField,
	/// A compressed secp256k1 point is invalid or is not compressed.
	InvalidPoint,
	/// The compact signature is malformed, high-S, or fails authentication.
	InvalidSignature,
	/// An accept does not bind the supplied init's header, amounts, or transcript.
	SetupMismatch,
}

impl fmt::Display for WireError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "invalid FFOR wire message: {self:?}")
	}
}

impl std::error::Error for WireError {}

/// A parsed signed envelope. Public fields remain untrusted until signature verification.
///
/// Encoding revalidates all fields and produces canonical bytes. It never signs or repairs
/// a signature: changing any header, payload or extension requires a new external signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
	/// Channel and epoch to which the signed message belongs.
	pub header: Header,
	/// Known message fields and known TLVs.
	pub payload: Payload,
	/// Unknown odd TLVs, in strictly increasing type order.
	pub extensions: Vec<Tlv>,
	/// Unverified compact low-S ECDSA signature in the final 64 wire bytes.
	pub signature: [u8; 64],
}

impl Message {
	/// Decode one complete peer message with no trailing framing bytes.
	///
	/// All input-dependent allocation is bounded by [`MAX_MESSAGE_LEN`]. Successful
	/// decoding does not verify the signature, live limits, or lifecycle ordering.
	pub fn decode(wire: &[u8]) -> Result<Self, WireError> {
		codec::decode(wire)
	}

	/// Encode canonical wire bytes after rechecking message-local invariants.
	///
	/// An existing signature is retained unchanged, even if fields were modified. Call
	/// [`Self::verify_signature`] before transmitting or acting on a reconstructed message.
	pub fn encode(&self) -> Result<Vec<u8>, WireError> {
		codec::encode(self)
	}

	/// The assigned two-byte message type, without authenticating the message.
	pub fn message_type(&self) -> u16 {
		match self.payload {
			Payload::Init(_) => 55001,
			Payload::Accept(_) => 55003,
			Payload::Activate(_) => 55045,
			Payload::ActivateAck(_) => 55047,
			Payload::Abort(_) => 55049,
			Payload::Close(_) => 55051,
			Payload::CloseAck(_) => 55053,
		}
	}

	/// The existing transcript module's domain-separated digest over the unsigned body.
	///
	/// The signature field is intentionally ignored so the channel's existing signer can
	/// sign newly constructed terms. All unsigned fields and the final envelope size are checked.
	pub fn signature_digest(&self) -> Result<Digest, WireError> {
		let wire = codec::encode_unsigned(self)?;
		Ok(transcript::message_digest(self.message_type(), &wire[2..]))
	}

	/// Authenticate the canonical envelope with the expected channel peer's node key.
	///
	/// This proves only the signed bytes, not the correctness of the peer's claims.
	pub fn verify_signature(&self, expected_signer: &PublicKey) -> Result<(), WireError> {
		let wire = self.encode()?;
		transcript::verify_message_signature(
			self.message_type(),
			&wire[2..wire.len() - 64],
			&self.signature,
			expected_signer,
		)
		.map_err(|_| WireError::InvalidSignature)
	}

	/// Check this accept against the exact canonical signed init it answers.
	///
	/// Verify both signatures first. Live channel capacity, negotiated limits, deadline
	/// margins and previously disclosed commitment secrets remain the engine's checks.
	pub fn validate_accept(&self, init: &Message) -> Result<(), WireError> {
		let (Payload::Accept(accept), Payload::Init(terms)) = (&self.payload, &init.payload) else {
			return Err(WireError::SetupMismatch);
		};
		self.encode()?;
		let init_wire = init.encode()?;
		if self.header != init.header
			|| accept.amounts_msat != terms.amounts_msat
			|| accept.init_hash != transcript::init_hash(&init_wire)
		{
			return Err(WireError::SetupMismatch);
		}
		Ok(())
	}
}

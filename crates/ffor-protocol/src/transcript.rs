//! Domain-separated SHA256 transcript primitives from FFOR section 7.5.
//!
//! Inputs are public transcript data. The caller must validate canonical wire encodings and
//! signatures before using these digests. Hashing alone authenticates nothing.

use bitcoin::hashes::{sha256, Hash, HashEngine};
use bitcoin::secp256k1::ecdsa::Signature;
use bitcoin::secp256k1::{Message, PublicKey, Secp256k1};
use std::fmt;

/// A 32-byte SHA256 transcript digest in raw byte order.
pub type Digest = [u8; 32];

/// Failure to authenticate a compact node-key signature from FFOR section 7.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignatureError {
	/// The signature is not a valid compact ECDSA encoding.
	Malformed,
	/// The signature uses a noncanonical high-S scalar.
	HighS,
	/// The expected node did not sign this message type and exact unsigned body.
	Invalid,
}

impl fmt::Display for SignatureError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "invalid FFOR message signature: {self:?}")
	}
}

impl std::error::Error for SignatureError {}

/// Authenticate the exact unsigned body under the expected peer's node key.
///
/// Uses the single SHA256 digest and low-S compact ECDSA encoding required by section 7.
/// The body includes the channel and epoch identifiers and every TLV, without a message type
/// prefix or trailing signature. Do not reconstruct it while discarding unknown odd TLVs.
/// The expected key must come from the authenticated channel peer, not the message itself.
///
/// This verifies identity and byte integrity only. The caller must still decode a canonical
/// supported message, check the sender's role, validate the transcript and enforce epoch state.
/// No signing key is used or exposed.
///
/// ```
/// use bitcoin::secp256k1::PublicKey;
/// use ffor_protocol::transcript::verify_message_signature;
/// let node = PublicKey::from_slice(&[
///     2, 0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62,
///     0x95, 0xce, 0x87, 0x0b, 0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28,
///     0xd9, 0x59, 0xf2, 0x81, 0x5b, 0x16, 0xf8, 0x17, 0x98,
/// ]).unwrap();
/// assert!(verify_message_signature(55001, &[0; 64], &[0; 64], &node).is_err());
/// ```
pub fn verify_message_signature(
	message_type: u16, unsigned_body: &[u8], compact_signature: &[u8; 64],
	expected_signer: &PublicKey,
) -> Result<(), SignatureError> {
	let signature =
		Signature::from_compact(compact_signature).map_err(|_| SignatureError::Malformed)?;
	let mut normalized = signature;
	normalized.normalize_s();
	if normalized != signature {
		return Err(SignatureError::HighS);
	}
	let message = Message::from_digest(message_digest(message_type, unsigned_body));
	Secp256k1::verification_only()
		.verify_ecdsa(&message, &signature, expected_signer)
		.map_err(|_| SignatureError::Invalid)
}

fn hash_parts(tag: &[u8], parts: &[&[u8]]) -> Digest {
	let mut engine = sha256::Hash::engine();
	engine.input(tag);
	for part in parts {
		engine.input(part);
	}
	sha256::Hash::from_engine(engine).to_byte_array()
}

/// Digest signed by a node key: one SHA256 over domain, type and unsigned body.
///
/// The unsigned body includes channel id, epoch id, fixed fields and every TLV, but excludes
/// the final 64-byte signature. Signature verification must separately reject high-S values.
pub fn message_digest(message_type: u16, unsigned_body: &[u8]) -> Digest {
	hash_parts(b"ffor/msg", &[&message_type.to_be_bytes(), unsigned_body])
}

/// Hash the complete, validated `ff_init` wire message including type and signature.
pub fn init_hash(init_wire: &[u8]) -> Digest {
	hash_parts(b"ffor/tr/init", &[init_wire])
}

/// Bind the complete `ff_accept` wire message to its preceding signed init.
pub fn setup_hash(init: &Digest, accept_wire: &[u8]) -> Digest {
	hash_parts(b"ffor/tr/setup", &[init, accept_wire])
}

/// Hash the canonical voucher book, including its epoch, variant, profile and ordered entries.
///
/// Canonical encoding and book validation are the caller's responsibility.
pub fn book_hash(canonical_book: &[u8]) -> Digest {
	hash_parts(b"ffor/book", &[canonical_book])
}

/// Bind the current commitment numbers and both transaction ids.
///
/// Transaction ids must be in internal byte order, the reverse of display hex. The caller
/// must obtain them from the actual signed, fully committed voucher transactions.
pub fn commitment_hash(
	receiver_number: u64, receiver_txid_internal: &[u8; 32], settlement_number: u64,
	settlement_txid_internal: &[u8; 32],
) -> Digest {
	hash_parts(
		b"ffor/commit",
		&[
			&receiver_number.to_be_bytes(),
			receiver_txid_internal,
			&settlement_number.to_be_bytes(),
			settlement_txid_internal,
		],
	)
}

/// Bind setup, voucher book, both commitments and the agreed activation height.
///
/// A matching digest does not establish durable ACTIVE state or invoice readiness.
pub fn activation_hash(
	setup: &Digest, book: &Digest, commitment: &Digest, start_height: u32,
) -> Digest {
	hash_parts(b"ffor/activate", &[setup, book, commitment, &start_height.to_be_bytes()])
}

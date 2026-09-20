//! Domain-separated SHA256 transcript primitives from FFOR section 7.5.
//!
//! Inputs are public transcript data. The caller must validate canonical wire encodings and
//! signatures before using these digests. Hashing alone authenticates nothing.

use bitcoin::hashes::{sha256, Hash, HashEngine};

/// A 32-byte SHA256 transcript digest in raw byte order.
pub type Digest = [u8; 32];

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

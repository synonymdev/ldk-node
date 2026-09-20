use bitcoin::secp256k1::PublicKey;

use crate::transcript::Digest;

/// Identifiers signed by every supported message; neither is a trusted channel lookup key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
	/// BOLT channel identifier.
	pub channel_id: [u8; 32],
	/// Receiver-selected epoch identifier, whose uniqueness the engine must enforce.
	pub epoch_id: [u8; 32],
}

/// An unknown optional extension retained in the signed canonical TLV stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tlv {
	/// Odd type number; duplicate or known types cannot be supplied as extensions.
	pub kind: u64,
	/// Exact extension bytes, bounded by the peer message limit.
	pub value: Vec<u8>,
}

/// Signed Variant D init terms. G and receiver point count are fixed to zero on wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Init {
	/// Exact sum of the proposed voucher amounts, excluding forwarding fees.
	pub budget_msat: u64,
	/// Minimum permitted amount for every voucher.
	pub min_payment_msat: u64,
	/// Absolute height after which new settlement is forbidden.
	pub settlement_deadline: u32,
	/// Uniform voucher HTLC expiry height, strictly after the deadline.
	pub voucher_expiry: u32,
	/// Proposed base forwarding fee in millisatoshis.
	pub fee_base_msat: u32,
	/// Proposed proportional forwarding fee in millionths.
	pub fee_proportional_millionths: u32,
	/// Required TLV 9 amounts in slot order; length encodes K, from 1 to 483.
	pub amounts_msat: Vec<u64>,
	/// Optional TLV 13 witness peer restriction, preserving peer order.
	pub witness_peers: Option<Vec<PublicKey>>,
	/// TLV 15 requests uniform hash-chained vouchers; chain verification needs the accept.
	pub hash_chain: bool,
}

/// Signed accept fields. Cross-message agreement is checked by `Message::validate_accept`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Accept {
	/// Peer commitment number before the voucher round.
	pub s_commitment_number: u64,
	/// Required TLV 1 payment hashes in slot order.
	pub payment_hashes: Vec<[u8; 32]>,
	/// Required TLV 7 first voucher HTLC id.
	pub s_htlc_id_base: u64,
	/// Required TLV 9 exact amounts, with one amount per payment hash.
	pub amounts_msat: Vec<u64>,
	/// Required TLV 11 digest of the exact signed init wire bytes.
	pub init_hash: Digest,
}

/// Receiver's proposed activation commitments, still requiring live engine validation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Activate {
	/// T_setup binding the init and accept.
	pub setup_hash: Digest,
	/// H_book for the canonical slot book.
	pub book_hash: Digest,
	/// H_commit for the real committed transaction pair.
	pub commit_hash: Digest,
	/// Receiver's activation height; the peer must check the six-block agreement rule.
	pub epoch_start_height: u32,
}

/// Signed pre-activation abort request; lifecycle permission is an engine concern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Abort {
	/// T_setup if accept was exchanged, otherwise T_init.
	pub transcript_hash: Digest,
	/// Normative reason code from 0 through 7.
	pub reason: u16,
	/// Bounded uninterpreted text or evidence; not assumed to be UTF-8.
	pub data: Vec<u8>,
}

/// A slot-numbered preimage in a Variant D close acknowledgement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Preimage {
	/// One-based slot index, in ascending order in the preimage list.
	pub slot: u16,
	/// Preimage whose SHA256 must match the corresponding authenticated book entry.
	pub value: [u8; 32],
}

/// Signed peer close report. Bitmap padding must be zero and every set bit has a preimage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloseAck {
	/// H_act of the active epoch.
	pub activation_hash: Digest,
	/// K from the agreed book, checked against that book by the engine.
	pub num_slots: u16,
	/// LSB-first settled bitmap, exactly ceil(K/8) bytes.
	pub settled: Vec<u8>,
	/// Exactly one record per set bit in TLV 1; missing is permitted only when no slots settled.
	pub preimages: Vec<Preimage>,
	/// Preserve explicit empty TLV 1 versus absence when the bitmap is empty.
	pub preimages_tlv_present: bool,
}

/// Supported Variant D signed messages; undefined and unsupported types are rejected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Payload {
	/// ff_init, type 55001.
	Init(Init),
	/// ff_accept, type 55003.
	Accept(Accept),
	/// ff_activate, type 55045.
	Activate(Activate),
	/// ff_activate_ack, type 55047, carrying H_act.
	ActivateAck(Digest),
	/// ff_abort, type 55049.
	Abort(Abort),
	/// ff_close, type 55051, carrying H_act.
	Close(Digest),
	/// ff_close_ack, type 55053.
	CloseAck(CloseAck),
}

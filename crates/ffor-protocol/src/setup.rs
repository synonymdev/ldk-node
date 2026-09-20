//! Authenticated Variant D setup transcripts and their canonical voucher books.
//!
//! These are immutable protocol data, not a second channel state machine. Construction checks
//! node signatures and signed agreement. Live capacity, revealed commitment secrets, voucher
//! parking, persisted claim material, freeze and invoice exposure remain engine responsibilities.

use std::collections::BTreeSet;
use std::fmt;

use bitcoin::hashes::{sha256, Hash};
use bitcoin::secp256k1::PublicKey;

use crate::amounts::FeePolicy;
use crate::book::BookTerms;
use crate::transcript::{self, Digest};
use crate::wire::{Header, Init, Message, Payload, WireError};

/// A signed statement is inconsistent with the agreed setup or expected peer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupError {
	/// The envelope is invalid or its expected peer's signature does not verify.
	Wire(WireError),
	/// Receiver and settlement peer must be distinct authenticated nodes.
	Roles,
	/// This operation received the wrong message type.
	MessageType,
	/// The channel or epoch identifier differs from the authenticated setup.
	Identity,
	/// A payment hash was assigned to more than one voucher.
	DuplicateHash,
	/// Requested chained vouchers do not satisfy the signed hash-chain relation.
	HashChain,
	/// A signed voucher's fee product or upstream gross amount exceeds u64.
	FeeOverflow,
	/// The claimed transcript, book, activation or actual commitment digest differs.
	Transcript,
	/// The activation height is too far from the local tip or admission has ended.
	Height,
	/// A close acknowledgement does not describe exactly this book's slots and hashes.
	CloseBook,
}

impl fmt::Display for SetupError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "invalid FFOR setup statement: {self:?}")
	}
}

impl std::error::Error for SetupError {}

impl From<WireError> for SetupError {
	fn from(error: WireError) -> Self {
		Self::Wire(error)
	}
}

/// One entry in an authenticated, fixed-amount Variant D book.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Voucher {
	/// One-based position in the signed payment hash list.
	pub slot: u16,
	/// Settlement-generated hash; no preimage is part of setup.
	pub payment_hash: [u8; 32],
	/// Exact promised payee amount, with forwarding fees charged separately.
	pub amount_msat: u64,
	/// Absolute HTLC expiry shared by the entire book.
	pub expiry: u32,
	/// Admission deadline shared by the entire book.
	pub deadline: u32,
	/// Settlement peer's actual outgoing HTLC identifier to use for this voucher.
	pub htlc_id: u64,
}

/// Immutable signed agreement and canonical book derived exclusively from init and accept.
///
/// The public messages used to construct this value are copied. Mutating them afterwards cannot
/// alter its hashes or vouchers. This value neither owns a channel nor authorizes activation.
#[derive(Clone, Debug)]
pub struct AuthenticatedSetup {
	init: Message,
	accept: Message,
	receiver: PublicKey,
	settlement: PublicKey,
	vouchers: Vec<Voucher>,
	setup_hash: Digest,
	book_hash: Digest,
	canonical_book: Vec<u8>,
}

impl AuthenticatedSetup {
	/// Verify both signatures and signed agreement, then derive the canonical slot book.
	///
	/// The node keys must come from the channel's authenticated identities. Check channel/epoch
	/// uniqueness and reject payment hashes matching disclosed commitment secrets in the engine.
	/// This constructor has no I/O and cannot make an invoice ready.
	pub fn new(
		init: &Message, accept: &Message, receiver: PublicKey, settlement: PublicKey,
	) -> Result<Self, SetupError> {
		if receiver == settlement {
			return Err(SetupError::Roles);
		}
		let (Payload::Init(terms), Payload::Accept(accepted)) = (&init.payload, &accept.payload)
		else {
			return Err(SetupError::MessageType);
		};
		init.verify_signature(&receiver)?;
		accept.verify_signature(&settlement)?;
		accept.validate_accept(init)?;
		let mut seen = BTreeSet::new();
		let mut vouchers = Vec::with_capacity(terms.amounts_msat.len());
		let fees = FeePolicy {
			base_msat: terms.fee_base_msat,
			proportional_millionths: terms.fee_proportional_millionths,
		};
		for (index, (&amount, &hash)) in
			terms.amounts_msat.iter().zip(&accepted.payment_hashes).enumerate()
		{
			fees.gross_msat(amount).map_err(|_| SetupError::FeeOverflow)?;
			if !seen.insert(hash) {
				return Err(SetupError::DuplicateHash);
			}
			if terms.hash_chain
				&& index > 0 && sha256::Hash::hash(&hash).to_byte_array()
				!= accepted.payment_hashes[index - 1]
			{
				return Err(SetupError::HashChain);
			}
			// Encoding validated the 483-slot bound and the entire HTLC id interval.
			vouchers.push(Voucher {
				slot: index as u16 + 1,
				payment_hash: hash,
				amount_msat: amount,
				expiry: terms.voucher_expiry,
				deadline: terms.settlement_deadline,
				htlc_id: accepted.s_htlc_id_base + index as u64,
			});
		}
		let canonical_book = encode_book(init.header.epoch_id, &vouchers);
		let init_hash = transcript::init_hash(&init.encode()?);
		Ok(Self {
			init: init.clone(),
			accept: accept.clone(),
			receiver,
			settlement,
			vouchers,
			setup_hash: transcript::setup_hash(&init_hash, &accept.encode()?),
			book_hash: transcript::book_hash(&canonical_book),
			canonical_book,
		})
	}

	/// The common channel and epoch identifiers authenticated by both nodes.
	pub fn header(&self) -> Header {
		self.init.header
	}

	/// Original init, including all signed optional extensions.
	pub fn init(&self) -> &Message {
		&self.init
	}

	/// Original accept, including all signed optional extensions.
	pub fn accept(&self) -> &Message {
		&self.accept
	}

	/// Canonical slot order, with one exact payment hash and amount per HTLC identifier.
	pub fn vouchers(&self) -> &[Voucher] {
		&self.vouchers
	}

	/// Section 7.5.3 bytes with variant 4 and fixed-amount profile 1.
	pub fn canonical_book(&self) -> &[u8] {
		&self.canonical_book
	}

	/// T_setup over the complete signed init and accept.
	pub fn setup_hash(&self) -> Digest {
		self.setup_hash
	}

	/// H_book over the canonical voucher entries, without a wire-message wrapper.
	pub fn book_hash(&self) -> Digest {
		self.book_hash
	}

	/// Signed arithmetic terms, to validate against limits read from the actual channel.
	pub fn terms(&self) -> BookTerms {
		let init = self.init_terms();
		BookTerms {
			amounts_msat: init.amounts_msat.clone(),
			budget_msat: init.budget_msat,
			minimum_payment_msat: init.min_payment_msat,
			fees: FeePolicy {
				base_msat: init.fee_base_msat,
				proportional_millionths: init.fee_proportional_millionths,
			},
			settlement_deadline: init.settlement_deadline,
			voucher_expiry: init.voucher_expiry,
		}
	}

	/// Authenticate activation against this setup and the channel engine's actual H_commit.
	///
	/// The caller must obtain `commitment_hash` from both fully signed, irrevocably committed
	/// transaction views under quiescence. A peer-supplied digest is not acceptable evidence.
	/// The returned H_act is only a digest: persist the complete epoch and enforce the channel
	/// freeze before acknowledging activation or exposing an invoice.
	pub fn validate_activation(
		&self, message: &Message, commitment_hash: Digest, current_height: u32,
	) -> Result<Digest, SetupError> {
		self.authenticate(message, &self.receiver)?;
		let Payload::Activate(activation) = &message.payload else {
			return Err(SetupError::MessageType);
		};
		if activation.setup_hash != self.setup_hash
			|| activation.book_hash != self.book_hash
			|| activation.commit_hash != commitment_hash
		{
			return Err(SetupError::Transcript);
		}
		let deadline = self.init_terms().settlement_deadline;
		if activation.epoch_start_height.abs_diff(current_height) > 6
			|| current_height >= deadline
			|| activation.epoch_start_height >= deadline
		{
			return Err(SetupError::Height);
		}
		Ok(transcript::activation_hash(
			&self.setup_hash,
			&self.book_hash,
			&commitment_hash,
			activation.epoch_start_height,
		))
	}

	/// Verify the settlement peer's acknowledgement of the locally validated H_act.
	///
	/// An authenticated acknowledgement does not establish persistence or recovery readiness.
	pub fn validate_activation_ack(
		&self, message: &Message, activation_hash: Digest,
	) -> Result<(), SetupError> {
		self.authenticate(message, &self.settlement)?;
		match message.payload {
			Payload::ActivateAck(hash) if hash == activation_hash => Ok(()),
			Payload::ActivateAck(_) => Err(SetupError::Transcript),
			_ => Err(SetupError::MessageType),
		}
	}

	/// Verify close accounting and every disclosed preimage against this signed book.
	///
	/// An unset bit never permits discarding a preimage learned from another source. Retain
	/// contradictory evidence and fulfil every known claim through the engine's drain path.
	pub fn validate_close_ack(
		&self, message: &Message, activation_hash: Digest,
	) -> Result<(), SetupError> {
		self.authenticate(message, &self.settlement)?;
		let Payload::CloseAck(close) = &message.payload else {
			return Err(SetupError::MessageType);
		};
		if close.activation_hash != activation_hash {
			return Err(SetupError::Transcript);
		}
		if usize::from(close.num_slots) != self.vouchers.len() {
			return Err(SetupError::CloseBook);
		}
		if self.init_terms().hash_chain
			&& close
				.preimages
				.iter()
				.enumerate()
				.any(|(index, preimage)| usize::from(preimage.slot) != index + 1)
		{
			return Err(SetupError::CloseBook);
		}
		for preimage in &close.preimages {
			let voucher = &self.vouchers[usize::from(preimage.slot) - 1];
			if sha256::Hash::hash(&preimage.value).to_byte_array() != voucher.payment_hash {
				return Err(SetupError::CloseBook);
			}
		}
		Ok(())
	}

	fn authenticate(&self, message: &Message, signer: &PublicKey) -> Result<(), SetupError> {
		message.verify_signature(signer)?;
		if message.header != self.header() {
			return Err(SetupError::Identity);
		}
		Ok(())
	}

	fn init_terms(&self) -> &Init {
		match &self.init.payload {
			Payload::Init(init) => init,
			_ => unreachable!("invariant: constructor validated immutable init payload"),
		}
	}
}

fn encode_book(epoch_id: [u8; 32], vouchers: &[Voucher]) -> Vec<u8> {
	let mut bytes = Vec::with_capacity(36 + 58 * vouchers.len());
	bytes.extend_from_slice(&epoch_id);
	bytes.extend_from_slice(&[4, 1]);
	bytes.extend_from_slice(&(vouchers.len() as u16).to_be_bytes());
	for voucher in vouchers {
		bytes.extend_from_slice(&voucher.slot.to_be_bytes());
		bytes.extend_from_slice(&voucher.payment_hash);
		bytes.extend_from_slice(&voucher.amount_msat.to_be_bytes());
		bytes.extend_from_slice(&voucher.expiry.to_be_bytes());
		bytes.extend_from_slice(&voucher.deadline.to_be_bytes());
		bytes.extend_from_slice(&voucher.htlc_id.to_be_bytes());
	}
	bytes
}

//! Arithmetic eligibility for ECDSA zero-fee-HTLC anchor channels.
//!
//! The caller must derive every limit from the same current channel state, with no other
//! pending HTLCs. This module does not establish negotiated support or sign commitments.

use std::fmt;

use crate::amounts::{AmountError, FeePolicy};

/// BOLT 2 upper bound, not a promise of available channel capacity.
pub const MAX_VOUCHERS: usize = 483;
const ANCHOR_COMMITMENT_WEIGHT: u64 = 1124;
const HTLC_OUTPUT_WEIGHT: u64 = 172;
const BOTH_ANCHORS_SAT: u64 = 660;

/// Proposed fixed-amount book. All amounts are millisatoshis and deadlines are block heights.
#[derive(Clone, Debug)]
pub struct BookTerms {
	/// Exact amount of every voucher, in hash-set order.
	pub amounts_msat: Vec<u64>,
	/// Signed sum of all voucher amounts, excluding forwarding fees.
	pub budget_msat: u64,
	/// Signed minimum voucher amount.
	pub minimum_payment_msat: u64,
	/// Signed forwarding policy.
	pub fees: FeePolicy,
	/// Last height at which the peer can admit delegated payments.
	pub settlement_deadline: u32,
	/// Uniform absolute HTLC expiry.
	pub voucher_expiry: u32,
}

/// Current limits for a channel already verified to use zero-fee-HTLC anchors.
///
/// These values must come from the channel engine, not application balances or peer claims.
/// Normal pending HTLCs must be drained before testing a new Variant D book.
#[derive(Clone, Copy, Debug)]
pub struct AnchorChannelLimits {
	/// Receiver's negotiated maximum accepted HTLC count.
	pub max_accepted_htlcs: u16,
	/// Receiver's negotiated maximum in-flight value in millisatoshis.
	pub max_in_flight_msat: u64,
	/// Minimum HTLC amount for offers from settlement peer to receiver.
	pub htlc_minimum_msat: u64,
	/// Dust limit in the receiver's commitment view.
	pub receiver_dust_sat: u64,
	/// Dust limit in the settlement peer's commitment view.
	pub settlement_dust_sat: u64,
	/// Settlement peer's balance before the voucher round, before commitment fee deduction.
	pub settlement_balance_msat: u64,
	/// Receiver's balance before the voucher round, before commitment fee deduction.
	pub receiver_balance_msat: u64,
	/// Reserve the settlement peer must retain in satoshis.
	pub settlement_reserve_sat: u64,
	/// Reserve the receiver must retain in satoshis.
	pub receiver_reserve_sat: u64,
	/// Whether the settlement peer funds the channel and pays commitment fees.
	pub settlement_is_funder: bool,
	/// Frozen commitment feerate in satoshis per 1000 weight units.
	pub feerate_sat_per_kw: u32,
}

/// An arithmetic condition preventing voucher setup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BookError {
	/// Empty book, protocol count exceeded, or negotiated count exceeded.
	SlotCount,
	/// A zero amount, signed minimum violation, or channel HTLC minimum violation.
	MinimumAmount,
	/// At least one voucher would be trimmed in a commitment view.
	Dust,
	/// The signed budget differs from the voucher sum.
	BudgetMismatch,
	/// The voucher sum exceeds the receiver's negotiated in-flight bound.
	InFlightLimit,
	/// Admission has already ended, or the required claim margin does not fit.
	Deadline,
	/// The settlement peer cannot fund the book and retain its reserve.
	SettlementReserve,
	/// The funder cannot retain its reserve, both anchors and the doubled-feerate buffer.
	FeeReserve,
	/// A required sum, product, or upstream amount exceeds its protocol bound.
	Overflow,
}

impl fmt::Display for BookError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "ineligible FFOR voucher book: {self:?}")
	}
}

impl std::error::Error for BookError {}

impl From<AmountError> for BookError {
	fn from(_: AmountError) -> Self {
		Self::Overflow
	}
}

/// Validate the arithmetic conditions for a proposed Variant D anchor-channel book.
///
/// `claim_margin_blocks` is a deployment policy, not an assumed offline window. Section 7.1
/// recommends at least 1008 blocks. The caller must additionally validate channel delay,
/// chain watching, negotiated variant, signatures, unique hashes, HTLC identities, actual
/// commitment outputs, and persistence. At activation, recheck these conditions using the
/// retained pre-round balances and current policy. Do not pass post-round balances here:
/// this function subtracts the book budget, which would debit the vouchers twice. Actual
/// committed outputs and remaining balances require separate engine validation.
///
/// This function has no side effects and never means an offline invoice is ready.
pub fn validate_anchor_book(
	terms: &BookTerms, channel: &AnchorChannelLimits, current_height: u32, claim_margin_blocks: u32,
) -> Result<(), BookError> {
	let count = terms.amounts_msat.len();
	if count == 0 || count > MAX_VOUCHERS || count > usize::from(channel.max_accepted_htlcs) {
		return Err(BookError::SlotCount);
	}
	let required_expiry = terms.settlement_deadline.checked_add(claim_margin_blocks);
	if terms.settlement_deadline <= current_height
		|| claim_margin_blocks == 0
		|| required_expiry.is_none_or(|expiry| terms.voucher_expiry < expiry)
	{
		return Err(BookError::Deadline);
	}
	let mut total_msat = 0_u64;
	for &amount in &terms.amounts_msat {
		if amount == 0 || amount < terms.minimum_payment_msat || amount < channel.htlc_minimum_msat
		{
			return Err(BookError::MinimumAmount);
		}
		if amount / 1000 < channel.receiver_dust_sat.max(channel.settlement_dust_sat) {
			return Err(BookError::Dust);
		}
		terms.fees.gross_msat(amount)?;
		total_msat = total_msat.checked_add(amount).ok_or(BookError::Overflow)?;
	}
	if total_msat != terms.budget_msat {
		return Err(BookError::BudgetMismatch);
	}
	if total_msat > channel.max_in_flight_msat {
		return Err(BookError::InFlightLimit);
	}
	let settlement_after_msat = channel
		.settlement_balance_msat
		.checked_sub(total_msat)
		.ok_or(BookError::SettlementReserve)?;
	if settlement_after_msat / 1000 < channel.settlement_reserve_sat {
		return Err(BookError::SettlementReserve);
	}
	let (funder_after_sat, funder_reserve_sat) = if channel.settlement_is_funder {
		(settlement_after_msat / 1000, channel.settlement_reserve_sat)
	} else {
		(channel.receiver_balance_msat / 1000, channel.receiver_reserve_sat)
	};
	// Count is bounded by 483 and feerate by u32, so this u64 product cannot overflow.
	let weight = ANCHOR_COMMITMENT_WEIGHT + HTLC_OUTPUT_WEIGHT * count as u64;
	let spike_fee_sat = u64::from(channel.feerate_sat_per_kw) * 2 * weight / 1000;
	let required_sat = funder_reserve_sat
		.checked_add(spike_fee_sat)
		.and_then(|amount| amount.checked_add(BOTH_ANCHORS_SAT))
		.ok_or(BookError::Overflow)?;
	if funder_after_sat < required_sat {
		return Err(BookError::FeeReserve);
	}
	Ok(())
}

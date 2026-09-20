//! Checked fixed-amount arithmetic from FFOR v0.9.4 section 7.6.

use std::fmt;

const MILLION: u64 = 1_000_000;

/// Invalid amount arithmetic. Errors must prevent setup or settlement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AmountError {
	/// A fixed-amount voucher cannot have a zero value.
	ZeroAmount,
	/// A multiplication, fee, or gross amount exceeds the protocol's u64 bound.
	Overflow,
	/// A plaintext hop attempts to credit a different amount than the signed voucher.
	WrongPayeeAmount,
	/// The upstream HTLC does not cover the forwarding amount and required fee.
	InsufficientFee,
}

impl fmt::Display for AmountError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(match self {
			Self::ZeroAmount => "FFOR voucher amount must be positive",
			Self::Overflow => "FFOR amount exceeds u64",
			Self::WrongPayeeAmount => "forwarding amount differs from voucher amount",
			Self::InsufficientFee => "incoming amount does not cover forwarding fee",
		})
	}
}

impl std::error::Error for AmountError {}

/// BOLT 7 forwarding terms, applied to the voucher's payee amount in millisatoshis.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FeePolicy {
	/// Fixed forwarding fee in millisatoshis.
	pub base_msat: u32,
	/// Proportional fee in millionths of the payee amount.
	pub proportional_millionths: u32,
}

impl FeePolicy {
	/// Calculate the fee without reducing the voucher amount.
	///
	/// Section 7.6 rejects a product exceeding u64 even if division would make it fit.
	/// This check must also run during setup, before committing the voucher.
	///
	/// ```
	/// use ffor_protocol::amounts::FeePolicy;
	/// let fees = FeePolicy { base_msat: 1_000, proportional_millionths: 5_000 };
	/// assert_eq!(fees.fee_msat(1_000_000), Ok(6_000));
	/// ```
	pub fn fee_msat(self, payee_msat: u64) -> Result<u64, AmountError> {
		let product = payee_msat
			.checked_mul(u64::from(self.proportional_millionths))
			.ok_or(AmountError::Overflow)?;
		(product / MILLION).checked_add(u64::from(self.base_msat)).ok_or(AmountError::Overflow)
	}

	/// Calculate the book-priced upstream amount with all protocol overflow checks.
	pub fn gross_msat(self, payee_msat: u64) -> Result<u64, AmountError> {
		payee_msat.checked_add(self.fee_msat(payee_msat)?).ok_or(AmountError::Overflow)
	}
}

/// Check the two plaintext amount conditions for a Variant D payment.
///
/// `qualifying_public_policy` may only be supplied after the engine verifies section 7.6's
/// public channel announcement, both node/funding-key bindings and the onion's actual SCID.
/// Private channels, aliases and unverified announcements require `None`. This function
/// cannot authenticate channel gossip. Never use it for blinded payments.
///
/// Successful amount checks do not authorize preimage release: the channel engine must also
/// verify the slot, durable epoch, deadlines and irrevocable upstream HTLC before settlement.
pub fn check_plaintext_amount(
	payee_msat: u64, forward_msat: u64, incoming_msat: u64, book_policy: FeePolicy,
	qualifying_public_policy: Option<FeePolicy>,
) -> Result<(), AmountError> {
	if payee_msat == 0 {
		return Err(AmountError::ZeroAmount);
	}
	if forward_msat != payee_msat {
		return Err(AmountError::WrongPayeeAmount);
	}
	let book_fee = book_policy.gross_msat(payee_msat)? - payee_msat;
	// Compare complete policies. Mixing the cheapest base and rate would undercharge.
	let required_fee = match qualifying_public_policy {
		Some(public) => {
			// The setup product restriction applies to the signed book. A later public policy
			// increase must never prevent settlement at the valid book fee.
			let public_fee = u128::from(public.base_msat)
				+ u128::from(payee_msat) * u128::from(public.proportional_millionths)
					/ u128::from(MILLION);
			u128::from(book_fee).min(public_fee) as u64
		},
		None => book_fee,
	};
	let paid_fee = incoming_msat.checked_sub(forward_msat).ok_or(AmountError::InsufficientFee)?;
	if paid_fee < required_fee {
		return Err(AmountError::InsufficientFee);
	}
	Ok(())
}

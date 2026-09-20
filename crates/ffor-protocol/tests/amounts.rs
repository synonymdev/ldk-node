use ffor_protocol::amounts::{check_plaintext_amount, AmountError, FeePolicy};
use proptest::prelude::*;

#[test]
fn appendix_amounts_preserve_invoice_value() {
	let fees = FeePolicy { base_msat: 1000, proportional_millionths: 5000 };
	for (amount, fee, gross) in [
		(1_000_000, 6000, 1_006_000),
		(994_000, 5970, 999_970),
		(546_250, 3731, 549_981),
		(49_749_000, 249_745, 49_998_745),
		(546_000, 3730, 549_730),
	] {
		assert_eq!(fees.fee_msat(amount), Ok(fee));
		assert_eq!(fees.gross_msat(amount), Ok(gross));
		assert_eq!(check_plaintext_amount(amount, amount, gross, fees, None), Ok(()));
		assert_eq!(
			check_plaintext_amount(amount, amount, gross - 1, fees, None),
			Err(AmountError::InsufficientFee)
		);
	}
}

#[test]
fn rejects_overflow_before_division_and_at_gross_sum() {
	let product = FeePolicy { base_msat: 0, proportional_millionths: 2 };
	assert_eq!(product.fee_msat(u64::MAX / 2 + 1), Err(AmountError::Overflow));
	assert!(product.fee_msat(u64::MAX / 2).is_ok());
	let base = FeePolicy { base_msat: 1, proportional_millionths: 0 };
	assert_eq!(base.gross_msat(u64::MAX), Err(AmountError::Overflow));
	assert_eq!(FeePolicy::default().gross_msat(u64::MAX), Ok(u64::MAX));
}

#[test]
fn public_policy_compares_complete_fees() {
	let book = FeePolicy { base_msat: 1000, proportional_millionths: 0 };
	let public = FeePolicy { base_msat: 0, proportional_millionths: 2000 };
	assert_eq!(
		check_plaintext_amount(1_000_000, 1_000_000, 1_000_999, book, Some(public)),
		Err(AmountError::InsufficientFee)
	);
	assert_eq!(check_plaintext_amount(1_000_000, 1_000_000, 1_001_000, book, Some(public)), Ok(()));
	assert_eq!(
		check_plaintext_amount(1_000_000, 1_000_000, 1_000_000, book, Some(FeePolicy::default())),
		Ok(())
	);
	assert_eq!(
		check_plaintext_amount(1_000_000, 1_000_000, 1_000_000, book, None),
		Err(AmountError::InsufficientFee)
	);
}

#[test]
fn public_policy_increase_cannot_disable_book_pricing() {
	let amount = u64::MAX / 2;
	let public = FeePolicy { base_msat: u32::MAX, proportional_millionths: u32::MAX };
	assert_eq!(
		check_plaintext_amount(amount, amount, amount, FeePolicy::default(), Some(public)),
		Ok(())
	);
}

#[test]
fn cheaper_public_policy_can_have_product_above_book_setup_bound() {
	let book = FeePolicy { base_msat: 5000, proportional_millionths: u32::MAX - 1 };
	let public = FeePolicy { base_msat: 0, proportional_millionths: u32::MAX };
	let amount = 4_294_967_298;
	let public_fee = 18_446_744_078_004;
	assert_eq!(
		check_plaintext_amount(amount, amount, amount + public_fee, book, Some(public)),
		Ok(())
	);
	assert_eq!(
		check_plaintext_amount(amount, amount, amount + public_fee - 1, book, Some(public)),
		Err(AmountError::InsufficientFee)
	);
}

#[test]
fn rejects_mismatched_payee_and_incoming_underflow() {
	for forward in [999, 1001] {
		assert_eq!(
			check_plaintext_amount(1000, forward, 2000, FeePolicy::default(), None),
			Err(AmountError::WrongPayeeAmount)
		);
	}
	assert_eq!(
		check_plaintext_amount(1000, 1000, 999, FeePolicy::default(), None),
		Err(AmountError::InsufficientFee)
	);
}

#[test]
fn public_discount_cannot_admit_invalid_signed_book_amounts() {
	let fees = FeePolicy { base_msat: 1, proportional_millionths: 0 };
	assert_eq!(
		check_plaintext_amount(u64::MAX, u64::MAX, u64::MAX, fees, Some(FeePolicy::default())),
		Err(AmountError::Overflow)
	);
	assert_eq!(
		check_plaintext_amount(0, 0, 0, FeePolicy::default(), None),
		Err(AmountError::ZeroAmount)
	);
}

proptest! {
	#[test]
	fn fee_bounds_match_wide_integer_oracle(amount in any::<u64>(), base in any::<u32>(), ppm in any::<u32>()) {
		let policy = FeePolicy { base_msat: base, proportional_millionths: ppm };
		let product = u128::from(amount) * u128::from(ppm);
		let gross = u128::from(amount) + u128::from(base) + product / 1_000_000;
		if product > u128::from(u64::MAX) || gross > u128::from(u64::MAX) {
			prop_assert_eq!(policy.gross_msat(amount), Err(AmountError::Overflow));
		} else {
			prop_assert_eq!(policy.gross_msat(amount), Ok(gross as u64));
		}
	}

	#[test]
	fn gross_payment_passes_and_payee_changes_fail(amount in 1_u64..1_000_000_000, base in any::<u32>(), ppm in 0_u32..1_000_000) {
		let policy = FeePolicy { base_msat: base, proportional_millionths: ppm };
		let gross = policy.gross_msat(amount).unwrap();
		prop_assert_eq!(check_plaintext_amount(amount, amount, gross, policy, None), Ok(()));
		prop_assert_eq!(check_plaintext_amount(amount, amount - 1, gross, policy, None), Err(AmountError::WrongPayeeAmount));
		prop_assert_eq!(check_plaintext_amount(amount, amount + 1, gross, policy, None), Err(AmountError::WrongPayeeAmount));
	}
}

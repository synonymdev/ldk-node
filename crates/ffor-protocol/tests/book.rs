use ffor_protocol::amounts::FeePolicy;
use ffor_protocol::book::{validate_anchor_book, AnchorChannelLimits, BookError, BookTerms};
use proptest::prelude::*;

fn terms(amounts_msat: Vec<u64>) -> BookTerms {
	BookTerms {
		budget_msat: amounts_msat.iter().sum(),
		amounts_msat,
		minimum_payment_msat: 546_000,
		fees: FeePolicy { base_msat: 1000, proportional_millionths: 5000 },
		settlement_deadline: 798_992,
		voucher_expiry: 800_000,
	}
}

fn channel() -> AnchorChannelLimits {
	AnchorChannelLimits {
		max_accepted_htlcs: 483,
		max_in_flight_msat: 5_000_000_000,
		htlc_minimum_msat: 1,
		receiver_dust_sat: 546,
		settlement_dust_sat: 546,
		settlement_balance_msat: 7_000_000_000,
		receiver_balance_msat: 3_000_000_000,
		settlement_reserve_sat: 10_000,
		receiver_reserve_sat: 10_000,
		settlement_is_funder: true,
		feerate_sat_per_kw: 2500,
	}
}

fn validate(book: &BookTerms, channel: &AnchorChannelLimits) -> Result<(), BookError> {
	validate_anchor_book(book, channel, 790_000, 1008)
}

#[test]
fn appendix_d_books_fit_both_funder_roles() {
	for amounts in
		[vec![1_000_000], vec![994_000, 546_250, 49_749_000], vec![546_000], vec![546_000; 483]]
	{
		for settlement_is_funder in [false, true] {
			let channel = AnchorChannelLimits { settlement_is_funder, ..channel() };
			assert_eq!(validate(&terms(amounts.clone()), &channel), Ok(()));
		}
	}
}

#[test]
fn unfunded_receiver_can_receive_in_settlement_funded_channel() {
	let mut channel = channel();
	channel.receiver_balance_msat = 0;
	assert_eq!(validate(&terms(vec![1_000_000]), &channel), Ok(()));
	channel.settlement_is_funder = false;
	assert_eq!(validate(&terms(vec![1_000_000]), &channel), Err(BookError::FeeReserve));
}

#[test]
fn requires_fee_spike_buffer_including_both_anchors() {
	let book = terms(vec![1_000_000]);
	let mut channel = channel();
	// D.1 fee at twice 2500 sat/kw is 6480 sat, plus 660 sat for anchors.
	channel.settlement_balance_msat = book.budget_msat + (10_000 + 6480 + 660) * 1000;
	assert_eq!(validate(&book, &channel), Ok(()));
	channel.settlement_balance_msat -= 1;
	assert_eq!(validate(&book, &channel), Err(BookError::FeeReserve));
	channel.settlement_balance_msat = book.budget_msat + channel.settlement_reserve_sat * 1000 - 1;
	assert_eq!(validate(&book, &channel), Err(BookError::SettlementReserve));
	channel.settlement_balance_msat = book.budget_msat - 1;
	assert_eq!(validate(&book, &channel), Err(BookError::SettlementReserve));
}

#[test]
fn validates_both_commitment_dust_limits() {
	let book = terms(vec![546_999]);
	let mut channel = channel();
	channel.receiver_dust_sat = 547;
	assert_eq!(validate(&book, &channel), Err(BookError::Dust));
	channel.receiver_dust_sat = 546;
	channel.settlement_dust_sat = 547;
	assert_eq!(validate(&book, &channel), Err(BookError::Dust));
	channel.settlement_dust_sat = u64::MAX;
	assert_eq!(validate(&book, &channel), Err(BookError::Dust));
}

#[test]
fn rejects_budget_and_channel_limit_violations() {
	let mut book = terms(vec![1_000_000]);
	let mut channel = channel();
	book.budget_msat += 1;
	assert_eq!(validate(&book, &channel), Err(BookError::BudgetMismatch));
	book.budget_msat -= 1;
	channel.max_in_flight_msat = book.budget_msat - 1;
	assert_eq!(validate(&book, &channel), Err(BookError::InFlightLimit));
	channel.max_in_flight_msat = book.budget_msat;
	channel.htlc_minimum_msat = book.budget_msat + 1;
	assert_eq!(validate(&book, &channel), Err(BookError::MinimumAmount));
	channel.htlc_minimum_msat = 1;
	book.minimum_payment_msat = book.budget_msat + 1;
	assert_eq!(validate(&book, &channel), Err(BookError::MinimumAmount));
	book.minimum_payment_msat = 0;
	book.amounts_msat = vec![0];
	assert_eq!(validate(&book, &channel), Err(BookError::MinimumAmount));
}

#[test]
fn bounds_slot_count_before_processing_amounts() {
	for count in [0, 484] {
		assert_eq!(validate(&terms(vec![546_000; count]), &channel()), Err(BookError::SlotCount));
	}
	let channel = AnchorChannelLimits { max_accepted_htlcs: 1, ..channel() };
	assert_eq!(validate(&terms(vec![546_000; 2]), &channel), Err(BookError::SlotCount));
}

#[test]
fn rejects_expired_or_overflowing_deadlines() {
	let mut book = terms(vec![1_000_000]);
	book.settlement_deadline = 790_000;
	assert_eq!(validate(&book, &channel()), Err(BookError::Deadline));
	book.settlement_deadline = u32::MAX;
	assert_eq!(validate(&book, &channel()), Err(BookError::Deadline));
	book.settlement_deadline = 798_993;
	assert_eq!(validate(&book, &channel()), Err(BookError::Deadline));
	assert_eq!(
		validate_anchor_book(&terms(vec![1_000_000]), &channel(), 790_000, 0),
		Err(BookError::Deadline)
	);
}

#[test]
fn rejects_arithmetic_overflow_without_wrapping() {
	let mut book = terms(vec![1_000_000]);
	book.amounts_msat = vec![u64::MAX];
	assert_eq!(validate(&book, &channel()), Err(BookError::Overflow));
	book.fees = FeePolicy::default();
	book.amounts_msat = vec![u64::MAX, 546_000];
	assert_eq!(validate(&book, &channel()), Err(BookError::Overflow));
	let mut channel = channel();
	channel.settlement_is_funder = false;
	channel.receiver_reserve_sat = u64::MAX;
	assert_eq!(validate(&terms(vec![1_000_000]), &channel), Err(BookError::Overflow));
}

proptest! {
	#[test]
	fn valid_books_cannot_exceed_signed_budget(amounts in prop::collection::vec(546_000_u64..2_000_000, 1..100)) {
		let mut book = terms(amounts);
		prop_assert_eq!(validate(&book, &channel()), Ok(()));
		book.budget_msat += 1;
		prop_assert_eq!(validate(&book, &channel()), Err(BookError::BudgetMismatch));
	}
}

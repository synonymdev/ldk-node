use ffor_protocol::reestablish::{
	Reestablish, ReestablishError, ReportedState, TLV_TYPE, VALUE_LEN,
};
use proptest::prelude::*;
use serde::Deserialize;

#[derive(Deserialize)]
struct Reference {
	source_revision: String,
	reestablish: Vec<Fixture>,
}

#[derive(Deserialize)]
struct Fixture {
	state: u8,
	r#type: u64,
	value: String,
}

#[test]
fn all_states_match_the_pinned_reference() {
	let reference: Reference =
		serde_json::from_str(include_str!("data/beignet-lifecycle.json")).unwrap();
	assert_eq!(reference.source_revision, "8aee31d18e596fe49a0d195b325a6e757d7a009b");
	assert_eq!(reference.reestablish.len(), 7);
	for fixture in reference.reestablish {
		assert_eq!(fixture.r#type, TLV_TYPE);
		let bytes: Vec<u8> = (0..fixture.value.len())
			.step_by(2)
			.map(|i| u8::from_str_radix(&fixture.value[i..i + 2], 16).unwrap())
			.collect();
		let report = Reestablish::decode(&bytes).unwrap();
		assert_eq!(report.epoch_id, [2; 32]);
		assert_eq!(report.state as u8, fixture.state);
		assert_eq!(report.encode().as_slice(), bytes);
		if report.state == ReportedState::Activating {
			// This unsigned report is deliberately preserved, not accepted as activation evidence.
			assert_eq!(report.activation_hash, [9; 32]);
		}
	}
}

#[test]
fn every_length_and_unknown_state_boundary_is_rejected() {
	let value = [0; VALUE_LEN];
	for length in 0..VALUE_LEN {
		assert_eq!(Reestablish::decode(&value[..length]), Err(ReestablishError::Length));
	}
	assert_eq!(Reestablish::decode(&[0; VALUE_LEN + 1]), Err(ReestablishError::Length));
	for state in 7..=255 {
		let mut value = value;
		value[32] = state;
		assert_eq!(Reestablish::decode(&value), Err(ReestablishError::State));
	}
}

#[test]
fn nonzero_sequence_is_never_variant_d_settlement_evidence() {
	for sequence in 1_u16..=u16::MAX {
		let mut value = [0; VALUE_LEN];
		value[32] = ReportedState::Active as u8;
		value[33..35].copy_from_slice(&sequence.to_be_bytes());
		assert_eq!(Reestablish::decode(&value), Err(ReestablishError::Sequence));
	}
}

proptest! {
	#[test]
	fn round_trip_preserves_all_report_bytes(epoch in any::<[u8; 32]>(), hash in any::<[u8; 32]>(), state in 0_u8..7) {
		let mut bytes = [0; VALUE_LEN];
		bytes[..32].copy_from_slice(&epoch);
		bytes[32] = state;
		bytes[35..].copy_from_slice(&hash);
		let report = Reestablish::decode(&bytes).unwrap();
		prop_assert_eq!(report.encode(), bytes);
	}

	#[test]
	fn arbitrary_inputs_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
		if let Ok(report) = Reestablish::decode(&bytes) {
			prop_assert_eq!(report.encode().to_vec(), bytes);
		}
	}
}

use bitcoin::secp256k1::PublicKey;
use ffor_protocol::transcript;
use ffor_protocol::wire::{
	Abort, CloseAck, Message, Payload, Preimage, Tlv, WireError, MAX_MESSAGE_LEN, MAX_TLV_COUNT,
};
use proptest::prelude::*;
use serde::Deserialize;

#[derive(Deserialize)]
struct Fixture {
	scenario: String,
	init_wire: String,
	accept_wire: String,
	activate_wire: String,
	ack_wire: String,
}

fn fixtures() -> Vec<Fixture> {
	serde_json::from_str(include_str!("data/appendix-d.json")).unwrap()
}

fn hex(value: &str) -> Vec<u8> {
	(0..value.len()).step_by(2).map(|i| u8::from_str_radix(&value[i..i + 2], 16).unwrap()).collect()
}

fn receiver() -> PublicKey {
	PublicKey::from_slice(&hex(
		"039fca7f8157aa768708894ffd92550fe970edd18526a5f936583ea3b54dab3228",
	))
	.unwrap()
}

fn settlement() -> PublicKey {
	PublicKey::from_slice(&hex(
		"02087b7d1b4789170f6e374f0a0e58a1b7a899e34929795314ab6964e69609e9c0",
	))
	.unwrap()
}

fn init() -> Message {
	Message::decode(&hex(&fixtures()[0].init_wire)).unwrap()
}
fn ack() -> Message {
	Message::decode(&hex(&fixtures()[0].ack_wire)).unwrap()
}

// Modified fixture signatures remain untrusted bytes, never newly signed messages.
fn append_tlv(mut wire: Vec<u8>, tlv: &[u8]) -> Vec<u8> {
	wire.splice(wire.len() - 64..wire.len() - 64, tlv.iter().copied());
	wire
}

fn close_ack() -> Message {
	let mut message = ack();
	message.payload = Payload::CloseAck(CloseAck {
		activation_hash: [3; 32],
		num_slots: 9,
		settled: vec![1, 1],
		preimages: vec![Preimage { slot: 1, value: [1; 32] }, Preimage { slot: 9, value: [9; 32] }],
		preimages_tlv_present: true,
	});
	message
}

#[test]
fn appendix_d_all_published_setup_and_activation_messages_round_trip() {
	for fixture in fixtures() {
		let initial = Message::decode(&hex(&fixture.init_wire)).unwrap();
		let accept = Message::decode(&hex(&fixture.accept_wire)).unwrap();
		accept.validate_accept(&initial).unwrap();
		for (wire, signer) in [
			(fixture.init_wire, receiver()),
			(fixture.accept_wire, settlement()),
			(fixture.activate_wire, receiver()),
			(fixture.ack_wire, settlement()),
		] {
			let bytes = hex(&wire);
			let message = Message::decode(&bytes).unwrap();
			assert_eq!(message.encode().unwrap(), bytes, "{}", fixture.scenario);
			message.verify_signature(&signer).unwrap();
			assert_eq!(
				message.signature_digest().unwrap(),
				transcript::message_digest(message.message_type(), &bytes[2..bytes.len() - 64])
			);
		}
	}
}

#[test]
fn truncation_and_oversized_peer_messages_are_rejected() {
	let fixture = &fixtures()[0];
	for encoded in
		[&fixture.init_wire, &fixture.accept_wire, &fixture.activate_wire, &fixture.ack_wire]
	{
		let bytes = hex(encoded);
		for end in 0..bytes.len() {
			assert!(Message::decode(&bytes[..end]).is_err(), "prefix {end}");
		}
	}
	assert_eq!(Message::decode(&vec![0; MAX_MESSAGE_LEN + 1]), Err(WireError::SizeLimit));
}

#[test]
fn unsupported_variants_and_unassigned_message_types_are_rejected() {
	for variant in [0, 1, 2, 3, 5, 255] {
		let mut bytes = init().encode().unwrap();
		bytes[66] = variant;
		assert_eq!(Message::decode(&bytes), Err(WireError::UnsupportedVariant(variant)));
	}
	for kind in [55011u16, 55021, 55055, 0, u16::MAX] {
		let mut bytes = init().encode().unwrap();
		bytes[..2].copy_from_slice(&kind.to_be_bytes());
		assert_eq!(Message::decode(&bytes), Err(WireError::UnsupportedMessage(kind)));
	}
}

#[test]
fn variant_d_disallows_escape_ladders_and_receiver_points() {
	for offset in [108, 110] {
		let mut bytes = init().encode().unwrap();
		bytes[offset] = 1;
		assert_eq!(Message::decode(&bytes), Err(WireError::InvalidField));
	}
	for kind in [1, 3, 5] {
		let mut bytes = init().encode().unwrap();
		bytes.splice(111..111, [kind, 0]);
		assert_eq!(Message::decode(&bytes), Err(WireError::InvalidTlv(u64::from(kind))));
	}
}

#[test]
fn slot_count_amount_lengths_minimum_budget_and_deadline_are_checked() {
	for count in [0u16, 484, u16::MAX] {
		let mut bytes = init().encode().unwrap();
		bytes[75..77].copy_from_slice(&count.to_be_bytes());
		assert!(Message::decode(&bytes).is_err());
	}
	let mut bytes = init().encode().unwrap();
	bytes[76] = 2;
	assert_eq!(Message::decode(&bytes), Err(WireError::InvalidTlv(9)));
	for mutate in [0, 1, 2, 3] {
		let mut message = init();
		let Payload::Init(terms) = &mut message.payload else { panic!() };
		match mutate {
			0 => terms.amounts_msat[0] = 0,
			1 => terms.min_payment_msat = u64::MAX,
			2 => terms.budget_msat += 1,
			_ => terms.voucher_expiry = terms.settlement_deadline,
		}
		assert!(message.encode().is_err());
	}
}

#[test]
fn amount_sum_overflow_is_rejected() {
	let mut message = init();
	let Payload::Init(terms) = &mut message.payload else { panic!() };
	terms.amounts_msat = vec![u64::MAX, 1];
	terms.budget_msat = 0;
	terms.min_payment_msat = 1;
	assert_eq!(message.encode(), Err(WireError::InvalidField));
}

#[test]
fn required_tlv_and_canonical_bigsize_rules_are_enforced() {
	let mut no_amounts = init().encode().unwrap();
	no_amounts.drain(111..no_amounts.len() - 64);
	assert_eq!(Message::decode(&no_amounts), Err(WireError::MissingTlv(9)));
	for tlv in [vec![9, 0], vec![7, 0]] {
		assert_eq!(
			Message::decode(&append_tlv(init().encode().unwrap(), &tlv)),
			Err(WireError::TlvOrder)
		);
	}
	assert_eq!(
		Message::decode(&append_tlv(init().encode().unwrap(), &[10, 0])),
		Err(WireError::UnknownEvenTlv(10))
	);
	for nonminimal in [
		vec![0xfd, 0, 11, 0],
		vec![11, 0xfd, 0, 0],
		vec![0xfe, 0, 0, 1, 1, 0],
		vec![0xff, 0, 0, 0, 0, 0, 1, 0, 0, 0],
	] {
		assert_eq!(
			Message::decode(&append_tlv(init().encode().unwrap(), &nonminimal)),
			Err(WireError::NonCanonicalBigSize)
		);
	}
	assert_eq!(
		Message::decode(&append_tlv(
			init().encode().unwrap(),
			&[11, 0xff, 255, 255, 255, 255, 255, 255, 255, 255]
		)),
		Err(WireError::SizeLimit)
	);
	assert!(Message::decode(&append_tlv(init().encode().unwrap(), &[11, 2, 1])).is_err());
}

#[test]
fn unknown_odd_extensions_preserve_exact_bytes_and_are_signed() {
	let mut message = init();
	message.extensions =
		vec![Tlv { kind: 257, value: vec![7; 253] }, Tlv { kind: u64::MAX, value: vec![] }];
	let wire = message.encode().unwrap();
	assert_eq!(Message::decode(&wire).unwrap(), message);
	assert_eq!(message.verify_signature(&receiver()), Err(WireError::InvalidSignature));
	assert_ne!(message.signature_digest().unwrap(), init().signature_digest().unwrap());
}

#[test]
fn witness_points_require_compressed_valid_curve_points_and_exact_lengths() {
	let mut message = init();
	let Payload::Init(terms) = &mut message.payload else { panic!() };
	terms.witness_peers = Some(vec![receiver(), settlement()]);
	let wire = message.encode().unwrap();
	assert_eq!(Message::decode(&wire).unwrap(), message);
	let mut invalid = vec![13, 35, 0, 1];
	invalid.extend_from_slice(&[0; 33]);
	assert_eq!(
		Message::decode(&append_tlv(init().encode().unwrap(), &invalid)),
		Err(WireError::InvalidPoint)
	);
	assert!(Message::decode(&append_tlv(init().encode().unwrap(), &[13, 2, 0, 1])).is_err());
	assert_eq!(
		Message::decode(&append_tlv(init().encode().unwrap(), &[13, 3, 0, 0, 0])),
		Err(WireError::InvalidTlv(13))
	);
}

#[test]
fn hash_chain_profile_has_one_canonical_encoding_and_requires_uniform_amounts() {
	for value in [vec![], vec![0], vec![2], vec![1, 1]] {
		let mut tlv = vec![15, value.len() as u8];
		tlv.extend(value);
		assert_eq!(
			Message::decode(&append_tlv(init().encode().unwrap(), &tlv)),
			Err(WireError::InvalidTlv(15))
		);
	}
	let mut message = init();
	let Payload::Init(terms) = &mut message.payload else { panic!() };
	terms.hash_chain = true;
	assert_eq!(Message::decode(&message.encode().unwrap()).unwrap(), message);
	let Payload::Init(terms) = &mut message.payload else { panic!() };
	terms.amounts_msat.push(terms.amounts_msat[0] + 1);
	terms.budget_msat = terms.amounts_msat.iter().sum();
	assert_eq!(message.encode(), Err(WireError::InvalidTlv(15)));
}

#[test]
fn accept_requires_each_known_tlv_and_consistent_slot_identity() {
	let fixture = &fixtures()[0];
	let original = Message::decode(&hex(&fixture.accept_wire)).unwrap();
	for mutate in [0, 1, 2] {
		let mut message = original.clone();
		let Payload::Accept(accept) = &mut message.payload else { panic!() };
		match mutate {
			0 => accept.amounts_msat.clear(),
			1 => accept.amounts_msat[0] = 0,
			_ => {
				accept.payment_hashes.push([2; 32]);
				accept.amounts_msat.push(1);
				accept.s_htlc_id_base = u64::MAX;
			},
		}
		assert!(message.encode().is_err());
	}
	for (start, len, kind) in [(74, 34, 1), (108, 10, 7), (118, 10, 9), (128, 34, 11)] {
		let mut bytes = original.encode().unwrap();
		bytes.drain(start..start + len);
		assert_eq!(Message::decode(&bytes), Err(WireError::MissingTlv(kind)));
	}
}

#[test]
fn accept_binds_header_exact_amounts_and_complete_signed_init_bytes() {
	let mut initial = init();
	let accept = Message::decode(&hex(&fixtures()[0].accept_wire)).unwrap();
	accept.validate_accept(&initial).unwrap();
	initial.extensions.push(Tlv { kind: 17, value: vec![1] });
	assert_eq!(accept.validate_accept(&initial), Err(WireError::SetupMismatch));
	let mut other = accept.clone();
	other.header.epoch_id[0] ^= 1;
	assert_eq!(other.validate_accept(&init()), Err(WireError::SetupMismatch));
	let Payload::Accept(terms) = &mut other.payload else { panic!() };
	terms.amounts_msat[0] += 1;
	other.header = init().header;
	assert_eq!(other.validate_accept(&init()), Err(WireError::SetupMismatch));
	assert_eq!(init().validate_accept(&accept), Err(WireError::SetupMismatch));
}

#[test]
fn lifecycle_structure_round_trips_but_changed_messages_do_not_authenticate() {
	for payload in [
		Payload::Abort(Abort { transcript_hash: [2; 32], reason: 7, data: vec![0, 255, 1] }),
		Payload::Close([3; 32]),
		close_ack().payload,
	] {
		let mut message = ack();
		message.payload = payload;
		let bytes = message.encode().unwrap();
		assert_eq!(Message::decode(&bytes).unwrap(), message);
		assert_eq!(message.verify_signature(&settlement()), Err(WireError::InvalidSignature));
	}
}

#[test]
fn close_ack_requires_preimages_matching_every_set_bit_in_order() {
	for mutate in [0, 1, 2, 3, 4, 5] {
		let mut message = close_ack();
		let Payload::CloseAck(close) = &mut message.payload else { panic!() };
		match mutate {
			0 => close.preimages.clear(),
			1 => close.preimages.swap(0, 1),
			2 => close.preimages[1].slot = 1,
			3 => close.settled[1] = 0x81,
			4 => close.num_slots = 0,
			_ => close.settled.push(0),
		}
		assert!(message.encode().is_err());
	}
	let mut message = close_ack();
	let Payload::CloseAck(close) = &mut message.payload else { panic!() };
	close.settled = vec![0, 0];
	close.preimages.clear();
	let mut bytes = message.encode().unwrap();
	assert_eq!(Message::decode(&bytes).unwrap(), message);
	let end = bytes.len() - 64;
	bytes.drain(end - 2..end);
	let absent = Message::decode(&bytes).unwrap();
	let Payload::CloseAck(close) = &absent.payload else { panic!() };
	assert!(!close.preimages_tlv_present);
	assert_eq!(absent.encode().unwrap(), bytes);
}

#[test]
fn encoder_rejects_oversized_data_and_noncanonical_extensions() {
	let mut message = ack();
	message.payload = Payload::Abort(Abort { transcript_hash: [2; 32], reason: 8, data: vec![] });
	assert_eq!(message.encode(), Err(WireError::InvalidField));
	let Payload::Abort(abort) = &mut message.payload else { panic!() };
	abort.reason = 0;
	abort.data = vec![0; MAX_MESSAGE_LEN];
	assert_eq!(message.encode(), Err(WireError::SizeLimit));
	for extensions in [
		vec![Tlv { kind: 17, value: vec![0; MAX_MESSAGE_LEN] }],
		vec![Tlv { kind: 13, value: vec![] }],
		vec![Tlv { kind: 15, value: vec![1] }],
		vec![Tlv { kind: 17, value: vec![] }, Tlv { kind: 17, value: vec![] }],
		vec![Tlv { kind: 19, value: vec![] }, Tlv { kind: 17, value: vec![] }],
		vec![Tlv { kind: 18, value: vec![] }],
	] {
		let mut message = init();
		message.extensions = extensions;
		assert!(message.encode().is_err());
	}
}

#[test]
fn zero_out_of_range_and_high_s_signatures_are_rejected() {
	for bytes in [[0; 64], [255; 64]] {
		let mut message = ack();
		message.signature = bytes;
		assert_eq!(message.encode(), Err(WireError::InvalidSignature));
	}
	let mut message = ack();
	let order = hex("fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141");
	let mut borrow = 0i16;
	for i in (0..32).rev() {
		let value = i16::from(order[i]) - i16::from(message.signature[i + 32]) - borrow;
		message.signature[i + 32] = value.rem_euclid(256) as u8;
		borrow = i16::from(value < 0);
	}
	assert_eq!(message.encode(), Err(WireError::InvalidSignature));
}

#[test]
fn unsigned_digest_can_be_requested_without_manufacturing_a_signature() {
	let mut message = init();
	let digest = message.signature_digest().unwrap();
	message.signature = [0; 64];
	assert_eq!(message.signature_digest().unwrap(), digest);
	assert_eq!(message.encode(), Err(WireError::InvalidSignature));
	message.extensions.push(Tlv { kind: 17, value: vec![1] });
	assert_ne!(message.signature_digest().unwrap(), digest);
}

#[test]
fn encoder_reserves_signature_space_and_bounds_collections_before_processing() {
	let mut message = ack();
	message.payload = Payload::Abort(Abort {
		transcript_hash: [2; 32],
		reason: 0,
		data: vec![7; MAX_MESSAGE_LEN - 166],
	});
	let bytes = message.encode().unwrap();
	assert_eq!(bytes.len(), MAX_MESSAGE_LEN);
	assert_eq!(Message::decode(&bytes).unwrap(), message);
	let Payload::Abort(abort) = &mut message.payload else { panic!() };
	abort.data.push(1);
	assert_eq!(message.signature_digest(), Err(WireError::SizeLimit));
	let mut oversized_extensions = init();
	oversized_extensions.extensions = vec![Tlv { kind: 17, value: vec![] }; MAX_TLV_COUNT + 1];
	assert_eq!(oversized_extensions.encode(), Err(WireError::SizeLimit));
	let mut oversized_points = init();
	let Payload::Init(terms) = &mut oversized_points.payload else { panic!() };
	terms.witness_peers = Some(vec![receiver(); MAX_MESSAGE_LEN / 33 + 1]);
	assert_eq!(oversized_points.encode(), Err(WireError::SizeLimit));
	assert_eq!(
		Message::decode(&append_tlv(init().encode().unwrap(), &[13, 2, 255, 255])),
		Err(WireError::SizeLimit)
	);
}

#[test]
fn malformed_known_tlv_value_lengths_are_rejected_on_the_wire() {
	let mut amounts = init().encode().unwrap();
	amounts[112] = 7;
	amounts.remove(120);
	assert_eq!(Message::decode(&amounts), Err(WireError::InvalidTlv(9)));
	let mut hashes = Message::decode(&hex(&fixtures()[0].accept_wire)).unwrap().encode().unwrap();
	hashes[75] = 31;
	hashes.remove(107);
	assert_eq!(Message::decode(&hashes), Err(WireError::InvalidTlv(1)));
	let mut preimages = close_ack().encode().unwrap();
	preimages[103] = 67;
	preimages.remove(171);
	assert_eq!(Message::decode(&preimages), Err(WireError::InvalidTlv(1)));
	let mut one_slot = close_ack();
	let Payload::CloseAck(close) = &mut one_slot.payload else { panic!() };
	close.num_slots = 1;
	close.settled = vec![1];
	close.preimages.truncate(1);
	let mut too_many_preimages = one_slot.encode().unwrap();
	too_many_preimages[102] = 68;
	too_many_preimages.splice(137..137, [0; 34]);
	assert_eq!(Message::decode(&too_many_preimages), Err(WireError::InvalidTlv(1)));
	let mut absent_preimages = one_slot.encode().unwrap();
	absent_preimages.drain(101..137);
	assert!(Message::decode(&absent_preimages).is_err());
}

#[test]
fn invalid_wire_abort_reason_and_oversized_constructed_abort_are_rejected() {
	let mut message = ack();
	message.payload = Payload::Abort(Abort { transcript_hash: [2; 32], reason: 0, data: vec![] });
	let mut wire = message.encode().unwrap();
	wire[99] = 8;
	assert_eq!(Message::decode(&wire), Err(WireError::InvalidField));
	let Payload::Abort(abort) = &mut message.payload else { panic!() };
	abort.data = vec![0; MAX_MESSAGE_LEN + 1];
	assert_eq!(message.encode(), Err(WireError::SizeLimit));
}

#[test]
fn known_extension_collisions_and_zero_s_are_rejected() {
	let mut accept = Message::decode(&hex(&fixtures()[0].accept_wire)).unwrap();
	accept.extensions.push(Tlv { kind: 1, value: vec![1] });
	assert_eq!(accept.encode(), Err(WireError::InvalidTlv(1)));
	let mut close = close_ack();
	close.extensions.push(Tlv { kind: 1, value: vec![] });
	assert_eq!(close.encode(), Err(WireError::InvalidTlv(1)));
	let mut message = ack();
	message.signature[32..].fill(0);
	assert_eq!(message.encode(), Err(WireError::InvalidSignature));
	assert!(WireError::InvalidSignature.to_string().contains("InvalidSignature"));
}

proptest! {
	#[test]
	fn arbitrary_bounded_input_never_panics_and_accepted_bytes_are_canonical(
		bytes in prop::collection::vec(any::<u8>(), 0..=MAX_MESSAGE_LEN + 1)
	) {
		if let Ok(message) = Message::decode(&bytes) {
			prop_assert_eq!(message.encode().unwrap(), bytes);
		}
	}

	#[test]
	fn unknown_odd_tlv_values_round_trip_without_normalization(value in prop::collection::vec(any::<u8>(), 0..2048)) {
		let mut message = init();
		message.extensions = vec![Tlv { kind: 65_537, value }];
		let bytes = message.encode().unwrap();
		prop_assert_eq!(Message::decode(&bytes).unwrap(), message);
	}

	#[test]
	fn close_bitmaps_round_trip_with_exactly_the_corresponding_preimages(bits in prop::collection::vec(any::<bool>(), 1..=483)) {
		let mut close = CloseAck { activation_hash: [3; 32], num_slots: bits.len() as u16,
			settled: vec![0; bits.len().div_ceil(8)], preimages: Vec::new(), preimages_tlv_present: true };
		for (index, bit) in bits.into_iter().enumerate() {
			if bit {
				close.settled[index / 8] |= 1 << (index % 8);
				close.preimages.push(Preimage { slot: index as u16 + 1, value: [index as u8; 32] });
			}
		}
		let mut message = ack();
		message.payload = Payload::CloseAck(close);
		prop_assert_eq!(Message::decode(&message.encode().unwrap()).unwrap(), message);
	}
}

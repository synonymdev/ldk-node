use bitcoin::hashes::{sha256, Hash};
use bitcoin::secp256k1::{Message as SecpMessage, PublicKey, Secp256k1, SecretKey};
use ffor_protocol::setup::{AuthenticatedSetup, SetupError};
use ffor_protocol::transcript;
use ffor_protocol::wire::{
	Accept, Activate, CloseAck, Header, Init, Message, Payload, Preimage, Tlv,
};
use proptest::prelude::*;
use serde::Deserialize;

fn key(value: u8) -> PublicKey {
	PublicKey::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[value; 32]).unwrap())
}

fn sign(message: &mut Message, value: u8) {
	let digest = message.signature_digest().unwrap();
	message.signature = Secp256k1::new()
		.sign_ecdsa(
			&SecpMessage::from_digest(digest),
			&SecretKey::from_slice(&[value; 32]).unwrap(),
		)
		.serialize_compact();
}

fn message(payload: Payload, signer: u8) -> Message {
	let mut message = Message {
		header: Header { channel_id: [1; 32], epoch_id: [2; 32] },
		payload,
		extensions: Vec::new(),
		signature: [0; 64],
	};
	sign(&mut message, signer);
	message
}

fn setup_messages() -> (Message, Message) {
	let init = message(
		Payload::Init(Init {
			budget_msat: 2_000_000,
			min_payment_msat: 354_000,
			settlement_deadline: 1000,
			voucher_expiry: 2008,
			fee_base_msat: 1000,
			fee_proportional_millionths: 100,
			amounts_msat: vec![1_000_000; 2],
			witness_peers: None,
			hash_chain: false,
		}),
		42,
	);
	let accept = message(
		Payload::Accept(Accept {
			s_commitment_number: 3,
			payment_hashes: vec![
				sha256::Hash::hash(&[5; 32]).to_byte_array(),
				sha256::Hash::hash(&[6; 32]).to_byte_array(),
			],
			s_htlc_id_base: 8,
			amounts_msat: vec![1_000_000; 2],
			init_hash: transcript::init_hash(&init.encode().unwrap()),
		}),
		43,
	);
	(init, accept)
}

fn setup() -> AuthenticatedSetup {
	let (init, accept) = setup_messages();
	AuthenticatedSetup::new(&init, &accept, key(42), key(43)).unwrap()
}

fn activate(setup: &AuthenticatedSetup, height: u32) -> Message {
	message(
		Payload::Activate(Activate {
			setup_hash: setup.setup_hash(),
			book_hash: setup.book_hash(),
			commit_hash: [9; 32],
			epoch_start_height: height,
		}),
		42,
	)
}

fn refresh_accept(init: &Message, accept: &mut Message) {
	let Payload::Accept(accepted) = &mut accept.payload else { unreachable!() };
	accepted.init_hash = transcript::init_hash(&init.encode().unwrap());
	sign(accept, 43);
}

fn hex(value: &str) -> Vec<u8> {
	(0..value.len()).step_by(2).map(|i| u8::from_str_radix(&value[i..i + 2], 16).unwrap()).collect()
}

#[derive(Deserialize)]
struct Fixture {
	init_wire: String,
	accept_wire: String,
	activate_wire: String,
	ack_wire: String,
	book: String,
	book_hash: String,
	setup_hash: String,
	commitment_hash: String,
	activation_hash: String,
}

#[test]
fn all_public_books_and_activation_statements_authenticate() {
	let fixtures: Vec<Fixture> =
		serde_json::from_str(include_str!("data/appendix-d.json")).unwrap();
	assert_eq!(fixtures.len(), 6);
	let receiver = PublicKey::from_slice(&hex(
		"039fca7f8157aa768708894ffd92550fe970edd18526a5f936583ea3b54dab3228",
	))
	.unwrap();
	let settlement = PublicKey::from_slice(&hex(
		"02087b7d1b4789170f6e374f0a0e58a1b7a899e34929795314ab6964e69609e9c0",
	))
	.unwrap();
	for fixture in fixtures {
		let init = Message::decode(&hex(&fixture.init_wire)).unwrap();
		let accept = Message::decode(&hex(&fixture.accept_wire)).unwrap();
		let setup = AuthenticatedSetup::new(&init, &accept, receiver, settlement).unwrap();
		assert_eq!(setup.canonical_book(), hex(&fixture.book));
		assert_eq!(setup.book_hash().as_slice(), hex(&fixture.book_hash));
		assert_eq!(setup.setup_hash().as_slice(), hex(&fixture.setup_hash));
		let activation = Message::decode(&hex(&fixture.activate_wire)).unwrap();
		let expected_commitment = hex(&fixture.commitment_hash).try_into().unwrap();
		let hash = setup.validate_activation(&activation, expected_commitment, 790_000).unwrap();
		assert_eq!(hash.as_slice(), hex(&fixture.activation_hash));
		let ack = Message::decode(&hex(&fixture.ack_wire)).unwrap();
		setup.validate_activation_ack(&ack, hash).unwrap();
	}
}

#[test]
fn setup_requires_distinct_expected_signers_and_unique_hashes() {
	let (init, mut accept) = setup_messages();
	assert!(matches!(
		AuthenticatedSetup::new(&init, &accept, key(42), key(42)),
		Err(SetupError::Roles)
	));
	assert!(matches!(
		AuthenticatedSetup::new(&init, &accept, key(43), key(42)),
		Err(SetupError::Wire(_))
	));
	let Payload::Accept(accepted) = &mut accept.payload else { unreachable!() };
	accepted.payment_hashes[1] = accepted.payment_hashes[0];
	sign(&mut accept, 43);
	assert!(matches!(
		AuthenticatedSetup::new(&init, &accept, key(42), key(43)),
		Err(SetupError::DuplicateHash)
	));
}

#[test]
fn setup_rejects_signed_fee_overflow() {
	let (mut init, mut accept) = setup_messages();
	let Payload::Init(terms) = &mut init.payload else { unreachable!() };
	terms.amounts_msat = vec![u64::MAX];
	terms.budget_msat = u64::MAX;
	terms.fee_proportional_millionths = 2;
	sign(&mut init, 42);
	let Payload::Accept(accepted) = &mut accept.payload else { unreachable!() };
	accepted.amounts_msat = vec![u64::MAX];
	accepted.payment_hashes.truncate(1);
	refresh_accept(&init, &mut accept);
	assert!(matches!(
		AuthenticatedSetup::new(&init, &accept, key(42), key(43)),
		Err(SetupError::FeeOverflow)
	));
}

#[test]
fn setup_copies_signed_inputs_and_preserves_optional_extensions() {
	let (mut init, mut accept) = setup_messages();
	init.extensions.push(Tlv { kind: 99, value: vec![1, 2, 3] });
	sign(&mut init, 42);
	refresh_accept(&init, &mut accept);
	let setup = AuthenticatedSetup::new(&init, &accept, key(42), key(43)).unwrap();
	let original_wire = setup.init().encode().unwrap();
	init.extensions[0].value[0] = 255;
	accept.header.epoch_id = [8; 32];
	assert_eq!(setup.init().encode().unwrap(), original_wire);
	assert_eq!(setup.accept().header, setup.header());
	assert_eq!(setup.vouchers()[1].htlc_id, 9);
	assert_eq!(setup.terms().fees.base_msat, 1000);
}

#[test]
fn requested_hash_chain_must_link_every_slot() {
	let (mut init, mut accept) = setup_messages();
	let Payload::Init(terms) = &mut init.payload else { unreachable!() };
	terms.hash_chain = true;
	sign(&mut init, 42);
	refresh_accept(&init, &mut accept);
	assert!(matches!(
		AuthenticatedSetup::new(&init, &accept, key(42), key(43)),
		Err(SetupError::HashChain)
	));
	let Payload::Accept(accepted) = &mut accept.payload else { unreachable!() };
	accepted.payment_hashes[0] = sha256::Hash::hash(&accepted.payment_hashes[1]).to_byte_array();
	sign(&mut accept, 43);
	AuthenticatedSetup::new(&init, &accept, key(42), key(43)).unwrap();
}

#[test]
fn activation_checks_actual_commitments_identity_and_height_boundaries() {
	let setup = setup();
	let activation = activate(&setup, 990);
	assert_eq!(setup.validate_activation(&activation, [8; 32], 990), Err(SetupError::Transcript));
	for height in [983, 997, 1000, u32::MAX] {
		assert_eq!(
			setup.validate_activation(&activation, [9; 32], height),
			Err(SetupError::Height)
		);
	}
	for height in [984, 990, 996] {
		setup.validate_activation(&activation, [9; 32], height).unwrap();
	}
	let mut other_epoch = activation;
	other_epoch.header.epoch_id[0] ^= 1;
	sign(&mut other_epoch, 42);
	assert_eq!(setup.validate_activation(&other_epoch, [9; 32], 990), Err(SetupError::Identity));
	let at_deadline = activate(&setup, 1000);
	assert_eq!(setup.validate_activation(&at_deadline, [9; 32], 999), Err(SetupError::Height));
}

#[test]
fn activation_ack_is_bound_to_role_type_and_activation_hash() {
	let setup = setup();
	let hash = setup.validate_activation(&activate(&setup, 990), [9; 32], 990).unwrap();
	let mut ack = message(Payload::ActivateAck(hash), 43);
	setup.validate_activation_ack(&ack, hash).unwrap();
	assert_eq!(setup.validate_activation_ack(&ack, [0; 32]), Err(SetupError::Transcript));
	sign(&mut ack, 42);
	assert!(matches!(setup.validate_activation_ack(&ack, hash), Err(SetupError::Wire(_))));
	let close = message(Payload::Close(hash), 43);
	assert_eq!(setup.validate_activation_ack(&close, hash), Err(SetupError::MessageType));
}

#[test]
fn lifecycle_validation_rejects_signed_wrong_message_types() {
	let setup = setup();
	let receiver_close = message(Payload::Close([9; 32]), 42);
	let settlement_close = message(Payload::Close([9; 32]), 43);
	assert_eq!(
		setup.validate_activation(&receiver_close, [9; 32], 990),
		Err(SetupError::MessageType)
	);
	assert_eq!(setup.validate_close_ack(&settlement_close, [9; 32]), Err(SetupError::MessageType));
	assert!(matches!(
		AuthenticatedSetup::new(&receiver_close, &settlement_close, key(42), key(43)),
		Err(SetupError::MessageType)
	));
}

#[test]
fn activation_cannot_admit_at_local_deadline_even_when_peer_tip_is_close() {
	let setup = setup();
	assert_eq!(
		setup.validate_activation(&activate(&setup, 999), [9; 32], 1000),
		Err(SetupError::Height)
	);
}

#[test]
fn close_ack_verifies_book_size_and_preimages() {
	let setup = setup();
	let mut ack = message(
		Payload::CloseAck(CloseAck {
			activation_hash: [9; 32],
			num_slots: 2,
			settled: vec![1],
			preimages_tlv_present: true,
			preimages: vec![Preimage { slot: 1, value: [5; 32] }],
		}),
		43,
	);
	setup.validate_close_ack(&ack, [9; 32]).unwrap();
	let mut missing_preimage_tlv = ack.clone();
	let Payload::CloseAck(close) = &mut missing_preimage_tlv.payload else { unreachable!() };
	close.preimages_tlv_present = false;
	assert!(missing_preimage_tlv.signature_digest().is_err());
	assert!(missing_preimage_tlv.encode().is_err());
	assert_eq!(setup.validate_close_ack(&ack, [8; 32]), Err(SetupError::Transcript));
	let Payload::CloseAck(close) = &mut ack.payload else { unreachable!() };
	close.preimages[0].value = [6; 32];
	sign(&mut ack, 43);
	assert_eq!(setup.validate_close_ack(&ack, [9; 32]), Err(SetupError::CloseBook));
	let Payload::CloseAck(close) = &mut ack.payload else { unreachable!() };
	close.num_slots = 3;
	sign(&mut ack, 43);
	assert_eq!(setup.validate_close_ack(&ack, [9; 32]), Err(SetupError::CloseBook));
}

#[test]
fn chained_close_cannot_claim_a_later_slot_without_every_prior_slot() {
	let (mut init, mut accept) = setup_messages();
	let Payload::Init(terms) = &mut init.payload else { unreachable!() };
	terms.hash_chain = true;
	sign(&mut init, 42);
	let first_preimage = sha256::Hash::hash(&[6; 32]).to_byte_array();
	let Payload::Accept(accepted) = &mut accept.payload else { unreachable!() };
	accepted.payment_hashes[0] = sha256::Hash::hash(&first_preimage).to_byte_array();
	refresh_accept(&init, &mut accept);
	let setup = AuthenticatedSetup::new(&init, &accept, key(42), key(43)).unwrap();
	let mut ack = message(
		Payload::CloseAck(CloseAck {
			activation_hash: [9; 32],
			num_slots: 2,
			settled: vec![2],
			preimages_tlv_present: true,
			preimages: vec![Preimage { slot: 2, value: [6; 32] }],
		}),
		43,
	);
	assert_eq!(setup.validate_close_ack(&ack, [9; 32]), Err(SetupError::CloseBook));
	let Payload::CloseAck(close) = &mut ack.payload else { unreachable!() };
	close.settled[0] = 3;
	close.preimages.insert(0, Preimage { slot: 1, value: first_preimage });
	sign(&mut ack, 43);
	setup.validate_close_ack(&ack, [9; 32]).unwrap();
}

proptest! {
	#[test]
	fn signed_wrong_activation_hashes_are_rejected(field in 0_u8..3, index in 0_usize..32, bit in 0_u8..8) {
		let setup = setup();
		let mut activation = activate(&setup, 990);
		let Payload::Activate(contents) = &mut activation.payload else { unreachable!() };
		let hash = match field { 0 => &mut contents.setup_hash, 1 => &mut contents.book_hash, _ => &mut contents.commit_hash };
		hash[index] ^= 1 << bit;
		sign(&mut activation, 42);
		prop_assert_eq!(setup.validate_activation(&activation, [9; 32], 990), Err(SetupError::Transcript));
	}
}

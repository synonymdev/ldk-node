use bitcoin::secp256k1::ecdsa::Signature;
use bitcoin::secp256k1::{Message, PublicKey, Secp256k1};
use ffor_protocol::transcript;
use proptest::prelude::*;
use serde::Deserialize;

#[derive(Deserialize)]
struct Fixture {
	scenario: String,
	init_wire: String,
	accept_wire: String,
	activate_wire: String,
	ack_wire: String,
	book: String,
	init_hash: String,
	setup_hash: String,
	book_hash: String,
	commitment_hash: String,
	activation_hash: String,
	receiver_txid: String,
	settlement_txid: String,
}

fn hex(value: &str) -> Vec<u8> {
	assert_eq!(value.len() % 2, 0);
	(0..value.len()).step_by(2).map(|i| u8::from_str_radix(&value[i..i + 2], 16).unwrap()).collect()
}

fn digest(value: &str) -> [u8; 32] {
	hex(value).try_into().unwrap()
}

#[test]
fn appendix_d_all_six_transcripts_match_published_bytes() {
	let fixtures: Vec<Fixture> =
		serde_json::from_str(include_str!("data/appendix-d.json")).unwrap();
	assert_eq!(fixtures.len(), 6);
	let receiver_key = PublicKey::from_slice(&hex(
		"039fca7f8157aa768708894ffd92550fe970edd18526a5f936583ea3b54dab3228",
	))
	.unwrap();
	let settlement_key = PublicKey::from_slice(&hex(
		"02087b7d1b4789170f6e374f0a0e58a1b7a899e34929795314ab6964e69609e9c0",
	))
	.unwrap();
	let secp = Secp256k1::verification_only();
	for fixture in fixtures {
		let init = transcript::init_hash(&hex(&fixture.init_wire));
		assert_eq!(init, digest(&fixture.init_hash), "{}", fixture.scenario);
		let setup = transcript::setup_hash(&init, &hex(&fixture.accept_wire));
		assert_eq!(setup, digest(&fixture.setup_hash));
		let book = transcript::book_hash(&hex(&fixture.book));
		assert_eq!(book, digest(&fixture.book_hash));
		let commitment = transcript::commitment_hash(
			43,
			&digest(&fixture.receiver_txid),
			43,
			&digest(&fixture.settlement_txid),
		);
		assert_eq!(commitment, digest(&fixture.commitment_hash));
		assert_eq!(
			transcript::activation_hash(&setup, &book, &commitment, 790_000),
			digest(&fixture.activation_hash)
		);
		for (encoded, key) in [
			(&fixture.init_wire, receiver_key),
			(&fixture.accept_wire, settlement_key),
			(&fixture.activate_wire, receiver_key),
			(&fixture.ack_wire, settlement_key),
		] {
			let wire = hex(encoded);
			let signature_start = wire.len() - 64;
			let signature = Signature::from_compact(&wire[signature_start..]).unwrap();
			let mut normalized = signature;
			normalized.normalize_s();
			assert_eq!(signature, normalized);
			let kind = u16::from_be_bytes(wire[..2].try_into().unwrap());
			let unsigned = &wire[2..signature_start];
			let message = Message::from_digest(transcript::message_digest(kind, unsigned));
			secp.verify_ecdsa(&message, &signature, &key).unwrap();
			let wrong_type = Message::from_digest(transcript::message_digest(kind + 2, unsigned));
			assert!(secp.verify_ecdsa(&wrong_type, &signature, &key).is_err());
		}
	}
}

proptest! {
	#[test]
	fn transcript_fields_and_domains_are_bound(bytes in prop::collection::vec(any::<u8>(), 0..2048), height in 0_u32..u32::MAX) {
		let init = transcript::init_hash(&bytes);
		let book = transcript::book_hash(&bytes);
		prop_assert_ne!(init, book);
		prop_assert_ne!(transcript::message_digest(55001, &bytes), transcript::message_digest(55003, &bytes));
		prop_assert_ne!(transcript::activation_hash(&init, &book, &init, height), transcript::activation_hash(&init, &book, &init, height + 1));
	}
}

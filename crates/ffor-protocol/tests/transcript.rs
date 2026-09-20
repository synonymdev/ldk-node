use bitcoin::hashes::{sha256, Hash};
use bitcoin::secp256k1::{Message, PublicKey, Secp256k1, SecretKey};
use ffor_protocol::transcript;
use ffor_protocol::transcript::{verify_message_signature, SignatureError};
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
			let signature = wire[signature_start..].try_into().unwrap();
			let kind = u16::from_be_bytes(wire[..2].try_into().unwrap());
			let unsigned = &wire[2..signature_start];
			verify_message_signature(kind, unsigned, &signature, &key).unwrap();
			assert_eq!(
				verify_message_signature(kind + 2, unsigned, &signature, &key),
				Err(SignatureError::Invalid)
			);
		}
	}
}

fn test_key() -> (SecretKey, PublicKey) {
	let secret = SecretKey::from_slice(&[42; 32]).unwrap();
	let public = PublicKey::from_secret_key(&Secp256k1::new(), &secret);
	(secret, public)
}

fn sign_digest(digest: [u8; 32]) -> [u8; 64] {
	Secp256k1::new().sign_ecdsa(&Message::from_digest(digest), &test_key().0).serialize_compact()
}

#[test]
fn signature_rejects_invalid_compact_scalars() {
	assert_eq!(
		verify_message_signature(55001, b"body", &[255; 64], &test_key().1),
		Err(SignatureError::Malformed)
	);
	assert_eq!(
		verify_message_signature(55001, b"body", &[0; 64], &test_key().1),
		Err(SignatureError::Invalid)
	);
}

#[test]
fn signature_rejects_high_s_without_normalizing_received_bytes() {
	let body = [7; 128];
	let mut signature = sign_digest(transcript::message_digest(55001, &body));
	// Negating s modulo the curve order preserves ECDSA validity but violates canonical low-S.
	let order = hex("fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141");
	let mut borrow = 0_i16;
	for index in (0..32).rev() {
		let value = i16::from(order[index]) - i16::from(signature[32 + index]) - borrow;
		signature[32 + index] = value.rem_euclid(256) as u8;
		borrow = i16::from(value < 0);
	}
	assert_eq!(
		verify_message_signature(55001, &body, &signature, &test_key().1),
		Err(SignatureError::HighS)
	);
}

#[test]
fn signature_requires_single_hash_and_ffor_domain() {
	let body = [9; 128];
	let digest = transcript::message_digest(55001, &body);
	for wrong_digest in
		[sha256::Hash::hash(&body).to_byte_array(), sha256::Hash::hash(&digest).to_byte_array()]
	{
		assert_eq!(
			verify_message_signature(55001, &body, &sign_digest(wrong_digest), &test_key().1),
			Err(SignatureError::Invalid)
		);
	}
}

proptest! {
	#[test]
	fn signed_bytes_and_expected_peer_cannot_be_substituted(mut body in prop::collection::vec(any::<u8>(), 64..2048), changed_index in any::<usize>()) {
		let (_, public) = test_key();
		let signature = sign_digest(transcript::message_digest(55001, &body));
		prop_assert_eq!(verify_message_signature(55001, &body, &signature, &public), Ok(()));
		let other_secret = SecretKey::from_slice(&[43; 32]).unwrap();
		let other_public = PublicKey::from_secret_key(&Secp256k1::new(), &other_secret);
		prop_assert_eq!(verify_message_signature(55001, &body, &signature, &other_public), Err(SignatureError::Invalid));
		let index = changed_index % body.len();
		body[index] ^= 1;
		prop_assert_eq!(verify_message_signature(55001, &body, &signature, &public), Err(SignatureError::Invalid));
	}

	#[test]
	fn transcript_fields_and_domains_are_bound(bytes in prop::collection::vec(any::<u8>(), 0..2048), height in 0_u32..u32::MAX) {
		let init = transcript::init_hash(&bytes);
		let book = transcript::book_hash(&bytes);
		prop_assert_ne!(init, book);
		prop_assert_ne!(transcript::message_digest(55001, &bytes), transcript::message_digest(55003, &bytes));
		prop_assert_ne!(transcript::activation_hash(&init, &book, &init, height), transcript::activation_hash(&init, &book, &init, height + 1));
	}
}

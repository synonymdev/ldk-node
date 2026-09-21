use bitcoin::hashes::{sha256, Hash};
use bitcoin::secp256k1::SecretKey;
use lightning::ln::ffor::FFORWitnessDecryptionError;
use lightning_ffor::setup::AuthenticatedSetup;
use lightning_ffor::wire::Message as WireMessage;
use lightning_ffor::witness::{SignedManifest, CIPHERTEXT_LEN};

use super::*;
use crate::ffor::witness_store::record::WitnessPolicy;

// Public deterministic keys only. Production material is generated with operating-system entropy.
fn secret(byte: u8) -> SecretKey {
	SecretKey::from_slice(&[byte; 32]).unwrap()
}

fn public(byte: u8) -> PublicKey {
	PublicKey::from_secret_key(&Secp256k1::new(), &secret(byte))
}

struct Fixture<'a>(&'a str);

impl Fixture<'_> {
	fn bytes(&self, name: &str) -> Vec<u8> {
		let value = self
			.0
			.lines()
			.find_map(|line| {
				let (key, value) = line.split_once('=')?;
				(key == name).then_some(value)
			})
			.unwrap();
		hex(value)
	}

	fn stored(&self) -> StoredWitnessEpoch {
		let receiver = PublicKey::from_slice(&hex(
			"039fca7f8157aa768708894ffd92550fe970edd18526a5f936583ea3b54dab3228",
		))
		.unwrap();
		let settlement = PublicKey::from_slice(&hex(
			"02087b7d1b4789170f6e374f0a0e58a1b7a899e34929795314ab6964e69609e9c0",
		))
		.unwrap();
		let setup = AuthenticatedSetup::new(
			&WireMessage::decode(&self.bytes("init")).unwrap(),
			&WireMessage::decode(&self.bytes("accept")).unwrap(),
			receiver,
			settlement,
		)
		.unwrap();
		let manifest = SignedManifest::decode(&self.bytes("manifest"), &setup).unwrap();
		let params = manifest.unsigned().parameters();
		let policy = WitnessPolicy {
			witness: public(43),
			retention_until: params.retention_until,
			minimum_receipts: params.minimum_receipts,
		};
		StoredWitnessEpoch {
			binding_digest: [1; 32],
			encryption_secret: Zeroizing::new([44; 32]),
			witnesses: vec![WitnessKeys {
				acknowledgement: None,
				policy,
				fetch_secret: Zeroizing::new([42; 32]),
				manifest,
			}],
		}
	}
}

fn fixtures() -> impl Iterator<Item = Fixture<'static>> {
	include_str!("fixtures.txt").trim().split("\n\n").map(Fixture)
}

fn hex(value: &str) -> Vec<u8> {
	(0..value.len()).step_by(2).map(|i| u8::from_str_radix(&value[i..i + 2], 16).unwrap()).collect()
}

#[test]
fn ffor_witness_key_use_fetch_binds_retained_mailbox_key_and_cursor() {
	for fixture in fixtures() {
		let stored = fixture.stored();
		let manifest = stored.manifest(public(43)).unwrap();
		let count = ((manifest.unsigned().canonical_book().len() - 36) / 58) as u16;
		for cursor in [None, Some(0), Some(count)] {
			let request = stored.prepare_fetch(public(43), cursor).unwrap();
			let fields = request.unsigned().parameters();
			assert_eq!(fields.mailbox_id, manifest.unsigned().parameters().mailbox_id);
			assert_eq!(fields.after_slot, cursor);
			assert!(fields.extensions.is_empty());
			assert_eq!(request.fetch_key(), public(42));
			assert_eq!(SignedFetch::decode(&request.encode(), public(42)).unwrap(), request);
			assert!(SignedFetch::decode(&request.encode(), public(41)).is_err());
		}
		assert_eq!(
			stored.prepare_fetch(public(43), Some(count + 1)),
			Err(WitnessKeyUseError::Request(WitnessError::Pagination))
		);
		assert_eq!(stored.prepare_fetch(public(40), None), Err(WitnessKeyUseError::UnknownWitness));
	}
}

#[test]
fn ffor_witness_key_use_fetch_entropy_failure_has_no_fallback() {
	let mut stored = fixtures().next().unwrap().stored();
	assert_eq!(
		stored.prepare_fetch_with(public(43), None, || Err(WitnessStoreError::Entropy)),
		Err(WitnessKeyUseError::Entropy)
	);
	assert_eq!(
		stored.prepare_fetch_with(public(40), None, || panic!("invalid witness drew entropy")),
		Err(WitnessKeyUseError::UnknownWitness)
	);
	assert_eq!(
		stored.prepare_fetch_with(public(43), Some(2), || panic!("invalid cursor drew entropy")),
		Err(WitnessKeyUseError::Request(WitnessError::Pagination))
	);
	stored.witnesses[0].fetch_secret = Zeroizing::new([0; 32]);
	assert_eq!(stored.prepare_fetch(public(43), None), Err(WitnessKeyUseError::KeyMaterial));
	stored.witnesses[0].fetch_secret = Zeroizing::new([41; 32]);
	assert!(matches!(stored.prepare_fetch(public(43), None), Err(WitnessKeyUseError::Request(_))));
}

#[test]
fn ffor_witness_key_use_opens_pinned_beignet_records_without_key_export() {
	for fixture in fixtures() {
		let stored = fixture.stored();
		let record = EncryptedRecord::decode(&fixture.bytes("record")).unwrap();
		let receipt = stored.decrypt_record(public(43), record.clone()).unwrap();
		assert_eq!(receipt.header(), record.header());
		assert_eq!(receipt.body().preimage().as_slice(), &fixture.bytes("body")[34..66]);
		assert_eq!(stored.decrypt_record(public(43), record).unwrap(), receipt);
		assert!(!format!("{receipt:?}").contains(&format!("{:?}", &fixture.bytes("body")[34..66])));
	}
}

#[test]
fn ffor_witness_key_use_rejects_other_witness_epoch_and_encryption_key() {
	let first = fixtures().next().unwrap();
	let record = EncryptedRecord::decode(&first.bytes("record")).unwrap();
	let mut stored = first.stored();
	assert_eq!(
		stored.decrypt_record(public(40), record.clone()),
		Err(WitnessKeyUseError::UnknownWitness)
	);
	stored.witnesses[0].policy.witness = public(40);
	assert_eq!(
		stored.decrypt_record(public(40), record.clone()),
		Err(WitnessKeyUseError::Record(WitnessError::Witness))
	);
	let other = fixtures().nth(1).unwrap().stored();
	assert_eq!(
		other.decrypt_record(public(43), record.clone()),
		Err(WitnessKeyUseError::Record(WitnessError::Mailbox))
	);
	stored.witnesses[0].policy.witness = public(43);
	stored.encryption_secret = Zeroizing::new([0; 32]);
	assert_eq!(
		stored.decrypt_record(public(43), record.clone()),
		Err(WitnessKeyUseError::KeyMaterial)
	);
	stored.encryption_secret = Zeroizing::new([45; 32]);
	assert_eq!(
		stored.decrypt_record(public(43), record),
		Err(WitnessKeyUseError::Decryption(FFORWitnessDecryptionError::EncryptionKey))
	);
}

fn resign_record(bytes: &mut [u8]) {
	let digest = sha256::Hash::hash(&[b"ffor/witness/record".as_slice(), &bytes[..235]].concat());
	let signature = Secp256k1::new().sign_ecdsa_with_noncedata(
		&Message::from_digest(digest.to_byte_array()),
		&secret(43),
		&[91; 32],
	);
	bytes[235..299].copy_from_slice(&signature.serialize_compact());
}

#[test]
fn ffor_witness_key_use_rejects_signed_mailbox_and_ciphertext_substitution() {
	let fixture = fixtures().next().unwrap();
	let stored = fixture.stored();
	let mut changed = fixture.bytes("record");
	changed[2] ^= 1;
	resign_record(&mut changed);
	let record = EncryptedRecord::decode(&changed).unwrap();
	assert_eq!(
		stored.decrypt_record(public(43), record),
		Err(WitnessKeyUseError::Record(WitnessError::Mailbox))
	);
	let mut changed = fixture.bytes("record");
	changed[68] ^= 1;
	resign_record(&mut changed);
	let record = EncryptedRecord::decode(&changed).unwrap();
	assert_eq!(
		stored.decrypt_record(public(43), record),
		Err(WitnessKeyUseError::Record(WitnessError::Transcript))
	);
	for offset in [301 + 33, 301 + CIPHERTEXT_LEN - 1] {
		let mut changed = fixture.bytes("record");
		changed[offset] ^= 1;
		let ciphertext_hash = sha256::Hash::hash(&changed[301..301 + CIPHERTEXT_LEN]);
		changed[203..235].copy_from_slice(ciphertext_hash.as_byte_array());
		resign_record(&mut changed);
		let record = EncryptedRecord::decode(&changed).unwrap();
		assert_eq!(
			stored.decrypt_record(public(43), record),
			Err(WitnessKeyUseError::Decryption(FFORWitnessDecryptionError::Ciphertext))
		);
	}
}

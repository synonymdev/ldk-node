use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use bitcoin::secp256k1::{Message as SecpMessage, PublicKey, Secp256k1, SecretKey};
use bitcoin::{OutPoint, Txid};
use lightning::io;
use lightning::util::persist::{KVStore, KVStoreSync};
use lightning_ffor::setup::AuthenticatedSetup;
use lightning_ffor::transcript;
use lightning_ffor::wire::{Accept, Activate, Header, Init, Message, Payload};
use proptest::prelude::*;

use crate::io::test_utils::InMemoryStore;

use super::*;

struct TestStore {
	inner: InMemoryStore,
	writes: AtomicUsize,
	write_failure: AtomicUsize,
	read_failure: AtomicBool,
}

impl TestStore {
	fn new() -> Arc<Self> {
		Arc::new(Self {
			inner: InMemoryStore::new(),
			writes: AtomicUsize::new(0),
			write_failure: AtomicUsize::new(0),
			read_failure: AtomicBool::new(false),
		})
	}
	fn bytes(&self, key: &str) -> Vec<u8> {
		KVStoreSync::read(&self.inner, PRIMARY_NAMESPACE, SECONDARY_NAMESPACE, key).unwrap()
	}
	fn replace(&self, key: &str, bytes: Vec<u8>) {
		KVStoreSync::write(&self.inner, PRIMARY_NAMESPACE, SECONDARY_NAMESPACE, key, bytes)
			.unwrap();
	}
}

impl KVStoreSync for TestStore {
	fn read(&self, primary: &str, secondary: &str, key: &str) -> io::Result<Vec<u8>> {
		if self.read_failure.load(Ordering::SeqCst) {
			return Err(io::Error::new(io::ErrorKind::Other, "injected read failure"));
		}
		KVStoreSync::read(&self.inner, primary, secondary, key)
	}
	fn write(&self, primary: &str, secondary: &str, key: &str, bytes: Vec<u8>) -> io::Result<()> {
		self.writes.fetch_add(1, Ordering::SeqCst);
		let failure = self.write_failure.swap(0, Ordering::SeqCst);
		if failure != 1 {
			KVStoreSync::write(&self.inner, primary, secondary, key, bytes)?;
		}
		if failure != 0 {
			Err(io::Error::new(io::ErrorKind::Other, "injected write failure"))
		} else {
			Ok(())
		}
	}
	fn remove(&self, primary: &str, secondary: &str, key: &str, lazy: bool) -> io::Result<()> {
		KVStoreSync::remove(&self.inner, primary, secondary, key, lazy)
	}
	fn list(&self, primary: &str, secondary: &str) -> io::Result<Vec<String>> {
		KVStoreSync::list(&self.inner, primary, secondary)
	}
}

impl KVStore for TestStore {
	fn read(
		&self, p: &str, s: &str, k: &str,
	) -> Pin<Box<dyn Future<Output = io::Result<Vec<u8>>> + Send>> {
		let result = KVStoreSync::read(self, p, s, k);
		Box::pin(async move { result })
	}
	fn write(
		&self, p: &str, s: &str, k: &str, bytes: Vec<u8>,
	) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send>> {
		let result = KVStoreSync::write(self, p, s, k, bytes);
		Box::pin(async move { result })
	}
	fn remove(
		&self, p: &str, s: &str, k: &str, lazy: bool,
	) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send>> {
		let result = KVStoreSync::remove(self, p, s, k, lazy);
		Box::pin(async move { result })
	}
	fn list(
		&self, p: &str, s: &str,
	) -> Pin<Box<dyn Future<Output = io::Result<Vec<String>>> + Send>> {
		let result = KVStoreSync::list(self, p, s);
		Box::pin(async move { result })
	}
}

fn key(byte: u8) -> PublicKey {
	PublicKey::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[byte; 32]).unwrap())
}

fn signed(header: Header, payload: Payload, signer: u8) -> Message {
	let mut message = Message { header, payload, extensions: vec![], signature: [0; 64] };
	message.signature = Secp256k1::new()
		.sign_ecdsa_with_noncedata(
			&SecpMessage::from_digest(message.signature_digest().unwrap()),
			&SecretKey::from_slice(&[signer; 32]).unwrap(),
			&[91; 32],
		)
		.serialize_compact();
	message
}

pub(super) fn evidence(
	epoch: u8, slots: usize,
) -> (WitnessStorageIdentity, AuthenticatedSetup, Message, Message) {
	let header = Header { channel_id: [1; 32], epoch_id: [epoch; 32] };
	let init = signed(
		header,
		Payload::Init(Init {
			budget_msat: 1_000_000 * slots as u64,
			min_payment_msat: 1_000_000,
			settlement_deadline: 100,
			voucher_expiry: 2000,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			amounts_msat: vec![1_000_000; slots],
			witness_peers: None,
			hash_chain: false,
		}),
		42,
	);
	let accept = signed(
		header,
		Payload::Accept(Accept {
			s_commitment_number: 3,
			payment_hashes: (0..slots)
				.map(|index| hash(&[&(index as u64).to_be_bytes()]))
				.collect(),
			s_htlc_id_base: 9,
			amounts_msat: vec![1_000_000; slots],
			init_hash: transcript::init_hash(&init.encode().unwrap()),
		}),
		43,
	);
	let setup = AuthenticatedSetup::new(&init, &accept, key(42), key(43)).unwrap();
	let activation = signed(
		header,
		Payload::Activate(Activate {
			setup_hash: setup.setup_hash(),
			book_hash: setup.book_hash(),
			commit_hash: [9; 32],
			epoch_start_height: 50,
		}),
		42,
	);
	let h_act = setup.validate_activation(&activation, [9; 32], 50).unwrap();
	let ack = signed(header, Payload::ActivateAck(h_act), 43);
	let identity = WitnessStorageIdentity {
		chain_hash: [4; 32],
		actual_node: key(42),
		settlement_node: key(43),
		original_funding: OutPoint { txid: Txid::from_byte_array([8; 32]), vout: 0 },
	};
	(identity, setup, activation, ack)
}

fn binding(epoch: u8) -> WitnessStorageBinding {
	let (identity, setup, activation, ack) = evidence(epoch, 2);
	WitnessStorageBinding::new(identity, setup, &activation, &ack).unwrap()
}

fn policies() -> Vec<WitnessPolicy> {
	vec![
		WitnessPolicy { witness: key(2), retention_until: 2144, minimum_receipts: 0 },
		WitnessPolicy { witness: key(3), retention_until: 2200, minimum_receipts: 2 },
	]
}

#[test]
fn ffor_witness_store_exact_manifests_survive_retry_and_restart() {
	let backend = TestStore::new();
	let store = WitnessSecretStore::open(&[5; 64], backend.clone()).unwrap();
	let binding = binding(1);
	let first = store.create(&binding, &policies()).unwrap();
	let saved = backend.bytes(&binding.storage_key());
	let mut reordered = policies();
	reordered.reverse();
	let repeated = store.create(&binding, &reordered).unwrap();
	let reopened = WitnessSecretStore::open(&[5; 64], backend.clone()).unwrap();
	let restored = reopened.load(&binding).unwrap();
	assert_eq!(backend.writes.load(Ordering::SeqCst), 2);
	assert_eq!(backend.bytes(&binding.storage_key()), saved);
	for policy in policies() {
		let manifest = first.manifest(policy.witness).unwrap();
		assert_eq!(repeated.manifest(policy.witness).unwrap().encode(), manifest.encode());
		assert_eq!(restored.manifest(policy.witness).unwrap().encode(), manifest.encode());
		assert_eq!(manifest.unsigned().activation_hash(), binding.activation_hash());
	}
	let mut changed = policies();
	changed[0].retention_until += 1;
	assert_eq!(store.create(&binding, &changed).unwrap_err(), WitnessStoreError::Conflict);
	assert_eq!(backend.bytes(&binding.storage_key()), saved);
}

#[test]
fn ffor_witness_store_fresh_keys_are_scoped_to_epoch_and_witness() {
	let store = WitnessSecretStore::open(&[5; 64], TestStore::new()).unwrap();
	let first = store.create(&binding(1), &policies()).unwrap();
	let next = store.create(&binding(2), &policies()).unwrap();
	let first_w = first.manifest(key(2)).unwrap().unsigned().parameters();
	let other_w = first.manifest(key(3)).unwrap().unsigned().parameters();
	let next_w = next.manifest(key(2)).unwrap().unsigned().parameters();
	assert_eq!(first_w.encryption_public_key, other_w.encryption_public_key);
	assert_ne!(first_w.fetch_public_key, other_w.fetch_public_key);
	assert_ne!(first_w.mailbox_id, other_w.mailbox_id);
	assert_ne!(first_w.encryption_public_key, next_w.encryption_public_key);
	assert_ne!(first_w.fetch_public_key, next_w.fetch_public_key);
	assert_ne!(first_w.mailbox_id, next_w.mailbox_id);
}

#[test]
fn ffor_witness_store_rejects_invalid_policy_before_any_storage_write() {
	let backend = TestStore::new();
	let store = WitnessSecretStore::open(&[5; 64], backend.clone()).unwrap();
	let binding = binding(1);
	assert_eq!(store.create(&binding, &[]).unwrap_err(), WitnessStoreError::Capacity);
	let policy = policies()[0];
	assert_eq!(store.create(&binding, &[policy, policy]).unwrap_err(), WitnessStoreError::Conflict);
	let short = WitnessPolicy { retention_until: 2143, ..policy };
	assert_eq!(store.create(&binding, &[short]).unwrap_err(), WitnessStoreError::Binding);
	assert_eq!(backend.writes.load(Ordering::SeqCst), 0);
	assert!(store.state.lock().unwrap().records.is_empty());
}

#[test]
fn ffor_witness_store_envelope_hides_raw_secrets_and_uses_fresh_aead_nonces() {
	let binding = binding(1);
	let record =
		StoredWitnessEpoch::generate(&binding, &record::checked_policies(&policies()).unwrap())
			.unwrap();
	let wrapping = WrappingKey::derive(&[5; 64]);
	let first = wrapping.seal(&binding, &record).unwrap();
	let second = wrapping.seal(&binding, &record).unwrap();
	assert_ne!(first, second);
	let raw = record.encode();
	for secret in [&raw[34..66], &raw[100..132]] {
		assert!(!first.windows(32).any(|window| window == secret));
		assert!(!second.windows(32).any(|window| window == secret));
	}
	assert_eq!(&*wrapping.open(&binding, &first).unwrap().encode(), &*raw);
	assert_eq!(&*wrapping.open(&binding, &second).unwrap().encode(), &*raw);
}

#[test]
fn ffor_witness_store_uncertain_writes_recover_only_the_exact_candidate() {
	for failure in [1, 2] {
		let backend = TestStore::new();
		let store = WitnessSecretStore::open(&[5; 64], backend.clone()).unwrap();
		let binding = binding(1);
		backend.write_failure.store(failure, Ordering::SeqCst);
		assert_eq!(store.create(&binding, &policies()).unwrap_err(), WitnessStoreError::Storage);
		let candidate = store.state.lock().unwrap().uncertain.as_ref().unwrap().bytes.clone();
		assert_eq!(store.load(&binding).unwrap_err(), WitnessStoreError::Uncertain);
		assert_eq!(store.create(&binding, &policies()).unwrap_err(), WitnessStoreError::Uncertain);
		backend.read_failure.store(true, Ordering::SeqCst);
		assert_eq!(store.recover_write(), Err(WitnessStoreError::Storage));
		backend.read_failure.store(false, Ordering::SeqCst);
		store.recover_write().unwrap();
		assert_eq!(backend.bytes(&binding.storage_key()), candidate);
		assert!(store.load(&binding).is_ok());
		assert_eq!(backend.writes.load(Ordering::SeqCst), 2);
		let reopened = WitnessSecretStore::open(&[5; 64], backend.clone()).unwrap();
		assert!(reopened.load(&binding).is_ok());
	}
}

#[test]
fn ffor_witness_store_conflicting_recovery_stays_blocked() {
	let backend = TestStore::new();
	let store = WitnessSecretStore::open(&[5; 64], backend.clone()).unwrap();
	backend.write_failure.store(2, Ordering::SeqCst);
	let binding = binding(1);
	assert!(store.create(&binding, &policies()).is_err());
	let original = backend.bytes(&binding.storage_key());
	let mut wrong = original.clone();
	wrong[50] ^= 1;
	backend.replace(&binding.storage_key(), wrong.clone());
	assert_eq!(store.recover_write(), Err(WitnessStoreError::Conflict));
	assert_eq!(store.load(&binding).unwrap_err(), WitnessStoreError::Uncertain);
	assert_eq!(backend.bytes(&binding.storage_key()), wrong);
	backend.replace(&binding.storage_key(), original);
	store.recover_write().unwrap();
}

#[test]
fn ffor_witness_store_restart_recovers_a_write_whose_success_reply_was_lost() {
	let backend = TestStore::new();
	let binding = binding(1);
	let candidate;
	{
		let store = WitnessSecretStore::open(&[5; 64], backend.clone()).unwrap();
		backend.write_failure.store(2, Ordering::SeqCst);
		assert_eq!(store.create(&binding, &policies()).unwrap_err(), WitnessStoreError::Storage);
		candidate = store.state.lock().unwrap().uncertain.as_ref().unwrap().bytes.clone();
	}
	let store = WitnessSecretStore::open(&[5; 64], backend.clone()).unwrap();
	assert!(store.load(&binding).is_ok());
	assert!(store.create(&binding, &policies()).is_ok());
	assert_eq!(backend.bytes(&binding.storage_key()), candidate);
	assert_eq!(backend.writes.load(Ordering::SeqCst), 2);
}

#[test]
fn ffor_witness_store_visible_bytes_need_a_successful_durability_retry() {
	let backend = TestStore::new();
	let binding = binding(1);
	let candidate;
	{
		let store = WitnessSecretStore::open(&[5; 64], backend.clone()).unwrap();
		backend.write_failure.store(2, Ordering::SeqCst);
		assert!(store.create(&binding, &policies()).is_err());
		candidate = backend.bytes(&binding.storage_key());
		backend.write_failure.store(2, Ordering::SeqCst);
		assert_eq!(store.recover_write(), Err(WitnessStoreError::Storage));
		assert_eq!(store.load(&binding).unwrap_err(), WitnessStoreError::Uncertain);
	}
	let restored = WitnessSecretStore::open(&[5; 64], backend.clone()).unwrap();
	backend.write_failure.store(2, Ordering::SeqCst);
	assert_eq!(restored.load(&binding).unwrap_err(), WitnessStoreError::Storage);
	assert_eq!(restored.load(&binding).unwrap_err(), WitnessStoreError::Uncertain);
	assert_eq!(backend.bytes(&binding.storage_key()), candidate);
	restored.recover_write().unwrap();
	assert!(restored.load(&binding).is_ok());
	assert_eq!(backend.bytes(&binding.storage_key()), candidate);
	assert_eq!(backend.writes.load(Ordering::SeqCst), 4);
}

#[test]
fn ffor_witness_store_binding_requires_signed_roles_and_exact_historical_terms() {
	let (mut identity, setup, activate, ack) = evidence(1, 2);
	identity.actual_node = key(44);
	assert!(WitnessStorageBinding::new(identity, setup, &activate, &ack).is_err());
	let (identity, setup, mut activate, ack) = evidence(1, 2);
	activate.signature[0] ^= 1;
	assert!(WitnessStorageBinding::new(identity, setup, &activate, &ack).is_err());
	let (identity, setup, activate, mut ack) = evidence(1, 2);
	ack.header.epoch_id[0] ^= 1;
	assert!(WitnessStorageBinding::new(identity, setup, &activate, &ack).is_err());
	let (identity, setup, activate, _) = evidence(1, 2);
	let ack = signed(setup.header(), Payload::ActivateAck([0; 32]), 43);
	assert!(WitnessStorageBinding::new(identity, setup, &activate, &ack).is_err());
	let backend = TestStore::new();
	let store = WitnessSecretStore::open(&[5; 64], backend).unwrap();
	store.create(&binding(1), &policies()).unwrap();
	let (mut identity, setup, activate, ack) = evidence(1, 2);
	identity.original_funding.vout = 1;
	let changed = WitnessStorageBinding::new(identity, setup, &activate, &ack).unwrap();
	assert_eq!(changed.storage_key(), binding(1).storage_key());
	assert_eq!(store.load(&changed).unwrap_err(), WitnessStoreError::Binding);
}

#[test]
fn ffor_witness_store_corruption_missing_and_wrong_seed_never_regenerate() {
	let backend = TestStore::new();
	let store = WitnessSecretStore::open(&[5; 64], backend.clone()).unwrap();
	let binding = binding(1);
	store.create(&binding, &policies()).unwrap();
	let original = backend.bytes(&binding.storage_key());
	let wrong_seed = WitnessSecretStore::open(&[6; 64], backend.clone()).unwrap();
	assert_eq!(wrong_seed.load(&binding).unwrap_err(), WitnessStoreError::Corrupt);
	for index in [0, 4, 8, 50, original.len() - 1] {
		let mut corrupted = original.clone();
		corrupted[index] ^= 1;
		backend.replace(&binding.storage_key(), corrupted.clone());
		assert_eq!(store.load(&binding).unwrap_err(), WitnessStoreError::Conflict);
		let restored = WitnessSecretStore::open(&[5; 64], backend.clone()).unwrap();
		assert!(restored.load(&binding).is_err());
		assert!(restored.create(&binding, &policies()).is_err());
		assert_eq!(backend.bytes(&binding.storage_key()), corrupted);
	}
	KVStoreSync::remove(
		&*backend,
		PRIMARY_NAMESPACE,
		SECONDARY_NAMESPACE,
		&binding.storage_key(),
		false,
	)
	.unwrap();
	assert_eq!(store.load(&binding).unwrap_err(), WitnessStoreError::Missing);
	assert_eq!(store.create(&binding, &policies()).unwrap_err(), WitnessStoreError::Missing);
	assert_eq!(backend.writes.load(Ordering::SeqCst), 1);
}

#[test]
fn ffor_witness_store_plaintext_parser_checks_every_key_and_manifest() {
	let binding = binding(1);
	let record =
		StoredWitnessEpoch::generate(&binding, &record::checked_policies(&policies()).unwrap())
			.unwrap();
	let encoded = record.encode();
	assert!(StoredWitnessEpoch::decode(&binding, &encoded).is_ok());
	for length in 0..encoded.len() {
		assert!(StoredWitnessEpoch::decode(&binding, &encoded[..length]).is_err());
	}
	for offset in [0, 2, 34, 66, 100, 132, 136, encoded.len() - 1] {
		let mut damaged = encoded.clone();
		damaged[offset] ^= 1;
		assert!(StoredWitnessEpoch::decode(&binding, &damaged).is_err(), "offset {offset}");
	}
	let mut invalid_witness = encoded.clone();
	invalid_witness[67] = 0;
	assert!(StoredWitnessEpoch::decode(&binding, &invalid_witness).is_err());
	let mut trailing = encoded.clone();
	trailing.push(0);
	assert!(StoredWitnessEpoch::decode(&binding, &trailing).is_err());
	let secret_debug = format!("{:?}", &encoded[34..66]);
	for debug in [format!("{record:?}"), format!("{record:#?}")] {
		assert!(debug.contains("[redacted]"));
		assert!(!debug.contains(&secret_debug));
	}
}

#[test]
fn ffor_witness_store_limits_refuse_without_eviction_or_partial_write() {
	let backend = TestStore::new();
	for index in 0..16 {
		backend.replace(&format!("{index:064x}"), vec![9; MAX_RECORD_BYTES]);
	}
	let store = WitnessSecretStore::open(&[5; 64], backend.clone()).unwrap();
	assert_eq!(store.create(&binding(1), &policies()).unwrap_err(), WitnessStoreError::Capacity);
	assert_eq!(backend.writes.load(Ordering::SeqCst), 0);
	backend.replace(&format!("{:064x}", 16), vec![9]);
	assert_eq!(
		WitnessSecretStore::open(&[5; 64], backend).unwrap_err(),
		WitnessStoreError::Capacity
	);
	let backend = TestStore::new();
	for index in 0..64 {
		backend.replace(&format!("{index:064x}"), vec![9]);
	}
	let store = WitnessSecretStore::open(&[5; 64], backend.clone()).unwrap();
	assert_eq!(store.create(&binding(1), &policies()).unwrap_err(), WitnessStoreError::Capacity);
	backend.replace(&format!("{:064x}", 64), vec![9]);
	assert_eq!(
		WitnessSecretStore::open(&[5; 64], backend).unwrap_err(),
		WitnessStoreError::Capacity
	);
	let backend = TestStore::new();
	backend.replace(&format!("{:064x}", 0), vec![0; MAX_RECORD_BYTES + 1]);
	assert_eq!(
		WitnessSecretStore::open(&[5; 64], backend).unwrap_err(),
		WitnessStoreError::Capacity
	);
}

#[test]
fn ffor_witness_store_maximum_book_with_four_witnesses_fits_supported_bound() {
	let (identity, setup, activate, ack) = evidence(1, 483);
	let binding = WitnessStorageBinding::new(identity, setup, &activate, &ack).unwrap();
	let mut policy = policies();
	policy.push(WitnessPolicy { witness: key(4), ..policy[0] });
	policy.push(WitnessPolicy { witness: key(5), ..policy[0] });
	let backend = TestStore::new();
	let store = WitnessSecretStore::open(&[5; 64], backend.clone()).unwrap();
	assert!(store.create(&binding, &policy).is_ok());
	assert!(backend.bytes(&binding.storage_key()).len() < MAX_RECORD_BYTES);
	policy.push(WitnessPolicy { witness: key(6), ..policy[0] });
	assert_eq!(store.create(&binding, &policy).unwrap_err(), WitnessStoreError::Capacity);
}

#[test]
fn ffor_witness_store_concurrent_create_shares_one_durable_candidate() {
	let backend = TestStore::new();
	let store = Arc::new(WitnessSecretStore::open(&[5; 64], backend.clone()).unwrap());
	let binding = binding(1);
	let tasks: Vec<_> = (0..8)
		.map(|_| {
			let store = store.clone();
			let binding = binding.clone();
			std::thread::spawn(move || {
				store.create(&binding, &policies()).unwrap().manifest(key(2)).unwrap().encode()
			})
		})
		.collect();
	let manifests: Vec<_> = tasks.into_iter().map(|task| task.join().unwrap()).collect();
	assert!(manifests.iter().all(|manifest| manifest == &manifests[0]));
	assert_eq!(backend.writes.load(Ordering::SeqCst), 1);
}

proptest! {
	#[test]
	fn ffor_witness_store_bounded_plaintext_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..2048)) {
		let binding = binding(1);
		if let Ok(record) = StoredWitnessEpoch::decode(&binding, &bytes) {
			prop_assert_eq!(&*record.encode(), &bytes);
		}
	}

	#[test]
	fn ffor_witness_store_ciphertext_mutations_fail_closed(offset in any::<usize>(), bit in 0u8..8) {
		let binding = binding(1);
		let record = StoredWitnessEpoch::generate(&binding, &record::checked_policies(&policies()).unwrap()).unwrap();
		let wrapping = WrappingKey::derive(&[5; 64]);
		let mut bytes = wrapping.seal(&binding, &record).unwrap();
		let index = offset % bytes.len();
		bytes[index] ^= 1 << bit;
		prop_assert!(wrapping.open(&binding, &bytes).is_err());
	}
}

use std::sync::atomic::Ordering;
use std::sync::Arc;

use bitcoin::hashes::Hash;
use bitcoin::{OutPoint, Txid};
use lightning::util::persist::KVStoreSync;
use lightning_ffor::setup::AuthenticatedSetup;
use lightning_ffor::wire::Message;
use lightning_ffor::witness::SignedManifest;
use proptest::prelude::*;

use super::*;
use crate::ffor::witness_store::tests::{
	binding, checked_acknowledgement, key, policies, TestStore,
};
use crate::ffor::witness_store::{
	record::ReceiptAllocation, Namespace, WitnessSecretStore, WitnessStorageIdentity, MAX_EPOCHS,
	SECONDARY_NAMESPACE,
};

fn hex(value: &str) -> Vec<u8> {
	(0..value.len()).step_by(2).map(|i| u8::from_str_radix(&value[i..i + 2], 16).unwrap()).collect()
}

fn field(text: &str, name: &str) -> Vec<u8> {
	hex(text
		.lines()
		.find_map(|line| {
			let (key, value) = line.split_once('=')?;
			(key == name).then_some(value)
		})
		.unwrap())
}

// Exact public Beignet D.1/D.2 evidence from the existing key-use fixtures and Appendix D activation
// vectors at lightning-ffor/tests/data/appendix-d.json. Keys 42/44 are public fixture constants.
fn fixture(index: usize) -> (WitnessStorageBinding, StoredWitnessEpoch, EncryptedRecord) {
	let data =
		include_str!("../record/key_use/fixtures.txt").trim().split("\n\n").nth(index).unwrap();
	let activation = include_str!("activation-fixtures.txt")
		.trim()
		.split("\n\n")
		.nth(if index == 0 { 0 } else { 1 })
		.unwrap();
	let identity = WitnessStorageIdentity {
		chain_hash: [4; 32],
		actual_node: PublicKey::from_slice(&hex(
			"039fca7f8157aa768708894ffd92550fe970edd18526a5f936583ea3b54dab3228",
		))
		.unwrap(),
		settlement_node: PublicKey::from_slice(&hex(
			"02087b7d1b4789170f6e374f0a0e58a1b7a899e34929795314ab6964e69609e9c0",
		))
		.unwrap(),
		original_funding: OutPoint { txid: Txid::from_byte_array([8; 32]), vout: 0 },
	};
	let setup = AuthenticatedSetup::new(
		&Message::decode(&field(data, "init")).unwrap(),
		&Message::decode(&field(data, "accept")).unwrap(),
		identity.actual_node,
		identity.settlement_node,
	)
	.unwrap();
	let binding = WitnessStorageBinding::new(
		identity,
		setup,
		&Message::decode(&field(activation, "activate")).unwrap(),
		&Message::decode(&field(activation, "ack")).unwrap(),
	)
	.unwrap();
	let manifest = SignedManifest::decode(&field(data, "manifest"), binding.setup()).unwrap();
	let mut plaintext = Zeroizing::new(Vec::new());
	plaintext.extend_from_slice(&2u16.to_be_bytes());
	plaintext.extend_from_slice(&binding.digest());
	plaintext.extend_from_slice(&[44; 32]);
	plaintext.push(1);
	plaintext.extend_from_slice(&key(43).serialize());
	plaintext.extend_from_slice(&[42; 32]);
	plaintext.extend_from_slice(&(manifest.encode().len() as u32).to_be_bytes());
	plaintext.extend_from_slice(&manifest.encode());
	plaintext.extend_from_slice(&[0; 21]);
	let stored = StoredWitnessEpoch::decode(&binding, &plaintext).unwrap();
	(binding, stored, EncryptedRecord::decode(&field(data, "record")).unwrap())
}

fn seeded(
	index: usize,
) -> (Arc<TestStore>, WitnessSecretStore, WitnessStorageBinding, EncryptedRecord) {
	let (binding, mut record, encrypted) = fixture(index);
	let backend = TestStore::new();
	let store = WitnessSecretStore::open(&[5; 64], backend.clone()).unwrap();
	let receipt = store
		.receipt_key
		.seal_plaintext(&binding, &ReceiptBook::empty(&binding, &record).encode())
		.unwrap();
	record.receipt_allocation = ReceiptAllocation::Pending(receipt.len() as u32);
	{
		let mut state = store.state.lock().unwrap();
		state.receipt_reservations.insert(binding.storage_key(), receipt.len());
		store
			.confirm_write(
				&mut state,
				Namespace::Secrets,
				binding.storage_key(),
				store.wrapping_key.seal(&binding, &record).unwrap(),
			)
			.unwrap();
		store
			.confirm_write(&mut state, Namespace::Receipts, binding.storage_key(), receipt)
			.unwrap();
		store.finish_initialization(&mut state, &binding, &mut record).unwrap();
	}
	(backend, store, binding, encrypted)
}

fn receipt_bytes(backend: &TestStore, binding: &WitnessStorageBinding) -> Vec<u8> {
	KVStoreSync::read(backend, NAMESPACE, SECONDARY_NAMESPACE, &binding.storage_key()).unwrap()
}

#[test]
fn ffor_receipt_retains_exact_core_deduplicates_and_reopens_all_fixture_slots() {
	for index in 0..4 {
		let (backend, store, binding, record) = seeded(index);
		let length = receipt_bytes(&backend, &binding).len();
		assert!(store.load_receipt(&binding, key(43), record.header().slot).unwrap().is_none());
		assert_eq!(store.retain_receipt(&binding, key(43), &record), Ok(ReceiptRetention::Stored));
		let retained = receipt_bytes(&backend, &binding);
		assert_eq!(retained.len(), length);
		let writes = backend.writes.load(Ordering::SeqCst);
		assert_eq!(
			store.retain_receipt(&binding, key(43), &record),
			Ok(ReceiptRetention::AlreadyStored)
		);
		// Guardian attachments are deliberately outside retained, authoritative signed evidence.
		let stripped = decode_core(&canonical_core(&record).unwrap()).unwrap();
		assert_eq!(stripped.encode().len(), CORE_BYTES + 1);
		assert!(stripped.receipts().is_empty());
		assert_eq!(
			store.retain_receipt(&binding, key(43), &stripped),
			Ok(ReceiptRetention::AlreadyStored)
		);
		assert_eq!(backend.writes.load(Ordering::SeqCst), writes);
		let expected =
			store.load_receipt(&binding, key(43), record.header().slot).unwrap().unwrap();
		drop(store);
		let restored = WitnessSecretStore::open(&[5; 64], backend.clone()).unwrap();
		assert_eq!(
			restored.load_receipt(&binding, key(43), record.header().slot).unwrap(),
			Some(expected)
		);
		assert_eq!(receipt_bytes(&backend, &binding), retained);
		assert_eq!(backend.writes.load(Ordering::SeqCst), writes + 2);
	}
}

#[test]
fn ffor_receipt_first_valid_evidence_survives_equivocation_and_later_errors() {
	let (backend, store, binding, first) = seeded(0);
	store.retain_receipt(&binding, key(43), &first).unwrap();
	let retained = receipt_bytes(&backend, &binding);
	let writes = backend.writes.load(Ordering::SeqCst);
	let different = EncryptedRecord::decode(&hex(include_str!("equivocation.txt").trim())).unwrap();
	let secrets = store.load(&binding).unwrap();
	assert!(secrets.decrypt_record(key(43), different.clone()).is_ok());
	assert_eq!(
		store.retain_receipt(&binding, key(43), &different),
		Err(WitnessStoreError::ReceiptConflict)
	);
	assert_eq!(store.retain_receipt(&binding, key(40), &first), Err(WitnessStoreError::Binding));
	let (_, _, other_epoch) = fixture(1);
	assert_eq!(
		store.retain_receipt(&binding, key(43), &other_epoch),
		Err(WitnessStoreError::InvalidReceipt)
	);
	assert!(store.load_receipt(&binding, key(43), 0).is_err());
	assert!(store.load_receipt(&binding, key(43), 2).is_err());
	assert_eq!(receipt_bytes(&backend, &binding), retained);
	assert_eq!(backend.writes.load(Ordering::SeqCst), writes);
	assert_eq!(store.load_receipt(&binding, key(43), 1).unwrap().unwrap().header(), first.header());
}

#[test]
fn ffor_receipt_initialization_each_write_failure_recovers_without_new_material() {
	for failure in [1, 2] {
		for target in 1..=3 {
			let backend = TestStore::new();
			let store = WitnessSecretStore::open(&[5; 64], backend.clone()).unwrap();
			let binding = binding(1);
			backend.fail_at.store(target, Ordering::SeqCst);
			backend.write_failure.store(failure, Ordering::SeqCst);
			assert_eq!(
				store.create(&binding, &policies()).unwrap_err(),
				WitnessStoreError::Storage
			);
			assert_eq!(store.load(&binding).unwrap_err(), WitnessStoreError::Uncertain);
			let before = {
				let state = store.state.lock().unwrap();
				let pending = state.uncertain.as_ref().unwrap();
				if pending.namespace == Namespace::Secrets {
					pending.bytes.clone()
				} else {
					state.records.get(&binding.storage_key()).unwrap().clone()
				}
			};
			let expected = store.wrapping_key.open(&binding, &before).unwrap();
			backend.fail_at.store(0, Ordering::SeqCst);
			backend.write_failure.store(2, Ordering::SeqCst);
			assert_eq!(store.recover_write(), Err(WitnessStoreError::Storage));
			assert_eq!(store.load(&binding).unwrap_err(), WitnessStoreError::Uncertain);
			store.recover_write().unwrap();
			drop(store);
			let restored = WitnessSecretStore::open(&[5; 64], backend.clone()).unwrap();
			let actual = restored.load(&binding).unwrap();
			assert!(matches!(actual.receipt_allocation, ReceiptAllocation::Reserved(_)));
			for policy in policies() {
				assert_eq!(actual.manifest(policy.witness), expected.manifest(policy.witness));
			}
			assert!(restored.load_receipt(&binding, key(2), 1).unwrap().is_none());
		}
	}
}

#[test]
fn ffor_receipt_pending_restart_retains_full_quota_and_finishes_all_visible_phases() {
	for target in 1..=3 {
		let backend = TestStore::new();
		let binding = binding(1);
		let reservation;
		{
			let store = WitnessSecretStore::open(&[5; 64], backend.clone()).unwrap();
			backend.fail_at.store(target, Ordering::SeqCst);
			backend.write_failure.store(2, Ordering::SeqCst);
			assert!(store.create(&binding, &policies()).is_err());
			reservation = store.state.lock().unwrap().receipt_reservations[&binding.storage_key()];
		}
		let restored = WitnessSecretStore::open(&[5; 64], backend.clone()).unwrap();
		assert_eq!(
			restored.state.lock().unwrap().receipt_reservations[&binding.storage_key()],
			reservation
		);
		backend.fail_at.store(0, Ordering::SeqCst);
		backend.write_failure.store(2, Ordering::SeqCst);
		assert_eq!(restored.load(&binding).unwrap_err(), WitnessStoreError::Storage);
		assert_eq!(restored.load(&binding).unwrap_err(), WitnessStoreError::Uncertain);
		restored.recover_write().unwrap();
		assert!(matches!(
			restored.load(&binding).unwrap().receipt_allocation,
			ReceiptAllocation::Reserved(_)
		));
		assert_eq!(receipt_bytes(&backend, &binding).len(), reservation);
	}
}

#[test]
fn ffor_receipt_uncertain_update_requires_successful_exact_rewrite_even_after_restart() {
	for failure in [1, 2] {
		let (backend, store, binding, receipt) = seeded(0);
		let before = receipt_bytes(&backend, &binding);
		backend.write_failure.store(failure, Ordering::SeqCst);
		assert_eq!(
			store.retain_receipt(&binding, key(43), &receipt),
			Err(WitnessStoreError::Storage)
		);
		let candidate = store.state.lock().unwrap().uncertain.as_ref().unwrap().bytes.clone();
		assert_ne!(candidate, before);
		assert_eq!(store.load_receipt(&binding, key(43), 1), Err(WitnessStoreError::Uncertain));
		backend.write_failure.store(2, Ordering::SeqCst);
		assert_eq!(store.recover_write(), Err(WitnessStoreError::Storage));
		assert_eq!(receipt_bytes(&backend, &binding), candidate);
		drop(store);
		let restored = WitnessSecretStore::open(&[5; 64], backend.clone()).unwrap();
		// Confirm secret ciphertext first, then fail the receipt's durability confirmation.
		backend.fail_at.store(backend.writes.load(Ordering::SeqCst) + 2, Ordering::SeqCst);
		backend.write_failure.store(2, Ordering::SeqCst);
		assert_eq!(restored.load_receipt(&binding, key(43), 1), Err(WitnessStoreError::Storage));
		assert_eq!(restored.load_receipt(&binding, key(43), 1), Err(WitnessStoreError::Uncertain));
		restored.recover_write().unwrap();
		assert!(restored.load_receipt(&binding, key(43), 1).unwrap().is_some());
		assert_eq!(receipt_bytes(&backend, &binding), candidate);
	}
}

#[test]
fn ffor_receipt_missing_corrupt_or_other_epoch_reservation_is_never_replaced() {
	let (backend, store, binding, receipt) = seeded(0);
	let initial = receipt_bytes(&backend, &binding);
	let writes = backend.writes.load(Ordering::SeqCst);
	for bytes in [vec![0; initial.len()], {
		let mut bytes = initial.clone();
		bytes[50] ^= 1;
		bytes
	}] {
		KVStoreSync::write(&backend.inner, NAMESPACE, "", &binding.storage_key(), bytes.clone())
			.unwrap();
		assert_eq!(
			store.retain_receipt(&binding, key(43), &receipt),
			Err(WitnessStoreError::Conflict)
		);
		assert!(WitnessSecretStore::open(&[5; 64], backend.clone()).is_err());
		assert_eq!(receipt_bytes(&backend, &binding), bytes);
	}
	KVStoreSync::remove(&backend.inner, NAMESPACE, "", &binding.storage_key(), false).unwrap();
	assert_eq!(store.load(&binding).unwrap_err(), WitnessStoreError::Missing);
	assert_eq!(
		WitnessSecretStore::open(&[5; 64], backend.clone()).unwrap_err(),
		WitnessStoreError::Missing
	);
	assert_eq!(backend.writes.load(Ordering::SeqCst), writes);
}

#[test]
fn ffor_receipt_late_orphan_refuses_before_either_namespace_write() {
	let backend = TestStore::new();
	let store = WitnessSecretStore::open(&[5; 64], backend.clone()).unwrap();
	let binding = binding(1);
	KVStoreSync::write(&backend.inner, NAMESPACE, "", &binding.storage_key(), vec![91; 300])
		.unwrap();
	assert_eq!(store.create(&binding, &policies()).unwrap_err(), WitnessStoreError::Conflict);
	assert_eq!(receipt_bytes(&backend, &binding), vec![91; 300]);
	assert_eq!(backend.writes.load(Ordering::SeqCst), 0);
	assert!(store.state.lock().unwrap().records.is_empty());
	assert!(store.state.lock().unwrap().receipt_reservations.is_empty());
}

#[test]
fn ffor_receipt_quota_is_reserved_before_write_and_retained_for_pending_records() {
	let backend = TestStore::new();
	let store = WitnessSecretStore::open(&[5; 64], backend.clone()).unwrap();
	store
		.state
		.lock()
		.unwrap()
		.receipt_reservations
		.insert("unrelated".into(), MAX_RECEIPT_STORE_BYTES);
	assert_eq!(store.create(&binding(1), &policies()).unwrap_err(), WitnessStoreError::Capacity);
	assert_eq!(backend.writes.load(Ordering::SeqCst), 0);
	assert!(store.state.lock().unwrap().records.is_empty());
	// Every authenticated Pending allocation is charged even with no receipt blob at all.
	let backend = TestStore::new();
	let wrapping = super::super::WrappingKey::derive(&[5; 64]);
	for epoch in 1..=9 {
		let binding = binding(epoch);
		let mut record = StoredWitnessEpoch::generate(&binding, &policies()).unwrap();
		record.receipt_allocation = ReceiptAllocation::Pending(MAX_RECEIPT_RECORD_BYTES as u32);
		backend.replace(&binding.storage_key(), wrapping.seal(&binding, &record).unwrap());
		if epoch == 8 {
			let store = WitnessSecretStore::open(&[5; 64], backend.clone()).unwrap();
			assert_eq!(
				store.state.lock().unwrap().receipt_reservations.values().sum::<usize>(),
				MAX_RECEIPT_STORE_BYTES
			);
			// Full binding validation recomputes the actual shape, refusing forged allocation size.
			assert_eq!(store.load(&binding).unwrap_err(), WitnessStoreError::Corrupt);
		}
	}
	assert_eq!(
		WitnessSecretStore::open(&[5; 64], backend).unwrap_err(),
		WitnessStoreError::Capacity
	);
	assert_eq!(MAX_EPOCHS, 64);
}

#[test]
fn ffor_receipt_concurrent_retention_and_acknowledgement_preserve_both() {
	let (backend, store, binding, receipt) = seeded(0);
	let store = Arc::new(store);
	let original = store.load(&binding).unwrap();
	let checked = checked_acknowledgement(&original, key(43), 1);
	let tasks: Vec<_> = (0..8)
		.map(|index| {
			let store = store.clone();
			let binding = binding.clone();
			let receipt = receipt.clone();
			let checked = checked.clone();
			std::thread::spawn(move || {
				if index % 2 == 0 {
					store.retain_receipt(&binding, key(43), &receipt).unwrap();
				} else {
					store.acknowledge(&binding, &checked).unwrap();
				}
			})
		})
		.collect();
	for task in tasks {
		task.join().unwrap();
	}
	assert!(store.load(&binding).unwrap().all_witnesses_acknowledged());
	assert!(store.load_receipt(&binding, key(43), 1).unwrap().is_some());
	assert_eq!(backend.writes.load(Ordering::SeqCst), 5);
}

proptest! {
	#[test]
	fn ffor_receipt_bounded_parser_refuses_malformed_reservations(bytes in prop::collection::vec(any::<u8>(), 0..2048)) {
		let result = ReceiptBook::validate_inventory(&bytes, [9; 32]);
		prop_assert!(result.is_err());
	}
}

#[test]
fn ffor_receipt_recovery_refuses_deleted_or_valid_substituted_predecessor() {
	let (backend, store, binding, receipt) = seeded(0);
	let previous = receipt_bytes(&backend, &binding);
	let (other_backend, _, other_binding, _) = seeded(1);
	let unrelated = receipt_bytes(&other_backend, &other_binding);
	backend.write_failure.store(1, Ordering::SeqCst);
	assert_eq!(store.retain_receipt(&binding, key(43), &receipt), Err(WitnessStoreError::Storage));
	KVStoreSync::remove(&backend.inner, NAMESPACE, "", &binding.storage_key(), false).unwrap();
	assert_eq!(store.recover_write(), Err(WitnessStoreError::Missing));
	KVStoreSync::write(&backend.inner, NAMESPACE, "", &binding.storage_key(), unrelated.clone())
		.unwrap();
	assert_eq!(store.recover_write(), Err(WitnessStoreError::Conflict));
	assert!(WitnessSecretStore::open(&[5; 64], backend.clone()).is_err());
	assert_eq!(receipt_bytes(&backend, &binding), unrelated);
	KVStoreSync::write(&backend.inner, NAMESPACE, "", &binding.storage_key(), previous).unwrap();
	store.recover_write().unwrap();
	assert!(store.load_receipt(&binding, key(43), 1).unwrap().is_some());
}

#[test]
fn ffor_receipt_legacy_records_remain_historical_without_allocating_receipts() {
	let (binding, record, receipt) = fixture(0);
	let backend = TestStore::new();
	let wrapping = super::super::WrappingKey::derive(&[5; 64]);
	backend.replace(&binding.storage_key(), wrapping.seal(&binding, &record).unwrap());
	let store = WitnessSecretStore::open(&[5; 64], backend.clone()).unwrap();
	let loaded = store.load(&binding).unwrap();
	assert_eq!(loaded.receipt_allocation, ReceiptAllocation::Legacy);
	assert_eq!(
		store.retain_receipt(&binding, key(43), &receipt),
		Err(WitnessStoreError::Unreserved)
	);
	assert_eq!(store.load_receipt(&binding, key(43), 1), Err(WitnessStoreError::Unreserved));
	assert_eq!(
		store.create(&binding, &record.policies()).unwrap().receipt_allocation,
		ReceiptAllocation::Legacy
	);
	assert!(KVStoreSync::list(&*backend, NAMESPACE, "").unwrap().is_empty());
	store.acknowledge(&binding, &checked_acknowledgement(&loaded, key(43), 1)).unwrap();
	assert!(store.load(&binding).unwrap().all_witnesses_acknowledged());
	assert!(store.state.lock().unwrap().receipt_reservations.is_empty());
}

#[test]
fn ffor_receipt_inventory_authenticates_and_bounds_allocation_metadata() {
	let binding = binding(1);
	let mut record = StoredWitnessEpoch::generate(&binding, &policies()).unwrap();
	record.receipt_allocation = ReceiptAllocation::Pending(5000);
	let original = record.encode();
	let wrapping = super::super::WrappingKey::derive(&[5; 64]);
	let mut cases = Vec::new();
	for (offset, value) in [(0, 9), (66, 0), (66, 5), (67, 0), (67, 3)] {
		let mut bytes = original.clone();
		bytes[offset] = value;
		cases.push(bytes);
	}
	for size in [0, MAX_RECEIPT_RECORD_BYTES as u32 + 1, u32::MAX] {
		let mut bytes = original.clone();
		bytes[68..72].copy_from_slice(&size.to_be_bytes());
		cases.push(bytes);
	}
	for length in 0..72 {
		cases.push(Zeroizing::new(original[..length].to_vec()));
	}
	for plaintext in cases {
		let backend = TestStore::new();
		let sealed = wrapping.seal_plaintext(&binding, &plaintext).unwrap();
		backend.replace(&binding.storage_key(), sealed.clone());
		assert!(WitnessSecretStore::open(&[5; 64], backend.clone()).is_err());
		assert_eq!(backend.bytes(&binding.storage_key()), sealed);
		assert_eq!(backend.writes.load(Ordering::SeqCst), 0);
	}
}

#[test]
fn ffor_receipt_parser_rejects_truncation_trailing_slots_and_manifest_substitution() {
	let (binding, record, encrypted) = fixture(0);
	let mut book = ReceiptBook::empty(&binding, &record);
	book.insert(key(43), &encrypted).unwrap();
	let encoded = book.encode();
	for length in 0..encoded.len() {
		assert!(ReceiptBook::decode(&binding, &record, &encoded[..length]).is_err());
	}
	let mut trailing = encoded.clone();
	trailing.push(0);
	assert!(ReceiptBook::decode(&binding, &record, &trailing).is_err());
	for offset in [0, 2, 34, 35, 36, 37, 70, 102, 103, encoded.len() - 1] {
		let mut changed = encoded.clone();
		changed[offset] ^= 1;
		assert!(ReceiptBook::decode(&binding, &record, &changed).is_err(), "offset {offset}");
	}
	// A fixed empty slot cannot smuggle nonzero bytes without its presence flag.
	let mut empty = ReceiptBook::empty(&binding, &record).encode();
	*empty.last_mut().unwrap() = 1;
	assert!(ReceiptBook::decode(&binding, &record, &empty).is_err());
	assert_eq!(CORE_BYTES, 492);
}

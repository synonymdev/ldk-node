use std::sync::atomic::Ordering;
use std::sync::Arc;

use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
use lightning::events::Event;
use lightning::ln::ffor::FFORWitnessReceiptProgress;

use super::*;
use crate::ffor::witness_owner::fetch::tests::record;
use crate::ffor::witness_owner::tests::Harness;
use crate::ffor::witness_store::WitnessStoreError;

fn monitor_counter(h: &Harness) -> u64 {
	h.node.chain_monitor.get_monitor(h.context.channel_id()).unwrap().get_latest_update_id()
}

fn process_without_credit(h: &Harness) {
	let events = h.node.channel_manager.get_and_clear_pending_events();
	assert!(!events.iter().any(|event| matches!(
		event,
		Event::PaymentClaimed { .. } | Event::PaymentClaimable { .. }
	)));
	assert!(h.node.list_payments().is_empty());
}

fn retain(h: &Harness, slot: u16) {
	let binding = WitnessStorageBinding::from_native_context(&h.context).unwrap();
	h.owner
		.store
		.retain_receipt(&binding, h.policies[0].witness, &record(h, slot, false, slot as u8))
		.unwrap();
}

#[test]
fn ffor_witness_recovery_missing_evidence_and_wrong_witness_do_not_touch_monitor() {
	let mut h = Harness::new();
	h.register_and_persist();
	let before = monitor_counter(&h);
	let writes = h.storage.writes.load(Ordering::SeqCst);
	assert_eq!(h.owner.recover_receipt(&h.context, h.policies[0].witness, 1), Ok(None));
	let other =
		PublicKey::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[81; 32]).unwrap());
	assert_eq!(
		h.owner.recover_receipt(&h.context, other, 1),
		Err(WitnessOwnerError::UnknownWitness)
	);
	assert_eq!(monitor_counter(&h), before);
	assert_eq!(h.storage.writes.load(Ordering::SeqCst), writes);
	process_without_credit(&h);
}

#[test]
fn ffor_witness_recovery_real_monitor_persists_once_and_survives_node_restore() {
	let mut h = Harness::new();
	h.register_and_persist();
	retain(&h, 1);
	let before = monitor_counter(&h);
	let pending = h.owner.recover_receipt(&h.context, h.policies[0].witness, 1).unwrap();
	let update_id = match pending {
		Some(FFORWitnessReceiptProgress::PendingMonitor { monitor_update_id }) => monitor_update_id,
		other => panic!("expected monitor submission, got {other:?}"),
	};
	assert!(update_id > before);
	assert_eq!(monitor_counter(&h), update_id);
	// Node's actual MonitorUpdatingPersister completes only after the KVStore write succeeds.
	// Normal native event processing observes that completion; the test never fabricates an ACK.
	process_without_credit(&h);
	let persisted =
		Some(FFORWitnessReceiptProgress::MonitorPersisted { monitor_update_id: update_id });
	let writes = h.storage.writes.load(Ordering::SeqCst);
	assert_eq!(h.owner.recover_receipt(&h.context, h.policies[0].witness, 1), Ok(persisted));
	assert_eq!(h.storage.writes.load(Ordering::SeqCst), writes);
	h.persist_manager();
	let storage = Arc::clone(&h.storage);
	drop(h);
	let mut restored = Harness::from_store(storage);
	assert_eq!(
		restored.owner.recover_receipt(&restored.context, restored.policies[0].witness, 1),
		Ok(persisted)
	);
	assert_eq!(monitor_counter(&restored), update_id);
	process_without_credit(&restored);
}

#[test]
fn ffor_witness_recovery_uncertain_receipt_write_cannot_reach_native_monitor() {
	for failure in [1, 2] {
		let mut h = Harness::new();
		h.register_and_persist();
		let binding = WitnessStorageBinding::from_native_context(&h.context).unwrap();
		let receipt = record(&h, 1, false, 1);
		let before = monitor_counter(&h);
		h.storage.write_failure.store(failure, Ordering::SeqCst);
		assert_eq!(
			h.owner.store.retain_receipt(&binding, h.policies[0].witness, &receipt),
			Err(WitnessStoreError::Storage)
		);
		assert_eq!(
			h.owner.recover_receipt(&h.context, h.policies[0].witness, 1),
			Err(WitnessOwnerError::Storage(WitnessStoreError::Uncertain))
		);
		assert_eq!(monitor_counter(&h), before);
		h.owner.recover_storage().unwrap();
		assert!(matches!(
			h.owner.recover_receipt(&h.context, h.policies[0].witness, 1),
			Ok(Some(FFORWitnessReceiptProgress::PendingMonitor { .. }))
		));
		process_without_credit(&h);
	}
}

#[test]
fn ffor_witness_recovery_restored_visible_receipt_waits_for_successful_confirmation() {
	for confirmation_failure in [1, 2] {
		let mut h = Harness::new();
		h.register_and_persist();
		let binding = WitnessStorageBinding::from_native_context(&h.context).unwrap();
		let witness = h.policies[0].witness;
		let receipt = record(&h, 1, false, 1);
		let before = monitor_counter(&h);
		// The ciphertext becomes readable, but the original receipt write was not confirmed.
		h.storage.write_failure.store(2, Ordering::SeqCst);
		assert_eq!(
			h.owner.store.retain_receipt(&binding, witness, &receipt),
			Err(WitnessStoreError::Storage)
		);
		let storage = Arc::clone(&h.storage);
		drop(h);
		let mut restored = Harness::from_store(storage);
		assert_eq!(monitor_counter(&restored), before);
		// Confirming restored secrets is the first write; confirming the receipt envelope is
		// the second. Only the latter fails, preserving the exact readable encrypted candidate.
		let writes = restored.storage.writes.load(Ordering::SeqCst);
		restored.storage.fail_at.store(writes + 2, Ordering::SeqCst);
		restored.storage.write_failure.store(confirmation_failure, Ordering::SeqCst);
		assert_eq!(
			restored.owner.recover_receipt(&restored.context, witness, 1),
			Err(WitnessOwnerError::Storage(WitnessStoreError::Storage))
		);
		assert_eq!(restored.storage.writes.load(Ordering::SeqCst), writes + 2);
		assert_eq!(monitor_counter(&restored), before);
		assert_eq!(
			restored.owner.recover_receipt(&restored.context, witness, 1),
			Err(WitnessOwnerError::Storage(WitnessStoreError::Uncertain))
		);
		restored.owner.recover_storage().unwrap();
		let pending = restored.owner.recover_receipt(&restored.context, witness, 1).unwrap();
		let update_id = match pending {
			Some(FFORWitnessReceiptProgress::PendingMonitor { monitor_update_id }) => {
				monitor_update_id
			},
			other => panic!("expected native monitor submission, got {other:?}"),
		};
		assert!(update_id > before);
		process_without_credit(&restored);
		assert_eq!(
			restored.owner.recover_receipt(&restored.context, witness, 1),
			Ok(Some(FFORWitnessReceiptProgress::MonitorPersisted { monitor_update_id: update_id }))
		);
		process_without_credit(&restored);
	}
}

#[test]
fn ffor_witness_recovery_archive_only_receipt_uses_durable_original_monitor_after_restart() {
	let mut h = Harness::new();
	h.register_and_persist();
	retain(&h, 2);
	h.node
		.channel_manager
		.force_close_broadcasting_latest_txn(
			&h.context.channel_id(),
			&h.context.settlement_node_id(),
			"historical witness recovery fixture".to_owned(),
		)
		.unwrap();
	process_without_credit(&h);
	assert!(h.node.list_channels().is_empty());
	h.persist_manager();
	let storage = Arc::clone(&h.storage);
	drop(h);
	let mut restored = Harness::from_store(storage);
	let before = monitor_counter(&restored);
	let witness = restored.policies[0].witness;
	let pending = restored.owner.recover_receipt(&restored.context, witness, 2).unwrap();
	let update_id = match pending {
		Some(FFORWitnessReceiptProgress::PendingMonitor { monitor_update_id }) => monitor_update_id,
		other => panic!("expected historical monitor submission, got {other:?}"),
	};
	assert!(update_id > before);
	process_without_credit(&restored);
	let persisted =
		Some(FFORWitnessReceiptProgress::MonitorPersisted { monitor_update_id: update_id });
	assert_eq!(restored.owner.recover_receipt(&restored.context, witness, 2), Ok(persisted));
	restored.persist_manager();
	let storage = Arc::clone(&restored.storage);
	drop(restored);
	let mut second_restore = Harness::from_store(storage);
	assert_eq!(
		second_restore.owner.recover_receipt(&second_restore.context, witness, 2),
		Ok(persisted)
	);
	process_without_credit(&second_restore);
}

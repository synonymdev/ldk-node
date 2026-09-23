//! Issuer adapter tests against the genuine exported native invoice fixture.

use bitcoin::block::{Header, Version};
use bitcoin::hash_types::TxMerkleNode;
use bitcoin::pow::CompactTarget;
use bitcoin::BlockHash;
use lightning::chain::Confirm;
use lightning::ln::ffor::FFORWitnessRouteEvidence;
use lightning::ln::msgs::{ChannelAnnouncement, ChannelUpdate, UnsignedGossipMessage};
use lightning::sign::{KeysManager, NodeSigner, Recipient};
use lightning::util::ser::LengthReadable;

use super::*;
use crate::data_store::ffor::FFORPaymentError;
use crate::data_store::StorableObjectId;
use crate::ffor::request_store::invoice::{ConfirmedInvoice, InvoiceProgress};
use crate::ffor::request_store::record::invoice::InvoicePolicy;
use crate::io::{
	PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE, PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
};
use crate::payment::store::{PaymentDetails, PaymentDetailsUpdate, PaymentStatus};

const ACTIVE: &[u8] = include_bytes!("../fixtures/invoice/active-manager.bin");
const ISSUED: &[u8] = include_bytes!("../fixtures/invoice/issued-manager.bin");
const MONITOR: &[u8] = include_bytes!("../fixtures/invoice/monitor.bin");

fn ifield(name: &str) -> &'static str {
	include_str!("../fixtures/invoice/fixture.txt")
		.lines()
		.find_map(|line| line.strip_prefix(&format!("{name}=")))
		.unwrap()
}

fn policy() -> InvoicePolicy {
	InvoicePolicy {
		expiry_seconds: ifield("expiry_seconds").parse().unwrap(),
		safety_margin_seconds: ifield("safety_margin_seconds").parse().unwrap(),
	}
}

fn issuer_intent() -> RequestIntent {
	RequestIntent::new(
		ifield("client_id").to_owned(),
		ifield("amount_msat").parse().unwrap(),
		ifield("description").to_owned(),
	)
	.unwrap()
}

fn issuer_plan(store: &RequestStore) -> RequestPlan {
	let mut plan = plan(store, ifield("client_id"));
	plan.parameters.settlement_deadline = ifield("settlement_deadline").parse().unwrap();
	plan.parameters.voucher_expiry = ifield("voucher_expiry").parse().unwrap();
	plan.parameters.witness_peers = Some(vec![PublicKey::from_str(ifield("witness")).unwrap()]);
	plan
}

fn install_native(storage: &TestStore, manager: &[u8]) {
	KVStoreSync::write(
		storage,
		CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
		CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
		CHANNEL_MANAGER_PERSISTENCE_KEY,
		manager.to_vec(),
	)
	.unwrap();
	KVStoreSync::write(
		storage,
		CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
		CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
		&format!("{}_{}", ifield("funding_txid"), ifield("funding_vout")),
		MONITOR.to_vec(),
	)
	.unwrap();
}

fn persist_manager(node: &Node, storage: &TestStore) {
	let token = node.channel_manager.capture_ffor_persistence();
	KVStoreSync::write(
		storage,
		CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
		CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
		CHANNEL_MANAGER_PERSISTENCE_KEY,
		node.channel_manager.encode(),
	)
	.unwrap();
	node.channel_manager.ffor_persistence_completed(token).unwrap();
}

fn now() -> u64 {
	std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

fn witness_keys() -> KeysManager {
	let seed = <[u8; 32]>::from_hex(ifield("witness_node_seed")).unwrap();
	let keys = KeysManager::new(&seed, 0, 0, true);
	assert_eq!(
		keys.get_node_id(Recipient::Node).unwrap(),
		PublicKey::from_str(ifield("witness")).unwrap()
	);
	keys
}

fn route_at(timestamp: Option<u64>) -> FFORWitnessRouteEvidence {
	let announcement = ChannelAnnouncement::read_from_fixed_length_buffer(
		&mut include_bytes!("../fixtures/invoice/route-announcement.bin").as_slice(),
	)
	.unwrap();
	let mut update = ChannelUpdate::read_from_fixed_length_buffer(
		&mut include_bytes!("../fixtures/invoice/route-update.bin").as_slice(),
	)
	.unwrap();
	if let Some(timestamp) = timestamp {
		update.contents.timestamp = timestamp as u32;
		update.signature = witness_keys()
			.sign_gossip_message(UnsignedGossipMessage::ChannelUpdate(&update.contents))
			.unwrap();
	}
	FFORWitnessRouteEvidence { announcement, update }
}

fn route() -> FFORWitnessRouteEvidence {
	route_at(Some(now()))
}

/// Begin the exact application record on the empty pair, then restore the native fixture.
fn bound(manager: &[u8]) -> (Arc<TestStore>, Node, RequestStore) {
	let storage = TestStore::new();
	install_fixture(&storage, false);
	let original = restore_node(Arc::clone(&storage));
	let mut store = open(Arc::clone(&storage), &original);
	store.begin(issuer_intent(), issuer_plan(&store)).unwrap();
	drop(store);
	drop(original);
	install_native(&storage, manager);
	let node = restore_node(Arc::clone(&storage));
	let mut store = open(Arc::clone(&storage), &node);
	let id = store.recover_native(CLIENT).unwrap().unwrap();
	assert_eq!(hex(&id.epoch_id()), ifield("epoch_id"));
	(storage, node, store)
}

fn drive(
	storage: &TestStore, node: &Node, store: &mut RequestStore,
	mut step: impl FnMut(&mut RequestStore) -> Result<InvoiceProgress, RequestStoreError>,
) -> Result<InvoiceProgress, RequestStoreError> {
	for _ in 0..4 {
		match step(store)? {
			InvoiceProgress::AwaitingPersistence => persist_manager(node, storage),
			InvoiceProgress::Retained => return Ok(InvoiceProgress::Retained),
		}
	}
	panic!("native persistence never completed")
}

fn issue_live(storage: &TestStore, node: &Node, store: &mut RequestStore) -> StoredRequest {
	assert_eq!(
		drive(storage, node, store, |store| store.issue_invoice(CLIENT, policy(), &route())),
		Ok(InvoiceProgress::Retained)
	);
	let record = store.lookup(CLIENT).unwrap().unwrap();
	assert!(record.invoice().unwrap().payment_confirmed());
	record
}

fn stored_payment(storage: &TestStore, payment: &PaymentDetails) -> Option<Vec<u8>> {
	KVStoreSync::read(
		storage,
		PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
		PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
		&payment.id.encode_to_hex_str(),
	)
	.ok()
}

fn native_digest(node: &Node, store: &RequestStore) -> Option<[u8; 32]> {
	let record = store.records.keys().next().cloned().unwrap();
	let record = store.envelope.open(&record, store.records.get(&record).unwrap()).unwrap();
	let selector = record.selector().unwrap();
	let context = node
		.channel_manager
		.ffor_receiver_recovery_context(&selector.channel, selector.epoch)
		.unwrap();
	node.channel_manager
		.ffor_receiver_invoice_for_storage(&context)
		.unwrap()
		.map(|stored| stored.invoice_digest())
}

fn release(
	store: &mut RequestStore, handle: &ConfirmedInvoice,
) -> Result<Option<String>, RequestStoreError> {
	let mut slot = None;
	let released = store.release_invoice(handle, &mut slot)?;
	assert_eq!(released, slot.is_some());
	Ok(slot)
}

#[test]
fn ffor_request_issuer_live_issue_retains_exact_bytes_and_payment_then_releases_repeatably() {
	let (storage, node, mut store) = bound(ACTIVE);
	assert_eq!(store.recover_invoice(CLIENT), Ok(None));
	assert!(store.confirmed_invoice(CLIENT).unwrap().is_none());
	assert_eq!(native_digest(&node, &store), None);
	assert_eq!(
		store.issue_invoice(CLIENT, policy(), &route()),
		Ok(InvoiceProgress::AwaitingPersistence)
	);
	assert!(store.lookup(CLIENT).unwrap().unwrap().invoice().is_none());
	let record = issue_live(&storage, &node, &mut store);
	let retained = record.invoice().unwrap();
	let invoice = retained.parsed().unwrap();
	assert_eq!(invoice.amount_milli_satoshis(), Some(2_000_000));
	assert_eq!(invoice.recover_payee_pub_key(), node.node_id());
	assert_eq!(invoice.route_hints().len(), 1);
	assert_eq!(invoice.route_hints()[0].0.len(), 2);
	assert_eq!(native_digest(&node, &store), Some(retained.digest()));
	let payment = retained.payment().unwrap();
	assert_eq!(node.payment_store.get(&payment.id), Some(payment.clone()));
	assert_eq!(stored_payment(&storage, &payment), Some(payment.encode()));

	let writes = storage.writes.load(Ordering::SeqCst);
	assert_eq!(store.issue_invoice(CLIENT, policy(), &route()), Ok(InvoiceProgress::Retained));
	assert_eq!(store.recover_invoice(CLIENT), Ok(Some(InvoiceProgress::Retained)));
	assert_eq!(storage.writes.load(Ordering::SeqCst), writes);
	assert_eq!(store.lookup(CLIENT).unwrap().unwrap().encode(), record.encode());

	let handle = store.confirmed_invoice(CLIENT).unwrap().unwrap();
	assert_eq!(storage.writes.load(Ordering::SeqCst), writes + 1);
	assert!(!format!("{handle:?}").contains("lntb"));
	assert_eq!(release(&mut store, &handle), Ok(Some(retained.invoice().to_owned())));
	assert_eq!(release(&mut store, &handle), Ok(Some(retained.invoice().to_owned())));
	assert_eq!(node.payment_store.get(&payment.id).unwrap().status, PaymentStatus::Pending);
	assert_eq!(storage.writes.load(Ordering::SeqCst), writes + 1);
}

#[test]
fn ffor_request_issuer_every_write_failure_keeps_exact_candidate_and_blocks_all_handles() {
	// Application writes after native persistence: retain, payment, marker, then the fresh
	// handle write. Failure 1 lands the bytes but reports failure; failure 2 lands nothing.
	for stage in 1..=4 {
		for failure in [1, 2] {
			let (storage, node, mut store) = bound(ACTIVE);
			persist_manager(&node, &storage);
			assert_eq!(
				store.issue_invoice(CLIENT, policy(), &route()),
				Ok(InvoiceProgress::AwaitingPersistence)
			);
			persist_manager(&node, &storage);
			let digest = native_digest(&node, &store).unwrap();
			let mut earlier = None;
			if stage == 4 {
				assert_eq!(
					store.issue_invoice(CLIENT, policy(), &route()),
					Ok(InvoiceProgress::Retained)
				);
				earlier = Some(store.confirmed_invoice(CLIENT).unwrap().unwrap());
			}
			let base = storage.writes.load(Ordering::SeqCst);
			storage.fail_at.store(base + if stage == 4 { 1 } else { stage }, Ordering::SeqCst);
			storage.write_failure.store(failure, Ordering::SeqCst);
			let outcome = if stage == 4 {
				store.confirmed_invoice(CLIENT).map(|_| ()).unwrap_err()
			} else {
				store.issue_invoice(CLIENT, policy(), &route()).unwrap_err()
			};
			assert!(
				matches!(
					outcome,
					RequestStoreError::Storage
						| RequestStoreError::Payment(FFORPaymentError::Storage)
				),
				"stage {stage} failure {failure}: {outcome:?}"
			);
			assert_eq!(
				stage == 2 || stage == 4,
				matches!(outcome, RequestStoreError::Payment(FFORPaymentError::Storage))
			);
			storage.fail_at.store(0, Ordering::SeqCst);
			assert_eq!(
				store.issue_invoice(CLIENT, policy(), &route()),
				Err(RequestStoreError::Uncertain)
			);
			assert_eq!(store.recover_invoice(CLIENT), Err(RequestStoreError::Uncertain));
			assert!(matches!(store.confirmed_invoice(CLIENT), Err(RequestStoreError::Uncertain)));
			if let Some(earlier) = &earlier {
				assert_eq!(release(&mut store, earlier), Err(RequestStoreError::Uncertain));
			}
			storage.write_failure.store(2, Ordering::SeqCst);
			assert!(matches!(
				store.recover_write(),
				Err(RequestStoreError::Storage
					| RequestStoreError::Payment(FFORPaymentError::Storage))
			));
			assert_eq!(store.recover_invoice(CLIENT), Err(RequestStoreError::Uncertain));
			store.recover_write().unwrap();
			assert_eq!(
				store.issue_invoice(CLIENT, policy(), &route()),
				Ok(InvoiceProgress::Retained)
			);
			let record = store.lookup(CLIENT).unwrap().unwrap();
			let retained = record.invoice().unwrap();
			assert!(retained.payment_confirmed());
			assert_eq!(retained.digest(), digest);
			assert_eq!(native_digest(&node, &store), Some(digest));
			let payment = retained.payment().unwrap();
			assert_eq!(node.payment_store.get(&payment.id), Some(payment.clone()));
			assert_eq!(stored_payment(&storage, &payment), Some(payment.encode()));
			if let Some(earlier) = &earlier {
				assert_eq!(release(&mut store, earlier), Ok(Some(retained.invoice().to_owned())));
			}
			let handle = store.confirmed_invoice(CLIENT).unwrap().unwrap();
			assert_eq!(release(&mut store, &handle), Ok(Some(retained.invoice().to_owned())));
		}
	}
}

#[test]
fn ffor_request_issuer_restart_requires_fresh_barrier_and_refuses_obsolete_handles() {
	let (storage, node, mut store) = bound(ACTIVE);
	let record = issue_live(&storage, &node, &mut store);
	let wire = record.invoice().unwrap().invoice().to_owned();
	let old = store.confirmed_invoice(CLIENT).unwrap().unwrap();
	persist_manager(&node, &storage);
	drop(store);
	drop(node);

	let node = restore_node(Arc::clone(&storage));
	let mut store = open(Arc::clone(&storage), &node);
	assert_eq!(store.recover_invoice(CLIENT), Ok(Some(InvoiceProgress::AwaitingPersistence)));
	assert!(store.confirmed_invoice(CLIENT).unwrap().is_none());
	persist_manager(&node, &storage);
	assert_eq!(store.recover_invoice(CLIENT), Ok(Some(InvoiceProgress::Retained)));
	assert_eq!(store.lookup(CLIENT).unwrap().unwrap().encode(), record.encode());
	assert_eq!(release(&mut store, &old), Err(RequestStoreError::Conflict));
	let fresh = store.confirmed_invoice(CLIENT).unwrap().unwrap();
	assert_eq!(release(&mut store, &fresh), Ok(Some(wire.clone())));

	drop(store);
	let mut reopened = open(Arc::clone(&storage), &node);
	assert_eq!(release(&mut reopened, &fresh), Err(RequestStoreError::Conflict));
	assert_eq!(reopened.recover_invoice(CLIENT), Ok(Some(InvoiceProgress::Retained)));
	let again = reopened.confirmed_invoice(CLIENT).unwrap().unwrap();
	assert_eq!(release(&mut reopened, &again), Ok(Some(wire)));
}

#[test]
fn ffor_request_issuer_historical_fixture_recovers_exact_bytes_but_never_publishes_expired() {
	let (storage, node, mut store) = bound(ISSUED);
	assert_eq!(store.recover_invoice(CLIENT), Ok(None));
	assert_eq!(
		drive(&storage, &node, &mut store, |store| store.issue_invoice(CLIENT, policy(), &route())),
		Ok(InvoiceProgress::Retained)
	);
	let record = store.lookup(CLIENT).unwrap().unwrap();
	let retained = record.invoice().unwrap();
	assert_eq!(native_digest(&node, &store), Some(retained.digest()));
	let invoice = retained.parsed().unwrap();
	assert_eq!(invoice.amount_milli_satoshis(), Some(2_000_000));
	assert!(invoice.is_expired());
	let payment = retained.payment().unwrap();
	assert_eq!(node.payment_store.get(&payment.id), Some(payment.clone()));
	let handle = store.confirmed_invoice(CLIENT).unwrap().unwrap();
	let writes = storage.writes.load(Ordering::SeqCst);
	assert!(matches!(release(&mut store, &handle), Err(RequestStoreError::Native(_))));
	assert_eq!(storage.writes.load(Ordering::SeqCst), writes);
	assert_eq!(store.recover_invoice(CLIENT), Ok(Some(InvoiceProgress::Retained)));
	assert_eq!(store.lookup(CLIENT).unwrap().unwrap().encode(), record.encode());
	assert_eq!(node.payment_store.get(&payment.id).unwrap().status, PaymentStatus::Pending);
}

#[test]
fn ffor_request_issuer_conflicting_policy_and_stale_route_refuse_without_native_assignment() {
	let (storage, node, mut store) = bound(ACTIVE);
	persist_manager(&node, &storage);
	let stale = route_at(Some(now() - 15 * 24 * 60 * 60));
	assert!(matches!(
		store.issue_invoice(CLIENT, policy(), &stale),
		Err(RequestStoreError::Native(_))
	));
	assert_eq!(native_digest(&node, &store), None);
	let record = store.lookup(CLIENT).unwrap().unwrap();
	assert_eq!(record.invoice_policy(), Some(policy()));
	assert_eq!(
		store.issue_invoice(CLIENT, InvoicePolicy { expiry_seconds: 1, ..policy() }, &route()),
		Err(RequestStoreError::Conflict)
	);
	assert_eq!(native_digest(&node, &store), None);
	assert_eq!(store.lookup(CLIENT).unwrap().unwrap().encode(), record.encode());
	assert!(store.confirmed_invoice(CLIENT).unwrap().is_none());
	issue_live(&storage, &node, &mut store);
	assert_eq!(
		store.issue_invoice(CLIENT, InvoicePolicy { expiry_seconds: 1, ..policy() }, &route()),
		Err(RequestStoreError::Conflict)
	);
	assert!(store.confirmed_invoice(CLIENT).unwrap().is_some());
}

#[test]
fn ffor_request_issuer_missing_corrupt_or_terminal_payment_blocks_handles_and_release() {
	let (storage, node, mut store) = bound(ACTIVE);
	let record = issue_live(&storage, &node, &mut store);
	let payment = record.invoice().unwrap().payment().unwrap();
	let handle = store.confirmed_invoice(CLIENT).unwrap().unwrap();
	let key = payment.id.encode_to_hex_str();

	let good = stored_payment(&storage, &payment).unwrap();
	KVStoreSync::write(
		&*storage,
		PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
		PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
		&key,
		vec![0, 1, 2],
	)
	.unwrap();
	assert_eq!(
		store.confirmed_invoice(CLIENT).map(|_| ()),
		Err(RequestStoreError::Payment(FFORPaymentError::Conflict))
	);
	assert_eq!(
		store.issue_invoice(CLIENT, policy(), &route()),
		Ok(InvoiceProgress::Retained),
		"a durable marker with an unchanged live row does not re-read disk"
	);
	KVStoreSync::write(
		&*storage,
		PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
		PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
		&key,
		good,
	)
	.unwrap();
	assert!(store.confirmed_invoice(CLIENT).unwrap().is_some());

	node.payment_store.remove(&payment.id).unwrap();
	assert_eq!(
		store.confirmed_invoice(CLIENT).map(|_| ()),
		Err(RequestStoreError::Payment(FFORPaymentError::Missing))
	);
	assert_eq!(
		store.issue_invoice(CLIENT, policy(), &route()),
		Err(RequestStoreError::Payment(FFORPaymentError::Missing))
	);
	assert_eq!(
		release(&mut store, &handle),
		Err(RequestStoreError::Payment(FFORPaymentError::Missing))
	);
	node.payment_store.insert(payment.clone()).unwrap();
	assert_eq!(
		release(&mut store, &handle),
		Ok(Some(record.invoice().unwrap().invoice().to_owned()))
	);

	let mut update = PaymentDetailsUpdate::new(payment.id);
	update.status = Some(PaymentStatus::Failed);
	node.payment_store.update(&update).unwrap();
	assert_eq!(
		store.confirmed_invoice(CLIENT).map(|_| ()),
		Err(RequestStoreError::Payment(FFORPaymentError::Terminal))
	);
	assert_eq!(
		store.issue_invoice(CLIENT, policy(), &route()),
		Err(RequestStoreError::Payment(FFORPaymentError::Terminal))
	);
	assert_eq!(
		release(&mut store, &handle),
		Err(RequestStoreError::Payment(FFORPaymentError::Terminal))
	);
	assert_eq!(store.lookup(CLIENT).unwrap().unwrap().encode(), record.encode());
	assert_eq!(native_digest(&node, &store), Some(record.invoice().unwrap().digest()));
}

#[test]
fn ffor_request_issuer_unbound_record_or_missing_native_history_refuses() {
	let (_, _, mut store) = fixture();
	store.begin(issuer_intent(), issuer_plan(&store)).unwrap();
	assert_eq!(
		store.issue_invoice(CLIENT, policy(), &route()),
		Err(RequestStoreError::MissingNative)
	);
	assert_eq!(store.recover_invoice(CLIENT), Ok(None));
	assert!(store.confirmed_invoice(CLIENT).unwrap().is_none());
	assert_eq!(store.lookup(CLIENT).unwrap().unwrap().invoice_policy(), Some(policy()));

	let (storage, node, mut store) = bound(ACTIVE);
	issue_live(&storage, &node, &mut store);
	drop(store);
	drop(node);
	install_fixture(&storage, false);
	let node = restore_node(Arc::clone(&storage));
	let mut store = open(Arc::clone(&storage), &node);
	assert_eq!(store.recover_invoice(CLIENT), Err(RequestStoreError::MissingNative));
	assert!(matches!(store.confirmed_invoice(CLIENT), Err(RequestStoreError::MissingNative)));
	assert_eq!(
		store.issue_invoice(CLIENT, policy(), &route()),
		Err(RequestStoreError::MissingNative)
	);
}

#[test]
fn ffor_request_issuer_monitor_tip_ahead_refuses_release_but_keeps_assignment() {
	let (storage, node, mut store) = bound(ACTIVE);
	let record = issue_live(&storage, &node, &mut store);
	let handle = store.confirmed_invoice(CLIENT).unwrap().unwrap();
	let deadline: u32 = ifield("settlement_deadline").parse().unwrap();
	let header = Header {
		version: Version::NO_SOFT_FORK_SIGNALLING,
		prev_blockhash: BlockHash::all_zeros(),
		merkle_root: TxMerkleNode::all_zeros(),
		time: now() as u32,
		bits: CompactTarget::from_consensus(0),
		nonce: 0,
	};
	let manager_height = node.channel_manager.current_best_block().height;
	node.chain_monitor.best_block_updated(&header, deadline - 1);
	assert_eq!(node.channel_manager.current_best_block().height, manager_height);
	assert!(matches!(release(&mut store, &handle), Err(RequestStoreError::Native(_))));
	assert_eq!(
		drive(&storage, &node, &mut store, |store| store
			.recover_invoice(CLIENT)
			.map(Option::unwrap)),
		Ok(InvoiceProgress::Retained)
	);
	assert_eq!(store.lookup(CLIENT).unwrap().unwrap().encode(), record.encode());
	assert_eq!(native_digest(&node, &store), Some(record.invoice().unwrap().digest()));
}

#[test]
fn ffor_request_issuer_release_performs_no_storage_io_under_publication() {
	let (storage, node, mut store) = bound(ACTIVE);
	let record = issue_live(&storage, &node, &mut store);
	let handle = store.confirmed_invoice(CLIENT).unwrap().unwrap();
	let reads = Arc::new(std::sync::Mutex::new(Vec::new()));
	let sink = Arc::clone(&reads);
	*storage.read_hook.lock().unwrap() =
		Some(Box::new(move |primary: &str| sink.lock().unwrap().push(primary.to_owned())));
	let writes = storage.writes.load(Ordering::SeqCst);
	assert_eq!(
		release(&mut store, &handle),
		Ok(Some(record.invoice().unwrap().invoice().to_owned()))
	);
	*storage.read_hook.lock().unwrap() = None;
	assert_eq!(storage.writes.load(Ordering::SeqCst), writes);
	let reads = reads.lock().unwrap();
	assert!(!reads.is_empty());
	assert!(reads.iter().all(|primary| primary == NAMESPACE), "{reads:?}");
}

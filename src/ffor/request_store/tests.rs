use std::str::FromStr;
use std::sync::atomic::Ordering;

use bitcoin::hex::FromHex;
use bitcoin::secp256k1::{Secp256k1, SecretKey};
use bitcoin::Network;
use lightning::ln::ffor::FFORReceiverParameters;
use lightning::ln::msgs::{BaseMessageHandler, Init};
use lightning::ln::types::ChannelId;
use lightning::util::persist::{
	CHANNEL_MANAGER_PERSISTENCE_KEY, CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
	CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE, CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
	CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
};
use lightning::util::ser::Writeable;
use lightning_types::features::InitFeatures;
use proptest::prelude::*;

use super::*;
use crate::builder::NodeBuilder;
use crate::ffor::witness_store::tests::TestStore;
use crate::{Config, Node};

const SEED: [u8; 64] = [91; 64];
const CLIENT: &str = "request-fixture";

fn field(name: &str) -> &'static str {
	include_str!("fixtures/fixture.txt")
		.lines()
		.find_map(|line| line.strip_prefix(&format!("{name}=")))
		.unwrap()
}

fn install_fixture(storage: &TestStore, pending: bool) {
	let manager = if pending {
		include_bytes!("fixtures/pending-manager.bin").as_slice()
	} else {
		include_bytes!("fixtures/empty-manager.bin").as_slice()
	};
	let monitor = if pending {
		include_bytes!("fixtures/pending-monitor.bin").as_slice()
	} else {
		include_bytes!("fixtures/empty-monitor.bin").as_slice()
	};
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
		&format!("{}_{}", field("funding_txid"), field("funding_vout")),
		monitor.to_vec(),
	)
	.unwrap();
}

fn restore_node(storage: Arc<TestStore>) -> Node {
	let mut builder =
		NodeBuilder::from_config(Config { network: Network::Testnet, ..Config::default() });
	builder.set_entropy_seed_bytes(SEED);
	builder.set_log_facade_logger();
	builder.build_with_store(storage).unwrap()
}

fn open(storage: Arc<TestStore>, node: &Node) -> RequestStore {
	RequestStore::open(&SEED, node, storage).unwrap()
}

fn fixture() -> (Arc<TestStore>, Node, RequestStore) {
	let storage = TestStore::new();
	install_fixture(&storage, false);
	let node = restore_node(Arc::clone(&storage));
	let store = open(Arc::clone(&storage), &node);
	(storage, node, store)
}

fn intent(client: &str) -> RequestIntent {
	RequestIntent::new(client.to_owned(), 2_000_000, "Exact receipt note 🔐".to_owned()).unwrap()
}

fn plan(store: &RequestStore, client: &str) -> RequestPlan {
	RequestPlan {
		channel: ChannelId(<[u8; 32]>::from_hex(field("channel_id")).unwrap()),
		settlement: PublicKey::from_str(field("settlement")).unwrap(),
		parameters: FFORReceiverParameters {
			local_request_id: store.local_request_id(client).unwrap(),
			amounts_msat: vec![2_000_000],
			minimum_payment_msat: 2_000_000,
			settlement_deadline: 110,
			voucher_expiry: 154,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			claim_margin_blocks: 20,
			witness_peers: None,
			hash_chain: false,
		},
	}
}

fn connect(node: &Node, peer: PublicKey) -> lightning::ln::ffor::FFORPeerConnection {
	let mut features = InitFeatures::empty();
	features.set_static_remote_key_optional();
	node.channel_manager
		.peer_connected(
			peer,
			&Init { features, networks: None, remote_network_address: None },
			false,
		)
		.unwrap();
	node.channel_manager.ffor_peer_connection(&peer).unwrap()
}

fn raw(storage: &TestStore, store: &RequestStore, client: &str) -> Vec<u8> {
	KVStoreSync::read(storage, NAMESPACE, "", &hex(&store.local_request_id(client).unwrap()))
		.unwrap()
}

fn pending() -> (Arc<TestStore>, Node, RequestStore) {
	let (storage, original, mut store) = fixture();
	store.begin(intent(CLIENT), plan(&store, CLIENT)).unwrap();
	drop(store);
	drop(original);
	install_fixture(&storage, true);
	let node = restore_node(Arc::clone(&storage));
	let store = open(Arc::clone(&storage), &node);
	(storage, node, store)
}

#[test]
fn ffor_request_store_retries_exact_intent_and_redacts_description() {
	let (storage, node, mut store) = fixture();
	assert_eq!(hex(&store.local_request_id(CLIENT).unwrap()), field("local_request_id"));
	let created = store.begin(intent(CLIENT), plan(&store, CLIENT)).unwrap();
	let bytes = raw(&storage, &store, CLIENT);
	let before = storage.writes.load(Ordering::SeqCst);
	let repeated = store.begin(intent(CLIENT), plan(&store, CLIENT)).unwrap();
	assert_eq!(created.encode(), repeated.encode());
	assert_eq!(storage.writes.load(Ordering::SeqCst), before);
	assert!(!format!("{created:?}").contains("Exact receipt note"));
	assert!(!bytes.windows(5).any(|window| window == b"Exact"));
	for changed in [
		RequestIntent::new(CLIENT.into(), 2_000_001, created.intent().description().into())
			.unwrap(),
		RequestIntent::new(CLIENT.into(), 2_000_000, "changed".into()).unwrap(),
	] {
		let mut selected = plan(&store, CLIENT);
		selected.parameters.amounts_msat = vec![changed.amount_msat()];
		assert!(matches!(store.begin(changed, selected), Err(RequestStoreError::Conflict)));
	}
	let mut changed = plan(&store, CLIENT);
	changed.parameters.settlement_deadline += 1;
	assert!(matches!(store.begin(intent(CLIENT), changed), Err(RequestStoreError::Conflict)));
	assert_eq!(raw(&storage, &store, CLIENT), bytes);
	drop(store);
	let mut restored = open(Arc::clone(&storage), &node);
	assert_eq!(restored.lookup(CLIENT).unwrap().unwrap().encode(), created.encode());
	assert_eq!(raw(&storage, &restored, CLIENT), bytes);
}

#[test]
fn ffor_request_store_real_native_recovery_binds_only_exact_retained_intent() {
	let (storage, node, mut store) = pending();
	assert!(store.lookup(CLIENT).unwrap().unwrap().selector().is_none());
	let connection = connect(&node, plan(&store, CLIENT).settlement);
	let actual = store.prepare_native(CLIENT, &connection).unwrap();
	assert_eq!(hex(&actual.epoch_id()), field("epoch_id"));
	assert_eq!(
		node.channel_manager
			.find_ffor_receiver_request(store.local_request_id(CLIENT).unwrap())
			.unwrap(),
		Some(actual)
	);
	let saved = raw(&storage, &store, CLIENT);
	assert_eq!(store.prepare_native(CLIENT, &connection).unwrap(), actual);
	assert_eq!(saved, raw(&storage, &store, CLIENT));
	assert!(store.lookup(CLIENT).unwrap().unwrap().selector().unwrap().matches(&actual));
	assert!(node.list_payments().is_empty());
}

#[test]
fn ffor_request_store_disconnected_recovery_binds_lost_response_without_native_mutation() {
	let (storage, node, mut store) = pending();
	let peer = plan(&store, CLIENT).settlement;
	assert!(node.channel_manager.ffor_peer_connection(&peer).is_err());
	assert!(store.lookup(CLIENT).unwrap().unwrap().selector().is_none());
	let before = node.channel_manager.encode();
	let id = store.recover_native(CLIENT).unwrap().unwrap();
	assert_eq!(node.channel_manager.encode(), before);
	assert!(store.lookup(CLIENT).unwrap().unwrap().selector().unwrap().matches(&id));
	let writes = storage.writes.load(Ordering::SeqCst);
	assert_eq!(store.recover_native(CLIENT), Ok(Some(id)));
	assert_eq!(storage.writes.load(Ordering::SeqCst), writes);
	drop(store);
	drop(node);
	let restored = restore_node(Arc::clone(&storage));
	let mut reopened = open(storage, &restored);
	assert!(restored.channel_manager.ffor_peer_connection(&peer).is_err());
	assert_eq!(reopened.recover_native(CLIENT), Ok(Some(id)));
}

#[test]
fn ffor_request_store_disconnected_recovery_does_not_allocate_an_unbound_intent() {
	let (storage, node, mut store) = fixture();
	store.begin(intent(CLIENT), plan(&store, CLIENT)).unwrap();
	let before = node.channel_manager.encode();
	let writes = storage.writes.load(Ordering::SeqCst);
	assert_eq!(store.recover_native(CLIENT), Ok(None));
	assert_eq!(storage.writes.load(Ordering::SeqCst), writes);
	assert_eq!(node.channel_manager.encode(), before);
	assert!(store.lookup(CLIENT).unwrap().unwrap().selector().is_none());
}

#[test]
fn ffor_request_store_disconnected_binding_failure_keeps_exact_candidate_until_recovery() {
	for failure in [1, 2] {
		let (storage, node, mut store) = pending();
		store.lookup(CLIENT).unwrap();
		let before = node.channel_manager.encode();
		storage.write_failure.store(failure, Ordering::SeqCst);
		assert_eq!(store.recover_native(CLIENT), Err(RequestStoreError::Storage));
		assert_eq!(store.recover_native(CLIENT), Err(RequestStoreError::Uncertain));
		assert_eq!(node.channel_manager.encode(), before);
		store.recover_write().unwrap();
		let id = store.recover_native(CLIENT).unwrap().unwrap();
		assert_eq!(node.channel_manager.encode(), before);
		drop(store);
		let mut reopened = open(storage, &node);
		assert_eq!(reopened.recover_native(CLIENT), Ok(Some(id)));
	}
}

#[test]
fn ffor_request_store_native_selector_does_not_certify_different_parameters() {
	let (storage, first, mut store) = fixture();
	let mut selected = plan(&store, CLIENT);
	selected.parameters.settlement_deadline += 1;
	store.begin(intent(CLIENT), selected).unwrap();
	drop(store);
	drop(first);
	install_fixture(&storage, true);
	let node = restore_node(Arc::clone(&storage));
	let mut store = open(storage, &node);
	assert!(node
		.channel_manager
		.find_ffor_receiver_request(store.local_request_id(CLIENT).unwrap())
		.unwrap()
		.is_some());
	assert_eq!(
		store.recover_native(CLIENT),
		Err(RequestStoreError::Native(FFORReceiverError::AlreadyRegistered))
	);
	let connection = connect(&node, plan(&store, CLIENT).settlement);
	assert_eq!(
		store.prepare_native(CLIENT, &connection),
		Err(RequestStoreError::Native(FFORReceiverError::AlreadyRegistered))
	);
	assert!(store.lookup(CLIENT).unwrap().unwrap().selector().is_none());
}

#[test]
fn ffor_request_store_missing_application_or_native_history_refuses_reconstruction() {
	let storage = TestStore::new();
	install_fixture(&storage, true);
	let node = restore_node(Arc::clone(&storage));
	let mut store = open(Arc::clone(&storage), &node);
	assert!(matches!(store.lookup(CLIENT), Err(RequestStoreError::Missing)));
	assert_eq!(store.recover_native(CLIENT), Err(RequestStoreError::Missing));
	assert!(matches!(
		store.begin(intent(CLIENT), plan(&store, CLIENT)),
		Err(RequestStoreError::Missing)
	));
	assert!(KVStoreSync::list(&*storage, NAMESPACE, "").unwrap().is_empty());
	drop(store);
	drop(node);
	let (storage, original, mut store) = pending();
	let connection = connect(&original, plan(&store, CLIENT).settlement);
	store.prepare_native(CLIENT, &connection).unwrap();
	drop(store);
	drop(original);
	install_fixture(&storage, false);
	let node = restore_node(Arc::clone(&storage));
	let mut store = open(storage, &node);
	assert_eq!(store.recover_native(CLIENT), Err(RequestStoreError::MissingNative));
	let connection = connect(&node, plan(&store, CLIENT).settlement);
	assert_eq!(store.prepare_native(CLIENT, &connection), Err(RequestStoreError::MissingNative));
}

#[test]
fn ffor_request_store_rejects_wrong_peer_and_stale_native_connection_before_binding() {
	let (_, node, mut store) = pending();
	let wrong_peer =
		PublicKey::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[13; 32]).unwrap());
	let wrong = connect(&node, wrong_peer);
	assert_eq!(store.prepare_native(CLIENT, &wrong), Err(RequestStoreError::Identity));
	let peer = plan(&store, CLIENT).settlement;
	let stale = connect(&node, peer);
	node.channel_manager.peer_disconnected(peer);
	let fresh = connect(&node, peer);
	assert!(matches!(store.prepare_native(CLIENT, &stale), Err(RequestStoreError::Native(_))));
	assert!(store.prepare_native(CLIENT, &fresh).is_ok());
}

#[test]
fn ffor_request_store_creation_failures_rewrite_exact_candidate_and_block_other_admission() {
	for failure in [1, 2] {
		let (storage, _, mut store) = fixture();
		storage.write_failure.store(failure, Ordering::SeqCst);
		assert!(matches!(
			store.begin(intent(CLIENT), plan(&store, CLIENT)),
			Err(RequestStoreError::Storage)
		));
		let candidate = store.uncertain.as_ref().unwrap().bytes.clone();
		assert!(matches!(store.lookup(CLIENT), Err(RequestStoreError::Uncertain)));
		assert!(matches!(
			store.begin(intent("other"), plan(&store, "other")),
			Err(RequestStoreError::Uncertain)
		));
		storage.write_failure.store(2, Ordering::SeqCst);
		assert_eq!(store.recover_write(), Err(RequestStoreError::Storage));
		assert_eq!(store.uncertain.as_ref().unwrap().bytes, candidate);
		store.recover_write().unwrap();
		assert_eq!(raw(&storage, &store, CLIENT), candidate);
		assert!(store.lookup(CLIENT).unwrap().is_some());
	}
}

#[test]
fn ffor_request_store_binding_failures_recover_existing_native_epoch_without_resigning() {
	for failure in [1, 2] {
		let (storage, node, mut store) = pending();
		store.lookup(CLIENT).unwrap();
		let connection = connect(&node, plan(&store, CLIENT).settlement);
		storage.write_failure.store(failure, Ordering::SeqCst);
		assert_eq!(store.prepare_native(CLIENT, &connection), Err(RequestStoreError::Storage));
		let candidate = store.uncertain.as_ref().unwrap().bytes.clone();
		assert_eq!(store.prepare_native(CLIENT, &connection), Err(RequestStoreError::Uncertain));
		store.recover_write().unwrap();
		let recovered = store.prepare_native(CLIENT, &connection).unwrap();
		assert_eq!(hex(&recovered.epoch_id()), field("epoch_id"));
		assert_eq!(raw(&storage, &store, CLIENT), candidate);
	}
}

#[test]
fn ffor_request_store_visible_failed_write_and_reopen_require_successful_confirmation() {
	let (storage, node, mut store) = fixture();
	storage.write_failure.store(2, Ordering::SeqCst);
	assert!(matches!(
		store.begin(intent(CLIENT), plan(&store, CLIENT)),
		Err(RequestStoreError::Storage)
	));
	let bytes = raw(&storage, &store, CLIENT);
	drop(store);
	for _ in 0..2 {
		let mut reopened = open(Arc::clone(&storage), &node);
		storage.write_failure.store(2, Ordering::SeqCst);
		assert!(matches!(reopened.lookup(CLIENT), Err(RequestStoreError::Storage)));
		assert_eq!(raw(&storage, &reopened, CLIENT), bytes);
	}
	let mut reopened = open(Arc::clone(&storage), &node);
	assert!(reopened.lookup(CLIENT).unwrap().is_some());
	assert_eq!(raw(&storage, &reopened, CLIENT), bytes);
}

#[test]
fn ffor_request_store_visible_binding_failure_survives_owner_restart_without_new_epoch() {
	let (storage, node, mut store) = pending();
	store.lookup(CLIENT).unwrap();
	let connection = connect(&node, plan(&store, CLIENT).settlement);
	storage.write_failure.store(2, Ordering::SeqCst);
	assert_eq!(store.prepare_native(CLIENT, &connection), Err(RequestStoreError::Storage));
	let candidate = raw(&storage, &store, CLIENT);
	drop(store);
	let mut reopened = open(Arc::clone(&storage), &node);
	storage.write_failure.store(2, Ordering::SeqCst);
	assert_eq!(reopened.prepare_native(CLIENT, &connection), Err(RequestStoreError::Storage));
	reopened.recover_write().unwrap();
	let id = reopened.prepare_native(CLIENT, &connection).unwrap();
	assert_eq!(hex(&id.epoch_id()), field("epoch_id"));
	assert_eq!(raw(&storage, &reopened, CLIENT), candidate);
}

#[test]
fn ffor_request_store_missing_or_replaced_binding_predecessor_is_never_overwritten() {
	for replacement in [None, Some(vec![4, 5, 6])] {
		let (storage, node, mut store) = pending();
		store.lookup(CLIENT).unwrap();
		let connection = connect(&node, plan(&store, CLIENT).settlement);
		storage.write_failure.store(1, Ordering::SeqCst);
		assert_eq!(store.prepare_native(CLIENT, &connection), Err(RequestStoreError::Storage));
		let key = hex(&store.local_request_id(CLIENT).unwrap());
		if let Some(bytes) = &replacement {
			KVStoreSync::write(&*storage, NAMESPACE, "", &key, bytes.clone()).unwrap();
		} else {
			KVStoreSync::remove(&*storage, NAMESPACE, "", &key, false).unwrap();
		}
		let before = storage.writes.load(Ordering::SeqCst);
		assert_eq!(
			store.recover_write(),
			Err(if replacement.is_some() {
				RequestStoreError::Conflict
			} else {
				RequestStoreError::Missing
			})
		);
		assert_eq!(storage.writes.load(Ordering::SeqCst), before);
		assert!(store.uncertain.is_some());
	}
}

#[test]
fn ffor_request_store_identity_key_and_parameter_substitution_are_authenticated() {
	let (_, _, store) = fixture();
	let record = StoredRequest::new(intent(CLIENT), plan(&store, CLIENT)).unwrap();
	let key = hex(&record.local_request_id());
	let bytes = store.envelope.seal(&key, &record).unwrap();
	let different_node =
		PublicKey::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[14; 32]).unwrap());
	for envelope in [
		EnvelopeKey::derive(&SEED, [0; 32], store.node),
		EnvelopeKey::derive(&SEED, store.chain, different_node),
	] {
		assert!(envelope.open(&key, &bytes).is_err());
	}
	assert!(store.envelope.open(&"0".repeat(64), &bytes).is_err());
	assert_ne!(
		local_id(store.chain, store.node, CLIENT),
		local_id(store.chain, store.node, "request-fixture ")
	);
	assert_ne!(local_id(store.chain, store.node, CLIENT), local_id([0; 32], store.node, CLIENT));
	assert_ne!(
		local_id(store.chain, store.node, CLIENT),
		local_id(store.chain, different_node, CLIENT)
	);
	let mut changed = plan(&store, CLIENT);
	changed.parameters.local_request_id = [1; 32];
	let changed = StoredRequest::new(intent(CLIENT), changed).unwrap();
	let storage = TestStore::new();
	KVStoreSync::write(
		&*storage,
		NAMESPACE,
		"",
		&key,
		store.envelope.seal(&key, &changed).unwrap(),
	)
	.unwrap();
	assert!(matches!(
		RequestStore::open_bound(
			&SEED,
			store.chain,
			store.node,
			Arc::clone(&store.manager),
			storage
		),
		Err(RequestStoreError::Corrupt)
	));
}

#[test]
fn ffor_request_store_corruption_replacement_deletion_and_wrong_seed_fail_closed() {
	let (storage, node, mut store) = fixture();
	store.begin(intent(CLIENT), plan(&store, CLIENT)).unwrap();
	let key = hex(&store.local_request_id(CLIENT).unwrap());
	let good = raw(&storage, &store, CLIENT);
	assert!(matches!(
		RequestStore::open(&[1; 64], &node, storage.clone()),
		Err(RequestStoreError::Corrupt)
	));
	let mut bad = good.clone();
	let last = bad.len() - 1;
	bad[last] ^= 1;
	KVStoreSync::write(&*storage, NAMESPACE, "", &key, bad).unwrap();
	assert!(matches!(store.lookup(CLIENT), Err(RequestStoreError::Conflict)));
	assert!(RequestStore::open(&SEED, &node, storage.clone()).is_err());
	KVStoreSync::remove(&*storage, NAMESPACE, "", &key, false).unwrap();
	assert!(matches!(store.lookup(CLIENT), Err(RequestStoreError::Missing)));
	assert!(matches!(
		store.begin(intent(CLIENT), plan(&store, CLIENT)),
		Err(RequestStoreError::Missing)
	));
	KVStoreSync::write(&*storage, NAMESPACE, "", &key, good).unwrap();
	assert!(store.lookup(CLIENT).unwrap().is_some());
}

#[test]
fn ffor_request_store_capacity_and_external_new_key_preserve_all_existing_bytes() {
	let (storage, _, mut store) = fixture();
	for index in 0..MAX_REQUESTS {
		let client = format!("request-{index}");
		store.begin(intent(&client), plan(&store, &client)).unwrap();
	}
	assert!(matches!(
		store.begin(intent(CLIENT), plan(&store, CLIENT)),
		Err(RequestStoreError::Capacity)
	));
	assert_eq!(store.list().unwrap().len(), MAX_REQUESTS);
	let foreign = hex(&store.local_request_id("external").unwrap());
	KVStoreSync::write(&*storage, NAMESPACE, "", &foreign, vec![7]).unwrap();
	assert!(matches!(
		store.begin(intent("external"), plan(&store, "external")),
		Err(RequestStoreError::Conflict)
	));
	assert_eq!(KVStoreSync::read(&*storage, NAMESPACE, "", &foreign).unwrap(), vec![7]);
}

#[test]
fn ffor_request_store_codec_rejects_trailing_truncated_and_invalid_parameters() {
	let (_, _, store) = fixture();
	let value = StoredRequest::new(intent(CLIENT), plan(&store, CLIENT)).unwrap();
	let bytes = value.encode();
	assert_eq!(StoredRequest::decode(&bytes).unwrap().encode(), bytes);
	for end in 0..bytes.len() {
		assert!(StoredRequest::decode(&bytes[..end]).is_err());
	}
	let mut extra = bytes.to_vec();
	extra.push(0);
	assert!(StoredRequest::decode(&extra).is_err());
	let mut future = bytes.to_vec();
	future[1] = 2;
	assert!(StoredRequest::decode(&future).is_err());
	let mut invalid = plan(&store, CLIENT);
	invalid.parameters.amounts_msat.push(1);
	assert!(StoredRequest::new(intent(CLIENT), invalid).is_err());
	assert!(RequestIntent::new("id".into(), 0, "".into()).is_err());
	assert!(RequestIntent::new("id".into(), 1, "a".repeat(640)).is_err());
	assert!(RequestIntent::new(" ".into(), 1, "".into()).is_err());
	assert!(RequestIntent::new("a".repeat(129), 1, "".into()).is_err());
}

proptest! {
	#[test]
	fn ffor_request_store_bounded_decoder_never_accepts_noncanonical_bytes(bytes in prop::collection::vec(any::<u8>(), 0..4097)) {
		if let Ok(record) = StoredRequest::decode(&bytes) { prop_assert_eq!(&*record.encode(), &bytes); }
	}
}

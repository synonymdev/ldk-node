//! Runtime state-machine tests against the genuine exported native fixtures, plus durable
//! completion-intent tests with injected write failures and restart.

use std::str::FromStr;
use std::sync::atomic::Ordering;

use bitcoin::block::{Header, Version};
use bitcoin::hash_types::TxMerkleNode;
use bitcoin::hashes::Hash;
use bitcoin::hex::FromHex;
use bitcoin::pow::CompactTarget;
use bitcoin::BlockHash;
use lightning::chain::Confirm;
use lightning::ln::channelmanager::PaymentId;
use lightning::ln::msgs::{BaseMessageHandler, ChannelAnnouncement, ChannelUpdate, Init};
use lightning::ln::peer_handler::CustomMessageHandler;
use lightning::ln::wire::{CustomMessageReader, Type};
use lightning::sign::{KeysManager, NodeSigner, Recipient};
use lightning::util::persist::{
	KVStoreSync, CHANNEL_MANAGER_PERSISTENCE_KEY, CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
	CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE, CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
	CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
};
use lightning::util::ser::{LengthReadable, Writeable};
use lightning_ffor::witness::{Acknowledgement, AcknowledgementResult};
use lightning_invoice::Bolt11Invoice;
use lightning_types::features::InitFeatures;
use lightning_types::payment::{PaymentHash, PaymentPreimage};

use super::credit::{credit_intent, mark_notified};
use super::ledger::{IntentKey, IntentState, OutcomeIntent, OutcomeLedger};
use super::*;
use crate::builder::{BuildError, NodeBuilder};
use crate::config::{OfflineReceiveConfig, OfflineReceiveWitnessConfig};
use crate::ffor::witness_store::tests::TestStore;
use crate::ffor::witness_store::{FixtureWitness, StoredWitnessEpoch, WitnessStorageBinding};
use crate::message_handler::NodeCustomMessageHandler;
use crate::payment::store::{PaymentDetails, PaymentDirection, PaymentKind, PaymentStatus};
use crate::{Config, Event, Node};

const SEED: [u8; 64] = [91; 64];
const CLIENT: &str = "request-fixture";
const AMOUNT: u64 = 2_000_000;
const EMPTY_MANAGER: &[u8] = include_bytes!("../request_store/fixtures/empty-manager.bin");
const EMPTY_MONITOR: &[u8] = include_bytes!("../request_store/fixtures/empty-monitor.bin");
const ACTIVE_MANAGER: &[u8] =
	include_bytes!("../request_store/fixtures/invoice/active-manager.bin");
const ACTIVE_MONITOR: &[u8] = include_bytes!("../request_store/fixtures/invoice/monitor.bin");
const MANIFEST: &[u8] = include_bytes!("../request_store/fixtures/invoice/witness-manifest.bin");

fn ifield(name: &str) -> &'static str {
	include_str!("../request_store/fixtures/invoice/fixture.txt")
		.lines()
		.find_map(|line| line.strip_prefix(&format!("{name}=")))
		.unwrap()
}

fn settlement() -> PublicKey {
	PublicKey::from_str(ifield("settlement")).unwrap()
}

fn witness() -> PublicKey {
	PublicKey::from_str(ifield("witness")).unwrap()
}

fn install_native(storage: &TestStore, manager: &[u8], monitor: &[u8]) {
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
		monitor.to_vec(),
	)
	.unwrap();
}

fn build(storage: Arc<TestStore>, offline_receive: Option<OfflineReceiveConfig>) -> Node {
	let mut builder =
		NodeBuilder::from_config(Config { network: Network::Testnet, ..Config::default() });
	if let Some(config) = offline_receive {
		builder.set_offline_receive_config(config);
	}
	builder.set_entropy_seed_bytes(SEED);
	builder.set_log_facade_logger();
	builder.build_with_store(storage).unwrap()
}

/// Fixture terms: tip 10, settlement deadline 145, voucher expiry 165, claim margin 20. The
/// witness retention follows the exported manifest so an existing native registration rejoins.
fn offline_config(retention_until: u32, minimum_receipts: u8) -> OfflineReceiveConfig {
	let voucher_expiry: u32 = ifield("voucher_expiry").parse().unwrap();
	let deadline: u32 = ifield("settlement_deadline").parse().unwrap();
	let tip: u32 = ifield("height").parse().unwrap();
	let mut config = OfflineReceiveConfig::new(
		settlement(),
		vec![OfflineReceiveWitnessConfig {
			node_id: witness(),
			retention_blocks: retention_until - voucher_expiry,
			minimum_receipts,
		}],
	);
	config.invoice_expiry_seconds = ifield("expiry_seconds").parse().unwrap();
	config.invoice_safety_margin_seconds = ifield("safety_margin_seconds").parse().unwrap();
	config.settlement_deadline_blocks = deadline - tip;
	config.voucher_expiry_blocks = voucher_expiry - tip;
	config.claim_margin_blocks = ifield("claim_margin_blocks").parse().unwrap();
	config
}

fn manifest_terms(storage: &Arc<TestStore>) -> (u32, u8) {
	let node = build(Arc::clone(storage), None);
	let context = node.channel_manager.list_ffor_receiver_recovery_contexts().unwrap().remove(0);
	let manifest =
		lightning_ffor::witness::SignedManifest::decode(MANIFEST, context.setup()).unwrap();
	let params = manifest.unsigned().parameters();
	(params.retention_until, params.minimum_receipts)
}

struct Harness {
	storage: Arc<TestStore>,
	node: Node,
	runtime: Arc<FforReceiverRuntime>,
	handler: NodeCustomMessageHandler,
}

impl Harness {
	fn empty() -> Self {
		let storage = TestStore::new();
		install_native(&storage, EMPTY_MANAGER, EMPTY_MONITOR);
		Self::from_storage(storage, offline_config(300, 0))
	}

	fn from_storage(storage: Arc<TestStore>, config: OfflineReceiveConfig) -> Self {
		let node = build(Arc::clone(&storage), Some(config));
		let runtime = node.offline_receive.clone().expect("runtime constructed from config");
		let handler =
			NodeCustomMessageHandler::new_ignoring().with_ffor_setup(Arc::clone(&runtime.setup));
		// Node::start performs this recovery before spawning the worker.
		runtime.recover().unwrap();
		Self { storage, node, runtime, handler }
	}

	fn connect(&self, peer: PublicKey) {
		let mut features = InitFeatures::empty();
		features.set_static_remote_key_optional();
		let init = Init { features, networks: None, remote_network_address: None };
		self.node.channel_manager.peer_connected(peer, &init, false).unwrap();
		self.handler.peer_connected(peer, &init, false).unwrap();
	}

	fn persist_manager(&self) {
		let token = self.node.channel_manager.capture_ffor_persistence();
		KVStoreSync::write(
			&*self.storage,
			CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_KEY,
			self.node.channel_manager.encode(),
		)
		.unwrap();
		self.node.channel_manager.ffor_persistence_completed(token).unwrap();
	}

	fn tick(&self) {
		self.node.runtime.block_on(self.runtime.tick());
	}

	fn status(&self) -> OfflineReceiveStatus {
		self.runtime.status(CLIENT).unwrap()
	}

	fn stage(&self) -> Stage {
		self.runtime.state.lock().unwrap().live.get(CLIENT).unwrap().stage.clone()
	}

	fn set_tip(&self, height: u32) {
		let header = Header {
			version: Version::NO_SOFT_FORK_SIGNALLING,
			prev_blockhash: BlockHash::all_zeros(),
			merkle_root: TxMerkleNode::all_zeros(),
			time: now() as u32,
			bits: CompactTarget::from_consensus(0),
			nonce: 0,
		};
		self.node.channel_manager.best_block_updated(&header, height);
	}

	fn install_sidecar(&self, acknowledged: bool) {
		let context =
			self.node.channel_manager.list_ffor_receiver_recovery_contexts().unwrap().remove(0);
		let binding = WitnessStorageBinding::from_native_context(&context).unwrap();
		let record = StoredWitnessEpoch::from_fixture(
			&binding,
			<[u8; 32]>::from_hex(ifield("encryption_secret")).unwrap(),
			vec![FixtureWitness {
				witness: witness(),
				fetch_secret: <[u8; 32]>::from_hex(ifield("fetch_secret")).unwrap(),
				manifest: MANIFEST.to_vec(),
				acknowledged,
			}],
		)
		.unwrap();
		let state = self.runtime.state.lock().unwrap();
		state.witness_store.install_fixture(&binding, record).unwrap();
	}

	fn install_route(&self) {
		let announcement = ChannelAnnouncement::read_from_fixed_length_buffer(
			&mut include_bytes!("../request_store/fixtures/invoice/route-announcement.bin")
				.as_slice(),
		)
		.unwrap();
		let mut update = ChannelUpdate::read_from_fixed_length_buffer(
			&mut include_bytes!("../request_store/fixtures/invoice/route-update.bin").as_slice(),
		)
		.unwrap();
		update.contents.timestamp = now() as u32;
		let seed = <[u8; 32]>::from_hex(ifield("witness_node_seed")).unwrap();
		let keys = KeysManager::new(&seed, 0, 0, true);
		assert_eq!(keys.get_node_id(Recipient::Node).unwrap(), witness());
		update.signature = keys
			.sign_gossip_message(lightning::ln::msgs::UnsignedGossipMessage::ChannelUpdate(
				&update.contents,
			))
			.unwrap();
		self.node.network_graph.update_channel_from_announcement_no_lookup(&announcement).unwrap();
		self.node.network_graph.update_channel(&update).unwrap();
	}

	fn outbound_types(&self) -> Vec<u16> {
		self.handler
			.get_and_clear_pending_msg()
			.into_iter()
			.map(|(_, message)| message.type_id())
			.collect()
	}
}

fn now() -> u64 {
	std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

fn prepared_active_storage() -> (Arc<TestStore>, OfflineReceiveConfig) {
	// Begin the durable record on the empty pair exactly as the issuer fixture did, then
	// restore the exported Active manager over it.
	let h = Harness::empty();
	assert_eq!(
		h.runtime.prepare(CLIENT.to_owned(), AMOUNT, ifield("description").to_owned()),
		Ok(OfflineReceiveStatus::Preparing)
	);
	let storage = Arc::clone(&h.storage);
	drop(h);
	install_native(&storage, ACTIVE_MANAGER, ACTIVE_MONITOR);
	let (retention_until, minimum_receipts) = manifest_terms(&storage);
	(storage, offline_config(retention_until, minimum_receipts))
}

#[test]
fn ffor_runtime_is_absent_without_config_and_refuses_invalid_config() {
	let storage = TestStore::new();
	install_native(&storage, EMPTY_MANAGER, EMPTY_MONITOR);
	let node = build(Arc::clone(&storage), None);
	assert!(node.offline_receive.is_none());
	let handler = node.offline_receive();
	assert_eq!(handler.can_receive(AMOUNT), Err(Error::OfflineReceiveDisabled));
	assert_eq!(handler.status(CLIENT.to_owned()), Err(Error::OfflineReceiveDisabled));
	assert_eq!(
		handler.prepare(CLIENT.to_owned(), AMOUNT, "x".to_owned()),
		Err(Error::OfflineReceiveDisabled)
	);
	assert_eq!(handler.cancel(CLIENT.to_owned()), Err(Error::OfflineReceiveDisabled));
	drop(node);

	let mut invalid = offline_config(300, 0);
	invalid.witnesses.clear();
	let mut builder =
		NodeBuilder::from_config(Config { network: Network::Testnet, ..Config::default() });
	builder.set_offline_receive_config(invalid);
	builder.set_entropy_seed_bytes(SEED);
	builder.set_log_facade_logger();
	let backing: Arc<crate::types::DynStore> = storage;
	assert!(matches!(
		builder.build_with_store(backing).map(|_| ()),
		Err(BuildError::InvalidOfflineReceiveConfig)
	));

	let mut config = offline_config(300, 0);
	config.witnesses[0].node_id = settlement();
	assert!(config.validate().is_err());
	let mut config = offline_config(300, 0);
	config.voucher_expiry_blocks = config.settlement_deadline_blocks;
	assert!(config.validate().is_err());
	assert!(offline_config(300, 0).validate().is_ok());
}

#[test]
fn ffor_runtime_handler_requires_running_node_for_mutations() {
	let h = Harness::empty();
	let handler = h.node.offline_receive();
	assert_eq!(handler.can_receive(AMOUNT), Err(Error::NotRunning));
	assert_eq!(handler.prepare(CLIENT.to_owned(), AMOUNT, "x".to_owned()), Err(Error::NotRunning));
	assert_eq!(handler.cancel(CLIENT.to_owned()), Err(Error::NotRunning));
	assert_eq!(handler.status(CLIENT.to_owned()), Err(Error::OfflineReceiveRequestNotFound));
}

#[test]
fn ffor_runtime_prepare_is_idempotent_and_eligibility_tracks_channel_and_live_requests() {
	let h = Harness::empty();
	assert_eq!(h.runtime.can_receive(0), Ok(false));
	assert_eq!(h.runtime.can_receive(AMOUNT), Ok(true));
	h.connect(settlement());
	assert_eq!(h.runtime.can_receive(AMOUNT), Ok(true));
	assert_eq!(h.runtime.can_receive(u64::MAX), Ok(false));
	assert_eq!(h.runtime.status(CLIENT), Err(RuntimeError::NotFound));
	assert_eq!(
		h.runtime.prepare(CLIENT.to_owned(), AMOUNT, ifield("description").to_owned()),
		Ok(OfflineReceiveStatus::Preparing)
	);
	{
		let mut state = h.runtime.state.lock().unwrap();
		let record = state.requests.lookup(CLIENT).unwrap().unwrap();
		assert_eq!(super::ledger::hex(&record.local_request_id()), ifield("local_request_id"));
		assert_eq!(record.plan().parameters.settlement_deadline, 145);
		assert_eq!(record.plan().parameters.voucher_expiry, 165);
		assert_eq!(record.plan().parameters.witness_peers, Some(vec![witness()]));
	}
	assert_eq!(
		h.runtime.prepare(CLIENT.to_owned(), AMOUNT, ifield("description").to_owned()),
		Ok(OfflineReceiveStatus::Preparing)
	);
	assert_eq!(
		h.runtime.prepare(CLIENT.to_owned(), AMOUNT + 1, ifield("description").to_owned()),
		Err(RuntimeError::Conflict)
	);
	assert_eq!(
		h.runtime.prepare(CLIENT.to_owned(), AMOUNT, "other".to_owned()),
		Err(RuntimeError::Conflict)
	);
	assert_eq!(h.runtime.can_receive(AMOUNT), Ok(false));
	assert_eq!(
		h.runtime.prepare("second".to_owned(), AMOUNT, "x".to_owned()),
		Err(RuntimeError::Ineligible)
	);

	// A connected but not yet reestablished channel refuses native preparation; the request
	// keeps waiting instead of failing, and nothing is released or registered natively.
	h.tick();
	assert_eq!(h.status(), OfflineReceiveStatus::Preparing);
	assert!(h
		.runtime
		.state
		.lock()
		.unwrap()
		.requests
		.lookup(CLIENT)
		.unwrap()
		.unwrap()
		.selector()
		.is_none());
	assert!(h.node.channel_manager.list_ffor_receiver_recovery_contexts().unwrap().is_empty());
	assert!(h.outbound_types().is_empty());
	assert!(h.node.event_queue.next_event().is_none());
}

#[test]
fn ffor_runtime_cancel_and_deadline_before_binding_are_durable_and_release_nothing() {
	let h = Harness::empty();
	h.runtime.prepare(CLIENT.to_owned(), AMOUNT, "a".to_owned()).unwrap();
	h.runtime.cancel(CLIENT).unwrap();
	h.tick();
	assert_eq!(h.status(), OfflineReceiveStatus::Failed { reason: "cancelled".to_owned() });
	let storage = Arc::clone(&h.storage);
	drop(h);
	let restored = Harness::from_storage(storage, offline_config(300, 0));
	assert_eq!(restored.status(), OfflineReceiveStatus::Failed { reason: "cancelled".to_owned() });
	assert_eq!(
		restored.runtime.prepare(CLIENT.to_owned(), AMOUNT, "a".to_owned()),
		Ok(OfflineReceiveStatus::Failed { reason: "cancelled".to_owned() })
	);

	let h = Harness::empty();
	h.runtime.prepare("late".to_owned(), AMOUNT, "a".to_owned()).unwrap();
	h.set_tip(145 - 6);
	h.tick();
	assert_eq!(
		h.runtime.status("late"),
		Ok(OfflineReceiveStatus::Failed { reason: "deadline".to_owned() })
	);
	assert!(h.outbound_types().is_empty());
	assert!(h.node.channel_manager.list_ffor_receiver_recovery_contexts().unwrap().is_empty());
}

#[test]
fn ffor_runtime_recovers_active_epoch_and_reports_ready_only_after_native_release() {
	let (storage, config) = prepared_active_storage();
	let h = Harness::from_storage(storage, config.clone());
	h.install_sidecar(true);
	assert_eq!(h.status(), OfflineReceiveStatus::AwaitingActivation);

	// Restored Active authority requires a fresh persistence barrier; no peer is connected.
	h.tick();
	assert_eq!(h.status(), OfflineReceiveStatus::AwaitingActivation);
	h.persist_manager();
	h.tick();
	assert_eq!(h.stage(), Stage::Witnesses);
	assert_eq!(h.status(), OfflineReceiveStatus::AwaitingWitnesses);
	h.tick();
	assert_eq!(h.stage(), Stage::Invoice);
	assert_eq!(h.status(), OfflineReceiveStatus::AwaitingWitnesses);

	// No signed witness route in the graph: nothing is issued.
	h.tick();
	assert_eq!(h.stage(), Stage::Invoice);
	assert!(h
		.runtime
		.state
		.lock()
		.unwrap()
		.requests
		.lookup(CLIENT)
		.unwrap()
		.unwrap()
		.confirmed_payment()
		.unwrap()
		.is_none());
	h.install_route();
	h.tick();
	assert_eq!(h.status(), OfflineReceiveStatus::AwaitingWitnesses);
	h.persist_manager();
	h.tick();
	let bolt11 = match h.status() {
		OfflineReceiveStatus::Ready { bolt11 } => bolt11,
		other => panic!("expected Ready, got {other:?}"),
	};
	let invoice: Bolt11Invoice = bolt11.parse().unwrap();
	assert_eq!(invoice.amount_milli_satoshis(), Some(AMOUNT));
	assert_eq!(invoice.recover_payee_pub_key(), h.node.node_id());
	let payment_id = PaymentId(invoice.payment_hash().to_byte_array());
	let payment = h.node.payment_store.get(&payment_id).unwrap();
	assert_eq!(payment.status, PaymentStatus::Pending);
	assert!(h.node.event_queue.next_event().is_none());
	assert!(h.runtime.state.lock().unwrap().credits.is_empty());

	// The Active epoch has no journal: the outcome getter credits nothing.
	let context = h.node.channel_manager.list_ffor_receiver_recovery_contexts().unwrap().remove(0);
	let voucher = context.setup().vouchers()[0];
	assert_eq!(voucher.payment_hash, payment_id.0);
	assert_eq!(
		h.node
			.channel_manager
			.ffor_receiver_voucher_outcome(&context, 1, PaymentHash(payment_id.0), AMOUNT)
			.unwrap(),
		None
	);

	// Restart: the same exact invoice is released again only after a fresh native barrier.
	h.persist_manager();
	let storage = Arc::clone(&h.storage);
	drop(h);
	let h = Harness::from_storage(storage, config);
	assert_eq!(h.stage(), Stage::Invoice);
	assert_eq!(h.status(), OfflineReceiveStatus::AwaitingWitnesses);
	h.install_route();
	h.tick();
	assert_eq!(h.status(), OfflineReceiveStatus::AwaitingWitnesses);
	h.persist_manager();
	h.tick();
	assert_eq!(h.status(), OfflineReceiveStatus::Ready { bolt11: bolt11.clone() });
	assert_eq!(h.node.payment_store.get(&payment_id).unwrap().status, PaymentStatus::Pending);

	// Deadline margin: the runtime requests a cooperative close and stops presenting the invoice.
	h.set_tip(145 - 6);
	h.tick();
	assert_eq!(h.stage(), Stage::Closing(CloseReason::Deadline));
	assert_eq!(h.status(), OfflineReceiveStatus::Expired);
	assert_eq!(h.runtime.can_receive(AMOUNT), Ok(false));
	assert!(h.node.event_queue.next_event().is_none());
}

#[test]
fn ffor_runtime_history_blocks_channel_until_every_epoch_is_terminal() {
	let (storage, config) = prepared_active_storage();
	let h = Harness::from_storage(storage, config);
	h.persist_manager();
	let contexts = h.node.channel_manager.list_ffor_receiver_recovery_contexts().unwrap();
	assert_eq!(contexts.len(), 1);
	let channel = contexts[0].channel_id();
	let other = ChannelId([9; 32]);
	// The Active epoch has no completed journal: not terminal, so the channel stays blocked
	// while a different channel is unaffected.
	assert!(!h.runtime.epoch_is_terminal(&contexts[0]));
	assert!(history_blocks_channel(&channel, &contexts, |c| h.runtime.epoch_is_terminal(c)));
	assert!(!history_blocks_channel(&other, &contexts, |_| false));
	assert!(!history_blocks_channel(&channel, &contexts, |_| true));
	assert!(history_blocks_channel(&channel, &contexts, |_| false));
	assert!(!history_blocks_channel(&channel, &[], |_| false));
	assert_eq!(h.runtime.can_receive(AMOUNT), Ok(false));
	assert_eq!(
		h.runtime.prepare("next".to_owned(), AMOUNT, "x".to_owned()),
		Err(RuntimeError::Ineligible)
	);
}

#[test]
fn ffor_runtime_missing_witness_sidecar_never_regenerates_keys_or_issues() {
	let (storage, config) = prepared_active_storage();
	let h = Harness::from_storage(storage, config);
	h.persist_manager();
	h.tick();
	assert_eq!(h.stage(), Stage::Witnesses);
	let writes = h.storage.writes.load(Ordering::SeqCst);
	h.tick();
	assert!(matches!(h.stage(), Stage::Failed(reason) if reason.contains("Missing")));
	assert_eq!(h.storage.writes.load(Ordering::SeqCst), writes);
	assert!(h
		.runtime
		.state
		.lock()
		.unwrap()
		.requests
		.lookup(CLIENT)
		.unwrap()
		.unwrap()
		.confirmed_payment()
		.unwrap()
		.is_none());
}

#[test]
fn ffor_runtime_unknown_witness_acknowledgement_is_dropped() {
	let (storage, config) = prepared_active_storage();
	let h = Harness::from_storage(storage, config);
	h.connect(witness());
	let ack = Acknowledgement::new(
		[7; 16],
		AcknowledgementResult::Accepted { witness: witness(), retention_until: 400 },
	)
	.unwrap()
	.encode();
	let message =
		h.handler.read(u16::from_be_bytes([ack[0], ack[1]]), &mut &ack[2..]).unwrap().unwrap();
	h.handler.handle_custom_message(message, witness()).unwrap();
	h.tick();
	assert!(h.runtime.state.lock().unwrap().pending_acks.is_empty());
	assert!(h.runtime.transport.pop().is_none());
}

fn pending_row(hash: PaymentHash) -> PaymentDetails {
	PaymentDetails::new(
		PaymentId(hash.0),
		PaymentKind::Bolt11 { hash, preimage: None, secret: None, description: None, bolt11: None },
		Some(AMOUNT),
		None,
		PaymentDirection::Inbound,
		PaymentStatus::Pending,
	)
}

fn intent(hash: PaymentHash) -> OutcomeIntent {
	OutcomeIntent {
		key: IntentKey { channel: ChannelId([3; 32]), epoch: [4; 32], slot: 1 },
		state: IntentState::Intended,
		payment_hash: hash,
		amount_msat: AMOUNT,
		client_id: CLIENT.to_owned(),
	}
}

/// Run the complete credit sequence once; every error is returned for the caller to inject.
fn run_credit(node: &Node, ledger: &OutcomeLedger, hash: PaymentHash) -> Result<(), RuntimeError> {
	let preimage = Some(PaymentPreimage([9; 32]));
	if let Some((credited, event)) =
		credit_intent(ledger, &node.payment_store, &node.event_queue, &intent(hash), preimage)?
	{
		node.runtime
			.block_on(node.event_queue.add_event(event))
			.map_err(|_| RuntimeError::Ledger(super::ledger::LedgerError::Storage))?;
		mark_notified(ledger, &credited)?;
	}
	Ok(())
}

fn received_events(node: &Node) -> usize {
	let mut count = 0;
	while let Some(event) = node.event_queue.next_event() {
		assert!(matches!(event, Event::PaymentReceived { amount_msat: AMOUNT, .. }));
		count += 1;
		node.runtime.block_on(node.event_queue.event_handled()).unwrap();
	}
	count
}

#[test]
fn ffor_runtime_credit_sequence_survives_a_failed_write_at_every_step_and_restart() {
	let hash = PaymentHash([5; 32]);
	// Writes: Intended, payment row, Credited, event queue, Notified.
	for step in 1..=5 {
		for failure in [1usize, 2] {
			let storage = TestStore::new();
			install_native(&storage, EMPTY_MANAGER, EMPTY_MONITOR);
			let node = build(Arc::clone(&storage), None);
			node.payment_store.insert(pending_row(hash)).unwrap();
			let backing: Arc<crate::types::DynStore> = storage.clone();
			let ledger = OutcomeLedger::new(backing);
			let base = storage.writes.load(Ordering::SeqCst);
			storage.fail_at.store(base + step, Ordering::SeqCst);
			storage.write_failure.store(failure, Ordering::SeqCst);
			let result = run_credit(&node, &ledger, hash);
			assert!(result.is_err(), "step {step} failure {failure} must surface");
			storage.fail_at.store(0, Ordering::SeqCst);
			drop(node);

			// Restart reloads the payment ledger and the event queue from the same storage.
			let node = build(Arc::clone(&storage), None);
			let backing: Arc<crate::types::DynStore> = storage.clone();
			let ledger = OutcomeLedger::new(backing);
			run_credit(&node, &ledger, hash).unwrap();
			let writes = storage.writes.load(Ordering::SeqCst);
			run_credit(&node, &ledger, hash).unwrap();
			assert_eq!(storage.writes.load(Ordering::SeqCst), writes, "third run is a no-op");
			let payment = node.payment_store.get(&PaymentId(hash.0)).unwrap();
			assert_eq!(payment.status, PaymentStatus::Succeeded);
			assert!(matches!(
				payment.kind,
				PaymentKind::Bolt11 { preimage: Some(preimage), .. } if preimage.0 == [9; 32]
			));
			assert_eq!(
				ledger.load(&intent(hash).key).unwrap().unwrap().state,
				IntentState::Notified
			);
			assert_eq!(received_events(&node), 1, "step {step} failure {failure}");
			assert!(ledger.unfinished().unwrap().is_empty());
		}
	}
}

#[test]
fn ffor_runtime_credit_sequence_refuses_conflicts_and_missing_rows() {
	let hash = PaymentHash([6; 32]);
	let storage = TestStore::new();
	install_native(&storage, EMPTY_MANAGER, EMPTY_MONITOR);
	let node = build(Arc::clone(&storage), None);
	let backing: Arc<crate::types::DynStore> = storage.clone();
	let ledger = OutcomeLedger::new(backing);
	assert_eq!(run_credit(&node, &ledger, hash), Err(RuntimeError::NotFound));
	assert_eq!(ledger.load(&intent(hash).key).unwrap().unwrap().state, IntentState::Intended);
	node.payment_store.insert(pending_row(hash)).unwrap();
	run_credit(&node, &ledger, hash).unwrap();
	let mut other = intent(hash);
	other.amount_msat += 1;
	assert_eq!(
		credit_intent(&ledger, &node.payment_store, &node.event_queue, &other, None).map(|_| ()),
		Err(RuntimeError::Conflict)
	);
	let mut regress = intent(hash);
	regress.state = IntentState::Credited;
	assert_eq!(ledger.write(&regress), Err(super::ledger::LedgerError::Conflict));
	assert_eq!(received_events(&node), 1);
}

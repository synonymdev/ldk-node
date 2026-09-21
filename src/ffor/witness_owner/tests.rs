//! Real NodeBuilder restore of a publicly reproducible funded, signed Active epoch.

use std::str::FromStr;
use std::sync::atomic::Ordering;

use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
use bitcoin::Network;
use lightning::ln::ffor::FFORReceiverRecoveryContext;
use lightning::ln::msgs::{BaseMessageHandler, Init};
use lightning::ln::peer_handler::CustomMessageHandler;
use lightning::ln::wire::CustomMessageReader;
use lightning::util::persist::{
	KVStoreSync, CHANNEL_MANAGER_PERSISTENCE_KEY, CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
	CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE, CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
	CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
};
use lightning::util::ser::Writeable;
use lightning_ffor::witness::{AcknowledgementResult, Provision, WitnessConnection};
use lightning_types::features::InitFeatures;

use super::provisioning::{ProvisioningProgress, RegistrationProgress};
use super::*;
use crate::builder::NodeBuilder;
use crate::ffor::witness_store::tests::TestStore;
use crate::ffor::witness_store::WitnessPolicy;
use crate::message_handler::ffor::FforSetupAdapter;
use crate::message_handler::{NodeCustomMessage, NodeCustomMessageHandler};
use crate::types::DynStore;
use crate::{Config, Node};

const SEED: [u8; 64] = [91; 64];

fn field(name: &str) -> &'static str {
	include_str!("fixtures/fixture.txt")
		.lines()
		.find_map(|line| line.strip_prefix(&format!("{name}=")))
		.unwrap()
}

fn key(byte: u8) -> PublicKey {
	PublicKey::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[byte; 32]).unwrap())
}

fn restore(storage: Arc<TestStore>) -> Node {
	let mut builder =
		NodeBuilder::from_config(Config { network: Network::Testnet, ..Config::default() });
	builder.set_entropy_seed_bytes(SEED);
	builder.set_log_facade_logger();
	let storage: Arc<DynStore> = storage;
	builder.build_with_store(storage).unwrap()
}

pub(super) struct Harness {
	pub(super) node: Node,
	pub(super) storage: Arc<TestStore>,
	pub(super) owner: WitnessOwner,
	pub(super) handler: NodeCustomMessageHandler,
	pub(super) context: FFORReceiverRecoveryContext,
	pub(super) policies: Vec<WitnessPolicy>,
}

impl Harness {
	pub(super) fn new() -> Self {
		let storage = TestStore::new();
		KVStoreSync::write(
			&*storage,
			CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_KEY,
			include_bytes!("fixtures/active-manager.bin").to_vec(),
		)
		.unwrap();
		KVStoreSync::write(
			&*storage,
			CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
			&format!("{}_{}", field("funding_txid"), field("funding_vout")),
			include_bytes!("fixtures/active-monitor.bin").to_vec(),
		)
		.unwrap();
		Self::from_store(storage)
	}

	pub(super) fn from_store(storage: Arc<TestStore>) -> Self {
		let node = restore(Arc::clone(&storage));
		assert_eq!(node.node_id(), PublicKey::from_str(field("receiver")).unwrap());
		let context =
			node.channel_manager.list_ffor_receiver_recovery_contexts().unwrap().remove(0);
		assert_eq!(context.settlement_node_id(), PublicKey::from_str(field("settlement")).unwrap());
		let transport = Arc::new(FforReceiverTransport::default());
		let adapter = Arc::new(FforSetupAdapter::new(
			Arc::clone(&node.channel_manager),
			Arc::clone(&node.chain_monitor),
			Arc::clone(&transport),
		));
		let handler = NodeCustomMessageHandler::new_ignoring().with_ffor_setup(adapter);
		let backing: Arc<DynStore> = storage.clone();
		let store = Arc::new(WitnessSecretStore::open(&SEED, backing).unwrap());
		let owner = WitnessOwner::new(
			Arc::clone(&node.channel_manager),
			Arc::clone(&node.chain_monitor),
			store,
			transport,
		);
		let expiry = context.setup().terms().voucher_expiry;
		let policies = vec![WitnessPolicy {
			witness: key(80),
			retention_until: expiry + 144,
			minimum_receipts: 0,
		}];
		Self { node, storage, owner, handler, context, policies }
	}

	pub(super) fn persist_manager(&self) {
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

	pub(super) fn connect(&self, peer: PublicKey) {
		let mut features = InitFeatures::empty();
		features.set_static_remote_key_optional();
		let init = Init { features, networks: None, remote_network_address: None };
		self.node.channel_manager.peer_connected(peer, &init, false).unwrap();
		self.handler.peer_connected(peer, &init, false).unwrap();
	}

	pub(super) fn disconnect(&self, peer: PublicKey) {
		self.node.channel_manager.peer_disconnected(peer);
		self.handler.peer_disconnected(peer);
	}

	pub(super) fn register_and_persist(&mut self) {
		self.persist_manager();
		assert_eq!(
			self.owner.register(&self.context, &self.policies),
			Ok(RegistrationProgress::AwaitingPersistence)
		);
		self.persist_manager();
		assert_eq!(
			self.owner.register(&self.context, &self.policies),
			Ok(RegistrationProgress::Retained)
		);
	}

	fn outgoing(&self) -> Vec<Provision> {
		self.handler
			.get_and_clear_pending_msg()
			.into_iter()
			.map(|(peer, message)| {
				assert_eq!(peer, self.policies[0].witness);
				match message {
					NodeCustomMessage::Ffor(frame) => {
						Provision::decode(frame.wire(), self.context.setup()).unwrap()
					},
					_ => panic!("unexpected LSPS message"),
				}
			})
			.collect()
	}

	fn accepted(&self, provision: &Provision) -> Acknowledgement {
		Acknowledgement::new(
			provision.request_id(),
			AcknowledgementResult::Accepted {
				witness: self.policies[0].witness,
				retention_until: provision.manifest().unsigned().parameters().retention_until,
			},
		)
		.unwrap()
	}

	fn receive(&self, peer: PublicKey, ack: &Acknowledgement) -> ReceivedFforMessage {
		let bytes = ack.encode();
		let message = self
			.handler
			.read(u16::from_be_bytes([bytes[0], bytes[1]]), &mut &bytes[2..])
			.unwrap()
			.unwrap();
		self.handler.handle_custom_message(message, peer).unwrap();
		self.owner.transport.pop().unwrap()
	}

	fn secret_bytes(&self) -> Vec<u8> {
		let binding = WitnessStorageBinding::from_native_context(&self.context).unwrap();
		self.storage.bytes(&binding.storage_key())
	}
}

#[test]
fn ffor_witness_owner_real_restore_registration_and_queue_require_durability() {
	let mut h = Harness::new();
	let w = h.policies[0].witness;
	h.connect(w);
	assert!(h
		.node
		.channel_manager
		.capture_ffor_receiver_active_context(
			&h.context.channel_id(),
			&h.context.settlement_node_id(),
			h.context.epoch_id()
		)
		.is_err());
	assert!(matches!(h.owner.register(&h.context, &h.policies), Err(WitnessOwnerError::Native(_))));
	assert!(h
		.node
		.channel_manager
		.ffor_receiver_witness_registration(&h.context)
		.unwrap()
		.is_none());
	assert!(h.outgoing().is_empty());
	h.persist_manager();
	assert_eq!(
		h.owner.register(&h.context, &h.policies),
		Ok(RegistrationProgress::AwaitingPersistence)
	);
	assert_eq!(h.owner.provision(&h.context, w), Ok(ProvisioningProgress::AwaitingPersistence));
	assert!(h.outgoing().is_empty());
	h.persist_manager();
	assert_eq!(h.owner.provision(&h.context, w), Ok(ProvisioningProgress::Queued));
	let first = h.outgoing().remove(0);
	assert_eq!(h.owner.provision(&h.context, w), Ok(ProvisioningProgress::AwaitingAcknowledgement));
	assert!(h.outgoing().is_empty());
	assert_eq!(h.owner.retry_provision(&h.context, w), Ok(ProvisioningProgress::Queued));
	let second = h.outgoing().remove(0);
	assert_ne!(first.request_id(), second.request_id());
	assert_eq!(first.manifest(), second.manifest());
	let old = h.receive(w, &h.accepted(&first));
	assert_eq!(h.owner.acknowledge(&old), Err(WitnessOwnerError::UnknownRequest));
	let current = h.receive(w, &h.accepted(&second));
	let before = h.secret_bytes();
	assert_eq!(h.owner.acknowledge(&current), Ok(AcknowledgementProgress::AwaitingPersistence));
	h.persist_manager();
	assert_eq!(h.owner.acknowledge(&current), Ok(AcknowledgementProgress::Retained));
	assert_ne!(before, h.secret_bytes());
	assert_eq!(h.owner.pending.usage(), (0, 0));
}

#[test]
fn ffor_witness_owner_real_ack_correlates_connection_and_retains_failed_write() {
	let mut h = Harness::new();
	h.register_and_persist();
	let w = h.policies[0].witness;
	h.connect(w);
	h.connect(key(81));
	assert_eq!(h.owner.provision(&h.context, w), Ok(ProvisioningProgress::Queued));
	let request = h.outgoing().remove(0);
	let ack = h.accepted(&request);
	let wrong = h.receive(key(81), &ack);
	assert!(matches!(h.owner.acknowledge(&wrong), Err(WitnessOwnerError::StaleConnection)));
	let stale = h.receive(w, &ack);
	h.disconnect(w);
	h.connect(w);
	assert_eq!(h.owner.acknowledge(&stale), Err(WitnessOwnerError::StaleConnection));
	assert_eq!(h.owner.provision(&h.context, w), Ok(ProvisioningProgress::Queued));
	let current = h.outgoing().remove(0);
	assert_ne!(request.request_id(), current.request_id());
	let input = h.receive(w, &h.accepted(&current));
	let before = h.secret_bytes();
	h.storage.write_failure.store(2, Ordering::SeqCst);
	assert_eq!(
		h.owner.acknowledge(&input),
		Err(WitnessOwnerError::Storage(WitnessStoreError::Storage))
	);
	assert!(h
		.node
		.channel_manager
		.ffor_receiver_witness_acknowledgements(&h.context)
		.unwrap()
		.unwrap()
		.acknowledgements()
		.is_empty());
	let visible_candidate = h.secret_bytes();
	assert_ne!(before, visible_candidate);
	assert_eq!(h.owner.pending.usage().0, 1);
	h.owner.recover_storage().unwrap();
	assert_eq!(h.owner.acknowledge(&input), Ok(AcknowledgementProgress::AwaitingPersistence));
	h.persist_manager();
	assert_eq!(h.owner.acknowledge(&input), Ok(AcknowledgementProgress::Retained));
	assert_eq!(visible_candidate, h.secret_bytes());
	assert_eq!(h.owner.pending.usage(), (0, 0));
}

#[test]
fn ffor_witness_owner_real_native_registration_never_recreates_missing_sidecar() {
	let mut h = Harness::new();
	h.register_and_persist();
	let binding = WitnessStorageBinding::from_native_context(&h.context).unwrap();
	let key = binding.storage_key();
	let before = h.secret_bytes();
	let mut changed = h.policies.clone();
	changed[0].minimum_receipts = 1;
	assert_eq!(h.owner.register(&h.context, &changed), Err(WitnessOwnerError::Conflict));
	assert_eq!(before, h.secret_bytes());
	let storage = Arc::clone(&h.storage);
	drop(h);
	for namespace in ["ffor_witness", "ffor_witness_receipts"] {
		KVStoreSync::remove(&*storage, namespace, "", &key, false).unwrap();
	}
	let mut restored = Harness::from_store(storage);
	restored.persist_manager();
	let writes = restored.storage.writes.load(Ordering::SeqCst);
	assert_eq!(
		restored.owner.register(&restored.context, &restored.policies),
		Err(WitnessOwnerError::Storage(WitnessStoreError::Missing))
	);
	assert_eq!(restored.storage.writes.load(Ordering::SeqCst), writes);
	assert!(restored.outgoing().is_empty());
}

#[test]
fn ffor_witness_owner_real_queue_backpressure_retains_exact_unsent_request() {
	let mut h = Harness::new();
	h.register_and_persist();
	let w = h.policies[0].witness;
	h.connect(w);
	let token = h.owner.transport.connection(w).unwrap();
	let manifests = h.owner.retained_manifests(&h.context).unwrap();
	for i in 0..8 {
		h.owner
			.transport
			.enqueue_provision(w, &token, &Provision::new([i; 16], manifests[0].1.clone()))
			.unwrap();
	}
	assert_eq!(h.owner.provision(&h.context, w), Ok(ProvisioningProgress::Backpressured));
	let count = h.owner.pending.usage();
	assert_eq!(count.0, 1);
	assert_eq!(h.outgoing().len(), 8);
	assert_eq!(h.owner.provision(&h.context, w), Ok(ProvisioningProgress::Queued));
	let sent = h.outgoing().remove(0);
	assert!(h.owner.pending.contains_request_id(sent.request_id()));
	assert_eq!(h.owner.pending.usage(), count);
}

#[test]
fn ffor_witness_owner_real_channel_removal_refuses_release_but_retains_historical_ack() {
	let mut h = Harness::new();
	h.register_and_persist();
	let w = h.policies[0].witness;
	h.connect(w);
	assert_eq!(h.owner.provision(&h.context, w), Ok(ProvisioningProgress::Queued));
	let provision = h.outgoing().remove(0);
	let response = h.receive(w, &h.accepted(&provision));
	h.node
		.channel_manager
		.force_close_broadcasting_latest_txn(
			&h.context.channel_id(),
			&h.context.settlement_node_id(),
			"fixture close".to_owned(),
		)
		.unwrap();
	assert!(h.node.channel_manager.list_channels().is_empty());
	assert!(matches!(h.owner.retry_provision(&h.context, w), Err(WitnessOwnerError::Native(_))));
	assert!(h.outgoing().is_empty());
	assert_eq!(h.owner.retained_manifests(&h.context).unwrap()[0].1, *provision.manifest());
	// This is a historical storage promise. It never restores the removed channel's authority.
	assert_eq!(h.owner.acknowledge(&response), Ok(AcknowledgementProgress::AwaitingPersistence));
	h.persist_manager();
	assert_eq!(h.owner.acknowledge(&response), Ok(AcknowledgementProgress::Retained));
	assert!(h.node.channel_manager.list_channels().is_empty());
}

#[test]
fn ffor_witness_owner_real_sidecar_failure_precedes_native_registration() {
	for stage in 1..=3 {
		for failure in [1, 2] {
			let mut h = Harness::new();
			h.persist_manager();
			let w = h.policies[0].witness;
			h.connect(w);
			h.storage
				.fail_at
				.store(h.storage.writes.load(Ordering::SeqCst) + stage, Ordering::SeqCst);
			h.storage.write_failure.store(failure, Ordering::SeqCst);
			assert_eq!(
				h.owner.register(&h.context, &h.policies),
				Err(WitnessOwnerError::Storage(WitnessStoreError::Storage))
			);
			assert!(h
				.node
				.channel_manager
				.ffor_receiver_witness_registration(&h.context)
				.unwrap()
				.is_none());
			assert!(h.outgoing().is_empty());
			assert_eq!(h.owner.provision(&h.context, w), Err(WitnessOwnerError::Unregistered));
			h.owner.recover_storage().unwrap();
			assert_eq!(
				h.owner.register(&h.context, &h.policies),
				Ok(RegistrationProgress::AwaitingPersistence)
			);
			h.persist_manager();
			assert_eq!(h.owner.provision(&h.context, w), Ok(ProvisioningProgress::Queued));
			assert_eq!(h.outgoing().len(), 1);
		}
	}
}

#[test]
fn ffor_witness_owner_real_failed_and_older_native_writes_do_not_release_registration() {
	let mut h = Harness::new();
	h.persist_manager();
	let older = h.node.channel_manager.capture_ffor_persistence();
	assert_eq!(
		h.owner.register(&h.context, &h.policies),
		Ok(RegistrationProgress::AwaitingPersistence)
	);
	let w = h.policies[0].witness;
	h.connect(w);
	let cancelled = h.node.channel_manager.capture_ffor_persistence();
	h.storage.write_failure.store(1, Ordering::SeqCst);
	assert!(KVStoreSync::write(
		&*h.storage,
		CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
		CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
		CHANNEL_MANAGER_PERSISTENCE_KEY,
		h.node.channel_manager.encode()
	)
	.is_err());
	drop(cancelled);
	assert_eq!(h.owner.provision(&h.context, w), Ok(ProvisioningProgress::AwaitingPersistence));
	// Persisting bytes after a captured older revision still cannot certify the new registration.
	KVStoreSync::write(
		&*h.storage,
		CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
		CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
		CHANNEL_MANAGER_PERSISTENCE_KEY,
		h.node.channel_manager.encode(),
	)
	.unwrap();
	h.node.channel_manager.ffor_persistence_completed(older).unwrap();
	assert_eq!(h.owner.provision(&h.context, w), Ok(ProvisioningProgress::AwaitingPersistence));
	assert!(h.outgoing().is_empty());
	h.persist_manager();
	assert_eq!(h.owner.provision(&h.context, w), Ok(ProvisioningProgress::Queued));
	assert_eq!(h.outgoing().len(), 1);
}

#[test]
fn ffor_witness_owner_real_fetch_admission_counts_pending_provisions() {
	let mut h = Harness::new();
	h.register_and_persist();
	let w = h.policies[0].witness;
	h.connect(w);
	let manifest = h.owner.retained_manifests(&h.context).unwrap().remove(0).1;
	let source =
		WitnessConnection { node_id: w, identity: h.owner.transport.connection(w).unwrap() };
	// Inject correlation-only pressure, not fake channel authority. None of these synthetic
	// lookup epochs is registered or released. The final attempted fetch uses the genuine epoch.
	let mut ids = Vec::new();
	for index in 0..64 {
		let epoch = pending::Epoch {
			channel: h.context.channel_id(),
			epoch: [index; 32],
			context_digest: h.context.context_digest(),
		};
		let request = h.owner.pending.stage(epoch, source.clone(), manifest.clone()).unwrap();
		ids.push(request.provision().request_id());
	}
	let before = h.owner.pending.usage();
	assert_eq!(h.owner.fetch(&h.context, w), Err(WitnessOwnerError::Capacity));
	assert_eq!(h.owner.pending.usage(), before);
	assert_eq!(h.owner.fetches.usage(), (0, 0));
	assert!(ids.iter().all(|id| h.owner.pending.contains_request_id(*id)));
	assert!(h.handler.get_and_clear_pending_msg().is_empty());
	h.owner.pending.complete(ids[0]);
	assert_eq!(h.owner.fetch(&h.context, w), Ok(fetch::FetchProgress::Queued));
	assert_eq!(h.owner.pending.usage().0 + h.owner.fetches.usage().0, 64);
	let messages = h.handler.get_and_clear_pending_msg();
	assert_eq!(messages.len(), 1);
	assert!(
		matches!(&messages[0].1, NodeCustomMessage::Ffor(frame) if frame.wire()[..2] == 55059u16.to_be_bytes())
	);
}

mod native_ack;

use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
use lightning::ln::msgs::Init;
use lightning::ln::peer_handler::CustomMessageHandler;
use lightning::ln::wire::CustomMessageReader;
use lightning_ffor::setup::AuthenticatedSetup;
use lightning_ffor::wire::Message;
use lightning_ffor::witness::AcknowledgementResult;
use lightning_types::features::InitFeatures;

use super::*;
use crate::message_handler::NodeCustomMessageHandler;

fn public(byte: u8) -> PublicKey {
	PublicKey::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[byte; 32]).unwrap())
}

fn bytes(value: &str) -> Vec<u8> {
	(0..value.len()).step_by(2).map(|i| u8::from_str_radix(&value[i..i + 2], 16).unwrap()).collect()
}

fn manifest(index: usize) -> SignedManifest {
	let fixture = include_str!("../../witness_store/record/key_use/fixtures.txt")
		.trim()
		.split("\n\n")
		.nth(index)
		.unwrap();
	let field = |name: &str| {
		bytes(fixture.lines().find_map(|line| line.strip_prefix(&format!("{name}="))).unwrap())
	};
	let receiver = PublicKey::from_slice(&bytes(
		"039fca7f8157aa768708894ffd92550fe970edd18526a5f936583ea3b54dab3228",
	))
	.unwrap();
	let settlement = PublicKey::from_slice(&bytes(
		"02087b7d1b4789170f6e374f0a0e58a1b7a899e34929795314ab6964e69609e9c0",
	))
	.unwrap();
	let setup = AuthenticatedSetup::new(
		&Message::decode(&field("init")).unwrap(),
		&Message::decode(&field("accept")).unwrap(),
		receiver,
		settlement,
	)
	.unwrap();
	SignedManifest::decode(&field("manifest"), &setup).unwrap()
}

fn epoch(value: u8) -> Epoch {
	Epoch { channel: ChannelId([1; 32]), epoch: [value; 32], context_digest: [value; 32] }
}

struct Harness {
	transport: Arc<FforReceiverTransport>,
	handler: NodeCustomMessageHandler,
}

impl Harness {
	fn new() -> Self {
		let transport = Arc::new(FforReceiverTransport::default());
		let handler =
			NodeCustomMessageHandler::new_ignoring().with_ffor_receiver(Arc::clone(&transport));
		Self { transport, handler }
	}
	fn connect(&self, peer: PublicKey) -> WitnessConnection<ConnectionToken> {
		let init =
			Init { features: InitFeatures::empty(), networks: None, remote_network_address: None };
		self.handler.peer_connected(peer, &init, false).unwrap();
		WitnessConnection { node_id: peer, identity: self.transport.connection(peer).unwrap() }
	}
	fn receive(&self, peer: PublicKey, ack: &Acknowledgement) -> ReceivedFforMessage {
		let wire = ack.encode();
		let message = self
			.handler
			.read(u16::from_be_bytes([wire[0], wire[1]]), &mut &wire[2..])
			.unwrap()
			.unwrap();
		self.handler.handle_custom_message(message, peer).unwrap();
		self.transport.pop().unwrap()
	}
}

fn accepted(request: &PendingProvision<ConnectionToken>) -> Acknowledgement {
	Acknowledgement::new(
		request.provision().request_id(),
		AcknowledgementResult::Accepted {
			witness: request.connection().node_id,
			retention_until: request.provision().manifest().unsigned().parameters().retention_until,
		},
	)
	.unwrap()
}

#[test]
fn ffor_witness_owner_shared_work_budget_preserves_existing_requests() {
	let harness = Harness::new();
	let source = harness.connect(public(43));
	let mut pending = PendingProvisions::default();
	let existing = pending.stage(epoch(1), source.clone(), manifest(0)).unwrap();
	let usage = pending.usage();
	pending.set_other_usage(MAX_PENDING - 1, MAX_PENDING_BYTES - usage.1).unwrap();
	assert!(matches!(
		pending.stage(epoch(2), source.clone(), manifest(0)),
		Err(WitnessOwnerError::Capacity)
	));
	assert_eq!(pending.usage(), usage);
	assert!(Arc::ptr_eq(&existing, &pending.stage(epoch(1), source.clone(), manifest(0)).unwrap()));
	assert_eq!(pending.set_other_usage(MAX_PENDING, 0), Err(WitnessOwnerError::Capacity));
	assert_eq!(pending.usage(), usage);
	assert!(pending.contains_request_id(existing.provision().request_id()));
	// Excluding every new ID simulates a failed freshness allocation without replacing correlation.
	assert!(matches!(
		pending.restart_excluding(epoch(1), source, manifest(0), |_| true),
		Err(WitnessOwnerError::Entropy)
	));
	assert!(pending.contains_request_id(existing.provision().request_id()));
}

#[test]
fn ffor_witness_owner_pending_precedes_queue_and_requires_successful_enqueue() {
	let harness = Harness::new();
	let source = harness.connect(public(43));
	let mut pending = PendingProvisions::default();
	let request = pending.stage(epoch(1), source.clone(), manifest(0)).unwrap();
	let ack = accepted(&request);
	let message = harness.receive(public(43), &ack);
	assert_eq!(pending.check(&message, &ack), Err(WitnessOwnerError::UnknownRequest));
	assert_eq!(pending.entries.len(), 1);
	harness.transport.enqueue_provision(public(43), &source.identity, request.provision()).unwrap();
	pending.mark_queued(&request);
	let (found, checked) = pending.check(&message, &ack).unwrap();
	assert_eq!(found, epoch(1));
	assert_eq!(checked.provision(), request.provision());
	// Checking does not remove work: a subsequent storage failure must retain correlation.
	assert!(pending.check(&message, &ack).is_ok());
	pending.complete(ack.request_id());
	assert!(pending.entries.is_empty());
	assert_eq!(pending.encoded_bytes, 0);
}

#[test]
fn ffor_witness_owner_retry_preserves_exact_request_and_reconnect_replaces_it() {
	let harness = Harness::new();
	let first = harness.connect(public(43));
	let mut pending = PendingProvisions::default();
	let request = pending.stage_with(epoch(1), first.clone(), manifest(0), || Ok([1; 16])).unwrap();
	let retry = pending
		.stage_with(epoch(1), first.clone(), manifest(0), || panic!("retry drew entropy"))
		.unwrap();
	assert!(Arc::ptr_eq(&request, &retry));
	assert_eq!(pending.stage(epoch(1), first, manifest(1)), Err(WitnessOwnerError::Conflict));
	let second = harness.connect(public(43));
	pending.discard_disconnected(&harness.transport);
	assert_eq!(pending.encoded_bytes, 0);
	let replacement = pending.stage_with(epoch(1), second, manifest(0), || Ok([2; 16])).unwrap();
	assert_ne!(request.connection(), replacement.connection());
	assert_ne!(request.provision().request_id(), replacement.provision().request_id());
	assert_eq!(request.provision().manifest(), replacement.provision().manifest());
	harness.handler.peer_disconnected(public(43));
	pending.discard_disconnected(&harness.transport);
	assert!(pending.entries.is_empty());
}

#[test]
fn ffor_witness_owner_rejects_wrong_source_request_retention_and_claimed_identity() {
	let harness = Harness::new();
	let source = harness.connect(public(43));
	harness.connect(public(44));
	let mut pending = PendingProvisions::default();
	let request = pending.stage(epoch(1), source, manifest(0)).unwrap();
	pending.mark_queued(&request);
	let ack = accepted(&request);
	let wrong_peer = harness.receive(public(44), &ack);
	assert!(pending.check(&wrong_peer, &ack).is_err());
	for result in [
		AcknowledgementResult::Accepted { witness: public(44), retention_until: u32::MAX },
		AcknowledgementResult::Accepted { witness: public(43), retention_until: 0 },
		AcknowledgementResult::Refused(b"unavailable".to_vec()),
	] {
		let bad = Acknowledgement::new(ack.request_id(), result).unwrap();
		let message = harness.receive(public(43), &bad);
		assert!(pending.check(&message, &bad).is_err());
	}
	let wrong_id = Acknowledgement::new([7; 16], ack.result().clone()).unwrap();
	assert_eq!(
		pending.check(&harness.receive(public(43), &wrong_id), &wrong_id),
		Err(WitnessOwnerError::UnknownRequest)
	);
	harness.connect(public(43));
	let new_connection = harness.receive(public(43), &ack);
	assert_eq!(
		pending.check(&new_connection, &ack),
		Err(WitnessOwnerError::Protocol(lightning_ffor::witness::WitnessError::Connection))
	);
	assert_eq!(pending.entries.len(), 1);
}

#[test]
fn ffor_witness_owner_pending_capacity_never_evicts_or_changes_existing_work() {
	let harness = Harness::new();
	let source = harness.connect(public(43));
	let mut pending = PendingProvisions::default();
	for index in 0..MAX_PENDING {
		pending
			.stage_with(epoch(index as u8), source.clone(), manifest(0), || Ok([index as u8; 16]))
			.unwrap();
	}
	let initial = pending.entries[0].request.clone();
	let bytes = pending.encoded_bytes;
	assert_eq!(
		pending.stage(epoch(80), source.clone(), manifest(0)),
		Err(WitnessOwnerError::Capacity)
	);
	assert_eq!(pending.entries.len(), MAX_PENDING);
	assert_eq!(pending.encoded_bytes, bytes);
	assert!(Arc::ptr_eq(&initial, &pending.stage(epoch(0), source, manifest(0)).unwrap()));
	let mut wrong = epoch(0);
	wrong.context_digest = [99; 32];
	assert_eq!(
		pending.stage(wrong, initial.connection().clone(), manifest(0)),
		Err(WitnessOwnerError::Conflict)
	);
	assert_eq!(pending.encoded_bytes, bytes);
}

#[test]
fn ffor_witness_owner_entropy_failure_and_collisions_preserve_pending_work() {
	let harness = Harness::new();
	let source = harness.connect(public(43));
	let mut pending = PendingProvisions::default();
	pending.stage_with(epoch(1), source.clone(), manifest(0), || Ok([1; 16])).unwrap();
	assert_eq!(
		pending
			.stage_with(epoch(2), source.clone(), manifest(0), || Err(WitnessOwnerError::Entropy)),
		Err(WitnessOwnerError::Entropy)
	);
	let mut draws = 0;
	assert_eq!(
		pending.stage_with(epoch(2), source, manifest(0), || {
			draws += 1;
			Ok([1; 16])
		}),
		Err(WitnessOwnerError::Entropy)
	);
	assert_eq!(draws, 16);
	assert_eq!(pending.entries.len(), 1);
}

#[test]
fn ffor_witness_owner_accepted_queue_waits_after_drain_until_explicit_fresh_retry() {
	let harness = Harness::new();
	let source = harness.connect(public(43));
	let mut pending = PendingProvisions::default();
	let request = pending.stage(epoch(1), source.clone(), manifest(0)).unwrap();
	harness.transport.enqueue_provision(public(43), &source.identity, request.provision()).unwrap();
	pending.mark_queued(&request);
	assert_eq!(harness.handler.get_and_clear_pending_msg().len(), 1);
	let unchanged = pending.stage(epoch(1), source.clone(), manifest(0)).unwrap();
	assert!(pending.is_queued(&unchanged));
	assert!(Arc::ptr_eq(&request, &unchanged));
	assert!(harness.handler.get_and_clear_pending_msg().is_empty());
	let replacement = pending.restart(epoch(1), source, manifest(0)).unwrap();
	assert!(!pending.is_queued(&replacement));
	assert_ne!(replacement.provision().request_id(), request.provision().request_id());
	assert_eq!(replacement.provision().manifest(), request.provision().manifest());
	assert_eq!(pending.entries.len(), 1);
}

#[test]
fn ffor_witness_owner_connection_sweep_preserves_replacement_pending_work() {
	let harness = Harness::new();
	let old = harness.connect(public(43));
	let mut pending = PendingProvisions::default();
	let request = pending.stage(epoch(1), old, manifest(0)).unwrap();
	pending.mark_queued(&request);
	let current = harness.connect(public(43));
	let replacement = pending.stage(epoch(1), current, manifest(0)).unwrap();
	// A delayed observer of the old disconnect consults current tokens rather than removing by W.
	pending.discard_disconnected(&harness.transport);
	assert_eq!(pending.entries.len(), 1);
	assert!(Arc::ptr_eq(&pending.entries[0].request, &replacement));
	assert!(!pending.is_queued(&replacement));
	let index = pending
		.existing(epoch(1), replacement.connection(), replacement.provision().manifest())
		.unwrap();
	assert_eq!(
		pending.install_new(
			epoch(1),
			replacement.connection().clone(),
			replacement.provision().manifest().clone(),
			index,
			|| Err(WitnessOwnerError::Entropy)
		),
		Err(WitnessOwnerError::Entropy)
	);
	assert!(Arc::ptr_eq(&pending.entries[0].request, &replacement));
}

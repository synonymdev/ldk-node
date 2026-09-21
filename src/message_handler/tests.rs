use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use bitcoin::hex::FromHex;
use bitcoin::secp256k1::{Secp256k1, SecretKey};
use lightning::ln::msgs::ErrorAction;
use lightning::util::logger::Level;
use lightning::util::ser::LengthReadable;
use lightning_ffor::setup::AuthenticatedSetup;
use lightning_ffor::wire::{Message as FforMessage, Tlv};
use lightning_ffor::witness::{
	Acknowledgement, AcknowledgementResult, EncryptedRecord, FetchResponse, FetchResult, Provision,
	SignedFetch, SignedManifest,
};
use lightning_liquidity::lsps0::ser::LSPS_MESSAGE_TYPE_ID;
use proptest::prelude::*;

use super::*;

#[derive(Default)]
struct LspsHandler {
	received: Mutex<Vec<(PublicKey, RawLSPSMessage)>>,
	pending: Mutex<Vec<(PublicKey, RawLSPSMessage)>>,
	connected: Mutex<Vec<(PublicKey, Init, bool)>>,
	disconnected: Mutex<Vec<PublicKey>>,
	fail_connect: AtomicBool,
	fail_message: AtomicBool,
}

impl CustomMessageReader for LspsHandler {
	type CustomMessage = RawLSPSMessage;
	fn read<R: LengthLimitedRead>(
		&self, message_type: u16, reader: &mut R,
	) -> Result<Option<RawLSPSMessage>, DecodeError> {
		if message_type == LSPS_MESSAGE_TYPE_ID {
			RawLSPSMessage::read_from_fixed_length_buffer(reader).map(Some)
		} else {
			Ok(None)
		}
	}
}

impl CustomMessageHandler for LspsHandler {
	fn handle_custom_message(
		&self, message: RawLSPSMessage, peer: PublicKey,
	) -> Result<(), LightningError> {
		if self.fail_message.load(Ordering::Relaxed) {
			return Err(LightningError {
				err: "LSPS test failure".into(),
				action: ErrorAction::IgnoreAndLog(Level::Trace),
			});
		}
		self.received.lock().unwrap().push((peer, message));
		Ok(())
	}
	fn get_and_clear_pending_msg(&self) -> Vec<(PublicKey, RawLSPSMessage)> {
		std::mem::take(&mut *self.pending.lock().unwrap())
	}
	fn peer_connected(&self, peer: PublicKey, init: &Init, inbound: bool) -> Result<(), ()> {
		if self.fail_connect.load(Ordering::Relaxed) {
			return Err(());
		}
		self.connected.lock().unwrap().push((peer, init.clone(), inbound));
		Ok(())
	}
	fn peer_disconnected(&self, peer: PublicKey) {
		self.disconnected.lock().unwrap().push(peer);
	}
	fn provided_node_features(&self) -> NodeFeatures {
		NodeFeatures::from_le_bytes(vec![2])
	}
	fn provided_init_features(&self, _peer: PublicKey) -> InitFeatures {
		InitFeatures::from_le_bytes(vec![2])
	}
}

fn key(seed: u8) -> PublicKey {
	PublicKey::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[seed; 32]).unwrap())
}

fn init() -> Init {
	Init { features: InitFeatures::empty(), networks: None, remote_network_address: None }
}

fn ack_wire(claimed_peer: PublicKey) -> Vec<u8> {
	Acknowledgement::new(
		[7; 16],
		AcknowledgementResult::Accepted { witness: claimed_peer, retention_until: 800000 },
	)
	.unwrap()
	.encode()
}

fn parse(handler: &NodeCustomMessageHandler<Arc<LspsHandler>>, wire: &[u8]) -> NodeCustomMessage {
	handler.read(u16::from_be_bytes([wire[0], wire[1]]), &mut &wire[2..]).unwrap().unwrap()
}

// Public Beignet D.1 vectors from native commit 8478895. The retained manifest/record projection
// has provenance beside fixtures.txt; the odd fetch is from lightning-ffor's
// tests/data/beignet-witness-fetch.json, Beignet revision 8aee31d18e596fe49a0d195b325a6e757d7a009b.
pub(super) fn witness_messages() -> (Provision, SignedFetch, FetchResponse) {
	let fixture = include_str!("../ffor/witness_store/record/key_use/fixtures.txt")
		.split("\n\n")
		.next()
		.unwrap();
	let bytes = |name: &str| {
		let value = fixture
			.lines()
			.find_map(|line| {
				let (key, value) = line.split_once('=')?;
				(key == name).then_some(value)
			})
			.unwrap();
		Vec::<u8>::from_hex(value).unwrap()
	};
	let receiver = PublicKey::from_slice(
		&Vec::<u8>::from_hex("039fca7f8157aa768708894ffd92550fe970edd18526a5f936583ea3b54dab3228")
			.unwrap(),
	)
	.unwrap();
	let settlement = PublicKey::from_slice(
		&Vec::<u8>::from_hex("02087b7d1b4789170f6e374f0a0e58a1b7a899e34929795314ab6964e69609e9c0")
			.unwrap(),
	)
	.unwrap();
	let setup = AuthenticatedSetup::new(
		&FforMessage::decode(&bytes("init")).unwrap(),
		&FforMessage::decode(&bytes("accept")).unwrap(),
		receiver,
		settlement,
	)
	.unwrap();
	let manifest = SignedManifest::decode(&bytes("manifest"), &setup).unwrap();
	let provision = Provision::new([1; 16], manifest);
	let fetch_wire = Vec::<u8>::from_hex(concat!(
		"d7131f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f",
		"0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b",
		"3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d",
		"b3ec3d984d49b4f071e2f3ba5265915ff08529dab4aa61f347cc9d9439683f6e",
		"3cef7f8b4483445adabfa4633635ee8a9c324a60cff27a3226c67c98bb2be36c030107",
	))
	.unwrap();
	let fetch_key = provision.manifest().unsigned().parameters().fetch_public_key;
	let fetch = SignedFetch::decode(&fetch_wire, fetch_key).unwrap();
	assert_eq!(fetch.encode(), fetch_wire);
	let response = FetchResponse::new(
		fetch.unsigned().parameters().request_id,
		FetchResult::Page {
			records: vec![EncryptedRecord::decode(&bytes("record")).unwrap()],
			next_after_slot: None,
			extensions: vec![],
		},
	)
	.unwrap();
	(provision, fetch, response)
}

#[test]
fn ffor_witness_typed_requests_keep_exact_bytes_and_do_not_open_a_service() {
	let receiver = Arc::new(FforReceiverTransport::default());
	let handler = NodeCustomMessageHandler::<Arc<LspsHandler>>::new_ignoring()
		.with_ffor_receiver(Arc::clone(&receiver));
	let peer = key(1);
	handler.peer_connected(peer, &init(), false).unwrap();
	let connection = receiver.connection(peer).unwrap();
	let (provision, fetch, _) = witness_messages();
	// The authenticated Noise peer is deliberately different from the retained fetch key.
	assert_ne!(peer, fetch.fetch_key());
	receiver.enqueue_provision(peer, &connection, &provision).unwrap();
	receiver.enqueue_fetch(peer, &connection, &fetch).unwrap();
	for wire in [provision.encode(), fetch.encode()] {
		assert_eq!(
			receiver.enqueue(peer, &connection, &wire),
			Err(ffor::OutboundError::InvalidMessage)
		);
		let mut body = &wire[2..];
		assert!(handler.read(u16::from_be_bytes([wire[0], wire[1]]), &mut body).unwrap().is_none());
		assert_eq!(body, &wire[2..]);
	}
	let outbound = handler.get_and_clear_pending_msg();
	for ((actual_peer, message), wire) in outbound.iter().zip([provision.encode(), fetch.encode()])
	{
		assert_eq!(*actual_peer, peer);
		assert_eq!(message.type_id().to_be_bytes(), wire[..2]);
		assert_eq!(message.encode(), wire[2..]);
		assert_eq!(
			format!("{message:?}"),
			format!(
				"Ffor(FforFrame {{ message_type: {}, length: {} }})",
				message.type_id(),
				wire.len()
			)
		);
	}
	assert_eq!(outbound.len(), 2);
	assert!(receiver.pop().is_none());
}

#[test]
fn ffor_witness_fetch_response_is_bounded_opaque_input_with_exact_extensions() {
	let receiver = Arc::new(FforReceiverTransport::default());
	let handler = NodeCustomMessageHandler::<Arc<LspsHandler>>::new_ignoring()
		.with_ffor_receiver(Arc::clone(&receiver));
	let peer = key(1);
	handler.peer_connected(peer, &init(), false).unwrap();
	let (_, _, response) = witness_messages();
	let mut wire = response.encode();
	wire.extend_from_slice(&[3, 1, 7]); // Canonical unknown odd field is preserved, not interpreted.
	let decoded = FetchResponse::decode(&wire).unwrap();
	assert!(
		matches!(decoded.result(), FetchResult::Page { extensions, .. } if extensions == &[Tlv { kind: 3, value: vec![7] }])
	);
	let message = parse(&handler, &wire);
	assert_eq!(message.type_id(), 55061);
	assert_eq!(message.encode(), wire[2..]);
	assert_eq!(
		format!("{message:?}"),
		format!("Ffor(FforFrame {{ message_type: 55061, length: {} }})", wire.len())
	);
	handler.handle_custom_message(message, peer).unwrap();
	let queued = receiver.pop().unwrap();
	assert_eq!(queued.peer(), peer);
	assert_eq!(queued.frame().wire(), wire);
	assert!(receiver.is_current(&queued));
	assert!(handler.get_and_clear_pending_msg().is_empty());
	let disabled = NodeCustomMessageHandler::<Arc<LspsHandler>>::new_ignoring();
	assert!(disabled.read(55061, &mut &wire[2..]).unwrap().is_none());
}

#[test]
fn ffor_witness_fetch_response_rejects_malformed_counts_lengths_and_tlvs() {
	let handler = NodeCustomMessageHandler::<Arc<LspsHandler>>::new_ignoring()
		.with_ffor_receiver(Arc::new(FforReceiverTransport::default()));
	let (_, _, response) = witness_messages();
	let wire = response.encode();
	for end in 2..wire.len() {
		assert!(handler.read(55061, &mut &wire[2..end]).is_err(), "truncated at {end}");
	}
	let mut mutations = Vec::new();
	let mut invalid_status = wire.clone();
	invalid_status[18] = 2;
	mutations.push(invalid_status);
	let mut excessive_count = wire.clone();
	excessive_count[19..21].copy_from_slice(&484u16.to_be_bytes());
	mutations.push(excessive_count);
	let mut invalid_length = wire.clone();
	invalid_length[21..23].copy_from_slice(&u16::MAX.to_be_bytes());
	mutations.push(invalid_length);
	for suffix in [&[2, 0][..], &[3, 0, 3, 0][..], &[0xfd, 0, 3, 0][..], &[1, 1, 0][..]] {
		let mut changed = wire.clone();
		changed.extend_from_slice(suffix);
		mutations.push(changed);
	}
	for changed in mutations {
		assert!(handler.read(55061, &mut &changed[2..]).is_err());
	}
	assert!(handler.read(55061, &mut &vec![0; 65534][..]).is_err());
	let refused = FetchResponse::new([3; 16], FetchResult::Refused(vec![0; 65514])).unwrap();
	assert_eq!(parse(&handler, &refused.encode()).encode(), refused.encode()[2..]);
}

#[test]
fn ffor_composes_lsps_read_outbox_features_and_callbacks() {
	let lsps = Arc::new(LspsHandler::default());
	let receiver = Arc::new(FforReceiverTransport::default());
	let handler = NodeCustomMessageHandler::new_liquidity_handler(Arc::clone(&lsps))
		.with_ffor_receiver(Arc::clone(&receiver));
	let peer = key(1);
	handler.peer_connected(peer, &init(), true).unwrap();
	assert_eq!(*lsps.connected.lock().unwrap(), vec![(peer, init(), true)]);
	assert_eq!(handler.provided_init_features(peer), lsps.provided_init_features(peer));
	assert_eq!(handler.provided_node_features(), lsps.provided_node_features());

	let raw = RawLSPSMessage { payload: "{\"jsonrpc\":\"2.0\",\"id\":\"42\"}".into() };
	let parsed = handler.read(LSPS_MESSAGE_TYPE_ID, &mut raw.encode().as_slice()).unwrap().unwrap();
	assert_eq!(parsed.type_id(), LSPS_MESSAGE_TYPE_ID);
	assert_eq!(parsed.encode(), raw.encode());
	handler.handle_custom_message(parsed, peer).unwrap();
	assert_eq!(*lsps.received.lock().unwrap(), vec![(peer, raw.clone())]);
	lsps.pending.lock().unwrap().push((peer, raw.clone()));
	handler.handle_custom_message(parse(&handler, &ack_wire(key(2))), peer).unwrap();
	assert_eq!(
		handler.get_and_clear_pending_msg(),
		vec![(peer, NodeCustomMessage::Liquidity(raw))]
	);
	assert!(handler.get_and_clear_pending_msg().is_empty());
	let incoming = receiver.pop().unwrap();
	assert_eq!(incoming.peer(), peer); // Claimed witness key(2) cannot replace transport key(1).
	assert!(receiver.is_current(&incoming));
	handler.peer_disconnected(peer);
	assert!(!receiver.is_current(&incoming));
	assert_eq!(*lsps.disconnected.lock().unwrap(), vec![peer]);
}

#[test]
fn ffor_outbound_merges_with_lsps_without_consuming_inbound_or_reordering() {
	let lsps = Arc::new(LspsHandler::default());
	let receiver = Arc::new(FforReceiverTransport::default());
	let handler = NodeCustomMessageHandler::new_liquidity_handler(Arc::clone(&lsps))
		.with_ffor_receiver(Arc::clone(&receiver));
	let peer = key(1);
	handler.peer_connected(peer, &init(), false).unwrap();
	let connection = receiver.connection(peer).unwrap();
	let fixtures: serde_json::Value = serde_json::from_str(include_str!("test_data.json")).unwrap();
	let wires = fixtures["messages"]
		.as_array()
		.unwrap()
		.iter()
		.take(2)
		.map(|fixture| Vec::<u8>::from_hex(fixture["wire"].as_str().unwrap()).unwrap())
		.collect::<Vec<_>>();
	for wire in &wires {
		receiver.enqueue(peer, &connection, wire).unwrap();
	}
	let requests =
		[RawLSPSMessage { payload: "first".into() }, RawLSPSMessage { payload: "second".into() }];
	lsps.pending.lock().unwrap().extend(requests.iter().cloned().map(|message| (peer, message)));
	handler.handle_custom_message(parse(&handler, &ack_wire(peer)), peer).unwrap();
	let messages = handler.get_and_clear_pending_msg();
	assert_eq!(messages.len(), 4);
	for (actual, expected) in messages[..2].iter().zip(requests) {
		assert_eq!(*actual, (peer, NodeCustomMessage::Liquidity(expected)));
	}
	for ((actual_peer, message), wire) in messages[2..].iter().zip(wires) {
		assert_eq!(*actual_peer, peer);
		assert_eq!(message.type_id().to_be_bytes(), wire[..2]);
		assert_eq!(message.encode(), wire[2..]);
		assert!(matches!(message, NodeCustomMessage::Ffor(_)));
	}
	assert!(handler.get_and_clear_pending_msg().is_empty());
	assert_eq!(receiver.pop().unwrap().frame().wire(), ack_wire(peer));
	assert_eq!(handler.provided_node_features(), lsps.provided_node_features());
	assert_eq!(handler.provided_init_features(peer), lsps.provided_init_features(peer));
}

#[test]
fn ffor_disabled_has_no_read_features_queue_or_connection_effects() {
	let handler = NodeCustomMessageHandler::<Arc<LspsHandler>>::new_ignoring();
	let peer = key(1);
	let wire = ack_wire(peer);
	let mut body = &wire[2..];
	assert!(handler.read(55057, &mut body).unwrap().is_none());
	assert_eq!(body, &wire[2..]);
	assert!(handler.read(LSPS_MESSAGE_TYPE_ID, &mut &b"{}"[..]).unwrap().is_none());
	assert_eq!(handler.provided_init_features(peer), InitFeatures::empty());
	assert_eq!(handler.provided_node_features(), NodeFeatures::empty());
	handler.peer_connected(peer, &init(), false).unwrap();
	handler.peer_disconnected(peer);
	assert!(handler.get_and_clear_pending_msg().is_empty());
	let lsps = Arc::new(LspsHandler::default());
	let liquidity_only = NodeCustomMessageHandler::new_liquidity_handler(lsps);
	assert!(liquidity_only.read(55057, &mut &wire[2..]).unwrap().is_none());
}

#[test]
fn ffor_binding_waits_for_lsps_success_and_preserves_errors() {
	let lsps = Arc::new(LspsHandler::default());
	let receiver = Arc::new(FforReceiverTransport::default());
	let handler = NodeCustomMessageHandler::new_liquidity_handler(Arc::clone(&lsps))
		.with_ffor_receiver(Arc::clone(&receiver));
	let peer = key(1);
	lsps.fail_connect.store(true, Ordering::Relaxed);
	assert_eq!(handler.peer_connected(peer, &init(), false), Err(()));
	let error = handler.handle_custom_message(parse(&handler, &ack_wire(peer)), peer).unwrap_err();
	assert_eq!(error.action, ErrorAction::IgnoreAndLog(Level::Debug));
	assert!(receiver.pop().is_none());
	lsps.fail_connect.store(false, Ordering::Relaxed);
	handler.peer_connected(peer, &init(), false).unwrap();
	handler.handle_custom_message(parse(&handler, &ack_wire(peer)), peer).unwrap();
	let old = receiver.pop().unwrap();
	receiver.enqueue(peer, old.connection(), &ack_wire(peer)).unwrap();
	lsps.fail_connect.store(true, Ordering::Relaxed);
	assert_eq!(handler.peer_connected(peer, &init(), false), Err(()));
	assert!(!receiver.is_current(&old));
	assert!(handler.get_and_clear_pending_msg().is_empty());
	assert!(handler.handle_custom_message(parse(&handler, &ack_wire(peer)), peer).is_err());
	lsps.fail_connect.store(false, Ordering::Relaxed);
	handler.peer_connected(peer, &init(), false).unwrap();
	lsps.fail_message.store(true, Ordering::Relaxed);
	let error = handler
		.handle_custom_message(
			NodeCustomMessage::Liquidity(RawLSPSMessage { payload: "{}".into() }),
			peer,
		)
		.unwrap_err();
	assert_eq!(error.err, "LSPS test failure");
	assert_eq!(error.action, ErrorAction::IgnoreAndLog(Level::Trace));
}

#[test]
fn ffor_lifecycle_fixtures_keep_exact_type_and_signed_bytes() {
	let fixtures: serde_json::Value = serde_json::from_str(include_str!("test_data.json")).unwrap();
	let receiver = Arc::new(FforReceiverTransport::default());
	let handler = NodeCustomMessageHandler::<Arc<LspsHandler>>::new_ignoring()
		.with_ffor_receiver(Arc::clone(&receiver));
	let peer = key(1);
	handler.peer_connected(peer, &init(), false).unwrap();
	for fixture in fixtures["messages"].as_array().unwrap() {
		let wire = Vec::<u8>::from_hex(fixture["wire"].as_str().unwrap()).unwrap();
		let message = parse(&handler, &wire);
		assert_eq!(message.type_id().to_be_bytes(), wire[..2]);
		assert_eq!(message.encode(), wire[2..]);
		handler.handle_custom_message(message, peer).unwrap();
		let received = receiver.pop().unwrap();
		assert_eq!(received.frame().wire(), wire);
		assert_eq!(received.peer(), peer);
	}
	assert!(handler.get_and_clear_pending_msg().is_empty());
}

#[test]
fn ffor_debug_keeps_lifecycle_payloads_out_of_peer_trace_logs() {
	let fixtures: serde_json::Value = serde_json::from_str(include_str!("test_data.json")).unwrap();
	let handler = NodeCustomMessageHandler::<Arc<LspsHandler>>::new_ignoring()
		.with_ffor_receiver(Arc::new(FforReceiverTransport::default()));
	for fixture in fixtures["messages"].as_array().unwrap() {
		let wire = Vec::<u8>::from_hex(fixture["wire"].as_str().unwrap()).unwrap();
		let message = parse(&handler, &wire);
		assert_eq!(
			format!("{:?}", message),
			format!(
				"Ffor(FforFrame {{ message_type: {}, length: {} }})",
				message.type_id(),
				wire.len(),
			)
		);
	}
}

#[test]
fn ffor_reader_rejects_malformed_oversized_and_ignores_unknown_types() {
	let handler = NodeCustomMessageHandler::<Arc<LspsHandler>>::new_ignoring()
		.with_ffor_receiver(Arc::new(FforReceiverTransport::default()));
	for typ in [55001, 55003, 55045, 55047, 55049, 55051, 55053, 55057, 55061] {
		assert!(handler.read(typ, &mut &[][..]).is_err());
		let oversized = vec![0; 65534];
		let mut body = oversized.as_slice();
		assert!(handler.read(typ, &mut body).is_err());
		assert_eq!(body.len(), oversized.len()); // Refuse before allocation or reading.
	}
	for typ in [55055, 55059, 55101, 0] {
		assert!(handler.read(typ, &mut &[][..]).unwrap().is_none());
	}
	let mut wire = ack_wire(key(1));
	wire[18] = 2;
	assert!(handler.read(55057, &mut &wire[2..]).is_err());
}

proptest! {
	#![proptest_config(ProptestConfig::with_cases(64))]
	#[test]
	fn ffor_untrusted_frames_preserve_exact_bytes_or_fail(data in prop::collection::vec(any::<u8>(), 0..66000), typ in prop::sample::select(vec![55001u16,55003,55045,55047,55049,55051,55053,55057,55061])) {
		let handler = NodeCustomMessageHandler::<Arc<LspsHandler>>::new_ignoring().with_ffor_receiver(Arc::new(FforReceiverTransport::default()));
		if let Ok(Some(message)) = handler.read(typ, &mut data.as_slice()) {
			prop_assert_eq!(message.type_id(), typ);
			prop_assert_eq!(message.encode(), data);
		}
	}
}

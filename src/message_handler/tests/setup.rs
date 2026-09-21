//! These tests use actual Node managers and opaque native connections. Native funded-channel
//! tests separately cover synchronous Accept ownership before the immediately following HTLC.

use bitcoin::Network;
use lightning::ln::ffor::{FFORCommitmentError, FFORReceiverError, FFORReceiverParameters};
use lightning::ln::msgs::BaseMessageHandler;
use lightning::ln::types::ChannelId;
use lightning_ffor::wire::{Abort, Payload};

use super::*;
use crate::builder::NodeBuilder;
use crate::io::test_utils::InMemoryStore;
use crate::message_handler::ffor::setup::FforSetupAdapter;
use crate::types::DynStore;
use crate::{Config, Node};

fn node(seed: u8) -> Node {
	let mut builder =
		NodeBuilder::from_config(Config { network: Network::Regtest, ..Config::default() });
	builder.set_entropy_seed_bytes([seed; 64]);
	builder.set_log_facade_logger();
	let store: Arc<DynStore> = Arc::new(InMemoryStore::new());
	builder.build_with_store(store).unwrap()
}

fn native_init() -> Init {
	let mut init = init();
	init.features.set_static_remote_key_optional();
	init
}

fn setup_frame(name: &str) -> FforFrame {
	let fixture = include_str!("../../ffor/witness_store/record/key_use/fixtures.txt");
	let wire = Vec::<u8>::from_hex(
		fixture.lines().find_map(|line| line.strip_prefix(&format!("{}=", name))).unwrap(),
	)
	.unwrap();
	FforFrame::read(u16::from_be_bytes([wire[0], wire[1]]), &mut &wire[2..]).unwrap()
}

fn signed_abort(seed: u8) -> FforFrame {
	let mut message = FforMessage::decode(setup_frame("accept").wire()).unwrap();
	message.payload =
		Payload::Abort(Abort { transcript_hash: [0; 32], reason: 1, data: Vec::new() });
	let digest = bitcoin::secp256k1::Message::from_digest(message.signature_digest().unwrap());
	message.signature = Secp256k1::new()
		.sign_ecdsa_with_noncedata(&digest, &SecretKey::from_slice(&[seed; 32]).unwrap(), &[1; 32])
		.serialize_compact();
	message.verify_signature(&key(seed)).unwrap();
	parse_frame(&message.encode().unwrap())
}

fn adapter(node: &Node) -> (Arc<FforSetupAdapter>, Arc<FforReceiverTransport>) {
	let transport = Arc::new(FforReceiverTransport::default());
	let setup =
		Arc::new(FforSetupAdapter::new(Arc::clone(&node.channel_manager), Arc::clone(&transport)));
	(setup, transport)
}

fn unavailable() -> FFORReceiverError {
	FFORCommitmentError::ChannelUnavailable.into()
}

#[test]
fn ffor_setup_dispatch_is_synchronous_and_propagates_native_error() {
	let node = node(71);
	let peer = key(72);
	let (setup, transport) = adapter(&node);
	let handler = NodeCustomMessageHandler::<Arc<LspsHandler>>::new_ignoring()
		.with_ffor_setup(Arc::clone(&setup));
	node.channel_manager.peer_connected(peer, &native_init(), false).unwrap();
	handler.peer_connected(peer, &native_init(), false).unwrap();
	let native = node.channel_manager.ffor_peer_connection(&peer).unwrap();
	for frame in [setup_frame("accept"), signed_abort(72)] {
		let expected =
			node.channel_manager.handle_ffor_receiver_message(&native, frame.wire()).unwrap_err();
		assert_eq!(expected, unavailable());
		assert_eq!(setup.handle(peer, &frame), Err(expected));
		let error =
			handler.handle_custom_message(NodeCustomMessage::Ffor(frame), peer).unwrap_err();
		assert_eq!(error.err, format!("FFOR receiver setup: {}", expected));
		assert!(matches!(error.action, ErrorAction::DisconnectPeer { msg: None }));
		// A deferred mailbox handler would return Ok and leave the input here. Neither is
		// allowed before PeerManager considers delivering a subsequent ordinary frame.
		assert!(transport.pop().is_none());
	}
	assert!(handler.get_and_clear_pending_msg().is_empty());
}

#[test]
fn ffor_setup_rejects_foreign_and_reconnected_native_generations() {
	let first = node(73);
	let second = node(74);
	let peer = key(75);
	let (setup, transport) = adapter(&first);
	let handler = NodeCustomMessageHandler::<Arc<LspsHandler>>::new_ignoring()
		.with_ffor_setup(Arc::clone(&setup));
	first.channel_manager.peer_connected(peer, &native_init(), false).unwrap();
	second.channel_manager.peer_connected(peer, &native_init(), false).unwrap();
	handler.peer_connected(peer, &native_init(), false).unwrap();
	let original = first.channel_manager.ffor_peer_connection(&peer).unwrap();
	let foreign = second.channel_manager.ffor_peer_connection(&peer).unwrap();
	let frame = setup_frame("accept");
	assert_ne!(original, foreign);
	assert_eq!(
		first.channel_manager.handle_ffor_receiver_message(&foreign, frame.wire()),
		Err(unavailable())
	);
	let old_transport = transport.connection(peer).unwrap();
	first.channel_manager.peer_disconnected(peer);
	first.channel_manager.peer_connected(peer, &native_init(), false).unwrap();
	let current = first.channel_manager.ffor_peer_connection(&peer).unwrap();
	assert_ne!(original, current);
	// Deliberately leave the custom callback behind the native callback. Its retained native
	// generation cannot be upgraded by merely checking the peer's public key.
	assert_eq!(setup.handle(peer, &frame), Err(unavailable()));
	assert_eq!(
		first.channel_manager.handle_ffor_receiver_message(&original, frame.wire()),
		Err(unavailable())
	);
	handler.peer_connected(peer, &native_init(), false).unwrap();
	assert_ne!(old_transport, transport.connection(peer).unwrap());
	assert_eq!(
		transport.enqueue(peer, &old_transport, frame.wire()),
		Err(ffor::OutboundError::StaleConnection)
	);
	assert!(transport.pop().is_none());
}

#[test]
fn ffor_setup_lsps_failure_disconnect_and_missing_native_clear_pairs() {
	let node = node(76);
	let peer = key(77);
	let (setup, transport) = adapter(&node);
	let lsps = Arc::new(LspsHandler::default());
	let handler = NodeCustomMessageHandler::new_liquidity_handler(Arc::clone(&lsps))
		.with_ffor_setup(Arc::clone(&setup));
	// A custom callback alone cannot mint a native generation.
	handler.peer_connected(peer, &native_init(), false).unwrap();
	assert!(transport.connection(peer).is_none());
	node.channel_manager.peer_connected(peer, &native_init(), false).unwrap();
	handler.peer_connected(peer, &native_init(), false).unwrap();
	let old = transport.connection(peer).unwrap();
	transport.enqueue(peer, &old, &ack_wire(peer)).unwrap();
	handler.handle_custom_message(parse(&handler, &ack_wire(peer)), peer).unwrap();
	lsps.fail_connect.store(true, Ordering::Relaxed);
	assert_eq!(handler.peer_connected(peer, &native_init(), false), Err(()));
	assert!(transport.connection(peer).is_none());
	assert!(transport.pop().is_none());
	assert!(handler.get_and_clear_pending_msg().is_empty());
	assert_eq!(setup.handle(peer, &setup_frame("accept")), Err(unavailable()));
	assert_eq!(
		transport.enqueue(peer, &old, &ack_wire(peer)),
		Err(ffor::OutboundError::StaleConnection)
	);
	lsps.fail_connect.store(false, Ordering::Relaxed);
	handler.peer_connected(peer, &native_init(), false).unwrap();
	assert!(transport.connection(peer).is_some());
	handler.peer_disconnected(peer);
	assert!(transport.connection(peer).is_none());
	assert_eq!(setup.handle(peer, &setup_frame("accept")), Err(unavailable()));
	assert_eq!(*lsps.disconnected.lock().unwrap(), vec![peer]);
}

#[test]
fn ffor_setup_preserves_witness_unsupported_lifecycle_and_lsps_mailboxes() {
	let node = node(78);
	let peer = key(79);
	let (setup, transport) = adapter(&node);
	let lsps = Arc::new(LspsHandler::default());
	let handler =
		NodeCustomMessageHandler::new_liquidity_handler(Arc::clone(&lsps)).with_ffor_setup(setup);
	node.channel_manager.peer_connected(peer, &native_init(), false).unwrap();
	handler.peer_connected(peer, &native_init(), false).unwrap();
	let (_, _, fetch_response) = witness_messages();
	for frame in
		[setup_frame("init"), parse_frame(&ack_wire(peer)), parse_frame(&fetch_response.encode())]
	{
		let wire = frame.wire().to_vec();
		handler.handle_custom_message(NodeCustomMessage::Ffor(frame), peer).unwrap();
		let received = transport.pop().unwrap();
		assert_eq!(received.peer(), peer);
		assert_eq!(received.frame().wire(), wire);
	}
	let message = RawLSPSMessage { payload: "{\"id\":1}".into() };
	handler.handle_custom_message(NodeCustomMessage::Liquidity(message.clone()), peer).unwrap();
	assert_eq!(*lsps.received.lock().unwrap(), vec![(peer, message)]);
	assert_eq!(handler.provided_node_features(), lsps.provided_node_features());
	assert_eq!(handler.provided_init_features(peer), lsps.provided_init_features(peer));
}

fn parse_frame(wire: &[u8]) -> FforFrame {
	FforFrame::read(u16::from_be_bytes([wire[0], wire[1]]), &mut &wire[2..]).unwrap()
}

#[test]
fn ffor_setup_intent_delegation_preserves_native_refusal_and_no_request() {
	let node = node(80);
	let peer = key(81);
	let (setup, transport) = adapter(&node);
	let parameters = FFORReceiverParameters {
		local_request_id: [1; 32],
		amounts_msat: vec![100_000],
		minimum_payment_msat: 100_000,
		settlement_deadline: 100,
		voucher_expiry: 144,
		fee_base_msat: 0,
		fee_proportional_millionths: 0,
		claim_margin_blocks: 20,
		witness_peers: None,
		hash_chain: false,
	};
	assert_eq!(setup.prepare(peer, &ChannelId([2; 32]), parameters.clone()), Err(unavailable()));
	node.channel_manager.peer_connected(peer, &native_init(), false).unwrap();
	setup.peer_connected(peer);
	let connection = node.channel_manager.ffor_peer_connection(&peer).unwrap();
	let expected = node.channel_manager.prepare_ffor_receiver(
		&ChannelId([2; 32]),
		&connection,
		parameters.clone(),
	);
	assert_eq!(setup.prepare(peer, &ChannelId([2; 32]), parameters), expected);
	assert_eq!(setup.find([1; 32]), Ok(None));
	assert!(transport.pop().is_none());
	assert!(transport.drain_outbound().is_empty());
}

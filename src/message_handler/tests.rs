use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use bitcoin::hex::FromHex;
use bitcoin::secp256k1::{Secp256k1, SecretKey};
use lightning::ln::msgs::ErrorAction;
use lightning::util::logger::Level;
use lightning::util::ser::LengthReadable;
use lightning_ffor::witness::{Acknowledgement, AcknowledgementResult};
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
	lsps.fail_connect.store(true, Ordering::Relaxed);
	assert_eq!(handler.peer_connected(peer, &init(), false), Err(()));
	assert!(!receiver.is_current(&old));
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
	for typ in [55001, 55003, 55045, 55047, 55049, 55051, 55053, 55057] {
		assert!(handler.read(typ, &mut &[][..]).is_err());
		let oversized = vec![0; 65534];
		let mut body = oversized.as_slice();
		assert!(handler.read(typ, &mut body).is_err());
		assert_eq!(body.len(), oversized.len()); // Refuse before allocation or reading.
	}
	for typ in [55055, 55059, 55061, 55101, 0] {
		assert!(handler.read(typ, &mut &[][..]).unwrap().is_none());
	}
	let mut wire = ack_wire(key(1));
	wire[18] = 2;
	assert!(handler.read(55057, &mut &wire[2..]).is_err());
}

proptest! {
	#![proptest_config(ProptestConfig::with_cases(64))]
	#[test]
	fn ffor_untrusted_frames_preserve_exact_bytes_or_fail(data in prop::collection::vec(any::<u8>(), 0..66000), typ in prop::sample::select(vec![55001u16,55003,55045,55047,55049,55051,55053,55057])) {
		let handler = NodeCustomMessageHandler::<Arc<LspsHandler>>::new_ignoring().with_ffor_receiver(Arc::new(FforReceiverTransport::default()));
		if let Ok(Some(message)) = handler.read(typ, &mut data.as_slice()) {
			prop_assert_eq!(message.type_id(), typ);
			prop_assert_eq!(message.encode(), data);
		}
	}
}

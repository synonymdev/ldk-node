use bitcoin::secp256k1::{Secp256k1, SecretKey};
use lightning_ffor::witness::AcknowledgementResult;
use proptest::prelude::*;

use super::*;

fn key(seed: u8) -> PublicKey {
	PublicKey::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[seed; 32]).unwrap())
}

fn frame(bytes: usize) -> FforFrame {
	unique_frame(bytes, 1)
}

fn unique_frame(bytes: usize, tag: u8) -> FforFrame {
	let wire = Acknowledgement::new([tag; 16], AcknowledgementResult::Refused(vec![0; bytes - 21]))
		.unwrap()
		.encode();
	assert_eq!(wire.len(), bytes);
	FforFrame::read(55057, &mut &wire[2..]).unwrap()
}

fn assert_accounting(receiver: &FforReceiverTransport) {
	let state = receiver.state.lock().unwrap();
	let queued = state.queue.iter().chain(state.outbound.iter()).collect::<Vec<_>>();
	assert!(state.peers.len() <= MAX_PEERS);
	assert!(queued.len() <= MAX_QUEUED_MESSAGES);
	assert!(state.bytes <= MAX_QUEUED_BYTES);
	assert_eq!(state.bytes, queued.iter().map(|message| message.frame.0.len()).sum::<usize>());
	for (key, peer) in state.peers.iter() {
		let queued = queued.iter().filter(|message| message.peer == *key).collect::<Vec<_>>();
		assert_eq!(peer.messages, queued.len());
		assert_eq!(peer.bytes, queued.iter().map(|message| message.frame.0.len()).sum::<usize>());
		assert!(peer.messages <= MAX_PEER_MESSAGES);
		assert!(peer.bytes <= MAX_PEER_BYTES);
		assert!(queued.iter().all(|message| message.connection == peer.connection));
	}
}

#[test]
fn ffor_outbound_retries_preserve_exact_fifo_and_separate_inbound_work() {
	let receiver = FforReceiverTransport::default();
	let peer = key(1);
	receiver.peer_connected(peer);
	let connection = receiver.connection(peer).unwrap();
	let first = frame(21);
	let second = frame(22);
	receiver.receive(peer, frame(23)).unwrap();
	receiver.enqueue(peer, &connection, first.wire()).unwrap();
	receiver.enqueue(peer, &connection, second.wire()).unwrap();
	receiver.enqueue(peer, &connection, first.wire()).unwrap();
	assert_eq!(receiver.state.lock().unwrap().outbound.len(), 2);
	assert_accounting(&receiver);
	assert_eq!(receiver.drain_outbound(), vec![(peer, first), (peer, second)]);
	assert_eq!(receiver.pop().unwrap().frame().wire().len(), 23);
	assert!(receiver.drain_outbound().is_empty());
	// The native owner decides whether later replay is permitted after a queue drain.
	receiver.enqueue(peer, &connection, frame(21).wire()).unwrap();
	assert_eq!(receiver.drain_outbound(), vec![(peer, frame(21))]);
	assert_accounting(&receiver);
}

#[test]
fn ffor_outbound_requires_exact_peer_connection_and_clears_on_disconnect() {
	let receiver = FforReceiverTransport::default();
	let peer = key(1);
	let other = key(2);
	assert!(receiver.connection(peer).is_none());
	receiver.peer_connected(peer);
	receiver.peer_connected(other);
	let original = receiver.connection(peer).unwrap();
	assert_eq!(
		receiver.enqueue(other, &original, frame(21).wire()),
		Err(OutboundError::StaleConnection)
	);
	receiver.enqueue(peer, &original, frame(21).wire()).unwrap();
	let other_connection = receiver.connection(other).unwrap();
	receiver.enqueue(other, &other_connection, frame(22).wire()).unwrap();
	receiver.peer_disconnected(peer);
	assert_eq!(
		receiver.enqueue(peer, &original, frame(21).wire()),
		Err(OutboundError::StaleConnection)
	);
	assert_eq!(receiver.drain_outbound(), vec![(other, frame(22))]);
	receiver.peer_connected(peer);
	let new = receiver.connection(peer).unwrap();
	assert_ne!(original, new);
	assert_eq!(
		receiver.enqueue(peer, &original, frame(21).wire()),
		Err(OutboundError::StaleConnection)
	);
	receiver.enqueue(peer, &new, frame(21).wire()).unwrap();
	receiver.peer_connected(peer);
	assert!(receiver.drain_outbound().is_empty());
	let another = FforReceiverTransport::default();
	another.peer_connected(peer);
	assert_eq!(another.enqueue(peer, &new, frame(21).wire()), Err(OutboundError::StaleConnection));
	assert_accounting(&receiver);
}

#[test]
fn ffor_outbound_rejects_malformed_without_consuming_capacity_or_existing_work() {
	let receiver = FforReceiverTransport::default();
	let peer = key(1);
	receiver.peer_connected(peer);
	let connection = receiver.connection(peer).unwrap();
	receiver.receive(peer, frame(21)).unwrap();
	receiver.enqueue(peer, &connection, frame(22).wire()).unwrap();
	let mut unknown = frame(21).wire().to_vec();
	unknown[..2].copy_from_slice(&55059u16.to_be_bytes());
	for bytes in [vec![], vec![0], vec![0, 0], unknown, vec![0; MAX_MESSAGE_LEN + 1]] {
		assert_eq!(receiver.enqueue(peer, &connection, &bytes), Err(OutboundError::InvalidMessage));
	}
	let wire = frame(21).wire().to_vec();
	for end in 2..wire.len() {
		assert_eq!(
			receiver.enqueue(peer, &connection, &wire[..end]),
			Err(OutboundError::InvalidMessage)
		);
	}
	assert_eq!(receiver.drain_outbound(), vec![(peer, frame(22))]);
	assert_eq!(receiver.pop().unwrap().frame().wire(), wire);
	assert_accounting(&receiver);
}

#[test]
fn ffor_outbound_backpressure_shares_peer_count_and_byte_budgets() {
	let receiver = FforReceiverTransport::default();
	let peer = key(1);
	receiver.peer_connected(peer);
	let connection = receiver.connection(peer).unwrap();
	for tag in 0..MAX_PEER_MESSAGES as u8 / 2 {
		receiver.receive(peer, frame(21)).unwrap();
		receiver.enqueue(peer, &connection, unique_frame(21, tag).wire()).unwrap();
	}
	assert_eq!(receiver.enqueue(peer, &connection, frame(22).wire()), Err(OutboundError::Capacity));
	// Already retained bytes remain a successful retry even when the shared budget is full.
	receiver.enqueue(peer, &connection, unique_frame(21, 0).wire()).unwrap();
	assert!(receiver.receive(peer, frame(21)).is_err());
	assert_accounting(&receiver);
	assert_eq!(receiver.drain_outbound().len(), MAX_PEER_MESSAGES / 2);
	while receiver.pop().is_some() {}
	for tag in 0..4 {
		receiver.enqueue(peer, &connection, unique_frame(65500, tag).wire()).unwrap();
	}
	receiver.receive(peer, frame(144)).unwrap();
	assert_eq!(receiver.state.lock().unwrap().bytes, MAX_PEER_BYTES);
	assert_eq!(receiver.enqueue(peer, &connection, frame(21).wire()), Err(OutboundError::Capacity));
	assert_accounting(&receiver);
	receiver.pop().unwrap();
	receiver.enqueue(peer, &connection, frame(21).wire()).unwrap();
	assert_eq!(receiver.drain_outbound().len(), 5);
	assert_accounting(&receiver);
}

#[test]
fn ffor_outbound_backpressure_shares_global_count_and_byte_budgets() {
	let receiver = FforReceiverTransport::default();
	for seed in 1..=17 {
		receiver.peer_connected(key(seed));
	}
	for seed in 1..=16 {
		let peer = key(seed);
		let connection = receiver.connection(peer).unwrap();
		for tag in 0..MAX_PEER_MESSAGES as u8 {
			receiver.enqueue(peer, &connection, unique_frame(21, tag).wire()).unwrap();
		}
	}
	let peer = key(17);
	let connection = receiver.connection(peer).unwrap();
	assert_eq!(receiver.enqueue(peer, &connection, frame(21).wire()), Err(OutboundError::Capacity));
	assert!(receiver.receive(peer, frame(21)).is_err());
	assert_accounting(&receiver);
	assert_eq!(receiver.drain_outbound().len(), MAX_QUEUED_MESSAGES);
	for seed in 1..=16 {
		let peer = key(seed);
		receiver.enqueue(peer, &receiver.connection(peer).unwrap(), frame(65500).wire()).unwrap();
	}
	receiver.receive(peer, frame(576)).unwrap();
	assert_eq!(receiver.state.lock().unwrap().bytes, MAX_QUEUED_BYTES);
	assert_eq!(receiver.enqueue(peer, &connection, frame(21).wire()), Err(OutboundError::Capacity));
	receiver.peer_disconnected(key(1));
	receiver.enqueue(peer, &connection, frame(65500).wire()).unwrap();
	assert_accounting(&receiver);
}

#[test]
fn ffor_outbound_disconnect_race_cannot_rebind_queued_bytes() {
	let receiver = Arc::new(FforReceiverTransport::default());
	let peer = key(1);
	receiver.peer_connected(peer);
	let old = receiver.connection(peer).unwrap();
	let barrier = Arc::new(std::sync::Barrier::new(2));
	let outbound = Arc::clone(&receiver);
	let worker_barrier = Arc::clone(&barrier);
	let worker = std::thread::spawn(move || {
		worker_barrier.wait();
		let _ = outbound.enqueue(peer, &old, frame(21).wire());
	});
	barrier.wait();
	receiver.peer_disconnected(peer);
	receiver.peer_connected(peer);
	worker.join().unwrap();
	assert!(receiver.drain_outbound().is_empty());
	assert_accounting(&receiver);
}

#[test]
fn ffor_witness_typed_outbound_uses_shared_budget_retry_and_disconnect_rules() {
	let receiver = FforReceiverTransport::default();
	let peer = key(1);
	receiver.peer_connected(peer);
	let connection = receiver.connection(peer).unwrap();
	let (provision, fetch, _) = super::super::tests::witness_messages();
	let another = Provision::new([8; 16], provision.manifest().clone());
	for _ in 0..MAX_PEER_MESSAGES - 2 {
		receiver.receive(peer, frame(21)).unwrap();
	}
	receiver.enqueue_provision(peer, &connection, &provision).unwrap();
	receiver.enqueue_fetch(peer, &connection, &fetch).unwrap();
	receiver.enqueue_provision(peer, &connection, &provision).unwrap();
	receiver.enqueue_fetch(peer, &connection, &fetch).unwrap();
	assert_eq!(
		receiver.enqueue_provision(peer, &connection, &another),
		Err(OutboundError::Capacity)
	);
	assert_accounting(&receiver);
	let pending = receiver.drain_outbound();
	assert_eq!(pending.len(), 2);
	assert_eq!(pending[0].1.wire(), provision.encode());
	assert_eq!(pending[1].1.wire(), fetch.encode());
	receiver.enqueue_provision(peer, &connection, &another).unwrap();
	receiver.peer_disconnected(peer);
	receiver.peer_connected(peer);
	assert!(receiver.drain_outbound().is_empty());
	assert!(receiver.pop().is_none());
	assert_eq!(
		receiver.enqueue_provision(peer, &connection, &provision),
		Err(OutboundError::StaleConnection)
	);
	assert_eq!(
		receiver.enqueue_fetch(peer, &connection, &fetch),
		Err(OutboundError::StaleConnection)
	);
	assert_accounting(&receiver);
}

#[test]
fn ffor_disconnect_and_rebind_invalidate_popped_and_pending_work() {
	let receiver = FforReceiverTransport::default();
	let peer = key(1);
	receiver.peer_connected(peer);
	receiver.receive(peer, frame(21)).unwrap();
	let old = receiver.pop().unwrap();
	assert!(receiver.is_current(&old));
	receiver.receive(peer, frame(100)).unwrap();
	receiver.peer_disconnected(peer);
	assert!(!receiver.is_current(&old));
	assert!(receiver.pop().is_none());
	assert!(receiver.receive(peer, frame(21)).is_err());
	receiver.peer_connected(peer);
	receiver.receive(peer, frame(21)).unwrap();
	let new = receiver.pop().unwrap();
	assert_ne!(old.connection(), new.connection());
	assert!(!receiver.is_current(&old));
	assert!(receiver.is_current(&new));
	receiver.receive(peer, frame(21)).unwrap();
	receiver.peer_connected(peer); // A repeated callback also clears stale ownership first.
	assert!(!receiver.is_current(&new));
	assert!(receiver.pop().is_none());
	assert_accounting(&receiver);

	let another = FforReceiverTransport::default();
	another.peer_connected(peer);
	assert!(!another.is_current(&old)); // Tokens cannot collide across handler instances.
}

#[test]
fn ffor_peer_count_refusal_does_not_evict_and_disconnect_releases_capacity() {
	let receiver = FforReceiverTransport::default();
	for seed in 1..=MAX_PEERS as u8 {
		receiver.peer_connected(key(seed));
	}
	receiver.receive(key(1), frame(21)).unwrap();
	receiver.peer_connected(key(65));
	assert!(receiver.receive(key(65), frame(21)).is_err());
	assert_eq!(receiver.pop().unwrap().peer(), key(1));
	receiver.peer_disconnected(key(1));
	receiver.peer_connected(key(65));
	receiver.receive(key(65), frame(21)).unwrap();
	assert_accounting(&receiver);
}

#[test]
fn ffor_per_peer_message_and_byte_limits_preserve_existing_work() {
	let receiver = FforReceiverTransport::default();
	let peer = key(1);
	receiver.peer_connected(peer);
	for _ in 0..MAX_PEER_MESSAGES {
		receiver.receive(peer, frame(21)).unwrap();
	}
	assert!(receiver.receive(peer, frame(21)).is_err());
	assert_accounting(&receiver);
	for _ in 0..MAX_PEER_MESSAGES {
		assert_eq!(receiver.pop().unwrap().frame().wire().len(), 21);
	}
	assert!(receiver.pop().is_none());
	for _ in 0..4 {
		receiver.receive(peer, frame(65500)).unwrap();
	}
	receiver.receive(peer, frame(144)).unwrap();
	assert_eq!(receiver.state.lock().unwrap().bytes, MAX_PEER_BYTES);
	assert!(receiver.receive(peer, frame(21)).is_err());
	receiver.pop().unwrap();
	receiver.receive(peer, frame(65500)).unwrap();
	assert_accounting(&receiver);
}

#[test]
fn ffor_global_message_and_byte_limits_release_exact_accounting() {
	let receiver = FforReceiverTransport::default();
	for seed in 1..=17 {
		receiver.peer_connected(key(seed));
	}
	for seed in 1..=16 {
		for _ in 0..MAX_PEER_MESSAGES {
			receiver.receive(key(seed), frame(21)).unwrap();
		}
	}
	assert!(receiver.receive(key(17), frame(21)).is_err());
	assert_accounting(&receiver);
	receiver.peer_disconnected(key(1));
	receiver.receive(key(17), frame(21)).unwrap();
	while receiver.pop().is_some() {}
	for seed in 2..=17 {
		receiver.receive(key(seed), frame(65500)).unwrap();
	}
	receiver.receive(key(2), frame(576)).unwrap();
	assert_eq!(receiver.state.lock().unwrap().bytes, MAX_QUEUED_BYTES);
	assert!(receiver.receive(key(3), frame(21)).is_err());
	receiver.peer_disconnected(key(2));
	receiver.receive(key(3), frame(65535)).unwrap();
	assert_accounting(&receiver);
}

#[test]
fn ffor_disconnect_can_race_with_receive_without_retaining_stale_work() {
	let receiver = Arc::new(FforReceiverTransport::default());
	let peer = key(1);
	receiver.peer_connected(peer);
	let barrier = Arc::new(std::sync::Barrier::new(2));
	let receiving = Arc::clone(&receiver);
	let thread_barrier = Arc::clone(&barrier);
	let worker = std::thread::spawn(move || {
		thread_barrier.wait();
		let _ = receiving.receive(peer, frame(21));
	});
	barrier.wait();
	receiver.peer_disconnected(peer);
	worker.join().unwrap();
	assert!(receiver.pop().is_none());
	assert_accounting(&receiver);
}

proptest! {
	#![proptest_config(ProptestConfig::with_cases(64))]
	#[test]
	fn ffor_arbitrary_connection_queue_sequences_preserve_bounds(operations in prop::collection::vec((0u8..6, 0u8..4, 21usize..65536), 0..300)) {
		let receiver = FforReceiverTransport::default();
		let peers = [key(1), key(2), key(3), key(4)];
		for (operation, peer, size) in operations {
			let peer = peers[peer as usize];
			match operation {
				0 => receiver.peer_connected(peer),
				1 => receiver.peer_disconnected(peer),
				2 => { let _ = receiver.receive(peer, frame(size)); },
				3 => { receiver.pop(); },
				4 => {
					if let Some(connection) = receiver.connection(peer) {
						let _ = receiver.enqueue(peer, &connection, frame(size).wire());
					}
				},
				_ => { receiver.drain_outbound(); },
			}
			assert_accounting(&receiver);
		}
	}
}

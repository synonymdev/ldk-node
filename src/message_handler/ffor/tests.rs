use bitcoin::secp256k1::{Secp256k1, SecretKey};
use lightning_ffor::witness::AcknowledgementResult;
use proptest::prelude::*;

use super::*;

fn key(seed: u8) -> PublicKey {
	PublicKey::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[seed; 32]).unwrap())
}

fn frame(bytes: usize) -> FforFrame {
	let wire = Acknowledgement::new([1; 16], AcknowledgementResult::Refused(vec![0; bytes - 21]))
		.unwrap()
		.encode();
	assert_eq!(wire.len(), bytes);
	FforFrame::read(55057, &mut &wire[2..]).unwrap()
}

fn assert_accounting(receiver: &FforReceiverTransport) {
	let state = receiver.state.lock().unwrap();
	assert!(state.peers.len() <= MAX_PEERS);
	assert!(state.queue.len() <= MAX_QUEUED_MESSAGES);
	assert!(state.bytes <= MAX_QUEUED_BYTES);
	assert_eq!(state.bytes, state.queue.iter().map(|message| message.frame.0.len()).sum::<usize>());
	for (key, peer) in state.peers.iter() {
		let queued = state.queue.iter().filter(|message| message.peer == *key).collect::<Vec<_>>();
		assert_eq!(peer.messages, queued.len());
		assert_eq!(peer.bytes, queued.iter().map(|message| message.frame.0.len()).sum::<usize>());
		assert!(peer.messages <= MAX_PEER_MESSAGES);
		assert!(peer.bytes <= MAX_PEER_BYTES);
		assert!(queued.iter().all(|message| message.connection == peer.connection));
	}
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
	fn ffor_arbitrary_connection_queue_sequences_preserve_bounds(operations in prop::collection::vec((0u8..4, 0u8..4, 21usize..65536), 0..300)) {
		let receiver = FforReceiverTransport::default();
		let peers = [key(1), key(2), key(3), key(4)];
		for (operation, peer, size) in operations {
			let peer = peers[peer as usize];
			match operation {
				0 => receiver.peer_connected(peer),
				1 => receiver.peer_disconnected(peer),
				2 => { let _ = receiver.receive(peer, frame(size)); },
				_ => { receiver.pop(); },
			}
			assert_accounting(&receiver);
		}
	}
}

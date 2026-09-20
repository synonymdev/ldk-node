#![no_main]

use bitcoin::secp256k1::PublicKey;
use ffor_protocol::setup::AuthenticatedSetup;
use ffor_protocol::wire::{Message, MAX_MESSAGE_LEN};
use libfuzzer_sys::fuzz_target;

fn keys() -> (PublicKey, PublicKey) {
	let receiver = [
		3, 159, 202, 127, 129, 87, 170, 118, 135, 8, 137, 79, 253, 146, 85, 15, 233, 112, 237, 209,
		133, 38, 165, 249, 54, 88, 62, 163, 181, 77, 171, 50, 40,
	];
	let settlement = [
		2, 8, 123, 125, 27, 71, 137, 23, 15, 110, 55, 79, 10, 14, 88, 161, 183, 168, 153, 227, 73,
		41, 121, 83, 20, 171, 105, 100, 230, 150, 9, 233, 192,
	];
	(PublicKey::from_slice(&receiver).unwrap(), PublicKey::from_slice(&settlement).unwrap())
}

fuzz_target!(|data: &[u8]| {
	if data.len() > 2 * MAX_MESSAGE_LEN + 2 {
		return;
	}
	let (receiver, settlement) = keys();
	if let Ok(message) = Message::decode(data) {
		assert_eq!(message.encode().unwrap(), data);
		let _ = message.verify_signature(&receiver);
		let _ = message.verify_signature(&settlement);
	}
	// Two-byte first-message length also permits real signed setup pairs in the corpus.
	if data.len() < 2 {
		return;
	}
	let split = usize::from(u16::from_be_bytes([data[0], data[1]]));
	let Some(first) = data.get(2..2 + split) else {
		return;
	};
	let Some(second) = data.get(2 + split..) else {
		return;
	};
	if let (Ok(init), Ok(accept)) = (Message::decode(first), Message::decode(second)) {
		let _ = AuthenticatedSetup::new(&init, &accept, receiver, settlement);
		let _ = AuthenticatedSetup::new(&init, &accept, settlement, receiver);
	}
});

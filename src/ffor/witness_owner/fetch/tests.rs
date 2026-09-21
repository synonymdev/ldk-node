use std::sync::atomic::Ordering;

use bitcoin::hashes::{sha256, Hash, HashEngine, Hmac, HmacEngine};
use bitcoin::secp256k1::ecdh::SharedSecret;
use bitcoin::secp256k1::{Message, Secp256k1, SecretKey};
use lightning::ln::peer_handler::CustomMessageHandler;
use lightning::ln::wire::CustomMessageReader;
use lightning_ffor::witness::{EncryptedRecord, FetchResult, RecordHeader, SignedManifest};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305};

use super::*;
use crate::ffor::witness_owner::tests::Harness;
use crate::ffor::witness_store::WitnessStoreError;
use crate::message_handler::NodeCustomMessage;

fn secret(byte: u8) -> SecretKey {
	SecretKey::from_slice(&[byte; 32]).unwrap()
}

fn public(byte: u8) -> PublicKey {
	PublicKey::from_secret_key(&Secp256k1::new(), &secret(byte))
}

fn start(h: &mut Harness) -> SignedFetch {
	h.register_and_persist();
	h.connect(h.policies[0].witness);
	assert_eq!(h.owner.fetch(&h.context, h.policies[0].witness), Ok(FetchProgress::Queued));
	outgoing(h).remove(0)
}

fn outgoing(h: &Harness) -> Vec<SignedFetch> {
	let manifests = h.owner.retained_manifests(&h.context).unwrap();
	h.handler
		.get_and_clear_pending_msg()
		.into_iter()
		.map(|(peer, message)| {
			let manifest = &manifests.iter().find(|(w, _)| *w == peer).unwrap().1;
			match message {
				NodeCustomMessage::Ffor(frame) => SignedFetch::decode(
					frame.wire(),
					manifest.unsigned().parameters().fetch_public_key,
				)
				.unwrap(),
				_ => panic!("unexpected LSPS message"),
			}
		})
		.collect()
}

fn receive(h: &Harness, peer: PublicKey, response: &FetchResponse) -> ReceivedFforMessage {
	let wire = response.encode();
	let message =
		h.handler.read(u16::from_be_bytes([wire[0], wire[1]]), &mut &wire[2..]).unwrap().unwrap();
	h.handler.handle_custom_message(message, peer).unwrap();
	h.owner.transport.pop().unwrap()
}

fn response(
	request: &SignedFetch, records: Vec<EncryptedRecord>, after: Option<u16>,
) -> FetchResponse {
	FetchResponse::new(
		request.unsigned().parameters().request_id,
		FetchResult::Page { records, next_after_slot: after, extensions: Vec::new() },
	)
	.unwrap()
}

pub(in crate::ffor::witness_owner) fn record(
	h: &Harness, slot: u16, invalid: bool, identity: u8,
) -> EncryptedRecord {
	let manifest: SignedManifest = h.owner.retained_manifests(&h.context).unwrap().remove(0).1;
	let params = manifest.unsigned().parameters();
	let voucher = &h.context.setup().vouchers()[usize::from(slot) - 1];
	// The native fixture is public test data. Its two generated preimages repeat a counter byte.
	let preimage = (0..=255)
		.map(|byte| [byte; 32])
		.find(|value| sha256::Hash::hash(value).to_byte_array() == voucher.payment_hash)
		.unwrap();
	let mut body = Vec::new();
	body.extend_from_slice(&h.context.epoch_id());
	body.extend_from_slice(&slot.to_be_bytes());
	body.extend_from_slice(&preimage);
	body.extend_from_slice(&voucher.payment_hash);
	body.extend_from_slice(&voucher.amount_msat.to_be_bytes());
	body.extend_from_slice(&voucher.expiry.to_be_bytes());
	body.extend_from_slice(&voucher.deadline.to_be_bytes());
	body.extend_from_slice(&[0; 28]);
	assert_eq!(body.len(), lightning_ffor::witness::RECORD_BODY_LEN);
	if invalid {
		body[34] ^= 1;
	}
	let book = manifest.unsigned().canonical_book();
	let offset = 36 + 58 * (usize::from(slot) - 1);
	let mut terms = b"ffor/terms".to_vec();
	terms.extend_from_slice(&book[offset..offset + 58]);
	let mut header = RecordHeader {
		mailbox_id: params.mailbox_id,
		record_id: [identity; 32],
		slot,
		activation_hash: manifest.unsigned().activation_hash(),
		terms_hash: sha256::Hash::hash(&terms).to_byte_array(),
		witness: h.policies[0].witness,
		encryption_public_key: params.encryption_public_key,
		recorded_height: 0,
		unbarriered: false,
		ciphertext_hash: [0; 32],
	};
	let shared = SharedSecret::new(&params.encryption_public_key, &secret(47));
	let mut extract = HmacEngine::<sha256::Hash>::new(&[]);
	extract.input(&shared.secret_bytes());
	let mut expand = HmacEngine::<sha256::Hash>::new(&Hmac::from_engine(extract).to_byte_array());
	expand.input(b"ffor/witness/body");
	expand.input(&[1]);
	let key = Hmac::from_engine(expand).to_byte_array();
	let cipher = LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, &key).unwrap());
	cipher
		.seal_in_place_append_tag(
			Nonce::assume_unique_for_key([0; 12]),
			Aad::from(header.associated_data()),
			&mut body,
		)
		.unwrap();
	let mut encrypted = public(47).serialize().to_vec();
	encrypted.extend_from_slice(&body);
	header.ciphertext_hash = sha256::Hash::hash(&encrypted).to_byte_array();
	let signature =
		Secp256k1::new().sign_ecdsa(&Message::from_digest(header.signing_digest()), &secret(80));
	let mut wire = header.encode();
	wire.extend_from_slice(&signature.serialize_compact());
	wire.extend_from_slice(&(encrypted.len() as u16).to_be_bytes());
	wire.extend_from_slice(&encrypted);
	wire.push(0);
	EncryptedRecord::decode(&wire).unwrap()
}

fn retained(h: &Harness, slot: u16) -> Option<lightning::ln::ffor::FFORWitnessReceipt> {
	let binding = WitnessStorageBinding::from_native_context(&h.context).unwrap();
	h.owner.store.load_receipt(&binding, h.policies[0].witness, slot).unwrap()
}

#[test]
fn ffor_witness_fetch_real_retry_correlates_identity_and_empty_response_grants_no_credit() {
	let mut h = Harness::new();
	let first = start(&mut h);
	let witness = h.policies[0].witness;
	assert_eq!(h.owner.fetch(&h.context, witness), Ok(FetchProgress::AwaitingResponse));
	assert!(outgoing(&h).is_empty());
	assert_eq!(h.owner.retry_fetch(&h.context, witness), Ok(FetchProgress::Queued));
	let second = outgoing(&h).remove(0);
	assert_ne!(first.unsigned().parameters().request_id, second.unsigned().parameters().request_id);
	assert_ne!(first.unsigned().parameters().nonce, second.unsigned().parameters().nonce);
	let old = receive(&h, witness, &response(&first, vec![], None));
	assert_eq!(h.owner.accept_fetch_page(&old), Err(WitnessOwnerError::UnknownRequest));
	h.connect(public(81));
	let wrong = receive(&h, public(81), &response(&second, vec![], None));
	assert!(matches!(h.owner.accept_fetch_page(&wrong), Err(WitnessOwnerError::Protocol(_))));
	let stale = receive(&h, witness, &response(&second, vec![], None));
	h.disconnect(witness);
	h.connect(witness);
	assert_eq!(h.owner.accept_fetch_page(&stale), Err(WitnessOwnerError::StaleConnection));
	assert_eq!(h.owner.fetch(&h.context, witness), Ok(FetchProgress::Queued));
	let third = outgoing(&h).remove(0);
	let empty = receive(&h, witness, &response(&third, vec![], None));
	assert_eq!(h.owner.accept_fetch_page(&empty), Ok(FetchProgress::Complete));
	assert_eq!(h.owner.fetches.usage(), (0, 0));
	assert!(retained(&h, 1).is_none());
	assert!(h.node.list_payments().is_empty());
}

#[test]
fn ffor_witness_fetch_real_paging_preserves_receipts_after_later_rejection_and_channel_removal() {
	let mut h = Harness::new();
	let first = start(&mut h);
	let witness = h.policies[0].witness;
	let receipt = record(&h, 1, false, 1);
	let page = receive(&h, witness, &response(&first, vec![receipt.clone()], Some(1)));
	assert_eq!(h.owner.accept_fetch_page(&page), Ok(FetchProgress::Queued));
	assert_eq!(retained(&h, 1).unwrap().header(), receipt.header());
	let second = outgoing(&h).remove(0);
	assert_eq!(second.unsigned().parameters().after_slot, Some(1));
	assert_ne!(first.unsigned().parameters().nonce, second.unsigned().parameters().nonce);
	let repeated = receive(&h, witness, &response(&second, vec![receipt], None));
	assert!(matches!(h.owner.accept_fetch_page(&repeated), Err(WitnessOwnerError::Protocol(_))));
	assert!(retained(&h, 1).is_some());
	h.node
		.channel_manager
		.force_close_broadcasting_latest_txn(
			&h.context.channel_id(),
			&h.context.settlement_node_id(),
			"receipt fixture close".to_owned(),
		)
		.unwrap();
	let final_page = receive(&h, witness, &response(&second, vec![record(&h, 2, false, 2)], None));
	assert_eq!(h.owner.accept_fetch_page(&final_page), Ok(FetchProgress::Complete));
	assert!(retained(&h, 1).is_some() && retained(&h, 2).is_some());
	assert!(h.node.list_payments().is_empty());
}

#[test]
fn ffor_witness_fetch_real_storage_failure_keeps_page_through_disconnect_and_retry() {
	for failure in [1, 2] {
		let mut h = Harness::new();
		let first = start(&mut h);
		let witness = h.policies[0].witness;
		let page = receive(&h, witness, &response(&first, vec![record(&h, 1, false, 1)], Some(1)));
		h.storage.write_failure.store(failure, Ordering::SeqCst);
		assert_eq!(
			h.owner.accept_fetch_page(&page),
			Err(WitnessOwnerError::Storage(WitnessStoreError::Storage))
		);
		h.disconnect(witness);
		h.owner.synchronize_connections();
		assert_eq!(h.owner.fetches.usage().0, 1);
		assert_eq!(
			h.owner.retry_fetch(&h.context, witness),
			Err(WitnessOwnerError::Storage(WitnessStoreError::Uncertain))
		);
		h.owner.recover_storage().unwrap();
		assert_eq!(h.owner.retry_fetch(&h.context, witness), Ok(FetchProgress::RestartRequired));
		assert!(retained(&h, 1).is_some());
		h.connect(witness);
		assert_eq!(h.owner.fetch(&h.context, witness), Ok(FetchProgress::Queued));
		let retry = outgoing(&h).remove(0);
		assert_eq!(retry.unsigned().parameters().after_slot, None);
		assert_ne!(first.unsigned().parameters().nonce, retry.unsigned().parameters().nonce);
	}
}

#[test]
fn ffor_witness_fetch_real_rejected_record_does_not_discard_later_valid_evidence() {
	for conflict in [false, true] {
		let mut h = Harness::new();
		let first = start(&mut h);
		let witness = h.policies[0].witness;
		if conflict {
			let binding = WitnessStorageBinding::from_native_context(&h.context).unwrap();
			h.owner.store.retain_receipt(&binding, witness, &record(&h, 1, false, 9)).unwrap();
		}
		let page = receive(
			&h,
			witness,
			&response(&first, vec![record(&h, 1, !conflict, 1), record(&h, 2, false, 2)], None),
		);
		// The first candidate is rejected without a write. The later valid record still needs one.
		h.storage.write_failure.store(2, Ordering::SeqCst);
		assert_eq!(
			h.owner.accept_fetch_page(&page),
			Err(WitnessOwnerError::Storage(WitnessStoreError::Storage))
		);
		h.owner.recover_storage().unwrap();
		assert_eq!(h.owner.retry_fetch(&h.context, witness), Ok(FetchProgress::RejectedPage));
		assert_eq!(h.owner.fetches.usage(), (0, 0));
		assert!(retained(&h, 2).is_some());
		assert_eq!(retained(&h, 1).is_some(), conflict);
		if conflict {
			assert_eq!(retained(&h, 1).unwrap().header().record_id, [9; 32]);
		}
		assert_eq!(h.owner.retry_fetch(&h.context, witness), Ok(FetchProgress::Queued));
		let retry = outgoing(&h).remove(0);
		assert_ne!(first.unsigned().parameters().nonce, retry.unsigned().parameters().nonce);
		let corrected = receive(
			&h,
			witness,
			&response(
				&retry,
				vec![record(&h, 1, false, if conflict { 9 } else { 1 }), record(&h, 2, false, 2)],
				None,
			),
		);
		assert_eq!(h.owner.accept_fetch_page(&corrected), Ok(FetchProgress::Complete));
		assert!(retained(&h, 1).is_some() && retained(&h, 2).is_some());
	}
}

#[test]
fn ffor_witness_fetch_and_provision_share_capacity_without_evicting_work() {
	let mut h = Harness::new();
	let first = start(&mut h);
	let witness = h.policies[0].witness;
	let manifest = h.owner.retained_manifests(&h.context).unwrap().remove(0).1;
	let source = WitnessConnection {
		node_id: witness,
		identity: h.owner.transport.connection(witness).unwrap(),
	};
	let usage = h.owner.fetches.usage();
	h.owner.pending.set_other_usage(usage.0, usage.1).unwrap();
	// These are transient correlation records only. No native registration or release is fabricated.
	for byte in 1..64 {
		let epoch = Epoch {
			channel: h.context.channel_id(),
			epoch: [byte; 32],
			context_digest: h.context.context_digest(),
		};
		h.owner.pending.stage(epoch, source.clone(), manifest.clone()).unwrap();
	}
	assert_eq!(h.owner.pending.usage().0 + h.owner.fetches.usage().0, MAX_WORK_COUNT);
	assert_eq!(h.owner.provision(&h.context, witness), Err(WitnessOwnerError::Capacity));
	assert_eq!(h.owner.pending.usage().0, 63);
	assert_eq!(h.owner.fetches.usage(), usage);
	assert_eq!(h.owner.fetch(&h.context, witness), Ok(FetchProgress::AwaitingResponse));
	assert!(outgoing(&h).is_empty());
	let page = receive(&h, witness, &response(&first, vec![], None));
	assert_eq!(h.owner.accept_fetch_page(&page), Ok(FetchProgress::Complete));
	assert_eq!(
		h.owner.provision(&h.context, witness),
		Ok(super::super::provisioning::ProvisioningProgress::Queued)
	);
}

#[test]
fn ffor_witness_fetch_byte_reservation_and_replacement_fail_without_eviction() {
	let mut h = Harness::new();
	let first = start(&mut h);
	let witness = h.policies[0].witness;
	let manifest = h.owner.retained_manifests(&h.context).unwrap().remove(0).1;
	let binding = WitnessStorageBinding::from_native_context(&h.context).unwrap();
	let source = WitnessConnection {
		node_id: witness,
		identity: h.owner.transport.connection(witness).unwrap(),
	};
	let epoch = h.owner.fetches.entries[0].epoch;
	let usage = h.owner.fetches.usage();
	let request = h.owner.fresh_fetch(&binding, witness, None).unwrap();
	let pending = PendingFetch::first(request, manifest, source).unwrap();
	assert_eq!(
		h.owner.fetches.install(epoch, pending, (0, MAX_WORK_BYTES - usage.1 + 1)),
		Err(WitnessOwnerError::Capacity)
	);
	assert_eq!(h.owner.fetches.usage(), usage);
	assert!(h.owner.fetches.contains_request_id(first.unsigned().parameters().request_id));
	let entry = &h.owner.fetches.entries[0];
	assert!(matches!(entry.state, FetchState::Request { queued: true, .. }));
}

#[test]
fn ffor_witness_fetch_backpressure_keeps_exact_unsent_request() {
	let mut h = Harness::new();
	h.register_and_persist();
	let witness = h.policies[0].witness;
	h.connect(witness);
	let token = h.owner.transport.connection(witness).unwrap();
	let binding = WitnessStorageBinding::from_native_context(&h.context).unwrap();
	for _ in 0..8 {
		let request = h.owner.store.load(&binding).unwrap().prepare_fetch(witness, None).unwrap();
		h.owner.transport.enqueue_fetch(witness, &token, &request).unwrap();
	}
	assert_eq!(h.owner.fetch(&h.context, witness), Ok(FetchProgress::Backpressured));
	let id = h.owner.fetches.entries[0].request_id;
	let nonce = h.owner.fetches.entries[0].nonce;
	let usage = h.owner.fetches.usage();
	assert_eq!(outgoing(&h).len(), 8);
	assert_eq!(h.owner.fetch(&h.context, witness), Ok(FetchProgress::Queued));
	let queued = outgoing(&h).remove(0);
	assert_eq!(queued.unsigned().parameters().request_id, id);
	assert_eq!(queued.unsigned().parameters().nonce, nonce);
	assert_eq!(h.owner.fetches.usage(), usage);
	assert_eq!(h.owner.fetch(&h.context, witness), Ok(FetchProgress::AwaitingResponse));
	assert!(outgoing(&h).is_empty());
}

#[test]
fn ffor_witness_fetch_retains_a_whole_page_with_one_write_and_deduplicates_retry() {
	let mut h = Harness::new();
	let first = start(&mut h);
	let witness = h.policies[0].witness;
	let records = vec![record(&h, 1, false, 1), record(&h, 2, false, 2)];
	let page = receive(&h, witness, &response(&first, records.clone(), None));
	let before = h.storage.writes.load(Ordering::SeqCst);
	assert_eq!(h.owner.accept_fetch_page(&page), Ok(FetchProgress::Complete));
	assert_eq!(h.storage.writes.load(Ordering::SeqCst), before + 1);
	assert!(retained(&h, 1).is_some() && retained(&h, 2).is_some());
	assert_eq!(h.owner.retry_fetch(&h.context, witness), Ok(FetchProgress::Queued));
	let retry = outgoing(&h).remove(0);
	let repeated = receive(&h, witness, &response(&retry, records, None));
	assert_eq!(h.owner.accept_fetch_page(&repeated), Ok(FetchProgress::Complete));
	assert_eq!(h.storage.writes.load(Ordering::SeqCst), before + 1);
}

#[test]
fn ffor_witness_fetch_uncertain_page_keeps_all_valid_evidence_until_exact_recovery() {
	for failure in [1, 2] {
		let mut h = Harness::new();
		let first = start(&mut h);
		let witness = h.policies[0].witness;
		let records = vec![record(&h, 1, false, 1), record(&h, 2, false, 2)];
		let page = receive(&h, witness, &response(&first, records, None));
		let before = h.storage.writes.load(Ordering::SeqCst);
		h.storage.write_failure.store(failure, Ordering::SeqCst);
		assert_eq!(
			h.owner.accept_fetch_page(&page),
			Err(WitnessOwnerError::Storage(WitnessStoreError::Storage))
		);
		assert_eq!(h.storage.writes.load(Ordering::SeqCst), before + 1);
		assert_eq!(h.owner.fetches.usage().0, 1);
		h.owner.recover_storage().unwrap();
		assert_eq!(h.storage.writes.load(Ordering::SeqCst), before + 2);
		assert_eq!(h.owner.fetch(&h.context, witness), Ok(FetchProgress::Complete));
		assert_eq!(h.storage.writes.load(Ordering::SeqCst), before + 2);
		assert!(retained(&h, 1).is_some() && retained(&h, 2).is_some());
	}
}

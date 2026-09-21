//! Protected codec tests only. Native publication authority is tested through the issuer adapter.

use bitcoin::hashes::{sha256, Hash};
use lightning_invoice::{Currency, InvoiceBuilder, RouteHint, RouteHintHop, RoutingFees};
use lightning_types::payment::PaymentSecret;

use super::*;
use crate::ffor::request_store::record::invoice::{InvoicePolicy, RetainedInvoice};

fn policy() -> InvoicePolicy {
	InvoicePolicy { expiry_seconds: 3600, safety_margin_seconds: 120 }
}
fn signed_invoice(note: &str, amount: u64) -> String {
	signed_invoice_with_routes(note, amount, 0)
}
fn signed_invoice_with_routes(note: &str, amount: u64, routes: usize) -> String {
	let root = bitcoin::bip32::Xpriv::new_master(Network::Testnet, &SEED).unwrap().private_key;
	let keys = lightning::sign::KeysManager::new(&root.secret_bytes(), 0, 0, true);
	let key = keys.get_node_secret_key();
	let mut builder = InvoiceBuilder::new(Currency::BitcoinTestnet)
		.description(note.to_owned())
		.duration_since_epoch(std::time::Duration::from_secs(1))
		.expiry_time(std::time::Duration::from_secs(3600))
		.amount_milli_satoshis(amount)
		.payment_hash(sha256::Hash::hash(&[9; 32]))
		.payment_secret(PaymentSecret([8; 32]))
		.min_final_cltv_expiry_delta(18);
	for index in 0..routes {
		builder = builder.private_route(RouteHint(vec![RouteHintHop {
			src_node_id: PublicKey::from_secret_key(&Secp256k1::new(), &key),
			short_channel_id: index as u64,
			fees: RoutingFees { base_msat: 0, proportional_millionths: 0 },
			cltv_expiry_delta: 18,
			htlc_minimum_msat: None,
			htlc_maximum_msat: None,
		}]));
	}
	builder.build_signed(|m| Secp256k1::new().sign_ecdsa_recoverable(m, &key)).unwrap().to_string()
}
fn reserved(store: &mut RequestStore) -> StoredRequest {
	store.recover_native(CLIENT).unwrap().unwrap();
	let mut record = store.lookup(CLIENT).unwrap().unwrap();
	assert!(record.reserve_invoice(policy()).unwrap());
	store.write_record(&record).unwrap();
	record
}
fn retain(record: &mut StoredRequest) {
	let invoice = RetainedInvoice::new(
		[7; 32],
		signed_invoice(record.intent().description(), record.intent().amount_msat()),
		5,
	)
	.unwrap();
	assert!(record.retain_invoice(invoice).unwrap());
}

#[test]
fn ffor_request_invoice_v2_upgrade_preserves_intent_and_rejects_policy_replacement() {
	let (storage, node, mut store) = pending();
	let legacy = store.lookup(CLIENT).unwrap().unwrap();
	assert_eq!(legacy.version(), LEGACY_VERSION);
	let upgraded = reserved(&mut store);
	assert_eq!(upgraded.reserved_bytes(), 8192);
	assert!(upgraded.same_request(&legacy));
	let ciphertext = raw(&storage, &store, CLIENT);
	let mut unchanged = upgraded.clone();
	assert!(!unchanged.reserve_invoice(policy()).unwrap());
	assert_eq!(
		unchanged.reserve_invoice(InvoicePolicy { expiry_seconds: 1, ..policy() }),
		Err(RequestStoreError::Conflict)
	);
	assert_eq!(raw(&storage, &store, CLIENT), ciphertext);
	drop(store);
	let mut reopened = open(storage, &node);
	assert_eq!(reopened.lookup(CLIENT).unwrap().unwrap().encode(), upgraded.encode());
}

#[test]
fn ffor_request_invoice_exact_bytes_and_payment_marker_are_monotonic() {
	let (storage, node, mut store) = pending();
	let mut record = reserved(&mut store);
	retain(&mut record);
	assert!(!format!("{record:?}").contains("lntb"));
	let payment = record.invoice().unwrap().payment().unwrap();
	assert!(!record.invoice().unwrap().payment_confirmed());
	store.write_record(&record).unwrap();
	assert!(record.confirm_invoice_payment(&payment).unwrap());
	assert!(!record.confirm_invoice_payment(&payment).unwrap());
	let mut changed = payment.clone();
	changed.latest_update_timestamp += 1;
	assert_eq!(record.confirm_invoice_payment(&changed), Err(RequestStoreError::Conflict));
	assert_eq!(
		record.retain_invoice(RetainedInvoice::new([8; 32], payment_invoice(&payment), 5).unwrap()),
		Err(RequestStoreError::Conflict)
	);
	store.write_record(&record).unwrap();
	drop(store);
	let mut reopened = open(storage, &node);
	let restored = reopened.lookup(CLIENT).unwrap().unwrap();
	assert_eq!(restored.encode(), record.encode());
	assert!(restored.invoice().unwrap().payment_confirmed());
	assert_eq!(restored.invoice().unwrap().payment().unwrap(), payment);
}
fn payment_invoice(payment: &crate::payment::store::PaymentDetails) -> String {
	match &payment.kind {
		crate::payment::store::PaymentKind::Bolt11 { bolt11: Some(wire), .. } => wire.clone(),
		_ => panic!("invoice"),
	}
}

#[test]
fn ffor_request_invoice_every_protected_write_keeps_exact_uncertain_candidate() {
	for stage in 0..3 {
		for failure in [1, 2] {
			let (storage, node, mut store) = pending();
			store.recover_native(CLIENT).unwrap();
			let mut record = store.lookup(CLIENT).unwrap().unwrap();
			record.reserve_invoice(policy()).unwrap();
			if stage >= 1 {
				store.write_record(&record).unwrap();
				retain(&mut record);
			}
			if stage == 2 {
				store.write_record(&record).unwrap();
				record
					.confirm_invoice_payment(&record.invoice().unwrap().payment().unwrap())
					.unwrap();
			}
			storage.write_failure.store(failure, Ordering::SeqCst);
			assert_eq!(store.write_record(&record), Err(RequestStoreError::Storage));
			let candidate = store.uncertain.as_ref().unwrap().bytes.clone();
			assert!(matches!(store.lookup(CLIENT), Err(RequestStoreError::Uncertain)));
			storage.write_failure.store(2, Ordering::SeqCst);
			assert_eq!(store.recover_write(), Err(RequestStoreError::Storage));
			assert_eq!(store.uncertain.as_ref().unwrap().bytes, candidate);
			drop(store);
			let mut reopened = open(Arc::clone(&storage), &node);
			storage.write_failure.store(2, Ordering::SeqCst);
			assert!(matches!(reopened.lookup(CLIENT), Err(RequestStoreError::Storage)));
			reopened.recover_write().unwrap();
			assert_eq!(reopened.lookup(CLIENT).unwrap().unwrap().encode(), record.encode());
			assert_eq!(raw(&storage, &reopened, CLIENT), candidate);
		}
	}
}

#[test]
fn ffor_request_invoice_v2_strict_framing_and_payment_digest_validation() {
	let (_, _, mut store) = pending();
	let mut value = reserved(&mut store);
	retain(&mut value);
	value.confirm_invoice_payment(&value.invoice().unwrap().payment().unwrap()).unwrap();
	let bytes = value.encode();
	for end in 0..bytes.len() {
		assert!(StoredRequest::decode(&bytes[..end]).is_err());
	}
	let mut extra = bytes.to_vec();
	extra.push(0);
	assert!(StoredRequest::decode(&extra).is_err());
	let mut corrupt = bytes.to_vec();
	*corrupt.last_mut().unwrap() ^= 1;
	assert!(StoredRequest::decode(&corrupt).is_err());
	let mut downgrade = bytes.to_vec();
	downgrade[1] = LEGACY_VERSION as u8;
	assert!(StoredRequest::decode(&downgrade).is_err());
	let mut future = bytes.to_vec();
	future[1] = 3;
	assert!(StoredRequest::decode(&future).is_err());
	let mut legacy = StoredRequest::new(intent(CLIENT), plan(&store, CLIENT)).unwrap();
	assert!(legacy.retain_invoice(value.invoice().unwrap().clone()).is_err());
	assert_eq!(
		legacy.reserve_invoice(InvoicePolicy { expiry_seconds: 0, ..policy() }),
		Err(RequestStoreError::InvalidIntent)
	);
	let wrong = RetainedInvoice::new([7; 32], signed_invoice("changed", 2_000_000), 5).unwrap();
	assert_eq!(value.retain_invoice(wrong), Err(RequestStoreError::Conflict));
}

#[test]
fn ffor_request_invoice_reserved_quota_allows_all64_upgrades_without_eviction() {
	let (_, _, mut store) = fixture();
	for index in 0..MAX_REQUESTS {
		let client = format!("quota-{index}");
		let mut record = store.begin(intent(&client), plan(&store, &client)).unwrap();
		record.reserve_invoice(policy()).unwrap();
		store.write_record(&record).unwrap();
	}
	assert_eq!(store.reserved_bytes_without("").unwrap(), MAX_STORE_BYTES);
	assert!(matches!(
		store.begin(intent(CLIENT), plan(&store, CLIENT)),
		Err(RequestStoreError::Capacity)
	));
	assert_eq!(store.list().unwrap().len(), MAX_REQUESTS);
}

#[test]
fn ffor_request_invoice_full_envelope_fits_reserved_capacity_and_binds_identity() {
	let (_, _, mut store) = pending();
	let native_id = store.recover_native(CLIENT).unwrap().unwrap();
	let client = "c".repeat(128);
	let note = "n".repeat(639);
	let intent = RequestIntent::new(client.clone(), 2_000_000, note.clone()).unwrap();
	let mut plan = plan(&store, &client);
	plan.parameters.witness_peers = Some(
		(1..=4)
			.map(|i| {
				PublicKey::from_secret_key(
					&Secp256k1::new(),
					&SecretKey::from_slice(&[i; 32]).unwrap(),
				)
			})
			.collect(),
	);
	let mut record = StoredRequest::new(intent, plan).unwrap();
	record.bind(&native_id).unwrap();
	record.reserve_invoice(policy()).unwrap();
	let mut largest = String::new();
	for count in 0..64 {
		let wire = signed_invoice_with_routes(&note, 2_000_000, count);
		if wire.len() > 4096 {
			break;
		}
		largest = wire;
	}
	assert!(largest.len() > 4000);
	record.retain_invoice(RetainedInvoice::new([7; 32], largest, u64::MAX).unwrap()).unwrap();
	record.confirm_invoice_payment(&record.invoice().unwrap().payment().unwrap()).unwrap();
	let key = hex(&record.local_request_id());
	let sealed = store.envelope.seal(&key, &record).unwrap();
	assert!(sealed.len() < MAX_RECORD_BYTES);
	assert_eq!(store.envelope.open(&key, &sealed).unwrap().encode(), record.encode());
	record.validate_identity(store.chain, store.node).unwrap();
	let wrong_key =
		PublicKey::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[42; 32]).unwrap());
	assert_eq!(record.validate_identity(store.chain, wrong_key), Err(RequestStoreError::Identity));
	let bitcoin =
		bitcoin::blockdata::constants::ChainHash::using_genesis_block(Network::Bitcoin).to_bytes();
	assert_eq!(record.validate_identity(bitcoin, store.node), Err(RequestStoreError::Identity));
}

#[test]
fn ffor_request_invoice_description_bytes_are_not_display_normalized() {
	let note = "exact\n\u{1b}note";
	let retained = RetainedInvoice::new([1; 32], signed_invoice(note, 2000), 5).unwrap();
	match retained.payment().unwrap().kind {
		crate::payment::store::PaymentKind::Bolt11 { description, .. } => {
			assert_eq!(description.as_deref(), Some(note))
		},
		_ => panic!("invoice kind"),
	}
}

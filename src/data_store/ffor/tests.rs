use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use bitcoin::hashes::{sha256, Hash};
use bitcoin::secp256k1::{Secp256k1, SecretKey};
use lightning::io;
use lightning::ln::channelmanager::PaymentId;
use lightning::util::persist::KVStore;
use lightning::util::test_utils::TestLogger;
use lightning_invoice::{Currency, InvoiceBuilder};
use lightning_types::payment::{PaymentHash, PaymentSecret};

use super::*;
use crate::io::test_utils::InMemoryStore;
use crate::payment::store::PaymentDetailsUpdate;
use crate::types::DynStore;

type RemovalGate = (mpsc::Sender<()>, mpsc::Receiver<()>, bool);

struct FaultStore {
	inner: InMemoryStore,
	failure: AtomicUsize,
	writes: AtomicUsize,
	remove_gate: Mutex<Option<RemovalGate>>,
}
impl KVStoreSync for FaultStore {
	fn read(&self, p: &str, s: &str, k: &str) -> io::Result<Vec<u8>> {
		KVStoreSync::read(&self.inner, p, s, k)
	}
	fn write(&self, p: &str, s: &str, k: &str, v: Vec<u8>) -> io::Result<()> {
		self.writes.fetch_add(1, Ordering::SeqCst);
		let failure = self.failure.swap(0, Ordering::SeqCst);
		if failure != 1 {
			KVStoreSync::write(&self.inner, p, s, k, v)?;
		}
		if failure == 0 {
			Ok(())
		} else {
			Err(io::Error::new(io::ErrorKind::Other, "injected write"))
		}
	}
	fn remove(&self, p: &str, s: &str, k: &str, lazy: bool) -> io::Result<()> {
		if let Some((entered, resume, fail)) = self.remove_gate.lock().unwrap().take() {
			entered.send(()).unwrap();
			resume.recv().unwrap();
			if fail {
				return Err(io::Error::new(io::ErrorKind::Other, "injected removal"));
			}
		}
		KVStoreSync::remove(&self.inner, p, s, k, lazy)
	}
	fn list(&self, p: &str, s: &str) -> io::Result<Vec<String>> {
		KVStoreSync::list(&self.inner, p, s)
	}
}
impl KVStore for FaultStore {
	fn read(
		&self, p: &str, s: &str, k: &str,
	) -> Pin<Box<dyn Future<Output = io::Result<Vec<u8>>> + Send>> {
		let r = KVStoreSync::read(self, p, s, k);
		Box::pin(async move { r })
	}
	fn write(
		&self, p: &str, s: &str, k: &str, v: Vec<u8>,
	) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send>> {
		let r = KVStoreSync::write(self, p, s, k, v);
		Box::pin(async move { r })
	}
	fn remove(
		&self, p: &str, s: &str, k: &str, lazy: bool,
	) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send>> {
		let r = KVStoreSync::remove(self, p, s, k, lazy);
		Box::pin(async move { r })
	}
	fn list(
		&self, p: &str, s: &str,
	) -> Pin<Box<dyn Future<Output = io::Result<Vec<String>>> + Send>> {
		let r = KVStoreSync::list(self, p, s);
		Box::pin(async move { r })
	}
}

type Payments = DataStore<PaymentDetails, Arc<TestLogger>>;
fn fixture() -> (Arc<FaultStore>, Arc<Payments>, PaymentDetails) {
	let storage = Arc::new(FaultStore {
		inner: InMemoryStore::new(),
		failure: AtomicUsize::new(0),
		writes: AtomicUsize::new(0),
		remove_gate: Mutex::new(None),
	});
	let payments = reopen(Arc::clone(&storage), Vec::new());
	let invoice = InvoiceBuilder::new(Currency::BitcoinTestnet)
		.description("receipt".into())
		.duration_since_epoch(std::time::Duration::from_secs(1))
		.amount_milli_satoshis(2000)
		.payment_hash(sha256::Hash::hash(&[9; 32]))
		.payment_secret(PaymentSecret([1; 32]))
		.min_final_cltv_expiry_delta(18)
		.build_signed(|m| {
			Secp256k1::new().sign_ecdsa_recoverable(m, &SecretKey::from_slice(&[42; 32]).unwrap())
		})
		.unwrap();
	let hash = PaymentHash(invoice.payment_hash().to_byte_array());
	let payment = PaymentDetails {
		id: PaymentId(hash.0),
		kind: PaymentKind::Bolt11 {
			hash,
			preimage: None,
			secret: Some(*invoice.payment_secret()),
			description: Some("receipt".into()),
			bolt11: Some(invoice.to_string()),
		},
		amount_msat: Some(2000),
		fee_paid_msat: None,
		direction: PaymentDirection::Inbound,
		status: PaymentStatus::Pending,
		latest_update_timestamp: 5,
	};
	(storage, payments, payment)
}
fn reopen(storage: Arc<FaultStore>, objects: Vec<PaymentDetails>) -> Arc<Payments> {
	let store: Arc<DynStore> = storage;
	Arc::new(DataStore::new(
		objects,
		"payments".into(),
		"".into(),
		store,
		Arc::new(TestLogger::new()),
	))
}

#[test]
fn ffor_pending_confirmation_rewrites_visible_failures_and_restored_rows() {
	for failure in [1, 2] {
		let (storage, payments, expected) = fixture();
		storage.failure.store(failure, Ordering::SeqCst);
		assert_eq!(payments.confirm_ffor_pending(&expected, false), Err(FFORPaymentError::Storage));
		assert!(payments.get(&expected.id).is_none());
		storage.failure.store(2, Ordering::SeqCst);
		assert_eq!(payments.confirm_ffor_pending(&expected, false), Err(FFORPaymentError::Storage));
		assert!(payments.get(&expected.id).is_none());
		payments.confirm_ffor_pending(&expected, false).unwrap();
		assert_eq!(payments.get(&expected.id), Some(expected.clone()));
		let restored = reopen(Arc::clone(&storage), vec![expected.clone()]);
		storage.failure.store(2, Ordering::SeqCst);
		assert_eq!(restored.confirm_ffor_pending(&expected, true), Err(FFORPaymentError::Storage));
		let before = storage.writes.load(Ordering::SeqCst);
		restored.confirm_ffor_pending(&expected, true).unwrap();
		assert_eq!(storage.writes.load(Ordering::SeqCst), before + 1);
	}
}

#[test]
fn ffor_pending_confirmation_never_recreates_confirmed_or_overwrites_terminal_data() {
	let (storage, payments, expected) = fixture();
	assert_eq!(payments.confirm_ffor_pending(&expected, true), Err(FFORPaymentError::Missing));
	payments.confirm_ffor_pending(&expected, false).unwrap();
	let key = expected.id.encode_to_hex_str();
	KVStoreSync::remove(&*storage, "payments", "", &key, false).unwrap();
	assert_eq!(payments.confirm_ffor_pending(&expected, true), Err(FFORPaymentError::Missing));
	KVStoreSync::write(&*storage, "payments", "", &key, expected.encode()).unwrap();
	for status in [PaymentStatus::Failed, PaymentStatus::Succeeded] {
		let mut terminal = expected.clone();
		terminal.status = status;
		payments.insert(terminal.clone()).unwrap();
		let before = storage.writes.load(Ordering::SeqCst);
		assert_eq!(
			payments.confirm_ffor_pending(&expected, false),
			Err(FFORPaymentError::Terminal)
		);
		assert_eq!(
			payments.with_ffor_pending(&expected, || panic!("terminal publication")),
			Err(FFORPaymentError::Terminal)
		);
		assert_eq!(storage.writes.load(Ordering::SeqCst), before);
		assert_eq!(payments.get(&expected.id), Some(terminal));
	}
}

#[test]
fn ffor_pending_publication_excludes_concurrent_terminal_update() {
	let (_, payments, expected) = fixture();
	payments.confirm_ffor_pending(&expected, false).unwrap();
	let (started, start) = mpsc::channel();
	let (done, finished) = mpsc::channel();
	let worker = Arc::clone(&payments);
	let update = PaymentDetailsUpdate {
		status: Some(PaymentStatus::Failed),
		..PaymentDetailsUpdate::new(expected.id)
	};
	let join = payments
		.with_ffor_pending(&expected, || {
			assert!(payments.objects.try_lock().is_err());
			let join = std::thread::spawn(move || {
				started.send(()).unwrap();
				worker.update(&update).unwrap();
				done.send(()).unwrap();
			});
			start.recv().unwrap();
			assert!(matches!(finished.try_recv(), Err(mpsc::TryRecvError::Empty)));
			join
		})
		.unwrap();
	join.join().unwrap();
	finished.recv().unwrap();
	assert_eq!(payments.with_ffor_pending(&expected, || ()), Err(FFORPaymentError::Terminal));
}

#[test]
fn ffor_pending_removal_holds_exclusion_through_success_or_storage_failure() {
	for fail in [false, true] {
		let (storage, payments, expected) = fixture();
		payments.confirm_ffor_pending(&expected, false).unwrap();
		let (entered, started) = mpsc::channel();
		let (release, resume) = mpsc::channel();
		*storage.remove_gate.lock().unwrap() = Some((entered, resume, fail));
		let worker = Arc::clone(&payments);
		let id = expected.id;
		let removal = std::thread::spawn(move || worker.remove(&id));
		started.recv().unwrap();
		assert!(
			payments.objects.try_lock().is_err(),
			"disk deletion must share the publication exclusion"
		);
		release.send(()).unwrap();
		assert_eq!(removal.join().unwrap().is_err(), fail);
		assert_eq!(payments.confirm_ffor_pending(&expected, true), Err(FFORPaymentError::Missing));
		assert_eq!(payments.with_ffor_pending(&expected, || ()), Err(FFORPaymentError::Missing));
	}
}

#[test]
fn ffor_pending_conflicting_disk_and_invalid_candidate_never_write() {
	let (storage, payments, expected) = fixture();
	let mut invalid = expected.clone();
	invalid.amount_msat = Some(1);
	assert_eq!(
		payments.confirm_ffor_pending(&invalid, false),
		Err(FFORPaymentError::InvalidPending)
	);
	KVStoreSync::write(&*storage, "payments", "", &expected.id.encode_to_hex_str(), vec![42])
		.unwrap();
	let writes = storage.writes.load(Ordering::SeqCst);
	assert_eq!(payments.confirm_ffor_pending(&expected, false), Err(FFORPaymentError::Conflict));
	assert_eq!(storage.writes.load(Ordering::SeqCst), writes);
}

#[test]
fn ffor_pending_failed_terminal_update_cannot_be_reconfirmed_or_published() {
	for status in [PaymentStatus::Failed, PaymentStatus::Succeeded] {
		for failure in [1, 2] {
			let (storage, payments, expected) = fixture();
			payments.confirm_ffor_pending(&expected, false).unwrap();
			storage.failure.store(failure, Ordering::SeqCst);
			let update = PaymentDetailsUpdate {
				status: Some(status),
				..PaymentDetailsUpdate::new(expected.id)
			};
			assert!(payments.update(&update).is_err());
			assert_eq!(payments.get(&expected.id).unwrap().status, status);
			let writes = storage.writes.load(Ordering::SeqCst);
			assert_eq!(
				payments.confirm_ffor_pending(&expected, true),
				Err(FFORPaymentError::Terminal)
			);
			assert_eq!(
				payments.with_ffor_pending(&expected, || ()),
				Err(FFORPaymentError::Terminal)
			);
			assert_eq!(storage.writes.load(Ordering::SeqCst), writes);
		}
	}
}

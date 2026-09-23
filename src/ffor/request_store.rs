//! Durable application intent, without invoice, activation or payment authority.
//!
//! One exclusive owner retains exact arguments before native preparation. Native alone owns the
//! epoch and lifecycle. Only the opt-in runtime constructs this store. The 64-record bound matches the
//! current native archive and includes unbound intents; neither this store nor native currently
//! retires historical requests. Legacy intents reserve 4 KiB; an explicit v2 upgrade reserves 8 KiB before invoice issuance,
//! for at most 512 KiB. Exact invoice and Pending payment confirmation are historical storage facts,
//! never a readiness flag, cancellation outcome or payment-completion ledger.
//! Repeated receives on the same channel still require native epoch reuse and retention policy.
//!
//! KVStore success is the durability contract. A failed write blocks this owner until its exact
//! ciphertext is successfully rewritten. Reopened records require the same confirmation before
//! use. AEAD detects corruption and identity substitution, but not rollback of valid backups.
//! Multiple owners or external namespace writers are unsupported. Missing retained data is never
//! reconstructed from native selectors, which do not contain the application's description.
//! Exact intent comparison preserves client-ID and description bytes without normalization.
//!
//! The issuer adapter in `invoice` joins native invoice assignment to this record and to the exact
//! Pending payment row. An ambiguous payment write is retained in memory as the exact expected
//! candidate and blocks every owner operation, including previously minted handles, until the same
//! confirmation succeeds. Restart drops the candidate; the missing protected confirmation marker
//! then forces the same idempotent confirmation before any handle exists.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use bitcoin::hashes::{sha256, Hash, HashEngine};
use bitcoin::secp256k1::PublicKey;
use lightning::ln::ffor::FFORReceiverError;
use lightning::util::persist::KVStoreSync;

use crate::data_store::ffor::FFORPaymentError;
use crate::payment::store::PaymentDetails;
use crate::types::{ChannelManager, DynStore, PaymentStore};

mod envelope;
mod invoice;
mod native;
mod record;
use envelope::EnvelopeKey;
pub(super) use invoice::InvoiceProgress;
pub(super) use record::invoice::InvoicePolicy;
pub(super) use record::{RequestIntent, RequestPlan, StoredRequest};

const NAMESPACE: &str = "ffor_requests";
const LEGACY_VERSION: u16 = 1;
const VERSION: u16 = 2;
const MAX_REQUESTS: usize = 64;
// Charged in full by authenticated record version, including selector and AEAD metadata.
// Version2 reserves invoice and payment-confirmation space before native issuance.
const LEGACY_RECORD_BYTES: usize = 4096;
const MAX_RECORD_BYTES: usize = 8192;
const MAX_STORE_BYTES: usize = MAX_REQUESTS * MAX_RECORD_BYTES;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RequestStoreError {
	InvalidIntent,
	Conflict,
	Missing,
	MissingNative,
	Corrupt,
	Identity,
	Capacity,
	Entropy,
	Storage,
	Uncertain,
	Unreserved,
	Native(FFORReceiverError),
	Payment(FFORPaymentError),
}

struct PendingWrite {
	key: String,
	bytes: Vec<u8>,
	previous: Option<Vec<u8>>,
}

/// Exact Pending payment whose confirmation write returned an ambiguous storage failure.
struct PaymentCandidate {
	expected: PaymentDetails,
}

static INSTANCES: AtomicU64 = AtomicU64::new(1);

/// Exclusive storage owner. Mutable methods serialize admission, recovery and binding.
pub(super) struct RequestStore {
	manager: Arc<ChannelManager>,
	payments: Arc<PaymentStore>,
	storage: Arc<DynStore>,
	envelope: EnvelopeKey,
	chain: [u8; 32],
	node: PublicKey,
	records: BTreeMap<String, Vec<u8>>,
	confirmed: BTreeSet<String>,
	uncertain: Option<PendingWrite>,
	uncertain_payment: Option<PaymentCandidate>,
	instance: u64,
}

impl RequestStore {
	pub(in crate::ffor) fn open_bound(
		seed: &[u8; 64], chain: [u8; 32], node: PublicKey, manager: Arc<ChannelManager>,
		payments: Arc<PaymentStore>, storage: Arc<DynStore>,
	) -> Result<Self, RequestStoreError> {
		let envelope = EnvelopeKey::derive(seed, chain, node);
		let keys =
			KVStoreSync::list(&*storage, NAMESPACE, "").map_err(|_| RequestStoreError::Storage)?;
		if keys.len() > MAX_REQUESTS {
			return Err(RequestStoreError::Capacity);
		}
		let mut records = BTreeMap::new();
		for key in keys {
			if key.len() != 64
				|| !key.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
			{
				return Err(RequestStoreError::Corrupt);
			}
			let bytes = read(&*storage, &key)?;
			let record = envelope.open(&key, &bytes)?;
			record.validate_identity(chain, node)?;
			if hex(&record.local_request_id()) != key
				|| record.local_request_id() != local_id(chain, node, record.intent().client_id())
				|| record.plan().settlement == node
				|| records.insert(key, bytes).is_some()
			{
				return Err(RequestStoreError::Corrupt);
			}
		}
		Ok(Self {
			manager,
			payments,
			storage,
			envelope,
			chain,
			node,
			records,
			confirmed: BTreeSet::new(),
			uncertain: None,
			uncertain_payment: None,
			instance: INSTANCES.fetch_add(1, Ordering::SeqCst),
		})
	}

	pub(super) fn local_request_id(&self, client_id: &str) -> Result<[u8; 32], RequestStoreError> {
		record::validate_client_id(client_id)?;
		Ok(local_id(self.chain, self.node, client_id))
	}

	/// Call before selecting new liquidity. A native allocation with missing application data
	/// refuses here, before the caller can interpret absence as a new request.
	pub(super) fn lookup(
		&mut self, client_id: &str,
	) -> Result<Option<StoredRequest>, RequestStoreError> {
		self.ensure_certain()?;
		let local_request_id = self.local_request_id(client_id)?;
		let key = hex(&local_request_id);
		if self.records.contains_key(&key) {
			return self.load_key(&key).map(Some);
		}
		match read(&*self.storage, &key) {
			Err(RequestStoreError::Missing) => {
				if self
					.manager
					.find_ffor_receiver_request(local_request_id)
					.map_err(RequestStoreError::Native)?
					.is_some()
				{
					Err(RequestStoreError::Missing)
				} else {
					Ok(None)
				}
			},
			Ok(_) => Err(RequestStoreError::Conflict),
			Err(error) => Err(error),
		}
	}

	/// Restored records are authenticated and confirmed individually before being returned.
	pub(super) fn list(&mut self) -> Result<Vec<StoredRequest>, RequestStoreError> {
		self.ensure_certain()?;
		let keys: Vec<_> = self.records.keys().cloned().collect();
		keys.iter().map(|key| self.load_key(key)).collect()
	}

	fn load_key(&mut self, key: &str) -> Result<StoredRequest, RequestStoreError> {
		let expected = self.records.get(key).ok_or(RequestStoreError::Missing)?;
		let actual = read(&*self.storage, key)?;
		if &actual != expected {
			return Err(RequestStoreError::Conflict);
		}
		let record = self.envelope.open(key, &actual)?;
		record.validate_identity(self.chain, self.node)?;
		if hex(&record.local_request_id()) != key
			|| record.local_request_id()
				!= local_id(self.chain, self.node, record.intent().client_id())
			|| record.plan().settlement == self.node
		{
			return Err(RequestStoreError::Corrupt);
		}
		if !self.confirmed.contains(key) {
			self.confirm_write(key.to_owned(), actual)?;
		}
		Ok(record)
	}

	fn write_record(&mut self, record: &StoredRequest) -> Result<(), RequestStoreError> {
		let key = hex(&record.local_request_id());
		record.validate_identity(self.chain, self.node)?;
		let bytes = self.envelope.seal(&key, record)?;
		if (!self.records.contains_key(&key) && self.records.len() >= MAX_REQUESTS)
			|| self
				.reserved_bytes_without(&key)?
				.checked_add(record.reserved_bytes())
				.filter(|bytes| *bytes <= MAX_STORE_BYTES)
				.is_none()
		{
			return Err(RequestStoreError::Capacity);
		}
		self.confirm_write(key, bytes)
	}

	fn reserved_bytes_without(&self, excluded: &str) -> Result<usize, RequestStoreError> {
		self.records.iter().filter(|(key, _)| key.as_str() != excluded).try_fold(
			0usize,
			|total, (key, bytes)| {
				total
					.checked_add(self.envelope.open(key, bytes)?.reserved_bytes())
					.ok_or(RequestStoreError::Capacity)
			},
		)
	}

	fn confirm_write(&mut self, key: String, bytes: Vec<u8>) -> Result<(), RequestStoreError> {
		let previous = self
			.uncertain
			.as_ref()
			.and_then(|pending| pending.previous.clone())
			.or_else(|| self.records.get(&key).cloned());
		self.uncertain = Some(PendingWrite { key: key.clone(), bytes: bytes.clone(), previous });
		KVStoreSync::write(&*self.storage, NAMESPACE, "", &key, bytes.clone())
			.map_err(|_| RequestStoreError::Storage)?;
		self.records.insert(key.clone(), bytes);
		self.confirmed.insert(key);
		self.uncertain = None;
		Ok(())
	}

	/// Visibility is insufficient: only a successful exact retry resolves uncertain durability.
	/// The same applies to an ambiguous exact Pending payment confirmation.
	pub(super) fn recover_write(&mut self) -> Result<(), RequestStoreError> {
		if let Some(pending) = &self.uncertain {
			match read(&*self.storage, &pending.key) {
				Ok(bytes)
					if bytes == pending.bytes || pending.previous.as_ref() == Some(&bytes) => {},
				Ok(_) => return Err(RequestStoreError::Conflict),
				Err(RequestStoreError::Missing) if pending.previous.is_none() => {},
				Err(error) => return Err(error),
			}
			self.confirm_write(pending.key.clone(), pending.bytes.clone())?;
		}
		if let Some(candidate) = &self.uncertain_payment {
			let expected = candidate.expected.clone();
			self.confirm_payment_candidate(&expected, false)?;
		}
		Ok(())
	}

	/// The candidate is installed before the write and cleared only by a definite outcome. An
	/// ambiguous storage failure keeps it, blocking this owner until an exact retry succeeds.
	fn confirm_payment_candidate(
		&mut self, expected: &PaymentDetails, require_existing: bool,
	) -> Result<(), RequestStoreError> {
		self.uncertain_payment = Some(PaymentCandidate { expected: expected.clone() });
		match self.payments.confirm_ffor_pending(expected, require_existing) {
			Ok(()) => {
				self.uncertain_payment = None;
				Ok(())
			},
			Err(FFORPaymentError::Storage) => {
				Err(RequestStoreError::Payment(FFORPaymentError::Storage))
			},
			Err(error) => {
				self.uncertain_payment = None;
				Err(RequestStoreError::Payment(error))
			},
		}
	}

	fn ensure_certain(&self) -> Result<(), RequestStoreError> {
		if self.uncertain.is_some() || self.uncertain_payment.is_some() {
			Err(RequestStoreError::Uncertain)
		} else {
			Ok(())
		}
	}
}

fn read(storage: &DynStore, key: &str) -> Result<Vec<u8>, RequestStoreError> {
	KVStoreSync::read(storage, NAMESPACE, "", key).map_err(|error| {
		if error.kind() == lightning::io::ErrorKind::NotFound {
			RequestStoreError::Missing
		} else {
			RequestStoreError::Storage
		}
	})
}

fn local_id(chain: [u8; 32], node: PublicKey, client_id: &str) -> [u8; 32] {
	let mut engine = sha256::Hash::engine();
	engine.input(b"ldk-node/ffor/request-id/v1");
	engine.input(&chain);
	engine.input(&node.serialize());
	engine.input(&(client_id.len() as u16).to_be_bytes());
	engine.input(client_id.as_bytes());
	sha256::Hash::from_engine(engine).to_byte_array()
}

fn hex(bytes: &[u8]) -> String {
	const DIGITS: &[u8] = b"0123456789abcdef";
	let mut result = String::with_capacity(bytes.len() * 2);
	for byte in bytes {
		result.push(DIGITS[(byte >> 4) as usize] as char);
		result.push(DIGITS[(byte & 15) as usize] as char);
	}
	result
}

#[cfg(test)]
mod tests;

//! Immutable per-epoch witness secrets and monotonic provisioning promises, sealed before use.
//!
//! One exclusive owner serializes the secret and encrypted-receipt namespaces. KVStore has no cross-process
//! compare-and-swap, so concurrent owners are unsupported. Store success is the durability contract;
//! authenticated encryption does not detect rollback to an older valid store. Opaque native history
//! supplies record bindings. Live native registration, fetch response correlation, transport and
//! native claim reconciliation and deletion are deliberately absent. Retained acknowledgements are historical promises, not current
//! provisioning authority or invoice readiness.
//! A future native registration must distinguish a new epoch from a missing sidecar after restore;
//! `load` never recreates secrets, and `create` is only for explicitly new registration material.
//!
//! Fresh material reserves the exact full receipt envelope before any write. Pending initialization
//! retains its quota across restart; Reserved records with missing receipts refuse recovery. Legacy
//! records remain historical-only. Evidence keeps the first authenticated encrypted core per W/slot,
//! without guardian attachments, plaintext preimages, eviction or inferred unpaid status.
//!
//! Fetch retries require new mailbox-wide nonces, including after a lost reply or restart. Provision
//! retries retain the exact signed manifest and use fresh connection-scoped request identifiers.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex};

use bitcoin::hashes::{sha256, Hash, HashEngine};
use lightning::ln::ffor::FFORWitnessReceipt;
use lightning::util::persist::KVStoreSync;
use lightning_ffor::witness::{CheckedAcknowledgement, EncryptedRecord};

use crate::types::DynStore;

mod binding;
mod envelope;
mod receipt;
mod record;
use receipt::{ReceiptBook, ReceiptRetention};
use record::ReceiptAllocation;

pub(super) use binding::WitnessStorageBinding;
#[cfg(test)]
use binding::WitnessStorageIdentity;
use envelope::WrappingKey;
#[cfg(test)]
use record::WitnessKeyUseError;
pub(super) use record::{StoredWitnessEpoch, WitnessPolicy};

const PRIMARY_NAMESPACE: &str = "ffor_witness";
const SECONDARY_NAMESPACE: &str = "";
// Implementation admission caps, not protocol promises. Four maximum canonical manifests plus
// their private material fit below the per-record cap. No record is evicted to admit another.
const MAX_WITNESSES: usize = 4;
const MAX_RECORD_BYTES: usize = 512 * 1024;
const MAX_STORE_BYTES: usize = 8 * 1024 * 1024;
const MAX_EPOCHS: usize = 64;
const SCHEMA_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WitnessStoreError {
	Storage,
	Missing,
	Corrupt,
	Binding,
	Conflict,
	Capacity,
	Entropy,
	Uncertain,
	Unreserved,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Namespace {
	Secrets,
	Receipts,
}

impl Namespace {
	fn name(self) -> &'static str {
		match self {
			Self::Secrets => PRIMARY_NAMESPACE,
			Self::Receipts => receipt::NAMESPACE,
		}
	}
}

struct PendingWrite {
	namespace: Namespace,
	key: String,
	bytes: Vec<u8>,
	// Only this exact predecessor may remain after a failed update. Absence is permitted only
	// for creation, never for an update to already retained recovery material.
	previous: Option<Vec<u8>>,
}

struct StoreState {
	// Ciphertext only. This bounds inventory and detects replacement by another owner.
	records: BTreeMap<String, Vec<u8>>,
	// A read may expose a rename whose directory fsync failed. Restored entries require one
	// authenticated, byte-identical successful write before this owner can return their material.
	confirmed: BTreeSet<String>,
	uncertain: Option<PendingWrite>,
	receipt_records: BTreeMap<String, Vec<u8>>,
	receipt_confirmed: BTreeSet<String>,
	receipt_reservations: BTreeMap<String, usize>,
}

pub(super) struct WitnessSecretStore {
	storage: Arc<DynStore>,
	wrapping_key: WrappingKey,
	receipt_key: WrappingKey,
	state: Mutex<StoreState>,
}

impl fmt::Debug for WitnessSecretStore {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("WitnessSecretStore").finish_non_exhaustive()
	}
}

impl WitnessSecretStore {
	/// Opens an exclusive, bounded namespace. The borrowed wallet seed never leaves this call.
	/// Loading this sidecar does not establish activation, funding state, or backup freshness.
	pub(super) fn open(seed: &[u8; 64], storage: Arc<DynStore>) -> Result<Self, WitnessStoreError> {
		let records = inventory(&*storage, Namespace::Secrets, MAX_RECORD_BYTES, MAX_STORE_BYTES)?;
		let receipt_records = inventory(
			&*storage,
			Namespace::Receipts,
			receipt::MAX_RECEIPT_RECORD_BYTES,
			receipt::MAX_RECEIPT_STORE_BYTES,
		)?;
		let wrapping_key = WrappingKey::derive(seed);
		let receipt_key = WrappingKey::derive_receipts(seed);
		let mut receipt_reservations = BTreeMap::new();
		let mut total = 0usize;
		for (key, bytes) in &records {
			let (digest, plaintext) = wrapping_key.open_inventory(key, bytes)?;
			let allocation = StoredWitnessEpoch::inventory_allocation(&plaintext, digest)?;
			let reserved = allocation.bytes();
			if reserved != 0 {
				total = total.checked_add(reserved).ok_or(WitnessStoreError::Capacity)?;
				if total > receipt::MAX_RECEIPT_STORE_BYTES {
					return Err(WitnessStoreError::Capacity);
				}
				receipt_reservations.insert(key.clone(), reserved);
			}
			match receipt_records.get(key) {
				Some(receipt) => {
					if receipt.len() != reserved {
						return Err(WitnessStoreError::Corrupt);
					}
					let (actual, plaintext) = receipt_key.open_inventory(key, receipt)?;
					if actual != digest {
						return Err(WitnessStoreError::Binding);
					}
					ReceiptBook::validate_inventory(&plaintext, digest)?;
				},
				None if matches!(allocation, ReceiptAllocation::Reserved(_)) => {
					return Err(WitnessStoreError::Missing)
				},
				None => {},
			}
		}
		if receipt_records.keys().any(|key| !records.contains_key(key)) {
			return Err(WitnessStoreError::Missing);
		}
		Ok(Self {
			storage,
			wrapping_key,
			receipt_key,
			state: Mutex::new(StoreState {
				records,
				confirmed: BTreeSet::new(),
				uncertain: None,
				receipt_records,
				receipt_confirmed: BTreeSet::new(),
				receipt_reservations,
			}),
		})
	}

	/// Creates one immutable record or returns the identical existing witness policy and manifests.
	/// Any failed write blocks this entire owner until `recover_write` resolves that exact attempt.
	pub(super) fn create(
		&self, binding: &WitnessStorageBinding, policies: &[WitnessPolicy],
	) -> Result<StoredWitnessEpoch, WitnessStoreError> {
		let mut state = self.state.lock().map_err(|_| WitnessStoreError::Uncertain)?;
		ensure_certain(&state)?;
		let policies = record::checked_policies(policies)?;
		let key = binding.storage_key();
		if state.records.contains_key(&key) {
			let record = self.load_locked(&mut state, binding)?;
			if record.policies() != policies {
				return Err(WitnessStoreError::Conflict);
			}
			return Ok(record);
		}
		if state.records.len() == MAX_EPOCHS {
			return Err(WitnessStoreError::Capacity);
		}
		match read(&*self.storage, Namespace::Secrets, &key) {
			Err(WitnessStoreError::Missing) => {},
			Ok(_) => return Err(WitnessStoreError::Conflict),
			Err(error) => return Err(error),
		}
		match read(&*self.storage, Namespace::Receipts, &key) {
			Err(WitnessStoreError::Missing) => {},
			Ok(_) => return Err(WitnessStoreError::Conflict),
			Err(error) => return Err(error),
		}
		let mut record = StoredWitnessEpoch::generate(binding, &policies)?;
		let receipt = self
			.receipt_key
			.seal_plaintext(binding, &ReceiptBook::empty(binding, &record).encode())?;
		record.receipt_allocation = ReceiptAllocation::Pending(
			receipt.len().try_into().map_err(|_| WitnessStoreError::Capacity)?,
		);
		let bytes = self.wrapping_key.seal(binding, &record)?;
		// Both complete encrypted envelopes, including metadata overhead, are charged before
		// the first write. Pending and Reserved plaintext differ in exactly one fixed byte.
		let secret_total = state.records.values().map(Vec::len).sum::<usize>();
		let receipt_total = state.receipt_reservations.values().sum::<usize>();
		if secret_total.checked_add(bytes.len()).filter(|n| *n <= MAX_STORE_BYTES).is_none()
			|| receipt_total
				.checked_add(receipt.len())
				.filter(|n| *n <= receipt::MAX_RECEIPT_STORE_BYTES)
				.is_none()
		{
			return Err(WitnessStoreError::Capacity);
		}
		state.receipt_reservations.insert(key.clone(), receipt.len());
		self.confirm_write(&mut state, Namespace::Secrets, key.clone(), bytes)?;
		self.confirm_write(&mut state, Namespace::Receipts, key, receipt)?;
		self.finish_initialization(&mut state, binding, &mut record)?;
		Ok(record)
	}

	/// Reads existing material only. Missing, altered or unauthenticated records never regenerate keys.
	pub(super) fn load(
		&self, binding: &WitnessStorageBinding,
	) -> Result<StoredWitnessEpoch, WitnessStoreError> {
		let mut state = self.state.lock().map_err(|_| WitnessStoreError::Uncertain)?;
		ensure_certain(&state)?;
		self.load_locked(&mut state, binding)
	}

	/// Durably retain a correlated witness promise for the exact stored manifest.
	/// The caller owns real transport correlation and current native provisioning authority.
	/// Neither a returned record nor all historical promises authorize an offline invoice.
	pub(super) fn acknowledge<C>(
		&self, binding: &WitnessStorageBinding, checked: &CheckedAcknowledgement<C>,
	) -> Result<StoredWitnessEpoch, WitnessStoreError> {
		let mut state = self.state.lock().map_err(|_| WitnessStoreError::Uncertain)?;
		ensure_certain(&state)?;
		let mut record = self.load_locked(&mut state, binding)?;
		if !record.retain_acknowledgement(checked)? {
			return Ok(record);
		}
		let key = binding.storage_key();
		let bytes = self.wrapping_key.seal(binding, &record)?;
		let previous = state.records.get(&key).ok_or(WitnessStoreError::Missing)?;
		let total = state.records.values().map(Vec::len).sum::<usize>() - previous.len();
		if total.checked_add(bytes.len()).filter(|sum| *sum <= MAX_STORE_BYTES).is_none() {
			return Err(WitnessStoreError::Capacity);
		}
		self.confirm_write(&mut state, Namespace::Secrets, key, bytes)?;
		Ok(record)
	}

	fn load_locked(
		&self, state: &mut StoreState, binding: &WitnessStorageBinding,
	) -> Result<StoredWitnessEpoch, WitnessStoreError> {
		let key = binding.storage_key();
		let expected = state.records.get(&key).ok_or(WitnessStoreError::Missing)?;
		let actual = read(&*self.storage, Namespace::Secrets, &key)?;
		if &actual != expected {
			return Err(WitnessStoreError::Conflict);
		}
		let mut record = self.wrapping_key.open(binding, &actual)?;
		if !state.confirmed.contains(&key) {
			self.confirm_write(state, Namespace::Secrets, key.clone(), actual)?;
		}
		match record.receipt_allocation {
			ReceiptAllocation::Pending(_) => {
				self.finish_initialization(state, binding, &mut record)?
			},
			ReceiptAllocation::Reserved(_) => {
				self.load_receipt_locked(state, binding, &record)?;
			},
			ReceiptAllocation::Legacy => {},
		}
		Ok(record)
	}

	fn finish_initialization(
		&self, state: &mut StoreState, binding: &WitnessStorageBinding,
		record: &mut StoredWitnessEpoch,
	) -> Result<(), WitnessStoreError> {
		let reserved = match record.receipt_allocation {
			ReceiptAllocation::Pending(size) => size,
			_ => return Err(WitnessStoreError::Unreserved),
		};
		let key = binding.storage_key();
		if state.receipt_reservations.get(&key) != Some(&(reserved as usize)) {
			return Err(WitnessStoreError::Corrupt);
		}
		if !state.receipt_records.contains_key(&key) {
			match read(&*self.storage, Namespace::Receipts, &key) {
				Err(WitnessStoreError::Missing) => {},
				Ok(_) => return Err(WitnessStoreError::Conflict),
				Err(error) => return Err(error),
			}
			let bytes = self
				.receipt_key
				.seal_plaintext(binding, &ReceiptBook::empty(binding, record).encode())?;
			if bytes.len() != reserved as usize {
				return Err(WitnessStoreError::Corrupt);
			}
			self.confirm_write(state, Namespace::Receipts, key.clone(), bytes)?;
		}
		let receipts = self.load_receipt_locked(state, binding, record)?;
		if !receipts.is_empty() {
			return Err(WitnessStoreError::Conflict);
		}
		record.receipt_allocation = ReceiptAllocation::Reserved(reserved);
		let bytes = self.wrapping_key.seal(binding, record)?;
		if bytes.len() != state.records.get(&key).ok_or(WitnessStoreError::Missing)?.len() {
			return Err(WitnessStoreError::Corrupt);
		}
		self.confirm_write(state, Namespace::Secrets, key, bytes)
	}

	fn load_receipt_locked(
		&self, state: &mut StoreState, binding: &WitnessStorageBinding, record: &StoredWitnessEpoch,
	) -> Result<ReceiptBook, WitnessStoreError> {
		if record.receipt_allocation == ReceiptAllocation::Legacy {
			return Err(WitnessStoreError::Unreserved);
		}
		let key = binding.storage_key();
		let expected = state.receipt_records.get(&key).ok_or(WitnessStoreError::Missing)?;
		let actual = read(&*self.storage, Namespace::Receipts, &key)?;
		if &actual != expected {
			return Err(WitnessStoreError::Conflict);
		}
		if actual.len() != record.receipt_allocation.bytes() {
			return Err(WitnessStoreError::Corrupt);
		}
		let plaintext = self.receipt_key.open_bound(binding, &actual)?;
		let receipts = ReceiptBook::decode(binding, record, &plaintext)?;
		if !state.receipt_confirmed.contains(&key) {
			self.confirm_write(state, Namespace::Receipts, key, actual)?;
		}
		Ok(receipts)
	}

	/// Retains the first valid signed encrypted core. No native claim or payment event occurs.
	pub(super) fn retain_receipt(
		&self, binding: &WitnessStorageBinding, witness: bitcoin::secp256k1::PublicKey,
		receipt: &EncryptedRecord,
	) -> Result<ReceiptRetention, WitnessStoreError> {
		let mut state = self.state.lock().map_err(|_| WitnessStoreError::Uncertain)?;
		ensure_certain(&state)?;
		let record = self.load_locked(&mut state, binding)?;
		let mut receipts = self.load_receipt_locked(&mut state, binding, &record)?;
		record.decrypt_record(witness, receipt.clone()).map_err(|_| WitnessStoreError::Corrupt)?;
		let result = receipts.insert(witness, receipt)?;
		if result == ReceiptRetention::Stored {
			let bytes = self.receipt_key.seal_plaintext(binding, &receipts.encode())?;
			if bytes.len() != record.receipt_allocation.bytes() {
				return Err(WitnessStoreError::Corrupt);
			}
			self.confirm_write(&mut state, Namespace::Receipts, binding.storage_key(), bytes)?;
		}
		Ok(result)
	}

	/// Re-decrypts durable retained evidence for a future native reconciliation boundary.
	/// Absence is no evidence of an unpaid voucher. This does not claim channel settlement.
	pub(super) fn load_receipt(
		&self, binding: &WitnessStorageBinding, witness: bitcoin::secp256k1::PublicKey, slot: u16,
	) -> Result<Option<FFORWitnessReceipt>, WitnessStoreError> {
		let mut state = self.state.lock().map_err(|_| WitnessStoreError::Uncertain)?;
		ensure_certain(&state)?;
		let record = self.load_locked(&mut state, binding)?;
		let receipts = self.load_receipt_locked(&mut state, binding, &record)?;
		receipts
			.get(witness, slot)?
			.map(|receipt| {
				record.decrypt_record(witness, receipt).map_err(|_| WitnessStoreError::Corrupt)
			})
			.transpose()
	}

	fn confirm_write(
		&self, state: &mut StoreState, namespace: Namespace, key: String, bytes: Vec<u8>,
	) -> Result<(), WitnessStoreError> {
		// Preserve the original predecessor through retries, including when the proposed new
		// ciphertext became visible but the backend reported a failed sync.
		let previous = state
			.uncertain
			.as_ref()
			.and_then(|pending| pending.previous.clone())
			.or_else(|| state.records(namespace).get(&key).cloned());
		state.uncertain =
			Some(PendingWrite { namespace, key: key.clone(), bytes: bytes.clone(), previous });
		write(&*self.storage, namespace, &key, bytes.clone())?;
		state.confirmed_mut(namespace).insert(key.clone());
		state.records_mut(namespace).insert(key, bytes);
		state.uncertain = None;
		Ok(())
	}

	/// Reconciles an uncertain write, accepting only its exact candidate or predecessor.
	/// An absent key is acceptable only for creation, never for an existing record update.
	/// Always rewrites retained ciphertext: visibility through read does not establish durability.
	/// No fresh keys, signatures or encryption nonces are allocated.
	/// A read error, conflict, or another failed write leaves the owner blocked.
	pub(super) fn recover_write(&self) -> Result<(), WitnessStoreError> {
		let mut state = self.state.lock().map_err(|_| WitnessStoreError::Uncertain)?;
		let pending = match &state.uncertain {
			Some(pending) => pending,
			None => return Ok(()),
		};
		match read(&*self.storage, pending.namespace, &pending.key) {
			Ok(bytes) if bytes == pending.bytes => {},
			Ok(bytes) if pending.previous.as_ref() == Some(&bytes) => {},
			Ok(_) => return Err(WitnessStoreError::Conflict),
			Err(WitnessStoreError::Missing) if pending.previous.is_none() => {},
			Err(error) => return Err(error),
		}
		let key = pending.key.clone();
		let bytes = pending.bytes.clone();
		let namespace = pending.namespace;
		self.confirm_write(&mut state, namespace, key, bytes)
	}
}

impl StoreState {
	fn records(&self, namespace: Namespace) -> &BTreeMap<String, Vec<u8>> {
		match namespace {
			Namespace::Secrets => &self.records,
			Namespace::Receipts => &self.receipt_records,
		}
	}
	fn records_mut(&mut self, namespace: Namespace) -> &mut BTreeMap<String, Vec<u8>> {
		match namespace {
			Namespace::Secrets => &mut self.records,
			Namespace::Receipts => &mut self.receipt_records,
		}
	}
	fn confirmed_mut(&mut self, namespace: Namespace) -> &mut BTreeSet<String> {
		match namespace {
			Namespace::Secrets => &mut self.confirmed,
			Namespace::Receipts => &mut self.receipt_confirmed,
		}
	}
}

fn inventory(
	storage: &DynStore, namespace: Namespace, maximum_record: usize, maximum_total: usize,
) -> Result<BTreeMap<String, Vec<u8>>, WitnessStoreError> {
	let keys = KVStoreSync::list(storage, namespace.name(), SECONDARY_NAMESPACE)
		.map_err(|_| WitnessStoreError::Storage)?;
	if keys.len() > MAX_EPOCHS {
		return Err(WitnessStoreError::Capacity);
	}
	let mut records = BTreeMap::new();
	let mut total = 0usize;
	for key in keys {
		if key.len() != 64
			|| !key.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
		{
			return Err(WitnessStoreError::Corrupt);
		}
		let bytes = read(storage, namespace, &key)?;
		if bytes.is_empty() || bytes.len() > maximum_record {
			return Err(WitnessStoreError::Capacity);
		}
		total = total.checked_add(bytes.len()).ok_or(WitnessStoreError::Capacity)?;
		if total > maximum_total || records.insert(key, bytes).is_some() {
			return Err(WitnessStoreError::Capacity);
		}
	}
	Ok(records)
}

fn ensure_certain(state: &StoreState) -> Result<(), WitnessStoreError> {
	if state.uncertain.is_some() {
		Err(WitnessStoreError::Uncertain)
	} else {
		Ok(())
	}
}

fn read(storage: &DynStore, namespace: Namespace, key: &str) -> Result<Vec<u8>, WitnessStoreError> {
	KVStoreSync::read(storage, namespace.name(), SECONDARY_NAMESPACE, key).map_err(|error| {
		if error.kind() == lightning::io::ErrorKind::NotFound {
			WitnessStoreError::Missing
		} else {
			WitnessStoreError::Storage
		}
	})
}

fn write(
	storage: &DynStore, namespace: Namespace, key: &str, bytes: Vec<u8>,
) -> Result<(), WitnessStoreError> {
	KVStoreSync::write(storage, namespace.name(), SECONDARY_NAMESPACE, key, bytes)
		.map_err(|_| WitnessStoreError::Storage)
}

fn hash(parts: &[&[u8]]) -> [u8; 32] {
	let mut engine = sha256::Hash::engine();
	for part in parts {
		engine.input(part);
	}
	sha256::Hash::from_engine(engine).to_byte_array()
}

#[cfg(test)]
mod tests;

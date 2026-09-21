//! Immutable per-epoch witness secrets sealed before they can leave the storage boundary.
//!
//! One exclusive owner serializes all operations for this namespace. KVStore has no cross-process
//! compare-and-swap, so concurrent owners are unsupported. Store success is the durability contract;
//! authenticated encryption does not detect rollback to an older valid store. Opaque native history
//! supplies record bindings. Live native registration, fetch nonce allocation, transport, witness
//! acknowledgement handling and deletion are deliberately absent.
//! A future native registration must distinguish a new epoch from a missing sidecar after restore;
//! `load` never recreates secrets, and `create` is only for explicitly new registration material.
//!
//! Fetch retries require new mailbox-wide nonces, including after a lost reply or restart. Provision
//! retries retain the exact signed manifest and use fresh connection-scoped request identifiers.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex};

use bitcoin::hashes::{sha256, Hash, HashEngine};
use lightning::util::persist::KVStoreSync;

use crate::types::DynStore;

mod binding;
mod envelope;
mod record;

pub(super) use binding::WitnessStorageBinding;
#[cfg(test)]
use binding::WitnessStorageIdentity;
use envelope::WrappingKey;
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
}

struct PendingWrite {
	key: String,
	bytes: Vec<u8>,
}

struct StoreState {
	// Ciphertext only. This bounds inventory and detects replacement by another owner.
	records: BTreeMap<String, Vec<u8>>,
	// A read may expose a rename whose directory fsync failed. Restored entries require one
	// authenticated, byte-identical successful write before this owner can return their material.
	confirmed: BTreeSet<String>,
	uncertain: Option<PendingWrite>,
}

pub(super) struct WitnessSecretStore {
	storage: Arc<DynStore>,
	wrapping_key: WrappingKey,
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
		let keys = KVStoreSync::list(&*storage, PRIMARY_NAMESPACE, SECONDARY_NAMESPACE)
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
			let bytes = read(&*storage, &key)?;
			check_record_size(&bytes)?;
			total = total.checked_add(bytes.len()).ok_or(WitnessStoreError::Capacity)?;
			if total > MAX_STORE_BYTES || records.insert(key, bytes).is_some() {
				return Err(WitnessStoreError::Capacity);
			}
		}
		Ok(Self {
			storage,
			wrapping_key: WrappingKey::derive(seed),
			state: Mutex::new(StoreState { records, confirmed: BTreeSet::new(), uncertain: None }),
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
		match read(&*self.storage, &key) {
			Err(WitnessStoreError::Missing) => {},
			Ok(_) => return Err(WitnessStoreError::Conflict),
			Err(error) => return Err(error),
		}
		let record = StoredWitnessEpoch::generate(binding, &policies)?;
		let bytes = self.wrapping_key.seal(binding, &record)?;
		check_record_size(&bytes)?;
		let total = state.records.values().map(Vec::len).sum::<usize>();
		if total.checked_add(bytes.len()).filter(|sum| *sum <= MAX_STORE_BYTES).is_none() {
			return Err(WitnessStoreError::Capacity);
		}
		self.confirm_write(&mut state, key, bytes)?;
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

	fn load_locked(
		&self, state: &mut StoreState, binding: &WitnessStorageBinding,
	) -> Result<StoredWitnessEpoch, WitnessStoreError> {
		let key = binding.storage_key();
		let expected = state.records.get(&key).ok_or(WitnessStoreError::Missing)?;
		let actual = read(&*self.storage, &key)?;
		if &actual != expected {
			return Err(WitnessStoreError::Conflict);
		}
		let record = self.wrapping_key.open(binding, &actual)?;
		if !state.confirmed.contains(&key) {
			self.confirm_write(state, key, actual)?;
		}
		Ok(record)
	}

	fn confirm_write(
		&self, state: &mut StoreState, key: String, bytes: Vec<u8>,
	) -> Result<(), WitnessStoreError> {
		state.uncertain = Some(PendingWrite { key: key.clone(), bytes: bytes.clone() });
		write(&*self.storage, &key, bytes.clone())?;
		state.confirmed.insert(key.clone());
		state.records.insert(key, bytes);
		state.uncertain = None;
		Ok(())
	}

	/// Reconciles an uncertain write, accepting only identical ciphertext or an absent key.
	/// Always rewrites retained ciphertext: visibility through read does not establish durability.
	/// No fresh keys, signatures or encryption nonces are allocated.
	/// A read error, conflict, or another failed write leaves the owner blocked.
	pub(super) fn recover_write(&self) -> Result<(), WitnessStoreError> {
		let mut state = self.state.lock().map_err(|_| WitnessStoreError::Uncertain)?;
		let pending = match &state.uncertain {
			Some(pending) => pending,
			None => return Ok(()),
		};
		match read(&*self.storage, &pending.key) {
			Ok(bytes) if bytes == pending.bytes => {},
			Ok(_) => return Err(WitnessStoreError::Conflict),
			Err(WitnessStoreError::Missing) => {},
			Err(error) => return Err(error),
		}
		let key = pending.key.clone();
		let bytes = pending.bytes.clone();
		self.confirm_write(&mut state, key, bytes)
	}
}

fn ensure_certain(state: &StoreState) -> Result<(), WitnessStoreError> {
	if state.uncertain.is_some() {
		Err(WitnessStoreError::Uncertain)
	} else {
		Ok(())
	}
}

fn check_record_size(bytes: &[u8]) -> Result<(), WitnessStoreError> {
	if bytes.is_empty() || bytes.len() > MAX_RECORD_BYTES {
		Err(WitnessStoreError::Capacity)
	} else {
		Ok(())
	}
}

fn read(storage: &DynStore, key: &str) -> Result<Vec<u8>, WitnessStoreError> {
	KVStoreSync::read(storage, PRIMARY_NAMESPACE, SECONDARY_NAMESPACE, key).map_err(|error| {
		if error.kind() == lightning::io::ErrorKind::NotFound {
			WitnessStoreError::Missing
		} else {
			WitnessStoreError::Storage
		}
	})
}

fn write(storage: &DynStore, key: &str, bytes: Vec<u8>) -> Result<(), WitnessStoreError> {
	KVStoreSync::write(storage, PRIMARY_NAMESPACE, SECONDARY_NAMESPACE, key, bytes)
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

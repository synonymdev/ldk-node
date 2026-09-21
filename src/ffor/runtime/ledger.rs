//! Durable, monotone completion intents for fulfilled vouchers and cancellation markers.
//!
//! One record per `(channel, epoch, slot)` moves `Intended -> Credited -> Notified`. Every step
//! is idempotent, so a crash between any two writes is resumed by re-running the sequence from
//! the persisted state. The record contains only the payment hash, amount and client ID that
//! already exist in the plaintext payment ledger; it is not a payment credit by itself.

use std::fmt;

use lightning::ln::types::ChannelId;
use lightning::util::persist::KVStoreSync;
use lightning_types::payment::PaymentHash;

use crate::types::DynStore;

pub(super) const NAMESPACE: &str = "ffor_runtime";
const VERSION: u16 = 1;
const MAX_CLIENT_ID_BYTES: usize = 128;
const MAX_RECORDS: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum IntentState {
	Intended,
	Credited,
	Notified,
}

impl IntentState {
	fn byte(self) -> u8 {
		match self {
			Self::Intended => 1,
			Self::Credited => 2,
			Self::Notified => 3,
		}
	}
	fn from_byte(byte: u8) -> Result<Self, LedgerError> {
		match byte {
			1 => Ok(Self::Intended),
			2 => Ok(Self::Credited),
			3 => Ok(Self::Notified),
			_ => Err(LedgerError::Corrupt),
		}
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct IntentKey {
	pub(crate) channel: ChannelId,
	pub(crate) epoch: [u8; 32],
	pub(crate) slot: u16,
}

impl IntentKey {
	fn storage_key(&self) -> String {
		format!("outcome_{}_{}_{}", hex(&self.channel.0), hex(&self.epoch), self.slot)
	}
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct OutcomeIntent {
	pub(crate) key: IntentKey,
	pub(crate) state: IntentState,
	pub(crate) payment_hash: PaymentHash,
	pub(crate) amount_msat: u64,
	pub(crate) client_id: String,
}

impl fmt::Debug for OutcomeIntent {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("OutcomeIntent")
			.field("slot", &self.key.slot)
			.field("state", &self.state)
			.field("amount_msat", &self.amount_msat)
			.finish_non_exhaustive()
	}
}

impl OutcomeIntent {
	fn encode(&self) -> Vec<u8> {
		let mut out = Vec::with_capacity(2 + 1 + 32 + 32 + 2 + 32 + 8 + 2 + self.client_id.len());
		out.extend_from_slice(&VERSION.to_be_bytes());
		out.push(self.state.byte());
		out.extend_from_slice(&self.key.channel.0);
		out.extend_from_slice(&self.key.epoch);
		out.extend_from_slice(&self.key.slot.to_be_bytes());
		out.extend_from_slice(&self.payment_hash.0);
		out.extend_from_slice(&self.amount_msat.to_be_bytes());
		out.extend_from_slice(&(self.client_id.len() as u16).to_be_bytes());
		out.extend_from_slice(self.client_id.as_bytes());
		out
	}

	fn decode(bytes: &[u8]) -> Result<Self, LedgerError> {
		let mut reader = Reader(bytes);
		if u16::from_be_bytes(reader.array()?) != VERSION {
			return Err(LedgerError::Corrupt);
		}
		let state = IntentState::from_byte(reader.array::<1>()?[0])?;
		let channel = ChannelId(reader.array()?);
		let epoch = reader.array()?;
		let slot = u16::from_be_bytes(reader.array()?);
		let payment_hash = PaymentHash(reader.array()?);
		let amount_msat = u64::from_be_bytes(reader.array()?);
		let len = usize::from(u16::from_be_bytes(reader.array()?));
		if len == 0 || len > MAX_CLIENT_ID_BYTES {
			return Err(LedgerError::Corrupt);
		}
		let client_id =
			String::from_utf8(reader.take(len)?.to_vec()).map_err(|_| LedgerError::Corrupt)?;
		if !reader.0.is_empty() || slot == 0 || amount_msat == 0 {
			return Err(LedgerError::Corrupt);
		}
		Ok(Self {
			key: IntentKey { channel, epoch, slot },
			state,
			payment_hash,
			amount_msat,
			client_id,
		})
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LedgerError {
	Storage,
	Corrupt,
	Conflict,
	Capacity,
}

/// Plain KV records. Storage success is the durability contract; a failed write is retried by
/// re-running the idempotent step, never by trusting visible bytes.
pub(crate) struct OutcomeLedger {
	storage: std::sync::Arc<DynStore>,
}

impl OutcomeLedger {
	pub(crate) fn new(storage: std::sync::Arc<DynStore>) -> Self {
		Self { storage }
	}

	pub(crate) fn load(&self, key: &IntentKey) -> Result<Option<OutcomeIntent>, LedgerError> {
		match KVStoreSync::read(&*self.storage, NAMESPACE, "", &key.storage_key()) {
			Ok(bytes) => {
				let intent = OutcomeIntent::decode(&bytes)?;
				if intent.key != *key {
					return Err(LedgerError::Corrupt);
				}
				Ok(Some(intent))
			},
			Err(error) if error.kind() == lightning::io::ErrorKind::NotFound => Ok(None),
			Err(_) => Err(LedgerError::Storage),
		}
	}

	/// Persist `intent`. The state may only stay equal or advance; payment identity is immutable.
	pub(crate) fn write(&self, intent: &OutcomeIntent) -> Result<(), LedgerError> {
		if let Some(existing) = self.load(&intent.key)? {
			if existing.payment_hash != intent.payment_hash
				|| existing.amount_msat != intent.amount_msat
				|| existing.client_id != intent.client_id
				|| existing.state > intent.state
			{
				return Err(LedgerError::Conflict);
			}
		}
		KVStoreSync::write(
			&*self.storage,
			NAMESPACE,
			"",
			&intent.key.storage_key(),
			intent.encode(),
		)
		.map_err(|_| LedgerError::Storage)
	}

	/// Every intent that has not reached `Notified`, for resumption after restart.
	pub(crate) fn unfinished(&self) -> Result<Vec<OutcomeIntent>, LedgerError> {
		let keys =
			KVStoreSync::list(&*self.storage, NAMESPACE, "").map_err(|_| LedgerError::Storage)?;
		if keys.len() > MAX_RECORDS {
			return Err(LedgerError::Capacity);
		}
		let mut out = Vec::new();
		for key in keys.iter().filter(|key| key.starts_with("outcome_")) {
			let bytes = KVStoreSync::read(&*self.storage, NAMESPACE, "", key)
				.map_err(|_| LedgerError::Storage)?;
			let intent = OutcomeIntent::decode(&bytes)?;
			if intent.key.storage_key() != *key {
				return Err(LedgerError::Corrupt);
			}
			if intent.state != IntentState::Notified {
				out.push(intent);
			}
		}
		Ok(out)
	}

	pub(crate) fn mark_cancelled(&self, local_request_id: [u8; 32]) -> Result<(), LedgerError> {
		KVStoreSync::write(
			&*self.storage,
			NAMESPACE,
			"",
			&cancel_key(local_request_id),
			VERSION.to_be_bytes().to_vec(),
		)
		.map_err(|_| LedgerError::Storage)
	}

	pub(crate) fn is_cancelled(&self, local_request_id: [u8; 32]) -> Result<bool, LedgerError> {
		match KVStoreSync::read(&*self.storage, NAMESPACE, "", &cancel_key(local_request_id)) {
			Ok(bytes) if bytes == VERSION.to_be_bytes() => Ok(true),
			Ok(_) => Err(LedgerError::Corrupt),
			Err(error) if error.kind() == lightning::io::ErrorKind::NotFound => Ok(false),
			Err(_) => Err(LedgerError::Storage),
		}
	}
}

fn cancel_key(local_request_id: [u8; 32]) -> String {
	format!("cancel_{}", hex(&local_request_id))
}

pub(super) fn hex(bytes: &[u8]) -> String {
	const DIGITS: &[u8] = b"0123456789abcdef";
	let mut result = String::with_capacity(bytes.len() * 2);
	for byte in bytes {
		result.push(DIGITS[(byte >> 4) as usize] as char);
		result.push(DIGITS[(byte & 15) as usize] as char);
	}
	result
}

struct Reader<'a>(&'a [u8]);
impl<'a> Reader<'a> {
	fn take(&mut self, count: usize) -> Result<&'a [u8], LedgerError> {
		let value = self.0.get(..count).ok_or(LedgerError::Corrupt)?;
		self.0 = &self.0[count..];
		Ok(value)
	}
	fn array<const N: usize>(&mut self) -> Result<[u8; N], LedgerError> {
		self.take(N)?.try_into().map_err(|_| LedgerError::Corrupt)
	}
}

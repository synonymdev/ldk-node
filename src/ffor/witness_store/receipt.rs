//! Fixed encrypted evidence reservations. These contain no plaintext preimages or payment status.

use bitcoin::secp256k1::PublicKey;
use lightning_ffor::witness::{EncryptedRecord, CIPHERTEXT_LEN, RECORD_HEADER_LEN};
use zeroize::Zeroizing;

use super::{
	hash, StoredWitnessEpoch, WitnessKeyUseError, WitnessStorageBinding, WitnessStoreError,
	MAX_WITNESSES,
};

pub(super) const NAMESPACE: &str = "ffor_witness_receipts";
pub(super) const MAX_RECEIPT_RECORD_BYTES: usize = 1024 * 1024;
pub(super) const MAX_RECEIPT_STORE_BYTES: usize = 8 * 1024 * 1024;
const VERSION: u16 = 1;
// The decoder below verifies this canonical prefix against the actual shared encoder. Guardian
// attachments follow this prefix and are excluded: they are unsigned, non-authoritative bytes.
const CORE_BYTES: usize = RECORD_HEADER_LEN + 64 + 2 + CIPHERTEXT_LEN;
const SLOT_BYTES: usize = 1 + CORE_BYTES;

type Core = [u8; CORE_BYTES];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReceiptRetention {
	Stored,
	AlreadyStored,
}

/// All valid page evidence is durable. Rejected means at least one candidate was invalid or
/// differed from the already retained valid core. Neither variant conveys payment status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReceiptPageRetention {
	Retained,
	Rejected,
}

struct WitnessEntries {
	witness: PublicKey,
	manifest_digest: [u8; 32],
	slots: Vec<Option<Core>>,
}

pub(super) struct ReceiptBook {
	binding_digest: [u8; 32],
	count: usize,
	witnesses: Vec<WitnessEntries>,
}

impl ReceiptBook {
	pub(super) fn empty(binding: &WitnessStorageBinding, record: &StoredWitnessEpoch) -> Self {
		let count = binding.setup().vouchers().len();
		let witnesses = record
			.policies()
			.iter()
			.map(|policy| WitnessEntries {
				witness: policy.witness,
				manifest_digest: hash(&[
					b"ldk-node/ffor/retained-manifest/v1",
					&record.manifest(policy.witness).unwrap().encode(),
				]),
				slots: vec![None; count],
			})
			.collect();
		Self { binding_digest: binding.digest(), count, witnesses }
	}

	pub(super) fn encode(&self) -> Zeroizing<Vec<u8>> {
		let length = 37 + self.witnesses.len() * (65 + self.count * SLOT_BYTES);
		let mut bytes = Zeroizing::new(Vec::with_capacity(length));
		bytes.extend_from_slice(&VERSION.to_be_bytes());
		bytes.extend_from_slice(&self.binding_digest);
		bytes.push(self.witnesses.len() as u8);
		bytes.extend_from_slice(&(self.count as u16).to_be_bytes());
		for witness in &self.witnesses {
			bytes.extend_from_slice(&witness.witness.serialize());
			bytes.extend_from_slice(&witness.manifest_digest);
			for slot in &witness.slots {
				match slot {
					Some(core) => {
						bytes.push(1);
						bytes.extend_from_slice(core);
					},
					None => {
						let length = bytes.len() + SLOT_BYTES;
						bytes.resize(length, 0);
					},
				}
			}
		}
		bytes
	}

	pub(super) fn decode(
		binding: &WitnessStorageBinding, secrets: &StoredWitnessEpoch, bytes: &[u8],
	) -> Result<Self, WitnessStoreError> {
		let book = Self::read(bytes, binding.digest())?;
		let expected = Self::empty(binding, secrets);
		if book.count != expected.count || book.witnesses.len() != expected.witnesses.len() {
			return Err(WitnessStoreError::Binding);
		}
		for (actual, expected) in book.witnesses.iter().zip(&expected.witnesses) {
			if actual.witness != expected.witness
				|| actual.manifest_digest != expected.manifest_digest
			{
				return Err(WitnessStoreError::Binding);
			}
			for core in actual.slots.iter().flatten() {
				secrets
					.decrypt_record(actual.witness, decode_core(core)?)
					.map_err(|_| WitnessStoreError::Corrupt)?;
			}
		}
		Ok(book)
	}

	pub(super) fn validate_inventory(
		bytes: &[u8], digest: [u8; 32],
	) -> Result<(), WitnessStoreError> {
		Self::read(bytes, digest).map(|_| ())
	}

	fn read(bytes: &[u8], digest: [u8; 32]) -> Result<Self, WitnessStoreError> {
		let mut reader = Reader(bytes);
		if u16::from_be_bytes(reader.array()?) != VERSION || reader.array::<32>()? != digest {
			return Err(WitnessStoreError::Binding);
		}
		let witness_count = reader.array::<1>()?[0] as usize;
		let count = u16::from_be_bytes(reader.array()?) as usize;
		if witness_count == 0 || witness_count > MAX_WITNESSES || count == 0 || count > 483 {
			return Err(WitnessStoreError::Capacity);
		}
		if reader.0.len() != witness_count * (65 + count * SLOT_BYTES) {
			return Err(WitnessStoreError::Corrupt);
		}
		let mut witnesses: Vec<WitnessEntries> = Vec::with_capacity(witness_count);
		for _ in 0..witness_count {
			let witness =
				PublicKey::from_slice(reader.take(33)?).map_err(|_| WitnessStoreError::Corrupt)?;
			if witnesses.last().is_some_and(|previous| previous.witness >= witness) {
				return Err(WitnessStoreError::Corrupt);
			}
			let manifest_digest = reader.array()?;
			let mut slots = Vec::with_capacity(count);
			for index in 0..count {
				let present = reader.array::<1>()?[0];
				let core = reader.array()?;
				slots.push(match present {
					0 if core == [0; CORE_BYTES] => None,
					1 => {
						let record = decode_core(&core)?;
						if record.header().witness != witness
							|| record.header().slot as usize != index + 1
						{
							return Err(WitnessStoreError::Binding);
						}
						Some(core)
					},
					_ => return Err(WitnessStoreError::Corrupt),
				});
			}
			witnesses.push(WitnessEntries { witness, manifest_digest, slots });
		}
		Ok(Self { binding_digest: digest, count, witnesses })
	}

	pub(super) fn is_empty(&self) -> bool {
		self.witnesses.iter().all(|witness| witness.slots.iter().all(Option::is_none))
	}

	pub(super) fn insert_authenticated(
		&mut self, secrets: &StoredWitnessEpoch, witness: PublicKey, record: &EncryptedRecord,
	) -> Result<ReceiptRetention, WitnessStoreError> {
		secrets.decrypt_record(witness, record.clone()).map_err(|error| match error {
			WitnessKeyUseError::Record(_) | WitnessKeyUseError::Decryption(_) => {
				WitnessStoreError::InvalidReceipt
			},
			WitnessKeyUseError::UnknownWitness => WitnessStoreError::Binding,
			_ => WitnessStoreError::Corrupt,
		})?;
		self.insert(witness, record)
	}

	pub(super) fn insert(
		&mut self, witness: PublicKey, record: &EncryptedRecord,
	) -> Result<ReceiptRetention, WitnessStoreError> {
		let entry = self
			.witnesses
			.iter_mut()
			.find(|entry| entry.witness == witness)
			.ok_or(WitnessStoreError::Binding)?;
		if record.header().witness != witness {
			return Err(WitnessStoreError::Binding);
		}
		let index =
			usize::from(record.header().slot).checked_sub(1).ok_or(WitnessStoreError::Binding)?;
		let slot = entry.slots.get_mut(index).ok_or(WitnessStoreError::Binding)?;
		let core = canonical_core(record)?;
		match slot {
			Some(existing) if existing == &core => Ok(ReceiptRetention::AlreadyStored),
			Some(_) => Err(WitnessStoreError::ReceiptConflict),
			None => {
				*slot = Some(core);
				Ok(ReceiptRetention::Stored)
			},
		}
	}

	pub(super) fn get(
		&self, witness: PublicKey, slot: u16,
	) -> Result<Option<EncryptedRecord>, WitnessStoreError> {
		let entry = self
			.witnesses
			.iter()
			.find(|entry| entry.witness == witness)
			.ok_or(WitnessStoreError::Binding)?;
		let index = usize::from(slot).checked_sub(1).ok_or(WitnessStoreError::Binding)?;
		entry
			.slots
			.get(index)
			.ok_or(WitnessStoreError::Binding)?
			.as_ref()
			.map(decode_core)
			.transpose()
	}
}

fn canonical_core(record: &EncryptedRecord) -> Result<Core, WitnessStoreError> {
	let wire = record.encode();
	let core: Core = wire
		.get(..CORE_BYTES)
		.ok_or(WitnessStoreError::Corrupt)?
		.try_into()
		.map_err(|_| WitnessStoreError::Corrupt)?;
	// Confirm shared canonical framing rather than truncating a differently encoded record.
	let decoded = decode_core(&core)?;
	if decoded.header() != record.header()
		|| decoded.ciphertext() != record.ciphertext()
		|| wire.get(CORE_BYTES).copied() != Some(record.receipts().len() as u8)
	{
		return Err(WitnessStoreError::Corrupt);
	}
	Ok(core)
}

fn decode_core(core: &Core) -> Result<EncryptedRecord, WitnessStoreError> {
	let mut bytes = core.to_vec();
	bytes.push(0);
	let record = EncryptedRecord::decode(&bytes).map_err(|_| WitnessStoreError::Corrupt)?;
	if record.encode() != bytes {
		return Err(WitnessStoreError::Corrupt);
	}
	Ok(record)
}

struct Reader<'a>(&'a [u8]);
impl Reader<'_> {
	fn take(&mut self, length: usize) -> Result<&[u8], WitnessStoreError> {
		let bytes = self.0.get(..length).ok_or(WitnessStoreError::Corrupt)?;
		self.0 = &self.0[length..];
		Ok(bytes)
	}
	fn array<const N: usize>(&mut self) -> Result<[u8; N], WitnessStoreError> {
		self.take(N)?.try_into().map_err(|_| WitnessStoreError::Corrupt)
	}
}

#[cfg(test)]
mod tests;

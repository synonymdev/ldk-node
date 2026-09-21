use bitcoin::hashes::{sha256, Hash, HashEngine, Hmac, HmacEngine};
use prost::Message;
use vss_client::types::Storable;
use vss_client::util::storable_builder::{EntropySource, StorableBuilder};
use zeroize::Zeroizing;

use super::record::{random_bytes, StoredWitnessEpoch};
use super::{WitnessStorageBinding, WitnessStoreError, MAX_RECORD_BYTES, SCHEMA_VERSION};

const HEADER_LENGTH: usize = 4 + 2 + 32;

pub(super) struct WrappingKey {
	key: Zeroizing<[u8; 32]>,
	receipts: bool,
}

impl WrappingKey {
	pub(super) fn derive(seed: &[u8; 64]) -> Self {
		Self::derive_domain(seed, false)
	}
	pub(super) fn derive_receipts(seed: &[u8; 64]) -> Self {
		Self::derive_domain(seed, true)
	}

	fn derive_domain(seed: &[u8; 64], receipts: bool) -> Self {
		// Independent HKDF purposes separate both storage keys from account and node keys.
		let purpose: &[u8] = if receipts {
			b"ldk-node/ffor/witness-receipt-storage/v1"
		} else {
			b"ldk-node/ffor/witness-storage/v1"
		};
		let mut extract = HmacEngine::<sha256::Hash>::new(purpose);
		extract.input(seed);
		let prk = Zeroizing::new(Hmac::from_engine(extract).to_byte_array());
		let mut expand = HmacEngine::<sha256::Hash>::new(prk.as_ref());
		expand.input(b"wrapping-key");
		expand.input(&[1]);
		Self { key: Zeroizing::new(Hmac::from_engine(expand).to_byte_array()), receipts }
	}

	pub(super) fn seal(
		&self, binding: &WitnessStorageBinding, record: &StoredWitnessEpoch,
	) -> Result<Vec<u8>, WitnessStoreError> {
		self.seal_plaintext(binding, &record.encode())
	}

	pub(super) fn seal_plaintext(
		&self, binding: &WitnessStorageBinding, plaintext: &[u8],
	) -> Result<Vec<u8>, WitnessStoreError> {
		let builder = StorableBuilder::new(EnvelopeNonce(random_bytes::<12>()?));
		let mut bytes = self.header(binding.digest());
		let aad = self.associated_data(&binding.storage_key(), &bytes);
		// Our buffers are zeroized. The dependency owns additional plaintext allocations which
		// it does not zeroize; this does not claim complete memory erasure.
		let sealed = builder.build(plaintext.to_vec(), i64::from(SCHEMA_VERSION), &self.key, &aad);
		bytes.extend_from_slice(&sealed.encode_to_vec());
		self.check_size(&bytes)?;
		Ok(bytes)
	}

	#[cfg(test)]
	pub(super) fn seal_plaintext_fixture(
		&self, binding: &WitnessStorageBinding, plaintext: &[u8],
	) -> Result<Vec<u8>, WitnessStoreError> {
		self.seal_plaintext(binding, plaintext)
	}

	pub(super) fn open(
		&self, binding: &WitnessStorageBinding, bytes: &[u8],
	) -> Result<StoredWitnessEpoch, WitnessStoreError> {
		let plaintext = self.open_bound(binding, bytes)?;
		StoredWitnessEpoch::decode(binding, &plaintext)
	}

	pub(super) fn open_bound(
		&self, binding: &WitnessStorageBinding, bytes: &[u8],
	) -> Result<Zeroizing<Vec<u8>>, WitnessStoreError> {
		let (digest, plaintext) = self.open_inventory(&binding.storage_key(), bytes)?;
		if digest != binding.digest() {
			return Err(WitnessStoreError::Binding);
		}
		Ok(plaintext)
	}

	// AEAD binds the actual namespace key and native binding digest. Inventory uses this only to
	// authenticate fixed allocation metadata; use still requires full native-context validation.
	pub(super) fn open_inventory(
		&self, key: &str, bytes: &[u8],
	) -> Result<([u8; 32], Zeroizing<Vec<u8>>), WitnessStoreError> {
		self.check_size(bytes)?;
		let header = bytes.get(..HEADER_LENGTH).ok_or(WitnessStoreError::Corrupt)?;
		let digest: [u8; 32] = header[6..].try_into().map_err(|_| WitnessStoreError::Corrupt)?;
		if header != self.header(digest) {
			return Err(WitnessStoreError::Binding);
		}
		let encoded = &bytes[HEADER_LENGTH..];
		let storable = Storable::decode(encoded).map_err(|_| WitnessStoreError::Corrupt)?;
		if storable.encode_to_vec() != encoded
			|| storable.encryption_metadata.as_ref().map(|m| m.cipher_format.as_str())
				!= Some("ChaCha20Poly1305")
		{
			return Err(WitnessStoreError::Corrupt);
		}
		let aad = self.associated_data(key, header);
		let (plaintext, version) = StorableBuilder::new(EnvelopeNonce([0; 12]))
			.deconstruct(storable, &self.key, &aad)
			.map_err(|_| WitnessStoreError::Corrupt)?;
		let plaintext = Zeroizing::new(plaintext);
		if version != i64::from(SCHEMA_VERSION) {
			return Err(WitnessStoreError::Corrupt);
		}
		Ok((digest, plaintext))
	}

	fn header(&self, digest: [u8; 32]) -> Vec<u8> {
		let magic: &[u8] = if self.receipts { b"FWRE" } else { b"FWSE" };
		[magic, &SCHEMA_VERSION.to_be_bytes(), &digest].concat()
	}
	fn associated_data(&self, key: &str, header: &[u8]) -> Vec<u8> {
		let purpose: &[u8] = if self.receipts {
			b"ldk-node/ffor/witness-receipt-envelope/v1"
		} else {
			b"ldk-node/ffor/witness-envelope/v1"
		};
		[purpose, key.as_bytes(), header].concat()
	}
	fn check_size(&self, bytes: &[u8]) -> Result<(), WitnessStoreError> {
		let maximum =
			if self.receipts { super::receipt::MAX_RECEIPT_RECORD_BYTES } else { MAX_RECORD_BYTES };
		if bytes.is_empty() || bytes.len() > maximum {
			Err(WitnessStoreError::Capacity)
		} else {
			Ok(())
		}
	}
}

// The pinned builder requests exactly one 12-byte AEAD nonce per build. Its infallible entropy
// trait is fed only after the fallible operating-system draw succeeds.
struct EnvelopeNonce([u8; 12]);
impl EntropySource for EnvelopeNonce {
	fn fill_bytes(&self, buffer: &mut [u8]) {
		buffer.copy_from_slice(&self.0);
	}
}

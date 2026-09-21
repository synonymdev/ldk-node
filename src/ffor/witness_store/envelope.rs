use bitcoin::hashes::{sha256, Hash, HashEngine, Hmac, HmacEngine};
use prost::Message;
use vss_client::types::Storable;
use vss_client::util::storable_builder::{EntropySource, StorableBuilder};
use zeroize::Zeroizing;

use super::record::{random_bytes, StoredWitnessEpoch};
use super::{check_record_size, WitnessStorageBinding, WitnessStoreError, SCHEMA_VERSION};

const MAGIC: &[u8; 4] = b"FWSE";
const HEADER_LENGTH: usize = 4 + 2 + 32;

pub(super) struct WrappingKey(Zeroizing<[u8; 32]>);

impl WrappingKey {
	pub(super) fn derive(seed: &[u8; 64]) -> Self {
		// HKDF extract/expand with a dedicated storage purpose, separate from all account/node keys.
		let mut extract = HmacEngine::<sha256::Hash>::new(b"ldk-node/ffor/witness-storage/v1");
		extract.input(seed);
		let prk = Zeroizing::new(Hmac::from_engine(extract).to_byte_array());
		let mut expand = HmacEngine::<sha256::Hash>::new(prk.as_ref());
		expand.input(b"wrapping-key");
		expand.input(&[1]);
		Self(Zeroizing::new(Hmac::from_engine(expand).to_byte_array()))
	}

	pub(super) fn seal(
		&self, binding: &WitnessStorageBinding, record: &StoredWitnessEpoch,
	) -> Result<Vec<u8>, WitnessStoreError> {
		let plaintext = record.encode();
		self.seal_plaintext(binding, &plaintext)
	}

	fn seal_plaintext(
		&self, binding: &WitnessStorageBinding, plaintext: &[u8],
	) -> Result<Vec<u8>, WitnessStoreError> {
		let nonce = random_bytes::<12>()?;
		let builder = StorableBuilder::new(EnvelopeNonce(nonce));
		let mut bytes = header(binding);
		let aad = associated_data(binding, &bytes);
		// Our buffer is zeroized. StorableBuilder owns additional plaintext allocations during
		// encryption and decryption which it does not zeroize. No total erasure is claimed.
		let sealed = builder.build(plaintext.to_vec(), i64::from(SCHEMA_VERSION), &self.0, &aad);
		bytes.extend_from_slice(&sealed.encode_to_vec());
		check_record_size(&bytes)?;
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
		check_record_size(bytes)?;
		if bytes.get(..HEADER_LENGTH) != Some(header(binding).as_slice()) {
			return Err(WitnessStoreError::Binding);
		}
		let encoded = &bytes[HEADER_LENGTH..];
		let storable = Storable::decode(encoded).map_err(|_| WitnessStoreError::Corrupt)?;
		if storable.encode_to_vec() != encoded
			|| storable.encryption_metadata.as_ref().map(|metadata| metadata.cipher_format.as_str())
				!= Some("ChaCha20Poly1305")
		{
			return Err(WitnessStoreError::Corrupt);
		}
		let aad = associated_data(binding, &bytes[..HEADER_LENGTH]);
		let (plaintext, version) = StorableBuilder::new(EnvelopeNonce([0; 12]))
			.deconstruct(storable, &self.0, &aad)
			.map_err(|_| WitnessStoreError::Corrupt)?;
		let plaintext = Zeroizing::new(plaintext);
		if version != i64::from(SCHEMA_VERSION) {
			return Err(WitnessStoreError::Corrupt);
		}
		StoredWitnessEpoch::decode(binding, &plaintext)
	}
}

fn header(binding: &WitnessStorageBinding) -> Vec<u8> {
	[MAGIC.as_slice(), &SCHEMA_VERSION.to_be_bytes(), &binding.digest()].concat()
}

fn associated_data(binding: &WitnessStorageBinding, header: &[u8]) -> Vec<u8> {
	[b"ldk-node/ffor/witness-envelope/v1".as_slice(), binding.storage_key().as_bytes(), header]
		.concat()
}

struct EnvelopeNonce([u8; 12]);

impl EntropySource for EnvelopeNonce {
	fn fill_bytes(&self, buffer: &mut [u8]) {
		// StorableBuilder::build requests exactly one 12-byte AEAD nonce. Entropy was obtained
		// fallibly before this infallible dependency callback, and this builder is used once.
		buffer.copy_from_slice(&self.0);
	}
}

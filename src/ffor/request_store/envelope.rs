use bitcoin::hashes::{sha256, Hash, HashEngine, Hmac, HmacEngine};
use bitcoin::secp256k1::PublicKey;
use prost::Message;
use rand::rngs::OsRng;
use rand::TryRngCore;
use vss_client::types::Storable;
use vss_client::util::storable_builder::{EntropySource, StorableBuilder};
use zeroize::Zeroizing;

use super::{RequestStoreError, StoredRequest, MAX_RECORD_BYTES, VERSION};

pub(super) struct EnvelopeKey {
	key: Zeroizing<[u8; 32]>,
	identity: Vec<u8>,
}

impl EnvelopeKey {
	pub(super) fn derive(seed: &[u8; 64], chain: [u8; 32], node: PublicKey) -> Self {
		let mut extract = HmacEngine::<sha256::Hash>::new(b"ldk-node/ffor/request-storage/v1");
		extract.input(seed);
		let prk = Zeroizing::new(Hmac::from_engine(extract).to_byte_array());
		let mut expand = HmacEngine::<sha256::Hash>::new(prk.as_ref());
		expand.input(b"wrapping-key");
		expand.input(&[1]);
		Self {
			key: Zeroizing::new(Hmac::from_engine(expand).to_byte_array()),
			identity: [chain.as_slice(), &node.serialize()].concat(),
		}
	}

	pub(super) fn seal(
		&self, key: &str, record: &StoredRequest,
	) -> Result<Vec<u8>, RequestStoreError> {
		let mut nonce = [0; 12];
		OsRng.try_fill_bytes(&mut nonce).map_err(|_| RequestStoreError::Entropy)?;
		let plaintext = record.encode();
		// Owned parsing buffers are zeroized. StorableBuilder's internal plaintext allocation
		// is not zeroized by that dependency; this makes no complete-erasure guarantee.
		let storable = StorableBuilder::new(Nonce(nonce)).build(
			plaintext.to_vec(),
			i64::from(VERSION),
			&self.key,
			&self.aad(key),
		);
		let bytes = storable.encode_to_vec();
		check_size(&bytes)?;
		Ok(bytes)
	}

	pub(super) fn open(&self, key: &str, bytes: &[u8]) -> Result<StoredRequest, RequestStoreError> {
		check_size(bytes)?;
		let sealed = Storable::decode(bytes).map_err(|_| RequestStoreError::Corrupt)?;
		if sealed.encode_to_vec() != bytes
			|| sealed.encryption_metadata.as_ref().map(|metadata| metadata.cipher_format.as_str())
				!= Some("ChaCha20Poly1305")
		{
			return Err(RequestStoreError::Corrupt);
		}
		let (plaintext, version) = StorableBuilder::new(Nonce([0; 12]))
			.deconstruct(sealed, &self.key, &self.aad(key))
			.map_err(|_| RequestStoreError::Corrupt)?;
		let plaintext = Zeroizing::new(plaintext);
		if version != i64::from(VERSION) {
			return Err(RequestStoreError::Corrupt);
		}
		StoredRequest::decode(&plaintext)
	}

	fn aad(&self, key: &str) -> Vec<u8> {
		[
			b"ldk-node/ffor/request-envelope/v1".as_slice(),
			&VERSION.to_be_bytes(),
			&self.identity,
			key.as_bytes(),
		]
		.concat()
	}
}

fn check_size(bytes: &[u8]) -> Result<(), RequestStoreError> {
	if bytes.is_empty() || bytes.len() > MAX_RECORD_BYTES {
		Err(RequestStoreError::Capacity)
	} else {
		Ok(())
	}
}

struct Nonce([u8; 12]);
impl EntropySource for Nonce {
	fn fill_bytes(&self, buffer: &mut [u8]) {
		buffer.copy_from_slice(&self.0);
	}
}

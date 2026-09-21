//! Operations using retained witness keys without exposing them to the runtime.

use bitcoin::secp256k1::{Message, PublicKey, Secp256k1};
use lightning::ln::ffor::{decrypt_ffor_witness_record, FFORWitnessReceipt};
use lightning_ffor::witness::{
	EncryptedRecord, FetchParameters, SignedFetch, UnsignedFetch, WitnessError,
};
use zeroize::Zeroizing;

use super::{
	random_bytes, StoredWitnessEpoch, TemporarySecret, WitnessKeyUseError, WitnessKeys,
	WitnessStoreError,
};

impl StoredWitnessEpoch {
	/// Signs a fresh request for a retained mailbox, including after a lost reply or restart.
	/// Every call draws a new 256-bit nonce and request identity from operating-system entropy.
	/// The runtime must correlate responses with the authenticated connection and must never resend
	/// an earlier request after ambiguous delivery. The witness still enforces nonce replay refusal.
	/// This operation neither sends a request nor establishes current native activation authority.
	pub(in crate::ffor) fn prepare_fetch(
		&self, witness: PublicKey, after_slot: Option<u16>,
	) -> Result<SignedFetch, WitnessKeyUseError> {
		self.prepare_fetch_with(witness, after_slot, random_bytes::<80>)
	}

	fn prepare_fetch_with(
		&self, witness: PublicKey, after_slot: Option<u16>,
		draw: impl FnOnce() -> Result<[u8; 80], WitnessStoreError>,
	) -> Result<SignedFetch, WitnessKeyUseError> {
		let keys = self.witness_keys(witness)?;
		let manifest = keys.manifest.unsigned();
		let count = (manifest.canonical_book().len() - 36) / 58;
		if usize::from(after_slot.unwrap_or(0)) > count {
			return Err(WitnessKeyUseError::Request(WitnessError::Pagination));
		}
		let entropy = Zeroizing::new(draw().map_err(|_| WitnessKeyUseError::Entropy)?);
		let mut request_id = [0; 16];
		let mut nonce = [0; 32];
		let mut auxiliary = Zeroizing::new([0; 32]);
		request_id.copy_from_slice(&entropy[..16]);
		nonce.copy_from_slice(&entropy[16..48]);
		auxiliary.copy_from_slice(&entropy[48..]);
		let parameters = manifest.parameters();
		let unsigned = UnsignedFetch::new(FetchParameters {
			request_id,
			mailbox_id: parameters.mailbox_id,
			nonce,
			after_slot,
			extensions: Vec::new(),
		})
		.map_err(WitnessKeyUseError::Request)?;
		let key = TemporarySecret::new(&keys.fetch_secret)
			.map_err(|_| WitnessKeyUseError::KeyMaterial)?;
		let signature = Secp256k1::new()
			.sign_ecdsa_with_noncedata(
				&Message::from_digest(unsigned.signing_digest()),
				&key.0,
				&auxiliary,
			)
			.serialize_compact();
		unsigned
			.authenticate(signature, parameters.fetch_public_key)
			.map_err(WitnessKeyUseError::Request)
	}

	/// Authenticates and decrypts a record using this epoch's retained manifest and private key.
	/// Historical recovery remains valid after admission expiry. The result proves knowledge of a
	/// voucher preimage, not successful receipt storage, channel reconciliation or payment credit.
	/// This does not correlate a Noise response or establish a current native phase.
	pub(in crate::ffor::witness_store) fn decrypt_record(
		&self, witness: PublicKey, record: EncryptedRecord,
	) -> Result<FFORWitnessReceipt, WitnessKeyUseError> {
		let keys = self.witness_keys(witness)?;
		let authenticated =
			record.authenticate(&keys.manifest, witness).map_err(WitnessKeyUseError::Record)?;
		let key = TemporarySecret::new(&self.encryption_secret)
			.map_err(|_| WitnessKeyUseError::KeyMaterial)?;
		decrypt_ffor_witness_record(&authenticated, &keys.manifest, &key.0)
			.map_err(WitnessKeyUseError::Decryption)
	}

	fn witness_keys(&self, witness: PublicKey) -> Result<&WitnessKeys, WitnessKeyUseError> {
		self.witnesses
			.iter()
			.find(|entry| entry.policy.witness == witness)
			.ok_or(WitnessKeyUseError::UnknownWitness)
	}
}

#[cfg(test)]
mod tests;

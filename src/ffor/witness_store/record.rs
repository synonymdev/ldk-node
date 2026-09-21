use std::fmt;

use bitcoin::secp256k1::{Message, PublicKey, Secp256k1, SecretKey};
use lightning_ffor::witness::{ManifestParameters, SignedManifest, UnsignedManifest};
use rand::rngs::OsRng;
use rand::TryRngCore;
use zeroize::Zeroizing;

use super::{WitnessStorageBinding, WitnessStoreError, MAX_WITNESSES, SCHEMA_VERSION};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WitnessPolicy {
	pub(super) witness: PublicKey,
	pub(super) retention_until: u32,
	pub(super) minimum_receipts: u8,
}

struct WitnessKeys {
	policy: WitnessPolicy,
	fetch_secret: Zeroizing<[u8; 32]>,
	manifest: SignedManifest,
}

/// Immutable material recovered from one successful store write, not proof of native activation.
/// There is no key-export or sending API. A future consumer must rejoin the native epoch owner.
pub(crate) struct StoredWitnessEpoch {
	binding_digest: [u8; 32],
	encryption_secret: Zeroizing<[u8; 32]>,
	witnesses: Vec<WitnessKeys>,
}

impl fmt::Debug for StoredWitnessEpoch {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("StoredWitnessEpoch")
			.field("binding_digest", &self.binding_digest)
			.field("witness_count", &self.witnesses.len())
			.field("secrets", &"[redacted]")
			.finish()
	}
}

impl StoredWitnessEpoch {
	pub(super) fn generate(
		binding: &WitnessStorageBinding, policies: &[WitnessPolicy],
	) -> Result<Self, WitnessStoreError> {
		let encryption_secret = generate_secret()?;
		let encryption_public_key = public_key(&encryption_secret)?;
		let mut witnesses: Vec<WitnessKeys> = Vec::with_capacity(policies.len());
		for policy in policies {
			let fetch_secret = generate_secret()?;
			let fetch_public_key = public_key(&fetch_secret)?;
			let mailbox_id = random_bytes::<32>()?;
			if fetch_public_key == encryption_public_key
				|| witnesses.iter().any(|entry| {
					let params = entry.manifest.unsigned().parameters();
					params.mailbox_id == mailbox_id || params.fetch_public_key == fetch_public_key
				}) {
				return Err(WitnessStoreError::Entropy);
			}
			let unsigned = UnsignedManifest::new(
				binding.setup(),
				ManifestParameters {
					mailbox_id,
					commitment_hash: binding.commitment_hash(),
					epoch_start_height: binding.epoch_start_height(),
					fetch_public_key,
					encryption_public_key,
					retention_until: policy.retention_until,
					minimum_receipts: policy.minimum_receipts,
				},
			)
			.map_err(|_| WitnessStoreError::Binding)?;
			let mut secret = TemporarySecret::new(&fetch_secret)?;
			let auxiliary = Zeroizing::new(random_bytes::<32>()?);
			let signature = Secp256k1::new()
				.sign_ecdsa_with_noncedata(
					&Message::from_digest(unsigned.signing_digest()),
					&secret.0,
					&auxiliary,
				)
				.serialize_compact();
			secret.erase();
			let manifest =
				unsigned.authenticate(signature).map_err(|_| WitnessStoreError::Corrupt)?;
			witnesses.push(WitnessKeys { policy: *policy, fetch_secret, manifest });
		}
		Ok(Self { binding_digest: binding.digest(), encryption_secret, witnesses })
	}

	pub(super) fn manifest(&self, witness: PublicKey) -> Option<&SignedManifest> {
		self.witnesses
			.iter()
			.find(|entry| entry.policy.witness == witness)
			.map(|entry| &entry.manifest)
	}

	pub(super) fn policies(&self) -> Vec<WitnessPolicy> {
		self.witnesses.iter().map(|entry| entry.policy).collect()
	}

	pub(super) fn encode(&self) -> Zeroizing<Vec<u8>> {
		let manifests: Vec<_> =
			self.witnesses.iter().map(|entry| entry.manifest.encode()).collect();
		// Reserve the exact size before copying secrets, avoiding freed intermediate allocations.
		let capacity = 67 + manifests.iter().map(|manifest| 69 + manifest.len()).sum::<usize>();
		let mut bytes = Zeroizing::new(Vec::with_capacity(capacity));
		bytes.extend_from_slice(&SCHEMA_VERSION.to_be_bytes());
		bytes.extend_from_slice(&self.binding_digest);
		bytes.extend_from_slice(self.encryption_secret.as_ref());
		bytes.push(self.witnesses.len() as u8);
		for (entry, manifest) in self.witnesses.iter().zip(manifests) {
			bytes.extend_from_slice(&entry.policy.witness.serialize());
			bytes.extend_from_slice(entry.fetch_secret.as_ref());
			bytes.extend_from_slice(&(manifest.len() as u32).to_be_bytes());
			bytes.extend_from_slice(&manifest);
		}
		bytes
	}

	pub(super) fn decode(
		binding: &WitnessStorageBinding, bytes: &[u8],
	) -> Result<Self, WitnessStoreError> {
		let mut reader = Reader(bytes);
		if u16::from_be_bytes(reader.array()?) != SCHEMA_VERSION
			|| reader.array::<32>()? != binding.digest()
		{
			return Err(WitnessStoreError::Binding);
		}
		let encryption_secret = Zeroizing::new(reader.array()?);
		let encryption_public_key = public_key(&encryption_secret)?;
		let count = usize::from(reader.array::<1>()?[0]);
		if count == 0 || count > MAX_WITNESSES {
			return Err(WitnessStoreError::Capacity);
		}
		let mut witnesses: Vec<WitnessKeys> = Vec::with_capacity(count);
		for _ in 0..count {
			let witness =
				PublicKey::from_slice(reader.take(33)?).map_err(|_| WitnessStoreError::Corrupt)?;
			let fetch_secret = Zeroizing::new(reader.array()?);
			let fetch_public_key = public_key(&fetch_secret)?;
			let length = u32::from_be_bytes(reader.array()?) as usize;
			if length > lightning_ffor::witness::MAX_MESSAGE_LEN {
				return Err(WitnessStoreError::Capacity);
			}
			let manifest = SignedManifest::decode(reader.take(length)?, binding.setup())
				.map_err(|_| WitnessStoreError::Corrupt)?;
			let params = manifest.unsigned().parameters();
			if params.fetch_public_key != fetch_public_key
				|| params.encryption_public_key != encryption_public_key
				|| manifest.unsigned().activation_hash() != binding.activation_hash()
				|| witnesses.iter().any(|entry| {
					entry.policy.witness >= witness
						|| entry.manifest.unsigned().parameters().mailbox_id == params.mailbox_id
						|| entry.manifest.unsigned().parameters().fetch_public_key
							== fetch_public_key
				}) || fetch_public_key == encryption_public_key
			{
				return Err(WitnessStoreError::Binding);
			}
			let policy = WitnessPolicy {
				witness,
				retention_until: params.retention_until,
				minimum_receipts: params.minimum_receipts,
			};
			witnesses.push(WitnessKeys { policy, fetch_secret, manifest });
		}
		if !reader.0.is_empty() {
			return Err(WitnessStoreError::Corrupt);
		}
		Ok(Self { binding_digest: binding.digest(), encryption_secret, witnesses })
	}
}

pub(super) fn checked_policies(
	policies: &[WitnessPolicy],
) -> Result<Vec<WitnessPolicy>, WitnessStoreError> {
	if policies.is_empty() || policies.len() > MAX_WITNESSES {
		return Err(WitnessStoreError::Capacity);
	}
	let mut policies = policies.to_vec();
	policies.sort_unstable_by_key(|policy| policy.witness);
	if policies.windows(2).any(|pair| pair[0].witness == pair[1].witness) {
		return Err(WitnessStoreError::Conflict);
	}
	Ok(policies)
}

pub(super) fn random_bytes<const N: usize>() -> Result<[u8; N], WitnessStoreError> {
	let mut bytes = Zeroizing::new([0; N]);
	OsRng.try_fill_bytes(bytes.as_mut()).map_err(|_| WitnessStoreError::Entropy)?;
	Ok(*bytes)
}

fn generate_secret() -> Result<Zeroizing<[u8; 32]>, WitnessStoreError> {
	generate_secret_with(random_bytes::<32>)
}

fn generate_secret_with(
	mut draw: impl FnMut() -> Result<[u8; 32], WitnessStoreError>,
) -> Result<Zeroizing<[u8; 32]>, WitnessStoreError> {
	for _ in 0..16 {
		let bytes = Zeroizing::new(draw()?);
		if TemporarySecret::new(&bytes).is_ok() {
			return Ok(bytes);
		}
	}
	Err(WitnessStoreError::Entropy)
}

fn public_key(bytes: &[u8; 32]) -> Result<PublicKey, WitnessStoreError> {
	let secret = TemporarySecret::new(bytes)?;
	Ok(PublicKey::from_secret_key(&Secp256k1::new(), &secret.0))
}

struct TemporarySecret(SecretKey);
impl TemporarySecret {
	fn new(bytes: &[u8; 32]) -> Result<Self, WitnessStoreError> {
		SecretKey::from_slice(bytes).map(Self).map_err(|_| WitnessStoreError::Corrupt)
	}
	fn erase(&mut self) {
		self.0.non_secure_erase();
	}
}
impl Drop for TemporarySecret {
	fn drop(&mut self) {
		self.erase();
	}
}

struct Reader<'a>(&'a [u8]);
impl<'a> Reader<'a> {
	fn take(&mut self, length: usize) -> Result<&'a [u8], WitnessStoreError> {
		let value = self.0.get(..length).ok_or(WitnessStoreError::Corrupt)?;
		self.0 = &self.0[length..];
		Ok(value)
	}
	fn array<const N: usize>(&mut self) -> Result<[u8; N], WitnessStoreError> {
		self.take(N)?.try_into().map_err(|_| WitnessStoreError::Corrupt)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn ffor_witness_store_secret_generation_is_bounded_and_fallible() {
		assert_eq!(
			generate_secret_with(|| Err(WitnessStoreError::Entropy)).unwrap_err(),
			WitnessStoreError::Entropy
		);
		let mut draws = 0;
		assert_eq!(
			generate_secret_with(|| {
				draws += 1;
				Ok([0; 32])
			})
			.unwrap_err(),
			WitnessStoreError::Entropy
		);
		assert_eq!(draws, 16);
		let mut draws = 0;
		let generated = generate_secret_with(|| {
			draws += 1;
			Ok(if draws == 1 { [255; 32] } else { [1; 32] })
		})
		.unwrap();
		assert_eq!(draws, 2);
		assert_eq!(*generated, [1; 32]);
	}
}

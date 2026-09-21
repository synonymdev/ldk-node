use std::fmt;

use bitcoin::secp256k1::{Message, PublicKey, Secp256k1, SecretKey};
use lightning::ln::ffor::FFORWitnessDecryptionError;
use lightning_ffor::witness::{
	CheckedAcknowledgement, ManifestParameters, SignedManifest, UnsignedManifest, WitnessError,
};
use rand::rngs::OsRng;
use rand::TryRngCore;
use zeroize::Zeroizing;

use super::{WitnessStorageBinding, WitnessStoreError, MAX_WITNESSES};

mod key_use;

const RECORD_VERSION: u16 = 2;
const RESERVED_RECORD_VERSION: u16 = 3;
const LEGACY_RECORD_VERSION: u16 = 1;
// Reserved even before provisioning so each acknowledgement fits without growing a new record.
const ACKNOWLEDGEMENT_BYTES: usize = 1 + 16 + 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WitnessKeyUseError {
	UnknownWitness,
	Entropy,
	KeyMaterial,
	Request(WitnessError),
	Record(WitnessError),
	Decryption(FFORWitnessDecryptionError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WitnessPolicy {
	pub(in crate::ffor) witness: PublicKey,
	pub(in crate::ffor) retention_until: u32,
	pub(in crate::ffor) minimum_receipts: u8,
}

/// Test-only fixture input for [`StoredWitnessEpoch::from_fixture`].
#[cfg(test)]
pub(in crate::ffor) struct FixtureWitness {
	pub(in crate::ffor) witness: PublicKey,
	pub(in crate::ffor) fetch_secret: [u8; 32],
	pub(in crate::ffor) manifest: Vec<u8>,
	pub(in crate::ffor) acknowledged: bool,
}

struct WitnessKeys {
	policy: WitnessPolicy,
	fetch_secret: Zeroizing<[u8; 32]>,
	manifest: SignedManifest,
	acknowledgement: Option<RetainedAcknowledgement>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RetainedAcknowledgement {
	request_id: [u8; 16],
	retention_until: u32,
}

/// Immutable secrets and monotonic promises from confirmed storage, not native activation authority.
/// There is no key-export or sending API. A future consumer must rejoin the native epoch owner.
pub(crate) struct StoredWitnessEpoch {
	binding_digest: [u8; 32],
	encryption_secret: Zeroizing<[u8; 32]>,
	witnesses: Vec<WitnessKeys>,
	pub(super) receipt_allocation: ReceiptAllocation,
}

/// Storage initialization only. No variant conveys protocol or invoice authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReceiptAllocation {
	Legacy,
	Pending(u32),
	Reserved(u32),
}

impl ReceiptAllocation {
	pub(super) fn bytes(self) -> usize {
		match self {
			Self::Legacy => 0,
			Self::Pending(bytes) | Self::Reserved(bytes) => bytes as usize,
		}
	}
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
			witnesses.push(WitnessKeys {
				policy: *policy,
				fetch_secret,
				manifest,
				acknowledgement: None,
			});
		}
		Ok(Self {
			binding_digest: binding.digest(),
			encryption_secret,
			witnesses,
			receipt_allocation: ReceiptAllocation::Legacy,
		})
	}

	/// Test-only: rebuild a record from public fixture secrets and the exact signed manifest.
	#[cfg(test)]
	pub(in crate::ffor) fn from_fixture(
		binding: &WitnessStorageBinding, encryption_secret: [u8; 32],
		witnesses: Vec<FixtureWitness>,
	) -> Result<Self, WitnessStoreError> {
		let mut entries = Vec::with_capacity(witnesses.len());
		for fixture in witnesses {
			let manifest = SignedManifest::decode(&fixture.manifest, binding.setup())
				.map_err(|_| WitnessStoreError::Corrupt)?;
			let params = manifest.unsigned().parameters();
			let policy = WitnessPolicy {
				witness: fixture.witness,
				retention_until: params.retention_until,
				minimum_receipts: params.minimum_receipts,
			};
			let acknowledgement = fixture.acknowledged.then_some(RetainedAcknowledgement {
				request_id: [1; 16],
				retention_until: params.retention_until,
			});
			entries.push(WitnessKeys {
				policy,
				fetch_secret: Zeroizing::new(fixture.fetch_secret),
				manifest,
				acknowledgement,
			});
		}
		entries.sort_by_key(|entry| entry.policy.witness);
		Ok(Self {
			binding_digest: binding.digest(),
			encryption_secret: Zeroizing::new(encryption_secret),
			witnesses: entries,
			receipt_allocation: ReceiptAllocation::Legacy,
		})
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

	/// Historical storage promises only. The native owner still controls current invoice readiness.
	pub(super) fn all_witnesses_acknowledged(&self) -> bool {
		!self.witnesses.is_empty()
			&& self.witnesses.iter().all(|entry| entry.acknowledgement.is_some())
	}

	/// Confirmed historical retention for the immutable manifest, with no current phase authority.
	pub(in crate::ffor) fn acknowledgement_retention(&self, witness: PublicKey) -> Option<u32> {
		self.witnesses
			.iter()
			.find(|entry| entry.policy.witness == witness)
			.and_then(|entry| entry.acknowledgement.map(|ack| ack.retention_until))
	}

	/// Retain the first correlated promise for this exact witness and immutable manifest.
	/// A later reprovision may use a new request ID; it cannot replace the original evidence.
	pub(super) fn retain_acknowledgement<C>(
		&mut self, checked: &CheckedAcknowledgement<C>,
	) -> Result<bool, WitnessStoreError> {
		let entry = self
			.witnesses
			.iter_mut()
			.find(|entry| entry.policy.witness == checked.connection().node_id)
			.ok_or(WitnessStoreError::Binding)?;
		if checked.provision().manifest() != &entry.manifest
			|| checked.retention_until() < entry.policy.retention_until
		{
			return Err(WitnessStoreError::Binding);
		}
		if entry.acknowledgement.is_some() {
			return Ok(false);
		}
		entry.acknowledgement = Some(RetainedAcknowledgement {
			request_id: checked.provision().request_id(),
			retention_until: checked.retention_until(),
		});
		Ok(true)
	}

	pub(super) fn encode(&self) -> Zeroizing<Vec<u8>> {
		let manifests: Vec<_> =
			self.witnesses.iter().map(|entry| entry.manifest.encode()).collect();
		// Reserve the exact size before copying secrets, avoiding freed intermediate allocations.
		let capacity = 67
			+ if self.receipt_allocation == ReceiptAllocation::Legacy { 0 } else { 5 }
			+ manifests
				.iter()
				.map(|manifest| 69 + manifest.len() + ACKNOWLEDGEMENT_BYTES)
				.sum::<usize>();
		let mut bytes = Zeroizing::new(Vec::with_capacity(capacity));
		let version = if self.receipt_allocation == ReceiptAllocation::Legacy {
			RECORD_VERSION
		} else {
			RESERVED_RECORD_VERSION
		};
		bytes.extend_from_slice(&version.to_be_bytes());
		bytes.extend_from_slice(&self.binding_digest);
		bytes.extend_from_slice(self.encryption_secret.as_ref());
		bytes.push(self.witnesses.len() as u8);
		match self.receipt_allocation {
			ReceiptAllocation::Legacy => {},
			ReceiptAllocation::Pending(size) | ReceiptAllocation::Reserved(size) => {
				bytes.push(if matches!(self.receipt_allocation, ReceiptAllocation::Pending(_)) {
					1
				} else {
					2
				});
				bytes.extend_from_slice(&size.to_be_bytes());
			},
		}
		for (entry, manifest) in self.witnesses.iter().zip(manifests) {
			bytes.extend_from_slice(&entry.policy.witness.serialize());
			bytes.extend_from_slice(entry.fetch_secret.as_ref());
			bytes.extend_from_slice(&(manifest.len() as u32).to_be_bytes());
			bytes.extend_from_slice(&manifest);
			if let Some(acknowledgement) = entry.acknowledgement {
				bytes.push(1);
				bytes.extend_from_slice(&acknowledgement.request_id);
				bytes.extend_from_slice(&acknowledgement.retention_until.to_be_bytes());
			} else {
				bytes.extend_from_slice(&[0; ACKNOWLEDGEMENT_BYTES]);
			}
		}
		bytes
	}

	pub(super) fn decode(
		binding: &WitnessStorageBinding, bytes: &[u8],
	) -> Result<Self, WitnessStoreError> {
		let mut reader = Reader(bytes);
		let version = u16::from_be_bytes(reader.array()?);
		if !matches!(version, LEGACY_RECORD_VERSION | RECORD_VERSION | RESERVED_RECORD_VERSION)
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
		let receipt_allocation = read_allocation(version, &mut reader)?;
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
			let acknowledgement = if version != LEGACY_RECORD_VERSION {
				let present = reader.array::<1>()?[0];
				let request_id = reader.array()?;
				let retention_until = u32::from_be_bytes(reader.array()?);
				match present {
					0 if request_id == [0; 16] && retention_until == 0 => None,
					1 if retention_until >= policy.retention_until => {
						Some(RetainedAcknowledgement { request_id, retention_until })
					},
					_ => return Err(WitnessStoreError::Corrupt),
				}
			} else {
				None
			};
			witnesses.push(WitnessKeys { policy, fetch_secret, manifest, acknowledgement });
		}
		if !reader.0.is_empty() {
			return Err(WitnessStoreError::Corrupt);
		}
		Ok(Self {
			binding_digest: binding.digest(),
			encryption_secret,
			witnesses,
			receipt_allocation,
		})
	}

	// The envelope is authenticated before inventory calls this. Read only the fixed allocation
	// prefix so reopen can charge Pending quotas without possessing a native context for every epoch.
	pub(super) fn inventory_allocation(
		bytes: &[u8], digest: [u8; 32],
	) -> Result<ReceiptAllocation, WitnessStoreError> {
		let mut reader = Reader(bytes);
		let version = u16::from_be_bytes(reader.array()?);
		if reader.array::<32>()? != digest {
			return Err(WitnessStoreError::Binding);
		}
		reader.take(32)?;
		let count = reader.array::<1>()?[0] as usize;
		if count == 0 || count > MAX_WITNESSES {
			return Err(WitnessStoreError::Corrupt);
		}
		read_allocation(version, &mut reader)
	}
}

fn read_allocation(
	version: u16, reader: &mut Reader<'_>,
) -> Result<ReceiptAllocation, WitnessStoreError> {
	match version {
		LEGACY_RECORD_VERSION | RECORD_VERSION => Ok(ReceiptAllocation::Legacy),
		RESERVED_RECORD_VERSION => {
			let state = reader.array::<1>()?[0];
			let size = u32::from_be_bytes(reader.array()?);
			if size == 0 || size as usize > super::receipt::MAX_RECEIPT_RECORD_BYTES {
				return Err(WitnessStoreError::Capacity);
			}
			match state {
				1 => Ok(ReceiptAllocation::Pending(size)),
				2 => Ok(ReceiptAllocation::Reserved(size)),
				_ => Err(WitnessStoreError::Corrupt),
			}
		},
		_ => Err(WitnessStoreError::Corrupt),
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

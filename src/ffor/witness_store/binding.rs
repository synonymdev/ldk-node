use bitcoin::consensus::encode::serialize;
use bitcoin::secp256k1::PublicKey;
use bitcoin::OutPoint;
use lightning::ln::ffor::FFORReceiverRecoveryContext;
use lightning_ffor::setup::AuthenticatedSetup;
use lightning_ffor::wire::{Message, Payload};

use super::{hash, WitnessStoreError, SCHEMA_VERSION};

/// Original native storage identity, without current channel ownership or activation authority.
#[derive(Clone, Debug)]
pub(crate) struct WitnessStorageIdentity {
	pub(crate) chain_hash: [u8; 32],
	pub(crate) actual_node: PublicKey,
	pub(crate) settlement_node: PublicKey,
	pub(crate) original_funding: OutPoint,
}

/// Immutable sidecar binding to signed historical evidence, requiring a live native authority check.
/// No durable/active flag can be supplied or inferred from this value.
#[derive(Clone, Debug)]
pub(crate) struct WitnessStorageBinding {
	identity: WitnessStorageIdentity,
	setup: AuthenticatedSetup,
	activation_hash: [u8; 32],
	commitment_hash: [u8; 32],
	epoch_start_height: u32,
	digest: [u8; 32],
}

impl WitnessStorageBinding {
	/// Binds to an opaque native history, including its stable digest and original funding identity.
	/// A retained acknowledgement is required for witness material, but does not prove current ACTIVE
	/// state, persistence, admission timing or permission to provision or expose an invoice.
	pub(crate) fn from_native_context(
		context: &FFORReceiverRecoveryContext,
	) -> Result<Self, WitnessStoreError> {
		let activate =
			Message::decode(context.activation_wire()).map_err(|_| WitnessStoreError::Binding)?;
		let acknowledgement =
			Message::decode(context.activation_ack_wire().ok_or(WitnessStoreError::Binding)?)
				.map_err(|_| WitnessStoreError::Binding)?;
		let identity = WitnessStorageIdentity {
			chain_hash: context.chain_hash().to_bytes(),
			actual_node: context.receiver_node_id(),
			settlement_node: context.settlement_node_id(),
			original_funding: context.funding_txo().into_bitcoin_outpoint(),
		};
		let binding = Self::from_evidence(
			identity,
			context.setup().clone(),
			&activate,
			&acknowledgement,
			context.context_digest(),
		)?;
		if binding.activation_hash != context.activation_hash()
			|| binding.setup.header().channel_id != context.channel_id().0
			|| binding.setup.header().epoch_id != context.epoch_id()
		{
			return Err(WitnessStoreError::Binding);
		}
		Ok(binding)
	}

	#[cfg(test)]
	pub(super) fn new(
		identity: WitnessStorageIdentity, setup: AuthenticatedSetup, activate: &Message,
		acknowledgement: &Message,
	) -> Result<Self, WitnessStoreError> {
		Self::from_evidence(identity, setup, activate, acknowledgement, [0; 32])
	}

	fn from_evidence(
		identity: WitnessStorageIdentity, setup: AuthenticatedSetup, activate: &Message,
		acknowledgement: &Message, native_context_digest: [u8; 32],
	) -> Result<Self, WitnessStoreError> {
		// Reauthenticate both roles against the supplied actual identities, not wire claims.
		let setup = AuthenticatedSetup::new(
			setup.init(),
			setup.accept(),
			identity.actual_node,
			identity.settlement_node,
		)
		.map_err(|_| WitnessStoreError::Binding)?;
		let terms = match &activate.payload {
			Payload::Activate(terms) => terms,
			_ => return Err(WitnessStoreError::Binding),
		};
		// Historical self-consistency only. A peer's signed H_commit is not a native monitor proof.
		let activation_hash = setup
			.validate_activation(activate, terms.commit_hash, terms.epoch_start_height)
			.map_err(|_| WitnessStoreError::Binding)?;
		setup
			.validate_activation_ack(acknowledgement, activation_hash)
			.map_err(|_| WitnessStoreError::Binding)?;
		let init = setup.init().encode().map_err(|_| WitnessStoreError::Binding)?;
		let accept = setup.accept().encode().map_err(|_| WitnessStoreError::Binding)?;
		let activation = activate.encode().map_err(|_| WitnessStoreError::Binding)?;
		let ack = acknowledgement.encode().map_err(|_| WitnessStoreError::Binding)?;
		let digest = hash(&[
			b"ldk-node/ffor/witness-binding/v1",
			&SCHEMA_VERSION.to_be_bytes(),
			&native_context_digest,
			&identity.chain_hash,
			&identity.actual_node.serialize(),
			&identity.settlement_node.serialize(),
			&serialize(&identity.original_funding),
			&hash(&[&init]),
			&hash(&[&accept]),
			&hash(&[&activation]),
			&hash(&[&ack]),
			setup.canonical_book(),
			&activation_hash,
		]);
		Ok(Self {
			identity,
			setup,
			activation_hash,
			commitment_hash: terms.commit_hash,
			epoch_start_height: terms.epoch_start_height,
			digest,
		})
	}

	pub(crate) fn storage_key(&self) -> String {
		let header = self.setup.header();
		let digest = hash(&[
			b"ldk-node/ffor/witness-key/v1",
			&self.identity.chain_hash,
			&self.identity.actual_node.serialize(),
			&header.channel_id,
			&header.epoch_id,
		]);
		const HEX: &[u8; 16] = b"0123456789abcdef";
		let mut key = String::with_capacity(64);
		for byte in digest {
			key.push(HEX[usize::from(byte >> 4)] as char);
			key.push(HEX[usize::from(byte & 15)] as char);
		}
		key
	}

	pub(crate) fn digest(&self) -> [u8; 32] {
		self.digest
	}
	pub(crate) fn setup(&self) -> &AuthenticatedSetup {
		&self.setup
	}
	pub(crate) fn activation_hash(&self) -> [u8; 32] {
		self.activation_hash
	}
	pub(crate) fn commitment_hash(&self) -> [u8; 32] {
		self.commitment_hash
	}
	pub(crate) fn epoch_start_height(&self) -> u32 {
		self.epoch_start_height
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn ffor_witness_store_native_context_digest_binds_same_epoch() {
		let (identity, setup, activate, ack) = super::super::tests::evidence(1, 2);
		let first = WitnessStorageBinding::from_evidence(
			identity.clone(),
			setup.clone(),
			&activate,
			&ack,
			[1; 32],
		)
		.unwrap();
		let changed =
			WitnessStorageBinding::from_evidence(identity, setup, &activate, &ack, [2; 32])
				.unwrap();
		// Same epoch addresses the existing record, but altered native provenance cannot open it.
		assert_eq!(first.storage_key(), changed.storage_key());
		assert_ne!(first.digest(), changed.digest());
	}
}

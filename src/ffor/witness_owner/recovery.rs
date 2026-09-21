//! Durable encrypted evidence enters only the concrete original native monitor.

use bitcoin::secp256k1::PublicKey;
use lightning::ln::ffor::{
	FFORCommitmentError, FFORReceiverRecoveryContext, FFORWitnessReceiptProgress,
};

use super::{WitnessOwner, WitnessOwnerError};
use crate::ffor::witness_store::WitnessStorageBinding;

impl WitnessOwner {
	/// Protect one retained receipt with the stock monitor. Absence means only that this store has
	/// no authenticated evidence for the selected witness and slot, never that the voucher is unpaid.
	/// Even MonitorPersisted grants neither settlement credit nor permission to issue an invoice.
	pub(crate) fn recover_receipt(
		&mut self, context: &FFORReceiverRecoveryContext, witness: PublicKey, slot: u16,
	) -> Result<Option<FFORWitnessReceiptProgress>, WitnessOwnerError> {
		// Rejoin immutable native registration before touching protected evidence. Historical
		// recovery does not require an Active channel, an unexpired deadline or a connected peer.
		if !self.retained_manifests(context)?.iter().any(|(selected, _)| *selected == witness) {
			return Err(WitnessOwnerError::UnknownWitness);
		}
		let binding = WitnessStorageBinding::from_native_context(context)
			.map_err(WitnessOwnerError::Storage)?;
		let receipt = match self
			.store
			.load_receipt(&binding, witness, slot)
			.map_err(WitnessOwnerError::Storage)?
		{
			Some(receipt) => receipt,
			None => return Ok(None),
		};
		let snapshot = {
			let monitor = self.monitor.get_monitor(context.channel_id()).map_err(|_| {
				WitnessOwnerError::Native(FFORCommitmentError::MonitorMismatch.into())
			})?;
			monitor
				.ffor_witness_receipt_snapshot(&receipt)
				.map_err(|error| WitnessOwnerError::Native(error.into()))?
		};
		// Native may synchronously call Watch. Holding the monitor guard here would deadlock;
		// the opaque snapshot and native counter/funding checks close the intervening state race.
		self.manager
			.import_ffor_receiver_witness_receipt(context, &receipt, &snapshot)
			.map(Some)
			.map_err(WitnessOwnerError::Native)
	}
}

#[cfg(test)]
mod tests;

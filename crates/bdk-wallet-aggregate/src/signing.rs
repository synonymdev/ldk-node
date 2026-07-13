// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Multi-wallet PSBT signing logic.

use std::collections::HashMap;
use std::fmt::Debug;
use std::hash::Hash;

#[allow(deprecated)]
use bdk_wallet::{PersistedWallet, SignOptions, WalletPersister};
use bitcoin::psbt::{self, Psbt};
use bitcoin::Transaction;

use crate::types::Error;

/// Sign an unsigned transaction using every wallet that owns one of its
/// inputs.  This works by creating a PSBT with the transaction's inputs
/// and outputs and then having each wallet sign its owned inputs.
pub fn sign_owned_inputs<K, P>(
	unsigned_tx: Transaction, wallets: &mut HashMap<K, PersistedWallet<P>>,
) -> Result<Transaction, Error>
where
	K: Eq + Hash + Copy + Debug,
	P: WalletPersister,
{
	let psbt = Psbt::from_unsigned_tx(unsigned_tx).map_err(|e| {
		log::error!("Failed to create PSBT from unsigned tx: {}", e);
		Error::OnchainTxSigningFailed
	})?;

	sign_psbt_all_wallets(psbt, wallets)
}

/// Sign a PSBT using every wallet that owns at least one of its inputs.
pub fn sign_psbt_all_wallets<K, P>(
	mut psbt: Psbt, wallets: &mut HashMap<K, PersistedWallet<P>>,
) -> Result<Transaction, Error>
where
	K: Eq + Hash + Copy + Debug,
	P: WalletPersister,
{
	populate_psbt_inputs_from_wallets(&mut psbt, wallets);

	#[allow(deprecated)]
	let sign_options = SignOptions { trust_witness_utxo: true, ..Default::default() };

	for (key, wallet) in wallets.iter_mut() {
		match wallet.sign(&mut psbt, sign_options.clone()) {
			Ok(_) => log::trace!("Wallet {:?} processed PSBT inputs", key),
			Err(e) => {
				log::trace!(
					"Wallet {:?} could not sign PSBT: {} (expected for foreign inputs)",
					key,
					e
				);
			},
		}
	}

	if psbt.inputs.iter().any(|input| !is_finalized(input)) {
		log::error!("At least one PSBT input remains unsigned");
		return Err(Error::OnchainTxSigningFailed);
	}

	match psbt.extract_tx() {
		Ok(tx) => Ok(tx),
		Err(psbt::ExtractTxError::MissingInputValue { tx }) => {
			log::warn!(
				"extract_tx could not verify fee (MissingInputValue) for txid {}",
				tx.compute_txid()
			);
			Ok(tx)
		},
		Err(e) => {
			log::error!("Failed to extract signed transaction: {}", e);
			Err(Error::OnchainTxSigningFailed)
		},
	}
}

fn is_finalized(input: &psbt::Input) -> bool {
	matches!(input.final_script_sig.as_ref(), Some(script) if !script.is_empty())
		|| matches!(input.final_script_witness.as_ref(), Some(witness) if !witness.is_empty())
}

/// Populate PSBT inputs with UTXO data from all wallets.
///
/// Handles both unspent and already-spent UTXOs (important for RBF where
/// the original transaction already consumed the inputs).
fn populate_psbt_inputs_from_wallets<K, P>(
	psbt: &mut Psbt, wallets: &HashMap<K, PersistedWallet<P>>,
) where
	K: Eq + Hash + Copy + Debug,
	P: WalletPersister,
{
	for (i, txin) in psbt.unsigned_tx.input.iter().enumerate() {
		if i >= psbt.inputs.len() {
			psbt.inputs.push(bitcoin::psbt::Input::default());
		}

		let psbt_input = &psbt.inputs[i];
		// Only skip if BOTH witness_utxo and non_witness_utxo are already populated.
		if psbt_input.witness_utxo.is_some() && psbt_input.non_witness_utxo.is_some() {
			continue;
		}

		let mut found = false;
		for wallet in wallets.values() {
			// Try get_utxo first (works for unspent outputs).
			if let Some(utxo) = wallet.get_utxo(txin.previous_output) {
				if psbt.inputs[i].witness_utxo.is_none() {
					psbt.inputs[i].witness_utxo = Some(utxo.txout.clone());
				}

				// Always populate non_witness_utxo to guard against SegWit fee
				// vulnerability and ensure BDK can verify input values.
				if psbt.inputs[i].non_witness_utxo.is_none() {
					if let Some(tx_node) = wallet.get_tx(txin.previous_output.txid) {
						psbt.inputs[i].non_witness_utxo = Some(tx_node.tx_node.tx.as_ref().clone());
					}
				}
				found = true;
				break;
			}

			// Fallback: look up via the transaction that created this output.
			// This handles the RBF case where the UTXO has already been spent
			// by the transaction we are replacing.
			if let Some(tx_node) = wallet.get_tx(txin.previous_output.txid) {
				if let Some(txout) =
					tx_node.tx_node.tx.output.get(txin.previous_output.vout as usize)
				{
					if psbt.inputs[i].witness_utxo.is_none() {
						psbt.inputs[i].witness_utxo = Some(txout.clone());
					}
					if psbt.inputs[i].non_witness_utxo.is_none() {
						psbt.inputs[i].non_witness_utxo = Some(tx_node.tx_node.tx.as_ref().clone());
					}
					found = true;
					break;
				}
			}
		}

		if !found {
			log::debug!(
				"Could not find UTXO data for input {:?} in any wallet",
				txin.previous_output
			);
		}
	}
}

#[cfg(test)]
mod tests {
	use bitcoin::{ScriptBuf, Witness};

	use super::is_finalized;

	#[test]
	fn rejects_empty_final_scripts() {
		assert!(!is_finalized(&bitcoin::psbt::Input::default()));
		let script_input =
			bitcoin::psbt::Input { final_script_sig: Some(ScriptBuf::new()), ..Default::default() };
		assert!(!is_finalized(&script_input));
		let witness_input = bitcoin::psbt::Input {
			final_script_witness: Some(Witness::new()),
			..Default::default()
		};
		assert!(!is_finalized(&witness_input));
	}

	#[test]
	fn accepts_finalized_script_or_witness() {
		let script_input = bitcoin::psbt::Input {
			final_script_sig: Some(ScriptBuf::from_bytes(vec![1])),
			..Default::default()
		};
		assert!(is_finalized(&script_input));

		let mut witness = Witness::new();
		witness.push([1]);
		let witness_input =
			bitcoin::psbt::Input { final_script_witness: Some(witness), ..Default::default() };
		assert!(is_finalized(&witness_input));
	}
}

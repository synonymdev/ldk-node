// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Cross-wallet RBF (Replace-By-Fee) transaction construction.

use std::collections::HashMap;
use std::fmt::Debug;
use std::hash::Hash;

use bdk_wallet::{KeychainKind, PersistedWallet, WalletPersister};
use bitcoin::{Amount, FeeRate, OutPoint, ScriptBuf, Transaction, TxIn, TxOut, Weight};

use crate::signing;
use crate::types::{Error, UtxoPsbtInfo};
use crate::utxo::DUST_LIMIT_SATS;

struct ReplacementRequirements {
	change_idx: Option<usize>,
	recipient_value: u64,
	required_fee: u64,
}

/// Return the minimum fee rate accepted for a replacement transaction.
///
/// This mirrors BDK's fee-bump policy by requiring at least a 1 sat/vB increase.
pub(crate) fn minimum_replacement_fee_rate(
	original_fee: Amount, original_weight: Weight,
) -> Result<FeeRate, Error> {
	let original_fee_rate = original_fee / original_weight;
	let minimum_sat_per_kwu = original_fee_rate
		.to_sat_per_kwu()
		.checked_add(FeeRate::BROADCAST_MIN.to_sat_per_kwu())
		.ok_or(Error::InvalidFeeRate)?;
	Ok(FeeRate::from_sat_per_kwu(minimum_sat_per_kwu))
}

fn original_input_value<K, P>(
	original_tx: &Transaction, wallets: &HashMap<K, PersistedWallet<P>>,
) -> Result<u64, Error>
where
	K: Eq + Hash + Copy + Debug,
	P: WalletPersister,
{
	original_tx.input.iter().try_fold(0u64, |total, input| {
		total
			.checked_add(get_input_value(&input.previous_output, wallets)?)
			.ok_or(Error::OnchainTxCreationFailed)
	})
}

fn replacement_inputs(original_tx: &Transaction, extra_utxos: &[UtxoPsbtInfo]) -> Vec<TxIn> {
	let mut inputs: Vec<TxIn> = original_tx
		.input
		.iter()
		.map(|txin| TxIn {
			previous_output: txin.previous_output,
			script_sig: ScriptBuf::new(),
			sequence: bitcoin::Sequence::ENABLE_RBF_NO_LOCKTIME,
			witness: bitcoin::Witness::default(),
		})
		.collect();
	inputs.extend(extra_utxos.iter().map(|info| TxIn {
		previous_output: info.outpoint,
		script_sig: ScriptBuf::new(),
		sequence: bitcoin::Sequence::ENABLE_RBF_NO_LOCKTIME,
		witness: bitcoin::Witness::default(),
	}));
	inputs
}

fn replacement_requirements<K, P>(
	wallets: &HashMap<K, PersistedWallet<P>>, primary_key: &K, original_tx: &Transaction,
	original_input_value: u64, replacement_inputs: &[TxIn], new_fee_rate: FeeRate,
) -> Result<ReplacementRequirements, Error>
where
	K: Eq + Hash + Copy + Debug,
	P: WalletPersister,
{
	let original_output_value = original_tx.output.iter().try_fold(0u64, |total, output| {
		total.checked_add(output.value.to_sat()).ok_or(Error::OnchainTxCreationFailed)
	})?;
	let original_fee = original_input_value
		.checked_sub(original_output_value)
		.ok_or(Error::OnchainTxCreationFailed)?;
	let minimum_fee_rate =
		minimum_replacement_fee_rate(Amount::from_sat(original_fee), original_tx.weight())?;
	if new_fee_rate < minimum_fee_rate {
		return Err(Error::InvalidFeeRate);
	}

	let estimated_weight = estimate_tx_weight(wallets, replacement_inputs, &original_tx.output);
	let target_fee = new_fee_rate.fee_wu(estimated_weight).ok_or(Error::InvalidFeeRate)?.to_sat();
	let incremental_fee =
		FeeRate::BROADCAST_MIN.fee_wu(estimated_weight).ok_or(Error::InvalidFeeRate)?.to_sat();
	let minimum_replacement_fee =
		original_fee.checked_add(incremental_fee).ok_or(Error::OnchainTxCreationFailed)?;
	let required_fee = std::cmp::max(target_fee, minimum_replacement_fee);

	let change_idx =
		find_internal_change_output(wallets, primary_key, original_tx).map(|(index, _)| index);
	let recipient_value = original_tx
		.output
		.iter()
		.enumerate()
		.filter(|(index, _)| Some(*index) != change_idx)
		.try_fold(0u64, |total, (_, output)| {
			total.checked_add(output.value.to_sat()).ok_or(Error::OnchainTxCreationFailed)
		})?;

	Ok(ReplacementRequirements { change_idx, recipient_value, required_fee })
}

/// Return the effective value that additional inputs must contribute.
pub(crate) fn additional_input_value_needed<K, P>(
	wallets: &HashMap<K, PersistedWallet<P>>, primary_key: &K, original_tx: &Transaction,
	new_fee_rate: FeeRate,
) -> Result<Amount, Error>
where
	K: Eq + Hash + Copy + Debug,
	P: WalletPersister,
{
	let original_input_value = original_input_value(original_tx, wallets)?;
	let replacement_inputs = replacement_inputs(original_tx, &[]);
	let requirements = replacement_requirements(
		wallets,
		primary_key,
		original_tx,
		original_input_value,
		&replacement_inputs,
		new_fee_rate,
	)?;
	if requirements.change_idx.is_none() {
		return Err(Error::InsufficientFunds);
	}
	let required_value = requirements
		.required_fee
		.checked_add(requirements.recipient_value)
		.and_then(|value| value.checked_add(DUST_LIMIT_SATS))
		.ok_or(Error::OnchainTxCreationFailed)?;

	Ok(Amount::from_sat(required_value.saturating_sub(original_input_value)))
}

/// Get the value of an input by looking up the referenced UTXO across all
/// wallets.
pub fn get_input_value<K, P>(
	outpoint: &OutPoint, wallets: &HashMap<K, PersistedWallet<P>>,
) -> Result<u64, Error>
where
	K: Eq + Hash + Copy + Debug,
	P: WalletPersister,
{
	for wallet in wallets.values() {
		if let Some(utxo) = wallet.get_utxo(*outpoint) {
			return Ok(utxo.txout.value.to_sat());
		}

		if let Some(tx_node) = wallet.get_tx(outpoint.txid) {
			if let Some(txout) = tx_node.tx_node.tx.output.get(outpoint.vout as usize) {
				return Ok(txout.value.to_sat());
			}
		}
	}

	Err(Error::UtxoNotFoundLocally(*outpoint))
}

/// Build a cross-wallet RBF replacement transaction.
///
/// Re-uses the original transaction's inputs, optionally adds `extra_utxos`
/// as new inputs, recalculates the fee at `new_fee_rate`, and adjusts the
/// change output accordingly.  Signs with all wallets.
///
/// Pass an empty slice for `extra_utxos` to attempt a change-only bump.
/// If that is insufficient the caller can select additional UTXOs and retry.
pub fn build_cross_wallet_rbf<K, P>(
	wallets: &mut HashMap<K, PersistedWallet<P>>, primary_key: &K, original_tx: &Transaction,
	new_fee_rate: FeeRate, extra_utxos: &[UtxoPsbtInfo],
) -> Result<Transaction, Error>
where
	K: Eq + Hash + Copy + Debug,
	P: WalletPersister,
{
	let original_input_value = original_input_value(original_tx, wallets)?;

	let mut total_input_value = original_input_value;
	for info in extra_utxos {
		let value = info
			.psbt_input
			.witness_utxo
			.as_ref()
			.map(|utxo| utxo.value.to_sat())
			.ok_or(Error::OnchainTxCreationFailed)?;
		total_input_value =
			total_input_value.checked_add(value).ok_or(Error::OnchainTxCreationFailed)?;
	}

	let new_inputs = replacement_inputs(original_tx, extra_utxos);
	let requirements = replacement_requirements(
		wallets,
		primary_key,
		original_tx,
		original_input_value,
		&new_inputs,
		new_fee_rate,
	)?;

	let mut new_outputs = original_tx.output.clone();
	let required_value = requirements
		.required_fee
		.checked_add(requirements.recipient_value)
		.ok_or(Error::OnchainTxCreationFailed)?;
	let available_for_change =
		total_input_value.checked_sub(required_value).ok_or(Error::InsufficientFunds)?;

	match requirements.change_idx {
		Some(idx) => {
			if available_for_change < DUST_LIMIT_SATS {
				log::error!(
					"Insufficient funds for cross-wallet RBF: available_for_change={}, required_fee={}, total_input={}",
					available_for_change,
					requirements.required_fee,
					total_input_value
				);
				return Err(Error::InsufficientFunds);
			}
			new_outputs[idx].value = Amount::from_sat(available_for_change);
		},
		None => {
			// A replacement cannot increase its fee without reducing a recipient output.
			return Err(Error::InsufficientFunds);
		},
	}

	let unsigned_tx = Transaction {
		version: original_tx.version,
		lock_time: original_tx.lock_time,
		input: new_inputs,
		output: new_outputs,
	};

	signing::sign_owned_inputs(unsigned_tx, wallets)
}

/// Find the primary wallet's internal output that the manual RBF builder will reduce.
pub(crate) fn find_internal_change_output<'a, K, P>(
	wallets: &HashMap<K, PersistedWallet<P>>, primary_key: &K, transaction: &'a Transaction,
) -> Option<(usize, &'a TxOut)>
where
	K: Eq + Hash + Copy + Debug,
	P: WalletPersister,
{
	if transaction.output.len() <= 1 {
		return None;
	}

	let primary_wallet = wallets.get(primary_key)?;
	transaction
		.output
		.iter()
		.enumerate()
		.filter(|(_, output)| {
			primary_wallet
				.derivation_of_spk(output.script_pubkey.clone())
				.is_some_and(|(keychain, _)| keychain == KeychainKind::Internal)
		})
		.last()
}

/// Estimate the weight of a transaction for fee calculation purposes.
fn estimate_tx_weight<K, P>(
	wallets: &HashMap<K, PersistedWallet<P>>, inputs: &[TxIn], outputs: &[TxOut],
) -> bitcoin::Weight
where
	K: Eq + Hash + Copy + Debug,
	P: WalletPersister,
{
	let unsigned_tx = Transaction {
		version: bitcoin::transaction::Version::TWO,
		lock_time: bitcoin::absolute::LockTime::ZERO,
		input: inputs.to_vec(),
		output: outputs.to_vec(),
	};
	let witness_overhead = Weight::from_wu(2 + inputs.len() as u64);
	let satisfaction_weight = inputs.iter().fold(Weight::ZERO, |weight, input| {
		weight
			+ crate::utxo::satisfaction_weight_for_outpoint(input.previous_output, wallets)
				.unwrap_or_else(|| Weight::from_wu(428))
	});

	unsigned_tx.weight() + witness_overhead + satisfaction_weight
}

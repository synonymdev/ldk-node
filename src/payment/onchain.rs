// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Holds a payment handler allowing to send and receive on-chain payments.

use std::sync::{Arc, Mutex, RwLock};

use bitcoin::{Address, Txid};

use crate::config::{AddressType, Config, OnchainWalletAccount};
use crate::error::Error;
use crate::fee_estimator::ConfirmationTarget;
use crate::logger::{log_error, log_info, LdkLogger, Logger};
use crate::runtime::RuntimeControl;
use crate::tx_broadcaster::{ExplicitBroadcastAdmission, ExplicitBroadcastGuard, TxBroadcastError};
use crate::types::{Broadcaster, ChannelManager, SpendableUtxo, Wallet};
use crate::wallet::{CoinSelectionAlgorithm, OnchainSendAmount};

#[cfg(not(feature = "uniffi"))]
type FeeRate = bitcoin::FeeRate;
#[cfg(feature = "uniffi")]
type FeeRate = Arc<bitcoin::FeeRate>;

macro_rules! maybe_map_fee_rate_opt {
	($fee_rate_opt:expr) => {{
		#[cfg(not(feature = "uniffi"))]
		{
			$fee_rate_opt
		}
		#[cfg(feature = "uniffi")]
		{
			$fee_rate_opt.map(|f| *f)
		}
	}};
}

/// The keychain an address was derived from.
///
/// Address derivation APIs can use [`KeychainKind::External`] for receive addresses or
/// [`KeychainKind::Internal`] for change addresses. APIs that generate or reveal new receive
/// addresses always operate on [`KeychainKind::External`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeychainKind {
	/// External receive-address keychain.
	External,
	/// Internal change-address keychain.
	Internal,
}

impl From<bdk_wallet::KeychainKind> for KeychainKind {
	fn from(keychain: bdk_wallet::KeychainKind) -> Self {
		match keychain {
			bdk_wallet::KeychainKind::External => Self::External,
			bdk_wallet::KeychainKind::Internal => Self::Internal,
		}
	}
}

impl From<KeychainKind> for bdk_wallet::KeychainKind {
	fn from(keychain: KeychainKind) -> Self {
		match keychain {
			KeychainKind::External => Self::External,
			KeychainKind::Internal => Self::Internal,
		}
	}
}

/// An explicit on-chain broadcast whose backend acceptance is still unresolved.
///
/// `txid` is the active transaction that [`OnchainPayment::rebroadcast_transaction`] and
/// [`OnchainPayment::abandon_pending_broadcast`] currently target. `lineage` is the complete
/// replacement history from the original spend through that active transaction and must be
/// independently reconciled as absent before abandonment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingBroadcastInfo {
	/// Active transaction ID currently awaiting backend reconciliation.
	pub txid: Txid,
	/// Complete RBF lineage from the original spend through [`Self::txid`].
	pub lineage: Vec<Txid>,
}

/// The durable reconciliation status of an explicit on-chain broadcast.
///
/// Only [`Self::Accepted`] proves that the configured backend accepted a transaction. Callers
/// must keep [`Self::Pending`] fail-closed, while [`Self::Abandoned`] proves that the wallet
/// completed the caller-authorized local abandonment workflow.
///
/// # Safety and threat model
///
/// No Rust memory-safety preconditions apply. Treat every status except [`Self::Accepted`] as
/// insufficient authorization to publish transaction proof to an external service.
///
/// # Example
///
/// ```
/// use ldk_node::payment::BroadcastOutcomeStatus;
///
/// assert_ne!(BroadcastOutcomeStatus::Pending, BroadcastOutcomeStatus::Accepted);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BroadcastOutcomeStatus {
	/// Backend acceptance is unresolved and no proof may be submitted.
	Pending,
	/// The configured backend accepted the authoritative transaction.
	Accepted,
	/// The unresolved spend was conclusively reconciled and abandoned locally.
	Abandoned,
}

/// A durable outcome for an explicit on-chain broadcast and its complete RBF lineage.
///
/// `txid` is the current retry target for [`BroadcastOutcomeStatus::Pending`], the authoritative
/// backend-observed member for [`BroadcastOutcomeStatus::Accepted`], and the last active member for
/// [`BroadcastOutcomeStatus::Abandoned`]. Looking up any member of `lineage` returns the same
/// record.
///
/// # Safety and threat model
///
/// No Rust memory-safety preconditions apply. Consumers must use `status`, not transaction
/// presence in ordinary payment history, as the broadcast-acceptance authority.
///
/// # Example
///
/// ```
/// use bitcoin::hashes::Hash;
/// use bitcoin::Txid;
/// use ldk_node::payment::{BroadcastOutcome, BroadcastOutcomeStatus};
///
/// let txid = Txid::from_byte_array([1; 32]);
/// let outcome = BroadcastOutcome {
///     status: BroadcastOutcomeStatus::Pending,
///     txid,
///     lineage: vec![txid],
/// };
/// assert_eq!(outcome.txid, txid);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BroadcastOutcome {
	/// Durable reconciliation status.
	pub status: BroadcastOutcomeStatus,
	/// Current active or terminal transaction ID for this outcome.
	pub txid: Txid,
	/// Complete replacement lineage from the original spend through [`Self::txid`].
	pub lineage: Vec<Txid>,
}

/// Metadata for an address derived by the on-chain wallet.
///
/// The `index` is the BIP32 child index within `keychain`. For receive addresses generated by
/// `new_address_info` methods, `keychain` is expected to be [`KeychainKind::External`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressInfo {
	/// Child index of this address within its keychain.
	pub index: u32,
	/// The derived Bitcoin address.
	pub address: Address,
	/// The keychain this address belongs to.
	pub keychain: KeychainKind,
}

impl From<bdk_wallet::AddressInfo> for AddressInfo {
	fn from(address_info: bdk_wallet::AddressInfo) -> Self {
		Self {
			index: address_info.index,
			address: address_info.address,
			keychain: address_info.keychain.into(),
		}
	}
}

/// A payment handler allowing to send and receive on-chain payments.
///
/// Should be retrieved by calling [`Node::onchain_payment`].
///
/// [`Node::onchain_payment`]: crate::Node::onchain_payment
pub struct OnchainPayment {
	wallet: Arc<Wallet>,
	tx_broadcaster: Arc<Broadcaster>,
	runtime: Arc<RuntimeControl>,
	channel_manager: Arc<ChannelManager>,
	config: Arc<Config>,
	is_running: Arc<RwLock<bool>>,
	logger: Arc<Logger>,
}

struct BroadcastDispatchLease {
	wallet: Arc<Wallet>,
	txids: Mutex<Vec<Txid>>,
}

impl BroadcastDispatchLease {
	fn acquire(wallet: &Arc<Wallet>, txid: Txid) -> Result<Self, Error> {
		wallet.begin_broadcast_dispatch(txid)?;
		Ok(Self { wallet: Arc::clone(wallet), txids: Mutex::new(vec![txid]) })
	}

	fn include(&self, txid: Txid) -> Result<(), Error> {
		let mut txids = self.txids.lock().unwrap();
		if !txids.contains(&txid) {
			self.wallet.begin_broadcast_dispatch(txid)?;
			txids.push(txid);
		}
		Ok(())
	}
}

impl Drop for BroadcastDispatchLease {
	fn drop(&mut self) {
		self.wallet.end_broadcast_dispatches(&self.txids.lock().unwrap());
	}
}

impl OnchainPayment {
	pub(crate) fn new(
		wallet: Arc<Wallet>, tx_broadcaster: Arc<Broadcaster>, runtime: Arc<RuntimeControl>,
		channel_manager: Arc<ChannelManager>, config: Arc<Config>, is_running: Arc<RwLock<bool>>,
		logger: Arc<Logger>,
	) -> Self {
		Self { wallet, tx_broadcaster, runtime, channel_manager, config, is_running, logger }
	}

	fn begin_explicit_broadcast(&self) -> Result<ExplicitBroadcastAdmission, Error> {
		self.tx_broadcaster.begin_explicit_broadcast().map_err(|_| Error::NotRunning)
	}

	fn dispatch_prepared_transaction(
		&self, admission: ExplicitBroadcastAdmission, tx: bitcoin::Transaction,
	) -> Result<Txid, Error> {
		let txid = tx.compute_txid();
		let dispatch_lease = Arc::new(BroadcastDispatchLease::acquire(&self.wallet, txid)?);
		self.wallet.prepare_pending_broadcast(&tx)?;
		let explicit_guard: ExplicitBroadcastGuard = dispatch_lease.clone();
		let dispatch_result = match self.runtime.try_block_on(
			self.tx_broadcaster.broadcast_transaction(admission, tx.clone(), Some(explicit_guard)),
		) {
			Ok(result) => result,
			Err(Error::NotRunning) => {
				drop(dispatch_lease);
				if let Err(error) = self
					.wallet
					.abandon_broadcast_intent(&tx)
					.and_then(|_| self.wallet.remove_transient_broadcast_outcome(&txid))
				{
					return Err(self.record_conclusive_cleanup_failure(txid, error));
				}
				return Err(Error::NotRunning);
			},
			Err(error) => return Err(error),
		};
		match dispatch_result {
			Ok(()) => self.record_accepted_broadcast(txid),
			Err(error @ (TxBroadcastError::Rejected | TxBroadcastError::NotDispatched)) => {
				drop(dispatch_lease);
				if let Err(cleanup_error) = self
					.wallet
					.abandon_broadcast_intent(&tx)
					.and_then(|_| self.wallet.remove_transient_broadcast_outcome(&txid))
				{
					return Err(self.record_conclusive_cleanup_failure(txid, cleanup_error));
				}
				Err(Self::initial_broadcast_error(error, txid))
			},
			Err(error @ (TxBroadcastError::Failed | TxBroadcastError::Timeout)) => {
				self.record_unknown_broadcast(txid);
				Err(Self::initial_broadcast_error(error, txid))
			},
		}
	}

	fn record_accepted_broadcast(&self, txid: Txid) -> Result<Txid, Error> {
		match self.wallet.clear_broadcast_intent(&txid) {
			Ok(()) => Ok(txid),
			Err(error) => {
				log_error!(self.logger, "Failed to persist accepted broadcast {}: {}", txid, error);
				if let Err(retention_error) = self.wallet.mark_broadcast_outcome_required(&txid) {
					log_error!(
						self.logger,
						"Failed to retain accepted broadcast outcome {}: {}",
						txid,
						retention_error
					);
				}
				Err(Error::OnchainTxBroadcastFailed { txid })
			},
		}
	}

	fn record_unknown_broadcast(&self, txid: Txid) {
		if let Err(e) = self.wallet.mark_broadcast_outcome_required(&txid) {
			log_error!(
				self.logger,
				"Failed to persist acceptance-unknown broadcast outcome {}: {}",
				txid,
				e
			);
		}
	}

	fn record_conclusive_cleanup_failure(&self, txid: Txid, error: Error) -> Error {
		log_error!(self.logger, "Failed to finalize conclusive broadcast {}: {}", txid, error);
		if let Err(retention_error) = self.wallet.retain_broadcast_outcome(&txid) {
			log_error!(
				self.logger,
				"Failed to retain conclusive broadcast cleanup outcome {}: {}",
				txid,
				retention_error
			);
		}
		Error::OnchainTxBroadcastFailed { txid }
	}

	fn initial_broadcast_error(error: TxBroadcastError, txid: Txid) -> Error {
		match error {
			TxBroadcastError::Rejected => Error::OnchainTxBroadcastRejected { txid },
			TxBroadcastError::NotDispatched => Error::OnchainTxBroadcastNotDispatched { txid },
			TxBroadcastError::Failed => Error::OnchainTxBroadcastFailed { txid },
			TxBroadcastError::Timeout => Error::OnchainTxBroadcastTimeout { txid },
		}
	}

	fn rebroadcast_error(error: TxBroadcastError, txid: Txid) -> Error {
		match error {
			TxBroadcastError::Timeout => Error::OnchainTxBroadcastTimeout { txid },
			TxBroadcastError::Rejected
			| TxBroadcastError::NotDispatched
			| TxBroadcastError::Failed => Error::OnchainTxBroadcastFailed { txid },
		}
	}

	/// Retrieve a new on-chain/funding address.
	pub fn new_address(&self) -> Result<Address, Error> {
		let funding_address = self.wallet.get_new_address()?;
		log_info!(self.logger, "Generated new funding address: {}", funding_address);
		Ok(funding_address)
	}

	/// Retrieve a new on-chain/funding address with derivation metadata.
	///
	/// This uses the same default address type and receive cursor as [`new_address`].
	///
	/// [`new_address`]: Self::new_address
	pub fn new_address_info(&self) -> Result<AddressInfo, Error> {
		let funding_address_info = AddressInfo::from(self.wallet.get_new_address_info()?);
		log_info!(self.logger, "Generated new funding address: {}", funding_address_info.address);
		Ok(funding_address_info)
	}

	/// Retrieve a new on-chain address for a specific address type.
	pub fn new_address_for_type(&self, address_type: AddressType) -> Result<Address, Error> {
		if !*self.is_running.read().unwrap() {
			return Err(Error::NotRunning);
		}

		let funding_address = self.wallet.get_new_address_for_type(address_type)?;
		log_info!(
			self.logger,
			"Generated new funding address for {:?}: {}",
			address_type,
			funding_address
		);
		Ok(funding_address)
	}

	/// Retrieve a new on-chain address with derivation metadata for a specific address type.
	pub fn new_address_info_for_type(
		&self, address_type: AddressType,
	) -> Result<AddressInfo, Error> {
		if !*self.is_running.read().unwrap() {
			return Err(Error::NotRunning);
		}

		let funding_address_info =
			AddressInfo::from(self.wallet.get_new_address_info_for_type(address_type)?);
		log_info!(
			self.logger,
			"Generated new funding address for {:?}: {}",
			address_type,
			funding_address_info.address
		);
		Ok(funding_address_info)
	}

	/// Retrieve a new on-chain address for a specific wallet account.
	///
	/// An unloaded derived account returns [`Error::OnchainWalletAccountNotRegistered`].
	pub fn new_address_for_account(
		&self, address_type: AddressType, account_index: u32,
	) -> Result<Address, Error> {
		if !*self.is_running.read().unwrap() {
			return Err(Error::NotRunning);
		}

		let wallet_account = OnchainWalletAccount { address_type, account_index };
		let funding_address = self.wallet.get_new_address_for_account(wallet_account)?;
		log_info!(
			self.logger,
			"Generated new funding address for {:?}: {}",
			wallet_account,
			funding_address
		);
		Ok(funding_address)
	}

	/// Retrieve a new on-chain address with derivation metadata for a specific wallet account.
	///
	/// An unloaded derived account returns [`Error::OnchainWalletAccountNotRegistered`].
	pub fn new_address_info_for_account(
		&self, address_type: AddressType, account_index: u32,
	) -> Result<AddressInfo, Error> {
		if !*self.is_running.read().unwrap() {
			return Err(Error::NotRunning);
		}

		let wallet_account = OnchainWalletAccount { address_type, account_index };
		let funding_address_info =
			AddressInfo::from(self.wallet.get_new_address_info_for_account(wallet_account)?);
		log_info!(
			self.logger,
			"Generated new funding address for {:?}: {}",
			wallet_account,
			funding_address_info.address
		);
		Ok(funding_address_info)
	}

	/// Derive address metadata for `address_type`, `keychain`, and `index` on account `0`.
	///
	/// This does not reveal, reserve, or advance any wallet cursor.
	pub fn address_info_for_type_at_index(
		&self, address_type: AddressType, keychain: KeychainKind, index: u32,
	) -> Result<AddressInfo, Error> {
		if !*self.is_running.read().unwrap() {
			return Err(Error::NotRunning);
		}

		self.wallet
			.get_address_info_for_type_at_index(address_type, keychain, index)
			.map(Into::into)
	}

	/// Derive address metadata for a wallet account without advancing its cursor.
	///
	/// An unloaded derived account returns [`Error::OnchainWalletAccountNotRegistered`].
	pub fn address_info_for_account_at_index(
		&self, address_type: AddressType, account_index: u32, keychain: KeychainKind, index: u32,
	) -> Result<AddressInfo, Error> {
		if !*self.is_running.read().unwrap() {
			return Err(Error::NotRunning);
		}

		let wallet_account = OnchainWalletAccount { address_type, account_index };
		self.wallet
			.get_address_info_for_account_at_index(wallet_account, keychain, index)
			.map(Into::into)
	}

	/// Derive address metadata for a contiguous account-`0` range without advancing any wallet
	/// cursor.
	///
	/// The returned vector contains `count` addresses starting at `start_index`. Batch requests are
	/// capped at 10,000 addresses per call.
	pub fn address_infos_for_type(
		&self, address_type: AddressType, keychain: KeychainKind, start_index: u32, count: u32,
	) -> Result<Vec<AddressInfo>, Error> {
		if !*self.is_running.read().unwrap() {
			return Err(Error::NotRunning);
		}

		self.wallet
			.get_address_infos_for_type(address_type, keychain, start_index, count)
			.map(|infos| infos.into_iter().map(Into::into).collect())
	}

	/// Derive address metadata for a contiguous wallet-account range without advancing its cursor.
	///
	/// Batch requests are capped at 10,000 addresses per call.
	/// An unloaded derived account returns [`Error::OnchainWalletAccountNotRegistered`].
	pub fn address_infos_for_account(
		&self, address_type: AddressType, account_index: u32, keychain: KeychainKind,
		start_index: u32, count: u32,
	) -> Result<Vec<AddressInfo>, Error> {
		if !*self.is_running.read().unwrap() {
			return Err(Error::NotRunning);
		}

		let wallet_account = OnchainWalletAccount { address_type, account_index };
		self.wallet
			.get_address_infos_for_account(wallet_account, keychain, start_index, count)
			.map(|infos| infos.into_iter().map(Into::into).collect())
	}

	/// Reveal external receive addresses for account-`0` `address_type` up to and including
	/// `index`.
	///
	/// After this returns successfully, normal address generation for `address_type` will not return
	/// any receive address with an index at or below `index`.
	pub fn reveal_receive_addresses_to(
		&self, address_type: AddressType, index: u32,
	) -> Result<(), Error> {
		if !*self.is_running.read().unwrap() {
			return Err(Error::NotRunning);
		}

		self.wallet.reveal_receive_addresses_to(address_type, index)
	}

	/// Reveal external receive addresses through `index` for a specific wallet account.
	///
	/// Apps issuing addresses from an exported account xpub should call this with the highest issued
	/// index so wallet sync includes the corresponding scripts.
	/// An unloaded derived account returns [`Error::OnchainWalletAccountNotRegistered`].
	pub fn reveal_receive_addresses_to_account(
		&self, address_type: AddressType, account_index: u32, index: u32,
	) -> Result<(), Error> {
		if !*self.is_running.read().unwrap() {
			return Err(Error::NotRunning);
		}

		let wallet_account = OnchainWalletAccount { address_type, account_index };
		self.wallet.reveal_receive_addresses_to_account(wallet_account, index)
	}

	/// Returns a list of all UTXOs that are safe to spend.
	///
	/// This excludes any outputs that are currently being used to fund Lightning channels.
	///
	/// **Note:** This does not account for anchor channel reserves. When using these UTXOs
	/// for transactions, ensure you maintain sufficient balance for any required reserves.
	pub fn list_spendable_outputs(&self) -> Result<Vec<SpendableUtxo>, Error> {
		if !*self.is_running.read().unwrap() {
			return Err(Error::NotRunning);
		}

		self.wallet
			.get_spendable_utxos(&self.channel_manager)
			.map(|outputs| outputs.into_iter().map(SpendableUtxo::from).collect())
	}

	/// Select UTXOs using a specific coin selection algorithm.
	///
	/// This method allows you to choose which algorithm to use for selecting UTXOs
	/// to meet a target amount. The selected UTXOs will be safe to spend (not funding channels).
	///
	/// # Arguments
	///
	/// * `target_amount_sats` - The target amount in satoshis
	/// * `fee_rate` - The fee rate to use (or None to estimate)
	/// * `algorithm` - The coin selection algorithm to use
	/// * `utxos` - Optional list of UTXO outpoints to select from (or None to use all spendable UTXOs)
	pub fn select_utxos_with_algorithm(
		&self, target_amount_sats: u64, fee_rate: Option<FeeRate>,
		algorithm: CoinSelectionAlgorithm, utxos: Option<Vec<SpendableUtxo>>,
	) -> Result<Vec<SpendableUtxo>, Error> {
		if !*self.is_running.read().unwrap() {
			return Err(Error::NotRunning);
		}

		// Get available UTXOs, optionally filtering by provided UTXOs
		let available_utxos = match utxos {
			Some(spendable_utxos) => {
				// Get all spendable UTXOs and filter by the provided UTXOs
				let wallet_outputs = self.wallet.get_spendable_utxos(&self.channel_manager)?;
				let outpoint_set: std::collections::HashSet<_> =
					spendable_utxos.iter().map(|u| u.outpoint).collect();
				wallet_outputs
					.into_iter()
					.filter(|output| outpoint_set.contains(&output.outpoint))
					.collect()
			},
			None => self.wallet.get_spendable_utxos(&self.channel_manager)?,
		};

		if available_utxos.is_empty() {
			return Err(Error::InsufficientFunds);
		}

		// Use the set fee_rate or default to fee estimation
		let confirmation_target = ConfirmationTarget::OnchainPayment;
		let fee_rate = maybe_map_fee_rate_opt!(fee_rate)
			.unwrap_or_else(|| self.wallet.estimate_fee_rate(confirmation_target));

		// Get a drain script (change address)
		let drain_script = self.wallet.get_drain_script()?;

		// Apply coin selection
		let selected_outpoints = self.wallet.select_utxos_with_algorithm(
			target_amount_sats,
			available_utxos.clone(),
			fee_rate,
			algorithm,
			&drain_script,
			&self.channel_manager,
		)?;

		// Convert selected outpoints back to SpendableUtxo by direct filtering
		let selected_utxos: Vec<SpendableUtxo> = available_utxos
			.into_iter()
			.filter(|utxo| selected_outpoints.contains(&utxo.outpoint))
			.map(SpendableUtxo::from)
			.collect();

		Ok(selected_utxos)
	}

	/// Calculates the total fee for an on-chain payment without sending it.
	///
	/// This method simulates creating a transaction to the given address for the specified amount
	/// and returns the total fee that would be paid. This is useful for displaying fee estimates
	/// to users before they confirm a transaction.
	///
	/// The calculation respects any on-chain reserve requirements and validates that sufficient
	/// funds are available, just like [`send_to_address`].
	///
	/// **Note on maximum amounts:** For calculating the fee when sending the entire spendable
	/// balance, prefer [`calculate_send_all_fee`] which is purpose-built for that use case.
	/// This method includes a best-effort fallback for near-max amounts, but it may not
	/// trigger in all cases depending on the underlying wallet error.
	///
	/// [`calculate_send_all_fee`]: Self::calculate_send_all_fee
	///
	/// # Arguments
	///
	/// * `address` - The Bitcoin address to send to
	/// * `amount_sats` - The amount to send in satoshis (or total balance for max send)
	/// * `fee_rate` - Optional fee rate to use (if None, will estimate based on current network conditions)
	/// * `utxos_to_spend` - Optional list of specific UTXOs to use for the transaction
	///
	/// # Returns
	///
	/// The total fee in satoshis that would be paid for this transaction.
	///
	/// # Errors
	///
	/// * [`Error::NotRunning`] - If the node is not running
	/// * [`Error::InvalidAddress`] - If the address is invalid
	/// * [`Error::InsufficientFunds`] - If there are insufficient funds for the payment
	/// * [`Error::WalletOperationFailed`] - If fee calculation fails
	///
	/// [`send_to_address`]: Self::send_to_address
	/// [`BalanceDetails::total_onchain_balance_sats`]: crate::balance::BalanceDetails::total_onchain_balance_sats
	pub fn calculate_total_fee(
		&self, address: &bitcoin::Address, amount_sats: u64, fee_rate: Option<FeeRate>,
		utxos_to_spend: Option<Vec<SpendableUtxo>>,
	) -> Result<u64, Error> {
		if !*self.is_running.read().unwrap() {
			return Err(Error::NotRunning);
		}
		let cur_anchor_reserve_sats =
			crate::total_anchor_channels_reserve_sats(&self.channel_manager, &self.config);

		// Get current balances
		let (_total_balance, spendable_balance) =
			self.wallet.get_balances(cur_anchor_reserve_sats).unwrap_or((0, 0));

		// First try with the exact amount
		let outpoints = utxos_to_spend.map(|utxos| utxos.into_iter().map(|u| u.outpoint).collect());
		let fee_rate_opt = maybe_map_fee_rate_opt!(fee_rate);

		// Try calculating with exact amount first
		let send_amount =
			OnchainSendAmount::ExactRetainingReserve { amount_sats, cur_anchor_reserve_sats };
		let result = self.wallet.calculate_transaction_fee(
			address,
			send_amount,
			fee_rate_opt,
			outpoints.clone(),
			&self.channel_manager,
		);

		// If we get InsufficientFunds and the amount is within the spendable balance,
		// try calculating as if sending all available funds
		if matches!(result, Err(Error::InsufficientFunds)) && amount_sats <= spendable_balance {
			// Try with AllRetainingReserve to calculate the fee for sending all
			let all_retaining = OnchainSendAmount::AllRetainingReserve { cur_anchor_reserve_sats };
			if let Ok(fee) = self.wallet.calculate_transaction_fee(
				address,
				all_retaining,
				fee_rate_opt,
				outpoints.clone(),
				&self.channel_manager,
			) {
				// Return the fee for sending all available funds
				return Ok(fee);
			}
		}

		result
	}

	/// Calculates the total fee for sending all available on-chain funds without
	/// actually broadcasting.
	///
	/// This is the fee-calculation counterpart of [`send_all_to_address`]. Use it to
	/// show the user how much a drain / send-all transaction would cost before they
	/// confirm.
	///
	/// When `retain_reserves` is `true`, the calculation accounts for the on-chain anchor
	/// channel reserve (see [`BalanceDetails::total_anchor_channels_reserve_sats`]), sending only
	/// the spendable portion. When `false`, the calculation covers draining the entire wallet
	/// balance, which may be dangerous if you have open anchor channels whose counterparty you
	/// don't trust to spend the anchor output after closure.
	///
	/// # Arguments
	///
	/// * `address` - The destination Bitcoin address
	/// * `retain_reserves` - If `true`, retains the anchor channel reserve; if `false`, drains everything
	/// * `fee_rate` - Optional fee rate to use (if `None`, will estimate based on current network conditions)
	///
	/// # Returns
	///
	/// The total fee in satoshis that would be paid for this transaction.
	///
	/// # Errors
	///
	/// * [`Error::NotRunning`] - If the node is not running
	/// * [`Error::InvalidAddress`] - If the address is invalid
	/// * [`Error::InsufficientFunds`] - If there are insufficient funds
	/// * [`Error::WalletOperationFailed`] - If fee calculation fails
	///
	/// [`send_all_to_address`]: Self::send_all_to_address
	/// [`BalanceDetails::total_anchor_channels_reserve_sats`]: crate::BalanceDetails::total_anchor_channels_reserve_sats
	pub fn calculate_send_all_fee(
		&self, address: &bitcoin::Address, retain_reserves: bool, fee_rate: Option<FeeRate>,
	) -> Result<u64, Error> {
		if !*self.is_running.read().unwrap() {
			return Err(Error::NotRunning);
		}

		let send_amount = if retain_reserves {
			let cur_anchor_reserve_sats =
				crate::total_anchor_channels_reserve_sats(&self.channel_manager, &self.config);
			OnchainSendAmount::AllRetainingReserve { cur_anchor_reserve_sats }
		} else {
			OnchainSendAmount::AllDrainingReserve
		};

		let fee_rate_opt = maybe_map_fee_rate_opt!(fee_rate);
		self.wallet.calculate_transaction_fee(
			address,
			send_amount,
			fee_rate_opt,
			None,
			&self.channel_manager,
		)
	}

	/// Send an on-chain payment to the given address.
	///
	/// This will respect any on-chain reserve we need to keep, i.e., won't allow to cut into
	/// [`BalanceDetails::total_anchor_channels_reserve_sats`].
	///
	/// If `fee_rate` is set it will be used on the resulting transaction. Otherwise we'll retrieve
	/// a reasonable estimate from the configured chain source.
	///
	/// Returns the transaction ID only after the configured backend accepts the transaction.
	/// The signed transaction is persisted before dispatch. Broadcast errors carry its transaction
	/// ID. If backend acceptance is unknown, the transaction remains available through
	/// [`Self::list_pending_broadcasts`] for reconciliation and exact-transaction retry with
	/// [`Self::rebroadcast_transaction`]. Callers must not create a new transaction for the same
	/// payment intent after [`Error::OnchainTxBroadcastFailed`] or
	/// [`Error::OnchainTxBroadcastTimeout`]. [`Error::OnchainTxBroadcastNotDispatched`] guarantees
	/// that the backend was not invoked and cleanup completed, permitting a fresh send.
	///
	/// [`BalanceDetails::total_anchor_channels_reserve_sats`]: crate::BalanceDetails::total_anchor_channels_reserve_sats
	pub fn send_to_address(
		&self, address: &bitcoin::Address, amount_sats: u64, fee_rate: Option<FeeRate>,
		utxos_to_spend: Option<Vec<SpendableUtxo>>,
	) -> Result<Txid, Error> {
		if !*self.is_running.read().unwrap() {
			return Err(Error::NotRunning);
		}
		let admission = self.begin_explicit_broadcast()?;

		let cur_anchor_reserve_sats =
			crate::total_anchor_channels_reserve_sats(&self.channel_manager, &self.config);
		let send_amount =
			OnchainSendAmount::ExactRetainingReserve { amount_sats, cur_anchor_reserve_sats };
		let outpoints = utxos_to_spend.map(|utxos| utxos.into_iter().map(|u| u.outpoint).collect());
		let fee_rate_opt = maybe_map_fee_rate_opt!(fee_rate);
		let tx = self.wallet.create_send_to_address_transaction(
			address,
			send_amount,
			fee_rate_opt,
			outpoints,
			&self.channel_manager,
		)?;
		self.dispatch_prepared_transaction(admission, tx)
	}

	/// Send an on-chain payment to the given address, draining the available funds.
	///
	/// This is useful if you have closed all channels and want to migrate funds to another
	/// on-chain wallet.
	///
	/// To preview the fee before broadcasting, use [`calculate_send_all_fee`].
	///
	/// Please note that if `retain_reserves` is set to `false` this will **not** retain any on-chain reserves, which might be potentially
	/// dangerous if you have open Anchor channels for which you can't trust the counterparty to
	/// spend the Anchor output after channel closure. If `retain_reserves` is set to `true`, this
	/// will try to send all spendable onchain funds, i.e.,
	/// [`BalanceDetails::spendable_onchain_balance_sats`].
	///
	/// If `fee_rate` is set it will be used on the resulting transaction. Otherwise a reasonable
	/// we'll retrieve an estimate from the configured chain source.
	///
	/// Returns the transaction ID only after the configured backend accepts the transaction.
	/// The signed transaction is persisted before dispatch. Broadcast errors carry its transaction
	/// ID. If backend acceptance is unknown, the transaction remains available through
	/// [`Self::list_pending_broadcasts`] for reconciliation and exact-transaction retry with
	/// [`Self::rebroadcast_transaction`]. Callers must not create a new transaction for the same
	/// payment intent after [`Error::OnchainTxBroadcastFailed`] or
	/// [`Error::OnchainTxBroadcastTimeout`]. [`Error::OnchainTxBroadcastNotDispatched`] guarantees
	/// that the backend was not invoked and cleanup completed, permitting a fresh send.
	///
	/// [`calculate_send_all_fee`]: Self::calculate_send_all_fee
	/// [`BalanceDetails::spendable_onchain_balance_sats`]: crate::balance::BalanceDetails::spendable_onchain_balance_sats
	pub fn send_all_to_address(
		&self, address: &bitcoin::Address, retain_reserves: bool, fee_rate: Option<FeeRate>,
	) -> Result<Txid, Error> {
		if !*self.is_running.read().unwrap() {
			return Err(Error::NotRunning);
		}
		let admission = self.begin_explicit_broadcast()?;

		let send_amount = if retain_reserves {
			let cur_anchor_reserve_sats =
				crate::total_anchor_channels_reserve_sats(&self.channel_manager, &self.config);
			OnchainSendAmount::AllRetainingReserve { cur_anchor_reserve_sats }
		} else {
			OnchainSendAmount::AllDrainingReserve
		};

		let fee_rate_opt = maybe_map_fee_rate_opt!(fee_rate);
		let tx = self.wallet.create_send_to_address_transaction(
			address,
			send_amount,
			fee_rate_opt,
			None,
			&self.channel_manager,
		)?;
		self.dispatch_prepared_transaction(admission, tx)
	}

	/// Rebroadcast a previously prepared on-chain transaction without creating a new spend.
	///
	/// Use this after recovering an acceptance-unknown transaction ID from
	/// [`Self::list_pending_broadcasts`] or a transaction-keyed broadcast error.
	/// The exact persisted transaction is reused, so retrying cannot create a second payment
	/// transaction. On success, the same transaction ID is returned. Any retry error preserves the
	/// original unresolved intent; a retry rejection or not-dispatched outcome therefore maps to
	/// [`Error::OnchainTxBroadcastFailed`] rather than permitting a fresh send. After external
	/// reconciliation proves the transaction absent, [`Self::abandon_pending_broadcast`] provides
	/// the explicit terminal transition that releases its inputs.
	///
	/// Returns [`Error::NotRunning`] if the node is stopped and [`Error::TransactionNotFound`] if the
	/// transaction is not available in the durable broadcast-intent store.
	pub fn rebroadcast_transaction(&self, txid: &Txid) -> Result<Txid, Error> {
		if !*self.is_running.read().unwrap() {
			return Err(Error::NotRunning);
		}
		let admission = self.begin_explicit_broadcast()?;
		let dispatch_lease = Arc::new(BroadcastDispatchLease::acquire(&self.wallet, *txid)?);

		let tx = self.wallet.recover_pending_broadcast(txid)?.ok_or(Error::TransactionNotFound)?;
		let explicit_guard: ExplicitBroadcastGuard = dispatch_lease.clone();
		let dispatch_result = match self.runtime.try_block_on(
			self.tx_broadcaster.broadcast_transaction(admission, tx, Some(explicit_guard)),
		) {
			Ok(result) => result,
			Err(error) => return Err(error),
		};
		match dispatch_result {
			Ok(()) => self.record_accepted_broadcast(*txid),
			Err(error) => {
				self.record_unknown_broadcast(*txid);
				Err(Self::rebroadcast_error(error, *txid))
			},
		}
	}

	/// List explicit broadcasts whose backend acceptance is still unresolved.
	///
	/// Entries survive process restart and ordinary mempool eviction. Each record exposes the
	/// active transaction ID plus the complete RBF lineage required to reconcile before
	/// [`Self::abandon_pending_broadcast`]. Callers must not create a new transaction for the same
	/// payment intent while an entry remains. RBF supersession atomically changes the active
	/// transaction ID while preserving predecessor transactions. An entry is removed only after a
	/// conclusive initial rejection/not-dispatched result, successful explicit broadcast, backend
	/// observation, or explicit reconciled abandon.
	pub fn list_pending_broadcasts(&self) -> Result<Vec<PendingBroadcastInfo>, Error> {
		Ok(self
			.wallet
			.list_pending_broadcast_infos()?
			.into_iter()
			.map(|(txid, lineage)| PendingBroadcastInfo { txid, lineage })
			.collect())
	}

	/// Returns the durable broadcast outcome associated with any transaction in an RBF lineage.
	///
	/// [`BroadcastOutcomeStatus::Pending`] is not proof of backend acceptance. Only
	/// [`BroadcastOutcomeStatus::Accepted`] identifies an authoritative accepted transaction.
	/// Terminal outcomes survive restart, confirmation, payment-history cleanup, and abandonment
	/// without a time-based expiry until [`Self::acknowledge_broadcast_outcome`] removes them.
	/// `None` means the transaction is unknown or its terminal outcome was already acknowledged.
	/// Persistence failures are returned and must remain unresolved by callers.
	///
	/// # Safety and trust boundary
	///
	/// No Rust memory-safety preconditions apply. The configured backend is the authority for
	/// acceptance, while explicit abandonment remains caller-authorized only after independent
	/// mempool and chain reconciliation.
	///
	/// # Example
	///
	/// ```
	/// # use bitcoin::Txid;
	/// # use ldk_node::payment::{BroadcastOutcome, OnchainPayment};
	/// # use ldk_node::NodeError;
	/// # fn outcome(
	/// #     payment: &OnchainPayment,
	/// #     txid: &Txid,
	/// # ) -> Result<Option<BroadcastOutcome>, NodeError> {
	/// payment.broadcast_outcome(txid)
	/// # }
	/// ```
	pub fn broadcast_outcome(&self, txid: &Txid) -> Result<Option<BroadcastOutcome>, Error> {
		Ok(self.wallet.broadcast_outcome(txid)?.map(|(status, txid, lineage)| BroadcastOutcome {
			status,
			txid,
			lineage,
		}))
	}

	/// Acknowledges and removes a durable terminal broadcast outcome.
	///
	/// The lookup key may be any member of the recorded RBF lineage. Acknowledgement is idempotent:
	/// an unknown or already acknowledged transaction succeeds without changing state. A tracked
	/// lineage that is still active returns [`Error::OnchainTxBroadcastFailed`]. Call this only after
	/// the consumer has durably handled a [`BroadcastOutcomeStatus::Accepted`] or
	/// [`BroadcastOutcomeStatus::Abandoned`] result. Storage failures are returned and leave the
	/// outcome queryable for retry.
	///
	/// # Safety and trust boundary
	///
	/// No Rust memory-safety preconditions apply. The caller owns durable downstream delivery and
	/// must not acknowledge before the terminal result can be recovered independently.
	///
	/// # Example
	///
	/// ```
	/// # use bitcoin::Txid;
	/// # use ldk_node::payment::OnchainPayment;
	/// # use ldk_node::NodeError;
	/// # fn acknowledge(
	/// #     payment: &OnchainPayment,
	/// #     txid: &Txid,
	/// # ) -> Result<(), NodeError> {
	/// payment.acknowledge_broadcast_outcome(txid)
	/// # }
	/// ```
	pub fn acknowledge_broadcast_outcome(&self, txid: &Txid) -> Result<(), Error> {
		self.wallet.acknowledge_broadcast_outcome(txid)
	}

	/// Abandon an unresolved explicit broadcast after conclusive external reconciliation.
	///
	/// Call this only after independently verifying that the active transaction and every member of
	/// its [`PendingBroadcastInfo::lineage`] are absent from the mempool and chain and will not be
	/// rebroadcast by another process. Do not create a new spend of the same inputs until that
	/// independent reconciliation is complete. This releases their reserved inputs and removes
	/// their pending payment records. If the active transaction is later observed, ordinary wallet
	/// sync will record it again.
	///
	/// Returns [`Error::NotRunning`] if the node is stopped, [`Error::TransactionNotFound`] if
	/// `txid` is not the active transaction returned by [`Self::list_pending_broadcasts`], and
	/// [`Error::OnchainTxBroadcastFailed`] while the transaction is being dispatched.
	///
	/// # Safety and trust boundary
	///
	/// No Rust memory-safety preconditions apply. The caller must treat the configured backend as
	/// insufficient evidence on its own after an acceptance-unknown result and reconcile against an
	/// independent authoritative mempool/chain source before abandoning.
	///
	/// # Example
	///
	/// ```
	/// # use bitcoin::Txid;
	/// # use ldk_node::payment::OnchainPayment;
	/// # use ldk_node::NodeError;
	/// # fn abandon_reconciled(
	/// #     payment: &OnchainPayment,
	/// #     txid: &Txid,
	/// # ) -> Result<(), NodeError> {
	/// payment.abandon_pending_broadcast(txid)
	/// # }
	/// ```
	pub fn abandon_pending_broadcast(&self, txid: &Txid) -> Result<(), Error> {
		if !*self.is_running.read().unwrap() {
			return Err(Error::NotRunning);
		}
		self.wallet.abandon_broadcast_intent_by_txid(txid)
	}

	/// Bumps the fee of an existing transaction using Replace-By-Fee (RBF).
	///
	/// This allows a previously sent transaction to be replaced with a new version
	/// that pays a higher fee. The original transaction must have been created with
	/// RBF enabled (which is the default for transactions created by LDK).
	///
	/// **Note:** This cannot be used on funding transactions as doing so would invalidate the channel.
	///
	/// # Arguments
	///
	/// * `txid` - The transaction ID of the transaction to be replaced
	/// * `fee_rate` - The new fee rate to use (must be higher than the original fee rate)
	///
	/// # Returns
	///
	/// The replacement transaction ID after the configured backend accepts it.
	///
	/// # Errors
	///
	/// * [`Error::NotRunning`] - If the node is not running
	/// * [`Error::TransactionNotFound`] - If the transaction can't be found in the wallet
	/// * [`Error::TransactionAlreadyConfirmed`] - If the transaction is already confirmed
	/// * [`Error::CannotRbfFundingTransaction`] - If the transaction is a channel funding transaction
	/// * [`Error::InvalidFeeRate`] - If the new fee rate is not higher than the original
	/// * [`Error::OnchainTxCreationFailed`] - If the new transaction couldn't be created
	/// * [`Error::OnchainTxBroadcastRejected`] - If the backend conclusively rejects the replacement
	/// * [`Error::OnchainTxBroadcastNotDispatched`] - If the replacement did not reach the backend
	/// * [`Error::OnchainTxBroadcastFailed`] - If replacement acceptance is unknown
	/// * [`Error::OnchainTxBroadcastTimeout`] - If replacement acceptance timed out
	pub fn bump_fee_by_rbf(&self, txid: &Txid, fee_rate: FeeRate) -> Result<Txid, Error> {
		if !*self.is_running.read().unwrap() {
			return Err(Error::NotRunning);
		}
		let admission = self.begin_explicit_broadcast()?;
		let dispatch_lease = Arc::new(BroadcastDispatchLease::acquire(&self.wallet, *txid)?);

		// Pass through to the wallet implementation
		#[cfg(not(feature = "uniffi"))]
		let fee_rate_param = fee_rate;
		#[cfg(feature = "uniffi")]
		let fee_rate_param = *fee_rate;

		let replacement =
			self.wallet.prepare_rbf_broadcast(txid, fee_rate_param, &self.channel_manager)?;
		let replacement_txid = replacement.compute_txid();
		dispatch_lease.include(replacement_txid)?;
		let explicit_guard: ExplicitBroadcastGuard = dispatch_lease.clone();
		let dispatch_result = match self.runtime.try_block_on(
			self.tx_broadcaster.broadcast_transaction(admission, replacement, Some(explicit_guard)),
		) {
			Ok(result) => result,
			Err(Error::NotRunning) => {
				if let Err(error) = self
					.wallet
					.reject_rbf_broadcast(&replacement_txid)
					.and_then(|_| self.wallet.remove_transient_broadcast_outcome(&replacement_txid))
				{
					return Err(self.record_conclusive_cleanup_failure(replacement_txid, error));
				}
				return Err(Error::NotRunning);
			},
			Err(error) => return Err(error),
		};
		match dispatch_result {
			Ok(()) => self.record_accepted_broadcast(replacement_txid),
			Err(error @ (TxBroadcastError::Rejected | TxBroadcastError::NotDispatched)) => {
				if let Err(cleanup_error) = self
					.wallet
					.reject_rbf_broadcast(&replacement_txid)
					.and_then(|_| self.wallet.remove_transient_broadcast_outcome(&replacement_txid))
				{
					return Err(
						self.record_conclusive_cleanup_failure(replacement_txid, cleanup_error)
					);
				}
				Err(Self::initial_broadcast_error(error, replacement_txid))
			},
			Err(error @ (TxBroadcastError::Failed | TxBroadcastError::Timeout)) => {
				self.record_unknown_broadcast(replacement_txid);
				Err(Self::initial_broadcast_error(error, replacement_txid))
			},
		}
	}

	/// Accelerates confirmation of a transaction using Child-Pays-For-Parent (CPFP).
	///
	/// This creates a new transaction (child) that spends an output from the
	/// transaction to be accelerated (parent), with a high enough fee to pay for both.
	///
	/// # Arguments
	///
	/// * `txid` - The transaction ID of the transaction to be accelerated
	/// * `fee_rate` - The fee rate to use for the child transaction (or None to calculate automatically)
	/// * `destination_address` - Optional address to send the funds to (if None, funds are sent to an internal address)
	///
	/// # Returns
	///
	/// The transaction ID of the child transaction if successful.
	///
	/// # Errors
	///
	/// * [`Error::NotRunning`] - If the node is not running
	/// * [`Error::TransactionNotFound`] - If the transaction can't be found
	/// * [`Error::TransactionAlreadyConfirmed`] - If the transaction is already confirmed
	/// * [`Error::NoSpendableOutputs`] - If the transaction has no spendable outputs
	/// * [`Error::OnchainTxCreationFailed`] - If the child transaction couldn't be created
	pub fn accelerate_by_cpfp(
		&self, txid: &Txid, fee_rate: Option<FeeRate>, destination_address: Option<Address>,
	) -> Result<Txid, Error> {
		if !*self.is_running.read().unwrap() {
			return Err(Error::NotRunning);
		}

		// Calculate fee rate if not provided
		#[cfg(not(feature = "uniffi"))]
		let fee_rate_param = match fee_rate {
			Some(rate) => rate,
			None => self.wallet.calculate_cpfp_fee_rate(txid, true)?,
		};

		#[cfg(feature = "uniffi")]
		let fee_rate_param = match fee_rate {
			Some(rate) => *rate,
			None => self.wallet.calculate_cpfp_fee_rate(txid, true)?,
		};

		// Pass through to the wallet implementation
		self.wallet.accelerate_by_cpfp(txid, fee_rate_param, destination_address)
	}

	/// Calculates an appropriate fee rate for a CPFP transaction to ensure
	/// the parent transaction gets confirmed within the target number of blocks.
	///
	/// This method analyzes the parent transaction's current fee rate and calculates
	/// how much the child transaction needs to pay to bring the combined package
	/// fee rate up to the target level.
	///
	/// # Arguments
	///
	/// * `parent_txid` - The transaction ID of the parent transaction to accelerate
	/// * `urgent` - If true, uses a more aggressive fee rate for faster confirmation
	///
	/// # Returns
	///
	/// The fee rate that should be used for the child transaction.
	///
	/// # Errors
	///
	/// * [`Error::NotRunning`] - If the node is not running
	/// * [`Error::TransactionNotFound`] - If the parent transaction can't be found
	/// * [`Error::TransactionAlreadyConfirmed`] - If the parent transaction is already confirmed
	/// * [`Error::WalletOperationFailed`] - If fee calculation fails
	pub fn calculate_cpfp_fee_rate(
		&self, parent_txid: &Txid, urgent: bool,
	) -> Result<FeeRate, Error> {
		if !*self.is_running.read().unwrap() {
			return Err(Error::NotRunning);
		}

		let fee_rate = self.wallet.calculate_cpfp_fee_rate(parent_txid, urgent)?;

		#[cfg(not(feature = "uniffi"))]
		{
			Ok(fee_rate)
		}
		#[cfg(feature = "uniffi")]
		{
			Ok(Arc::new(fee_rate))
		}
	}
}

#[cfg(test)]
mod tests {
	use std::future::Future;
	use std::pin::Pin;
	use std::sync::{Arc, Condvar, Mutex};
	use std::time::Duration;

	use bitcoin::absolute::LockTime;
	use bitcoin::block::Header;
	use bitcoin::blockdata::constants::genesis_block;
	use bitcoin::hashes::Hash;
	use bitcoin::transaction::Version;
	use bitcoin::{Amount, Block, Network, ScriptBuf, Transaction, TxMerkleNode, TxOut, Txid};
	use lightning::io;
	use lightning::util::persist::{KVStore, KVStoreSync};

	use super::OnchainPayment;
	use crate::builder::NodeBuilder;
	use crate::config::{AddressType, Config, OnchainWalletAccount};
	use crate::error::Error;
	use crate::io::{
		test_utils::InMemoryStore, ONCHAIN_BROADCAST_EVENT_PRIMARY_NAMESPACE,
		ONCHAIN_BROADCAST_INTENT_PRIMARY_NAMESPACE, ONCHAIN_BROADCAST_OUTCOME_PRIMARY_NAMESPACE,
	};
	use crate::payment::BroadcastOutcomeStatus;
	use crate::tx_broadcaster::TxBroadcastError;
	use crate::types::DynStore;
	use crate::{Event, Node};

	#[derive(Default)]
	struct BroadcastWriteState {
		armed: bool,
		blocked: bool,
		released: bool,
	}

	struct BlockingBroadcastIntentStore {
		inner: InMemoryStore,
		state: Mutex<BroadcastWriteState>,
		state_changed: Condvar,
		fail_next_write_namespace: Mutex<Option<String>>,
		fail_next_remove_namespace: Mutex<Option<String>>,
	}

	impl BlockingBroadcastIntentStore {
		fn new() -> Self {
			Self {
				inner: InMemoryStore::new(),
				state: Mutex::new(BroadcastWriteState::default()),
				state_changed: Condvar::new(),
				fail_next_write_namespace: Mutex::new(None),
				fail_next_remove_namespace: Mutex::new(None),
			}
		}

		fn fail_next_write_in(&self, primary_namespace: &str) {
			*self.fail_next_write_namespace.lock().unwrap() = Some(primary_namespace.to_owned());
		}

		fn fail_next_remove_in(&self, primary_namespace: &str) {
			*self.fail_next_remove_namespace.lock().unwrap() = Some(primary_namespace.to_owned());
		}

		fn block_next_broadcast_intent_write(&self) {
			let mut state = self.state.lock().unwrap();
			*state = BroadcastWriteState { armed: true, blocked: false, released: false };
		}

		fn wait_until_broadcast_intent_write_is_blocked(&self) {
			let mut state = self.state.lock().unwrap();
			while !state.blocked {
				state = self.state_changed.wait(state).unwrap();
			}
		}

		fn release_broadcast_intent_write(&self) {
			let mut state = self.state.lock().unwrap();
			state.released = true;
			self.state_changed.notify_all();
		}

		fn maybe_block_broadcast_intent_write(&self, primary_namespace: &str) {
			if primary_namespace != crate::io::ONCHAIN_BROADCAST_INTENT_PRIMARY_NAMESPACE {
				return;
			}
			let mut state = self.state.lock().unwrap();
			if !state.armed {
				return;
			}
			state.blocked = true;
			self.state_changed.notify_all();
			while !state.released {
				state = self.state_changed.wait(state).unwrap();
			}
			state.armed = false;
		}

		fn fail_armed_write(&self, primary_namespace: &str) -> io::Result<()> {
			let mut namespace = self.fail_next_write_namespace.lock().unwrap();
			if namespace.as_deref() == Some(primary_namespace) {
				namespace.take();
				Err(io::Error::new(io::ErrorKind::Other, "Injected namespace write failure"))
			} else {
				Ok(())
			}
		}

		fn fail_armed_remove(&self, primary_namespace: &str) -> io::Result<()> {
			let mut namespace = self.fail_next_remove_namespace.lock().unwrap();
			if namespace.as_deref() == Some(primary_namespace) {
				namespace.take();
				Err(io::Error::new(io::ErrorKind::Other, "Injected namespace remove failure"))
			} else {
				Ok(())
			}
		}
	}

	impl KVStore for BlockingBroadcastIntentStore {
		fn read(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
		) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, io::Error>> + 'static + Send>> {
			KVStore::read(&self.inner, primary_namespace, secondary_namespace, key)
		}

		fn write(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
		) -> Pin<Box<dyn Future<Output = Result<(), io::Error>> + 'static + Send>> {
			KVStore::write(&self.inner, primary_namespace, secondary_namespace, key, buf)
		}

		fn remove(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
		) -> Pin<Box<dyn Future<Output = Result<(), io::Error>> + 'static + Send>> {
			KVStore::remove(&self.inner, primary_namespace, secondary_namespace, key, lazy)
		}

		fn list(
			&self, primary_namespace: &str, secondary_namespace: &str,
		) -> Pin<Box<dyn Future<Output = Result<Vec<String>, io::Error>> + 'static + Send>> {
			KVStore::list(&self.inner, primary_namespace, secondary_namespace)
		}
	}

	impl KVStoreSync for BlockingBroadcastIntentStore {
		fn read(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
		) -> io::Result<Vec<u8>> {
			KVStoreSync::read(&self.inner, primary_namespace, secondary_namespace, key)
		}

		fn write(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
		) -> io::Result<()> {
			self.maybe_block_broadcast_intent_write(primary_namespace);
			self.fail_armed_write(primary_namespace)?;
			KVStoreSync::write(&self.inner, primary_namespace, secondary_namespace, key, buf)
		}

		fn remove(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
		) -> io::Result<()> {
			self.fail_armed_remove(primary_namespace)?;
			KVStoreSync::remove(&self.inner, primary_namespace, secondary_namespace, key, lazy)
		}

		fn list(
			&self, primary_namespace: &str, secondary_namespace: &str,
		) -> io::Result<Vec<String>> {
			KVStoreSync::list(&self.inner, primary_namespace, secondary_namespace)
		}
	}

	fn test_node(store: Arc<DynStore>) -> Node {
		let config = Config { network: Network::Regtest, ..Config::default() };
		let mut builder = NodeBuilder::from_config(config);
		builder.set_chain_source_esplora("http://127.0.0.1:1".to_string(), None);
		builder.set_entropy_seed_bytes([43u8; 64]);
		builder.set_log_facade_logger();
		builder.build_with_store(store).unwrap()
	}

	fn test_transaction() -> Transaction {
		Transaction {
			version: Version::TWO,
			lock_time: LockTime::from_consensus(47),
			input: vec![],
			output: vec![],
		}
	}

	fn tracked_test_transaction(script_pubkey: ScriptBuf) -> Transaction {
		Transaction {
			version: Version::TWO,
			lock_time: LockTime::from_consensus(47),
			input: vec![],
			output: vec![TxOut { value: Amount::from_sat(1), script_pubkey }],
		}
	}

	fn confirmation_block(transaction: Transaction) -> Block {
		let genesis = genesis_block(Network::Regtest);
		Block {
			header: Header {
				version: bitcoin::block::Version::ONE,
				prev_blockhash: genesis.block_hash(),
				merkle_root: TxMerkleNode::from_byte_array(
					transaction.compute_txid().to_byte_array(),
				),
				time: genesis.header.time.saturating_add(1),
				bits: genesis.header.bits,
				nonce: 1,
			},
			txdata: vec![transaction],
		}
	}

	#[test]
	fn rebroadcast_errors_preserve_the_original_unknown_outcome() {
		let txid = Txid::all_zeros();
		for error in
			[TxBroadcastError::Rejected, TxBroadcastError::NotDispatched, TxBroadcastError::Failed]
		{
			assert_eq!(
				OnchainPayment::rebroadcast_error(error, txid),
				Error::OnchainTxBroadcastFailed { txid }
			);
		}
		assert_eq!(
			OnchainPayment::rebroadcast_error(TxBroadcastError::Timeout, txid),
			Error::OnchainTxBroadcastTimeout { txid }
		);
	}

	#[test]
	fn accepted_result_persistence_failure_returns_a_transaction_keyed_unknown_outcome() {
		let concrete_store = Arc::new(BlockingBroadcastIntentStore::new());
		let store: Arc<DynStore> = concrete_store.clone();
		let node = test_node(store);
		let tx =
			tracked_test_transaction(node.onchain_payment().new_address().unwrap().script_pubkey());
		let txid = tx.compute_txid();
		node.wallet.begin_broadcast_dispatch(txid).unwrap();
		node.wallet.prepare_pending_broadcast(&tx).unwrap();
		concrete_store.fail_next_write_in(ONCHAIN_BROADCAST_OUTCOME_PRIMARY_NAMESPACE);

		assert_eq!(
			node.onchain_payment().record_accepted_broadcast(txid),
			Err(Error::OnchainTxBroadcastFailed { txid })
		);
		node.wallet.end_broadcast_dispatches(&[txid]);
		assert_eq!(
			node.onchain_payment().broadcast_outcome(&txid).unwrap().map(|outcome| outcome.status),
			Some(BroadcastOutcomeStatus::Pending)
		);
	}

	#[test]
	fn accepted_result_marker_cleanup_failure_keeps_observed_outcome() {
		let concrete_store = Arc::new(BlockingBroadcastIntentStore::new());
		let store: Arc<DynStore> = concrete_store.clone();
		let node = test_node(store);
		let tx =
			tracked_test_transaction(node.onchain_payment().new_address().unwrap().script_pubkey());
		let txid = tx.compute_txid();
		node.wallet.begin_broadcast_dispatch(txid).unwrap();
		node.wallet.prepare_pending_broadcast(&tx).unwrap();
		node.wallet.apply_mempool_txs(vec![(tx, 1)], Vec::new()).unwrap();
		node.wallet.mark_locally_applied_unconfirmed_delivered(txid).unwrap();
		concrete_store.fail_next_remove_in(ONCHAIN_BROADCAST_EVENT_PRIMARY_NAMESPACE);

		assert_eq!(
			node.onchain_payment().record_accepted_broadcast(txid),
			Err(Error::OnchainTxBroadcastFailed { txid })
		);
		node.wallet.end_broadcast_dispatches(&[txid]);
		assert_eq!(
			node.onchain_payment().broadcast_outcome(&txid).unwrap().map(|outcome| outcome.status),
			Some(BroadcastOutcomeStatus::Accepted)
		);
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn pre_dispatch_retention_survives_unknown_result_write_failure() {
		let concrete_store = Arc::new(BlockingBroadcastIntentStore::new());
		let store: Arc<DynStore> = concrete_store.clone();
		let node = test_node(Arc::clone(&store));
		let tx =
			tracked_test_transaction(node.onchain_payment().new_address().unwrap().script_pubkey());
		let txid = tx.compute_txid();
		let payment = node.onchain_payment();
		let admission = payment.begin_explicit_broadcast().unwrap();
		let send_tx = tx.clone();
		let dispatch = tokio::task::spawn_blocking(move || {
			payment.dispatch_prepared_transaction(admission, send_tx)
		});
		let mut receivers = node.tx_broadcaster.get_broadcast_queue_receivers().await;
		let request = receivers.recv().await.unwrap();
		concrete_store.fail_next_write_in(ONCHAIN_BROADCAST_INTENT_PRIMARY_NAMESPACE);
		request.send_result(Err(TxBroadcastError::Failed));
		drop(receivers);

		assert_eq!(dispatch.await.unwrap(), Err(Error::OnchainTxBroadcastFailed { txid }));
		drop(node);

		let restarted = test_node(store);
		restarted.wallet.apply_mempool_txs(vec![(tx, 1)], Vec::new()).unwrap();
		assert_eq!(
			restarted
				.onchain_payment()
				.broadcast_outcome(&txid)
				.unwrap()
				.map(|outcome| outcome.status),
			Some(BroadcastOutcomeStatus::Accepted)
		);
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn retry_success_retains_outcome_after_promotion_write_failure() {
		let concrete_store = Arc::new(BlockingBroadcastIntentStore::new());
		let store: Arc<DynStore> = concrete_store.clone();
		let node = test_node(Arc::clone(&store));
		let tx =
			tracked_test_transaction(node.onchain_payment().new_address().unwrap().script_pubkey());
		let txid = tx.compute_txid();
		let payment = node.onchain_payment();
		let admission = payment.begin_explicit_broadcast().unwrap();
		let send_tx = tx.clone();
		let initial = tokio::task::spawn_blocking(move || {
			payment.dispatch_prepared_transaction(admission, send_tx)
		});
		let mut receivers = node.tx_broadcaster.get_broadcast_queue_receivers().await;
		let request = receivers.recv().await.unwrap();
		concrete_store.fail_next_write_in(ONCHAIN_BROADCAST_INTENT_PRIMARY_NAMESPACE);
		request.send_result(Err(TxBroadcastError::Failed));
		drop(receivers);
		assert_eq!(initial.await.unwrap(), Err(Error::OnchainTxBroadcastFailed { txid }));
		drop(node);

		let retried = test_node(Arc::clone(&store));
		*retried.is_running.write().unwrap() = true;
		let payment = retried.onchain_payment();
		let retry = tokio::task::spawn_blocking(move || payment.rebroadcast_transaction(&txid));
		let mut receivers = retried.tx_broadcaster.get_broadcast_queue_receivers().await;
		let request = receivers.recv().await.unwrap();
		assert_eq!(request.package, vec![tx]);
		request.send_result(Ok(()));
		drop(receivers);
		assert_eq!(retry.await.unwrap(), Ok(txid));
		drop(retried);

		let restarted = test_node(store);
		assert_eq!(
			restarted
				.onchain_payment()
				.broadcast_outcome(&txid)
				.unwrap()
				.map(|outcome| outcome.status),
			Some(BroadcastOutcomeStatus::Accepted)
		);
		restarted.onchain_payment().acknowledge_broadcast_outcome(&txid).unwrap();
		assert!(restarted.onchain_payment().broadcast_outcome(&txid).unwrap().is_none());
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn failed_send_survives_eviction_restart_and_retry_until_accepted() {
		let store: Arc<DynStore> = Arc::new(InMemoryStore::new());
		let node = test_node(Arc::clone(&store));
		let tx =
			tracked_test_transaction(node.onchain_payment().new_address().unwrap().script_pubkey());
		let txid = tx.compute_txid();
		let payment = node.onchain_payment();
		let send_tx = tx.clone();
		let admission = payment.begin_explicit_broadcast().unwrap();
		let initial_call = tokio::task::spawn_blocking(move || {
			payment.dispatch_prepared_transaction(admission, send_tx)
		});
		let mut receivers = node.tx_broadcaster.get_broadcast_queue_receivers().await;
		let initial_request = receivers.recv().await.unwrap();
		assert_eq!(initial_request.package, vec![tx.clone()]);
		initial_request.send_result(Err(TxBroadcastError::Failed));
		drop(receivers);
		assert_eq!(initial_call.await.unwrap(), Err(Error::OnchainTxBroadcastFailed { txid }));
		crate::chain::process_wallet_events(
			Vec::new(),
			&node.wallet,
			&node.event_queue,
			&node.logger,
			Some(&node.channel_manager),
			None,
		)
		.await
		.unwrap();
		assert!(matches!(
			node.next_event(),
			Some(Event::OnchainTransactionReceived { txid: received_txid, .. }) if received_txid == txid
		));
		node.event_handled().unwrap();
		node.wallet.apply_mempool_txs(Vec::new(), vec![(txid, 1)]).unwrap();
		assert_eq!(node.wallet.list_pending_broadcasts().unwrap(), vec![txid]);
		drop(node);

		let restarted_node = test_node(store);
		assert_eq!(restarted_node.wallet.list_pending_broadcasts().unwrap(), vec![txid]);
		*restarted_node.is_running.write().unwrap() = true;

		let payment = restarted_node.onchain_payment();
		let retry_call =
			tokio::task::spawn_blocking(move || payment.rebroadcast_transaction(&txid));
		let mut receivers = restarted_node.tx_broadcaster.get_broadcast_queue_receivers().await;
		let retry_request = receivers.recv().await.unwrap();
		assert_eq!(retry_request.package, vec![tx.clone()]);
		retry_request.send_result(Err(TxBroadcastError::NotDispatched));
		drop(receivers);
		assert_eq!(retry_call.await.unwrap(), Err(Error::OnchainTxBroadcastFailed { txid }));
		assert_eq!(restarted_node.wallet.list_pending_broadcasts().unwrap(), vec![txid]);

		let payment = restarted_node.onchain_payment();
		let accepted_call =
			tokio::task::spawn_blocking(move || payment.rebroadcast_transaction(&txid));
		let mut receivers = restarted_node.tx_broadcaster.get_broadcast_queue_receivers().await;
		let accepted_request = receivers.recv().await.unwrap();
		assert_eq!(accepted_request.package, vec![tx]);
		accepted_request.send_result(Ok(()));
		drop(receivers);
		assert_eq!(accepted_call.await.unwrap(), Ok(txid));
		assert!(restarted_node.wallet.list_pending_broadcasts().unwrap().is_empty());
		crate::chain::process_wallet_events(
			Vec::new(),
			&restarted_node.wallet,
			&restarted_node.event_queue,
			&restarted_node.logger,
			Some(&restarted_node.channel_manager),
			None,
		)
		.await
		.unwrap();
		assert!(restarted_node.next_event().is_none());
		*restarted_node.is_running.write().unwrap() = false;
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn accepted_send_publishes_received_event_after_restart() {
		let store: Arc<DynStore> = Arc::new(InMemoryStore::new());
		let node = test_node(Arc::clone(&store));
		let tx =
			tracked_test_transaction(node.onchain_payment().new_address().unwrap().script_pubkey());
		let txid = tx.compute_txid();
		let payment = node.onchain_payment();
		let admission = payment.begin_explicit_broadcast().unwrap();
		let send = tokio::task::spawn_blocking(move || {
			payment.dispatch_prepared_transaction(admission, tx)
		});
		let mut receivers = node.tx_broadcaster.get_broadcast_queue_receivers().await;
		let request = receivers.recv().await.unwrap();
		request.send_result(Ok(()));
		drop(receivers);
		assert_eq!(send.await.unwrap(), Ok(txid));
		drop(node);

		let restarted = test_node(store);
		crate::chain::process_wallet_events(
			Vec::new(),
			&restarted.wallet,
			&restarted.event_queue,
			&restarted.logger,
			Some(&restarted.channel_manager),
			None,
		)
		.await
		.unwrap();
		assert!(matches!(
			restarted.next_event(),
			Some(Event::OnchainTransactionReceived { txid: received_txid, .. }) if received_txid == txid
		));
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn handled_received_event_is_not_repeated_after_delivery_write_failure() {
		let concrete_store = Arc::new(BlockingBroadcastIntentStore::new());
		let store: Arc<DynStore> = concrete_store.clone();
		let node = test_node(Arc::clone(&store));
		let tx =
			tracked_test_transaction(node.onchain_payment().new_address().unwrap().script_pubkey());
		let txid = tx.compute_txid();
		node.wallet.prepare_pending_broadcast(&tx).unwrap();
		node.wallet.publish_locally_applied_unconfirmed(txid).unwrap();
		concrete_store.fail_next_write_in(crate::io::ONCHAIN_BROADCAST_EVENT_PRIMARY_NAMESPACE);

		assert_eq!(
			crate::chain::process_wallet_events(
				Vec::new(),
				&node.wallet,
				&node.event_queue,
				&node.logger,
				Some(&node.channel_manager),
				None,
			)
			.await,
			Err(Error::PersistenceFailed)
		);
		assert!(matches!(
			node.next_event(),
			Some(Event::OnchainTransactionReceived { txid: received_txid, .. }) if received_txid == txid
		));
		node.event_handled().unwrap();
		drop(node);

		let restarted = test_node(store);
		crate::chain::process_wallet_events(
			Vec::new(),
			&restarted.wallet,
			&restarted.event_queue,
			&restarted.logger,
			Some(&restarted.channel_manager),
			None,
		)
		.await
		.unwrap();
		assert!(restarted.next_event().is_none());
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn received_marker_failure_followed_by_confirmation_emits_both_transitions() {
		let concrete_store = Arc::new(BlockingBroadcastIntentStore::new());
		let store: Arc<DynStore> = concrete_store.clone();
		let node = test_node(Arc::clone(&store));
		let tx =
			tracked_test_transaction(node.onchain_payment().new_address().unwrap().script_pubkey());
		let txid = tx.compute_txid();
		node.wallet.prepare_pending_broadcast(&tx).unwrap();
		node.wallet.apply_mempool_txs(vec![(tx.clone(), 1)], Vec::new()).unwrap();
		concrete_store.fail_next_remove_in(crate::io::ONCHAIN_BROADCAST_EVENT_PRIMARY_NAMESPACE);

		assert_eq!(
			crate::chain::process_wallet_events(
				Vec::new(),
				&node.wallet,
				&node.event_queue,
				&node.logger,
				Some(&node.channel_manager),
				None,
			)
			.await,
			Err(Error::PersistenceFailed)
		);
		assert!(matches!(
			node.next_event(),
			Some(Event::OnchainTransactionReceived { txid: received_txid, .. })
				if received_txid == txid
		));
		node.event_handled().unwrap();

		let block = confirmation_block(tx);
		node.wallet
			.apply_block_to_account(
				OnchainWalletAccount::account_zero(AddressType::NativeSegwit),
				&block,
				1,
			)
			.unwrap();
		node.wallet.finish_pending_sync(false).unwrap();
		crate::chain::process_wallet_events(
			Vec::new(),
			&node.wallet,
			&node.event_queue,
			&node.logger,
			Some(&node.channel_manager),
			None,
		)
		.await
		.unwrap();
		assert!(matches!(
			node.next_event(),
			Some(Event::OnchainTransactionConfirmed { txid: confirmed_txid, .. })
				if confirmed_txid == txid
		));
		node.event_handled().unwrap();
		drop(node);

		let restarted = test_node(store);
		crate::chain::process_wallet_events(
			Vec::new(),
			&restarted.wallet,
			&restarted.event_queue,
			&restarted.logger,
			Some(&restarted.channel_manager),
			None,
		)
		.await
		.unwrap();
		assert!(restarted.next_event().is_none());
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn handled_confirmation_is_not_repeated_after_delivery_write_failure() {
		let concrete_store = Arc::new(BlockingBroadcastIntentStore::new());
		let store: Arc<DynStore> = concrete_store.clone();
		let node = test_node(Arc::clone(&store));
		let tx =
			tracked_test_transaction(node.onchain_payment().new_address().unwrap().script_pubkey());
		let txid = tx.compute_txid();
		node.wallet.prepare_pending_broadcast(&tx).unwrap();
		node.wallet.apply_mempool_txs(vec![(tx.clone(), 1)], Vec::new()).unwrap();
		let block = confirmation_block(tx);
		node.wallet
			.apply_block_to_account(
				OnchainWalletAccount::account_zero(AddressType::NativeSegwit),
				&block,
				1,
			)
			.unwrap();
		node.wallet.finish_pending_sync(false).unwrap();
		concrete_store.fail_next_remove_in(crate::io::ONCHAIN_BROADCAST_EVENT_PRIMARY_NAMESPACE);

		assert_eq!(
			crate::chain::process_wallet_events(
				Vec::new(),
				&node.wallet,
				&node.event_queue,
				&node.logger,
				Some(&node.channel_manager),
				None,
			)
			.await,
			Err(Error::PersistenceFailed)
		);
		assert!(matches!(
			node.next_event(),
			Some(Event::OnchainTransactionConfirmed { txid: confirmed_txid, .. })
				if confirmed_txid == txid
		));
		node.event_handled().unwrap();
		drop(node);

		let restarted = test_node(store);
		crate::chain::process_wallet_events(
			Vec::new(),
			&restarted.wallet,
			&restarted.event_queue,
			&restarted.logger,
			Some(&restarted.channel_manager),
			None,
		)
		.await
		.unwrap();
		assert!(restarted.next_event().is_none());
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn concurrent_wallet_event_delivery_is_serialized() {
		let store: Arc<DynStore> = Arc::new(InMemoryStore::new());
		let node = test_node(store);
		let tx =
			tracked_test_transaction(node.onchain_payment().new_address().unwrap().script_pubkey());
		let txid = tx.compute_txid();
		node.wallet.prepare_pending_broadcast(&tx).unwrap();
		node.wallet.publish_locally_applied_unconfirmed(txid).unwrap();

		let wallet = Arc::clone(&node.wallet);
		let event_queue = Arc::clone(&node.event_queue);
		let logger = Arc::clone(&node.logger);
		let channel_manager = Arc::clone(&node.channel_manager);
		let delivery_guard = node.wallet.lock_broadcast_event_delivery().await;
		let mut first = tokio::spawn(async move {
			crate::chain::process_wallet_events(
				Vec::new(),
				&wallet,
				&event_queue,
				&logger,
				Some(&channel_manager),
				None,
			)
			.await
		});
		assert!(tokio::time::timeout(Duration::from_millis(25), &mut first).await.is_err());

		let wallet = Arc::clone(&node.wallet);
		let event_queue = Arc::clone(&node.event_queue);
		let logger = Arc::clone(&node.logger);
		let channel_manager = Arc::clone(&node.channel_manager);
		let second = tokio::spawn(async move {
			crate::chain::process_wallet_events(
				Vec::new(),
				&wallet,
				&event_queue,
				&logger,
				Some(&channel_manager),
				None,
			)
			.await
		});

		drop(delivery_guard);
		first.await.unwrap().unwrap();
		node.event_handled().unwrap();
		second.await.unwrap().unwrap();
		assert!(node.next_event().is_none());
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn backend_observation_after_restart_publishes_failed_send_event() {
		let store: Arc<DynStore> = Arc::new(InMemoryStore::new());
		let node = test_node(Arc::clone(&store));
		let tx =
			tracked_test_transaction(node.onchain_payment().new_address().unwrap().script_pubkey());
		let txid = tx.compute_txid();
		node.wallet.prepare_pending_broadcast(&tx).unwrap();
		node.wallet.mark_broadcast_outcome_required(&txid).unwrap();
		drop(node);

		let restarted = test_node(store);
		restarted.wallet.apply_mempool_txs(vec![(tx, 1)], Vec::new()).unwrap();
		crate::chain::process_wallet_events(
			Vec::new(),
			&restarted.wallet,
			&restarted.event_queue,
			&restarted.logger,
			Some(&restarted.channel_manager),
			None,
		)
		.await
		.unwrap();
		assert!(matches!(
			restarted.next_event(),
			Some(Event::OnchainTransactionReceived { txid: received_txid, .. }) if received_txid == txid
		));
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn claimed_dispatch_cannot_be_abandoned() {
		let store: Arc<DynStore> = Arc::new(InMemoryStore::new());
		let node = test_node(store);
		let tx = test_transaction();
		let txid = tx.compute_txid();
		let payment = node.onchain_payment();
		let admission = payment.begin_explicit_broadcast().unwrap();
		let dispatch = tokio::task::spawn_blocking(move || {
			payment.dispatch_prepared_transaction(admission, tx)
		});
		let mut receivers = node.tx_broadcaster.get_broadcast_queue_receivers().await;
		let request = receivers.recv().await.unwrap();
		assert_eq!(
			node.wallet.abandon_broadcast_intent_by_txid(&txid),
			Err(Error::OnchainTxBroadcastFailed { txid })
		);
		request.send_result(Err(TxBroadcastError::Failed));
		drop(receivers);
		assert_eq!(dispatch.await.unwrap(), Err(Error::OnchainTxBroadcastFailed { txid }));
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn stopped_worker_retains_dispatch_lease_until_blocking_backend_finishes() {
		let store: Arc<DynStore> = Arc::new(InMemoryStore::new());
		let node = test_node(store);
		let tx = test_transaction();
		let txid = tx.compute_txid();
		let payment = node.onchain_payment();
		let admission = payment.begin_explicit_broadcast().unwrap();
		let dispatch = tokio::task::spawn_blocking(move || {
			payment.dispatch_prepared_transaction(admission, tx)
		});
		let mut receivers = node.tx_broadcaster.get_broadcast_queue_receivers().await;
		let request = receivers.recv().await.unwrap();
		let explicit_guard = request.take_explicit_guard();
		let crate::tx_broadcaster::BroadcastRequest {
			result_sender,
			package: _,
			explicit_claim: _,
			ldk_claim: _,
		} = request;
		let (release_sender, release_receiver) = std::sync::mpsc::channel();
		let backend = tokio::task::spawn_blocking(move || {
			let _explicit_guard = explicit_guard.unwrap();
			release_receiver.recv().unwrap();
		});
		node.tx_broadcaster.pause_explicit_broadcasts();
		drop(result_sender);
		drop(receivers);

		assert_eq!(dispatch.await.unwrap(), Err(Error::OnchainTxBroadcastFailed { txid }));
		assert_eq!(
			node.wallet.abandon_broadcast_intent_by_txid(&txid),
			Err(Error::OnchainTxBroadcastFailed { txid })
		);

		release_sender.send(()).unwrap();
		backend.await.unwrap();
		node.wallet.abandon_broadcast_intent_by_txid(&txid).unwrap();
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn accepted_send_returns_unknown_when_event_outbox_write_fails() {
		let concrete_store = Arc::new(BlockingBroadcastIntentStore::new());
		let store: Arc<DynStore> = concrete_store.clone();
		let node = test_node(Arc::clone(&store));
		let tx =
			tracked_test_transaction(node.onchain_payment().new_address().unwrap().script_pubkey());
		let txid = tx.compute_txid();
		let payment = node.onchain_payment();
		let admission = payment.begin_explicit_broadcast().unwrap();
		let send_tx = tx.clone();
		let dispatch = tokio::task::spawn_blocking(move || {
			payment.dispatch_prepared_transaction(admission, send_tx)
		});
		let mut receivers = node.tx_broadcaster.get_broadcast_queue_receivers().await;
		let request = receivers.recv().await.unwrap();
		concrete_store.fail_next_write_in(crate::io::ONCHAIN_BROADCAST_EVENT_PRIMARY_NAMESPACE);
		let explicit_guard = request.take_explicit_guard();
		let crate::tx_broadcaster::BroadcastRequest {
			result_sender,
			package: _,
			explicit_claim: _,
			ldk_claim: _,
		} = request;
		result_sender.unwrap().send(Ok(())).unwrap();
		drop(explicit_guard);
		drop(receivers);

		assert_eq!(dispatch.await.unwrap(), Err(Error::OnchainTxBroadcastFailed { txid }));
		assert_eq!(node.wallet.list_pending_broadcasts().unwrap(), vec![txid]);
		assert_eq!(node.wallet.ready_locally_applied_unconfirmed_txids().unwrap(), vec![txid]);
		drop(node);

		let restarted = test_node(store);
		restarted.wallet.apply_mempool_txs(vec![(tx, 1)], Vec::new()).unwrap();
		crate::chain::process_wallet_events(
			Vec::new(),
			&restarted.wallet,
			&restarted.event_queue,
			&restarted.logger,
			Some(&restarted.channel_manager),
			None,
		)
		.await
		.unwrap();
		assert!(matches!(
			restarted.next_event(),
			Some(Event::OnchainTransactionReceived { txid: received_txid, .. }) if received_txid == txid
		));
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn unknown_send_preserves_txid_error_when_event_outbox_write_fails() {
		for backend_error in [TxBroadcastError::Failed, TxBroadcastError::Timeout] {
			let concrete_store = Arc::new(BlockingBroadcastIntentStore::new());
			let store: Arc<DynStore> = concrete_store.clone();
			let node = test_node(store);
			let tx = test_transaction();
			let txid = tx.compute_txid();
			let payment = node.onchain_payment();
			let admission = payment.begin_explicit_broadcast().unwrap();
			let dispatch = tokio::task::spawn_blocking(move || {
				payment.dispatch_prepared_transaction(admission, tx)
			});
			let mut receivers = node.tx_broadcaster.get_broadcast_queue_receivers().await;
			let request = receivers.recv().await.unwrap();
			concrete_store.fail_next_write_in(crate::io::ONCHAIN_BROADCAST_EVENT_PRIMARY_NAMESPACE);
			let explicit_guard = request.take_explicit_guard();
			let crate::tx_broadcaster::BroadcastRequest {
				result_sender,
				package: _,
				explicit_claim: _,
				ldk_claim: _,
			} = request;
			result_sender.unwrap().send(Err(backend_error)).unwrap();
			drop(explicit_guard);
			drop(receivers);

			let expected_error = match backend_error {
				TxBroadcastError::Failed => Error::OnchainTxBroadcastFailed { txid },
				TxBroadcastError::Timeout => Error::OnchainTxBroadcastTimeout { txid },
				_ => unreachable!(),
			};
			assert_eq!(dispatch.await.unwrap(), Err(expected_error));
			assert_eq!(node.wallet.list_pending_broadcasts().unwrap(), vec![txid]);
		}
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn sync_delivery_during_dispatch_is_not_republished_by_late_success() {
		let store: Arc<DynStore> = Arc::new(InMemoryStore::new());
		let node = test_node(store);
		let tx =
			tracked_test_transaction(node.onchain_payment().new_address().unwrap().script_pubkey());
		let txid = tx.compute_txid();
		let payment = node.onchain_payment();
		let admission = payment.begin_explicit_broadcast().unwrap();
		let send_tx = tx.clone();
		let dispatch = tokio::task::spawn_blocking(move || {
			payment.dispatch_prepared_transaction(admission, send_tx)
		});
		let mut receivers = node.tx_broadcaster.get_broadcast_queue_receivers().await;
		let request = receivers.recv().await.unwrap();

		node.wallet.apply_mempool_txs(vec![(tx, 1)], Vec::new()).unwrap();
		crate::chain::process_wallet_events(
			Vec::new(),
			&node.wallet,
			&node.event_queue,
			&node.logger,
			Some(&node.channel_manager),
			None,
		)
		.await
		.unwrap();
		assert!(matches!(
			node.next_event(),
			Some(Event::OnchainTransactionReceived { txid: received_txid, .. }) if received_txid == txid
		));
		node.event_handled().unwrap();

		let explicit_guard = request.take_explicit_guard();
		let crate::tx_broadcaster::BroadcastRequest {
			result_sender,
			package: _,
			explicit_claim: _,
			ldk_claim: _,
		} = request;
		result_sender.unwrap().send(Ok(())).unwrap();
		drop(explicit_guard);
		drop(receivers);
		assert_eq!(dispatch.await.unwrap(), Ok(txid));

		crate::chain::process_wallet_events(
			Vec::new(),
			&node.wallet,
			&node.event_queue,
			&node.logger,
			Some(&node.channel_manager),
			None,
		)
		.await
		.unwrap();
		assert!(node.next_event().is_none());
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn retry_rejection_can_be_conclusively_abandoned() {
		let store: Arc<DynStore> = Arc::new(InMemoryStore::new());
		let tx = test_transaction();
		let txid = tx.compute_txid();
		let node = test_node(store);
		node.wallet.prepare_pending_broadcast(&tx).unwrap();
		*node.is_running.write().unwrap() = true;

		let payment = node.onchain_payment();
		let retry = tokio::task::spawn_blocking(move || payment.rebroadcast_transaction(&txid));
		let mut receivers = node.tx_broadcaster.get_broadcast_queue_receivers().await;
		let request = receivers.recv().await.unwrap();
		request.send_result(Err(TxBroadcastError::Rejected));
		drop(receivers);

		assert_eq!(retry.await.unwrap(), Err(Error::OnchainTxBroadcastFailed { txid }));
		assert_eq!(node.wallet.list_pending_broadcasts().unwrap(), vec![txid]);
		node.onchain_payment().abandon_pending_broadcast(&txid).unwrap();
		assert!(node.wallet.list_pending_broadcasts().unwrap().is_empty());
		assert_eq!(node.wallet.recover_pending_broadcast(&txid).unwrap(), None);
		*node.is_running.write().unwrap() = false;
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn pre_stop_admission_cannot_enqueue_after_restart_when_prepare_stalls() {
		let concrete_store = Arc::new(BlockingBroadcastIntentStore::new());
		let store: Arc<DynStore> = concrete_store.clone();
		let tx = test_transaction();
		let txid = tx.compute_txid();
		let node = test_node(store);
		*node.is_running.write().unwrap() = true;
		let payment = node.onchain_payment();
		let admission = payment.begin_explicit_broadcast().unwrap();
		concrete_store.block_next_broadcast_intent_write();

		let dispatch = tokio::task::spawn_blocking(move || {
			payment.dispatch_prepared_transaction(admission, tx)
		});
		let blocking_store = Arc::clone(&concrete_store);
		tokio::task::spawn_blocking(move || {
			blocking_store.wait_until_broadcast_intent_write_is_blocked()
		})
		.await
		.unwrap();

		node.tx_broadcaster.pause_explicit_broadcasts();
		node.tx_broadcaster.drain_explicit_broadcasts().await;
		node.tx_broadcaster.resume_explicit_broadcasts();
		concrete_store.release_broadcast_intent_write();

		assert_eq!(dispatch.await.unwrap(), Err(Error::OnchainTxBroadcastNotDispatched { txid }));
		assert!(node.wallet.list_pending_broadcasts().unwrap().is_empty());
		let mut receivers = node.tx_broadcaster.get_broadcast_queue_receivers().await;
		assert!(tokio::time::timeout(std::time::Duration::from_millis(20), receivers.recv())
			.await
			.is_err());
		*node.is_running.write().unwrap() = false;
	}
}

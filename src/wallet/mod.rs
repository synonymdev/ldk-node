// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::ops::Deref;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use bdk_chain::spk_client::{FullScanRequest, SyncRequest};
use bdk_chain::ConfirmationBlockTime;
use bdk_wallet::event::WalletEvent;
use bdk_wallet::{
	AddressInfo, Balance, KeychainKind as BdkKeychainKind, LocalOutput, PersistedWallet, Update,
};
use bdk_wallet_aggregate::{AggregateWallet, UtxoPsbtInfo};
use bitcoin::address::NetworkUnchecked;
use bitcoin::bip32::Xpriv;
use bitcoin::blockdata::constants::WITNESS_SCALE_FACTOR;
use bitcoin::blockdata::locktime::absolute::LockTime;
use bitcoin::consensus::{deserialize, serialize};
use bitcoin::hashes::Hash;
use bitcoin::key::XOnlyPublicKey;
use bitcoin::psbt::{self, Psbt};
use bitcoin::secp256k1::ecdh::SharedSecret;
use bitcoin::secp256k1::ecdsa::{RecoverableSignature, Signature};
use bitcoin::secp256k1::{All, PublicKey, Scalar, Secp256k1, SecretKey};
use bitcoin::{
	Address, Amount, FeeRate, Network, OutPoint, PubkeyHash, Script, ScriptBuf, Transaction, TxIn,
	TxOut, Txid, WPubkeyHash, Weight, WitnessProgram, WitnessVersion,
};
use lightning::chain::chaininterface::BroadcasterInterface;
use lightning::chain::channelmonitor::ANTI_REORG_DELAY;
use lightning::chain::{BestBlock, Listen};
use lightning::events::bump_transaction::{Input, Utxo, WalletSource};
use lightning::ln::channelmanager::PaymentId;
use lightning::ln::funding::FundingTxInput;
use lightning::ln::inbound_payment::ExpandedKey;
use lightning::ln::msgs::UnsignedGossipMessage;
use lightning::ln::script::ShutdownScript;
use lightning::sign::{
	ChangeDestinationSource, EntropySource, InMemorySigner, KeysManager, NodeSigner, OutputSpender,
	PeerStorageKey, Recipient, SignerProvider, SpendableOutputDescriptor,
};
use lightning::util::message_signing;
use lightning::util::persist::KVStoreSync;
use lightning_invoice::RawBolt11Invoice;
use persist::KVStoreWalletPersister;
use zeroize::Zeroizing;

use crate::config::{
	AddressType, AddressTypeRuntimeConfig, Config, OnchainWalletAccount, WALLET_KEYS_SEED_LEN,
};
use crate::event::{TxInput, TxOutput};
use crate::fee_estimator::{ConfirmationTarget, FeeEstimator, OnchainFeeEstimator};
use crate::io::{
	ONCHAIN_BROADCAST_INTENT_PRIMARY_NAMESPACE, ONCHAIN_BROADCAST_INTENT_SECONDARY_NAMESPACE,
};
use crate::logger::{log_debug, log_error, log_info, log_trace, LdkLogger, Logger};
use crate::payment::store::ConfirmationStatus;
use crate::payment::{KeychainKind, PaymentDetails, PaymentDirection, PaymentStatus};
use crate::types::{Broadcaster, ChannelManager, DynStore, PaymentStore};
use crate::{Error, NodeMetrics};

// Minimum economical output value (dust limit)
const DUST_LIMIT_SATS: u64 = 546;
const BIP32_MAX_NORMAL_INDEX: u32 = (1 << 31) - 1;
const MAX_ADDRESS_INFO_BATCH_COUNT: u32 = bdk_wallet_aggregate::MAX_ADDRESS_INFO_BATCH_COUNT;
const LEGACY_ONCHAIN_BROADCAST_INTENT_SERIALIZATION_VERSION: u8 = 1;
const LEGACY_RBF_BROADCAST_INTENT_SERIALIZATION_VERSION: u8 = 2;
const ONCHAIN_BROADCAST_INTENT_SERIALIZATION_VERSION: u8 = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
struct BroadcastIntent {
	transactions: Vec<Transaction>,
	// Transactions before this index are retained lineage. Equality with `len` means resolved.
	first_pending_index: u32,
}

impl BroadcastIntent {
	fn new(tx: Transaction) -> Self {
		Self { transactions: vec![tx], first_pending_index: 0 }
	}

	fn replacement(
		existing: Option<Self>, original: Transaction, replacement: Transaction,
	) -> Result<Self, Error> {
		let is_new_intent = existing.is_none();
		let mut intent = existing.unwrap_or_else(|| Self::new(original));
		intent.supersede(replacement)?;
		if is_new_intent {
			// The original was already accepted. Retain it only as reconciliation evidence.
			intent.first_pending_index = 1;
		}
		Ok(intent)
	}

	fn key(&self) -> Txid {
		self.transactions.first().expect("broadcast intents are non-empty").compute_txid()
	}

	fn active_transaction(&self) -> &Transaction {
		self.transactions.last().expect("broadcast intents are non-empty")
	}

	fn active_txid(&self) -> Txid {
		self.active_transaction().compute_txid()
	}

	fn has_pending_transaction(&self) -> bool {
		self.transactions.len() > self.first_pending_index as usize
	}

	fn mark_resolved(&mut self) -> Result<(), Error> {
		self.first_pending_index =
			u32::try_from(self.transactions.len()).map_err(|_| Error::WalletOperationFailed)?;
		Ok(())
	}

	fn supersede(&mut self, replacement: Transaction) -> Result<(), Error> {
		let replaces_active = replacement.input.iter().any(|replacement_input| {
			self.active_transaction().input.iter().any(|active_input| {
				active_input.previous_output == replacement_input.previous_output
			})
		});
		if !replaces_active {
			return Err(Error::OnchainTxCreationFailed);
		}
		self.transactions.push(replacement);
		Ok(())
	}
}

fn validate_derivation_index(index: u32) -> Result<(), Error> {
	if index > BIP32_MAX_NORMAL_INDEX {
		return Err(Error::InvalidQuantity);
	}
	Ok(())
}

fn validate_derivation_range(start_index: u32, count: u32) -> Result<(), Error> {
	validate_derivation_index(start_index)?;
	if count > MAX_ADDRESS_INFO_BATCH_COUNT {
		return Err(Error::InvalidQuantity);
	}
	if count == 0 {
		return Ok(());
	}

	let last_index = start_index.checked_add(count - 1).ok_or(Error::InvalidQuantity)?;
	validate_derivation_index(last_index)
}

fn backend_observed_txids(update: &Update) -> HashSet<Txid> {
	update
		.tx_update
		.anchors
		.iter()
		.map(|(_, txid)| *txid)
		.chain(update.tx_update.seen_ats.iter().map(|(txid, _)| *txid))
		.collect()
}

fn backend_confirmed_txids(update: &Update) -> HashSet<Txid> {
	update.tx_update.anchors.iter().map(|(_, txid)| *txid).collect()
}

fn observed_broadcast_intents(
	intents: &[(Txid, BroadcastIntent)], observed_txids: &HashSet<Txid>,
	confirmed_txids: &HashSet<Txid>,
) -> Vec<(Txid, Txid, bool)> {
	intents
		.iter()
		.filter_map(|(key, intent)| {
			if let Some(txid) = intent
				.transactions
				.iter()
				.rev()
				.map(|tx| tx.compute_txid())
				.find(|txid| confirmed_txids.contains(txid))
			{
				return Some((*key, txid, true));
			}
			intent
				.transactions
				.iter()
				.rev()
				.map(|tx| tx.compute_txid())
				.find(|txid| observed_txids.contains(txid))
				.map(|txid| (*key, txid, false))
		})
		.collect()
}

fn map_wallet_account_error(
	wallet_account: OnchainWalletAccount, error: bdk_wallet_aggregate::Error,
) -> Error {
	match error {
		bdk_wallet_aggregate::Error::WalletNotFound if wallet_account.is_derived() => {
			Error::OnchainWalletAccountNotRegistered
		},
		_ => Error::WalletOperationFailed,
	}
}

fn additional_input_weight(utxos: &[UtxoPsbtInfo]) -> Result<Weight, Error> {
	let base_weight = TxIn::default().segwit_weight().to_wu();
	let total = utxos.iter().try_fold(0u64, |total, utxo| {
		total
			.checked_add(base_weight)
			.and_then(|weight| weight.checked_add(utxo.weight.to_wu()))
			.ok_or(Error::InvalidFeeRate)
	})?;
	Ok(Weight::from_wu(total))
}

#[derive(Clone, Copy)]
pub(crate) enum OnchainSendAmount {
	ExactRetainingReserve { amount_sats: u64, cur_anchor_reserve_sats: u64 },
	AllRetainingReserve { cur_anchor_reserve_sats: u64 },
	AllDrainingReserve,
}

/// Available coin selection algorithms
#[derive(Debug, Clone, Copy)]
pub enum CoinSelectionAlgorithm {
	/// Branch and bound algorithm (tries to find exact match)
	BranchAndBound,
	/// Select largest UTXOs first
	LargestFirst,
	/// Select oldest UTXOs first
	OldestFirst,
	/// Select UTXOs randomly
	SingleRandomDraw,
}

pub(crate) mod persist;
pub(crate) mod ser;

pub(crate) struct Wallet {
	inner: Mutex<AggregateWallet<OnchainWalletAccount, KVStoreWalletPersister>>,
	// Serializes raw-intent persistence with wallet reservation and backend reconciliation.
	broadcast_intent_lock: Mutex<()>,
	// Keyed by the first transaction ID so an RBF replacement can atomically update one record.
	broadcast_intents: Mutex<HashMap<Txid, BroadcastIntent>>,
	// Serializes account membership, primary selection, and account reloads.
	operation_lock: Mutex<()>,
	account_generation: AtomicU64,
	synced_derived_accounts: Mutex<HashSet<OnchainWalletAccount>>,
	broadcaster: Arc<Broadcaster>,
	fee_estimator: Arc<OnchainFeeEstimator>,
	payment_store: Arc<PaymentStore>,
	payment_store_update_pending: AtomicBool,
	config: Arc<Config>,
	kv_store: Arc<DynStore>,
	seed_bytes: Zeroizing<[u8; WALLET_KEYS_SEED_LEN]>,
	address_type_runtime_config: Arc<RwLock<AddressTypeRuntimeConfig>>,
	node_metrics: Arc<RwLock<NodeMetrics>>,
	logger: Arc<Logger>,
	derived_account_lookahead: u32,
}

impl Wallet {
	pub(crate) fn new(
		wallet: bdk_wallet::PersistedWallet<KVStoreWalletPersister>,
		wallet_persister: KVStoreWalletPersister,
		additional_wallets: Vec<(
			OnchainWalletAccount,
			PersistedWallet<KVStoreWalletPersister>,
			KVStoreWalletPersister,
		)>,
		seed_bytes: [u8; WALLET_KEYS_SEED_LEN], broadcaster: Arc<Broadcaster>,
		fee_estimator: Arc<OnchainFeeEstimator>, payment_store: Arc<PaymentStore>,
		config: Arc<Config>, kv_store: Arc<DynStore>,
		address_type_runtime_config: Arc<RwLock<AddressTypeRuntimeConfig>>,
		node_metrics: Arc<RwLock<NodeMetrics>>, logger: Arc<Logger>,
		derived_account_lookahead: u32,
	) -> Self {
		let primary_account = OnchainWalletAccount::account_zero(config.address_type);
		let aggregate =
			AggregateWallet::new(wallet, wallet_persister, primary_account, additional_wallets);
		let inner = Mutex::new(aggregate);
		let operation_lock = Mutex::new(());
		Self {
			inner,
			broadcast_intent_lock: Mutex::new(()),
			broadcast_intents: Mutex::new(HashMap::new()),
			operation_lock,
			account_generation: AtomicU64::new(0),
			synced_derived_accounts: Mutex::new(HashSet::new()),
			broadcaster,
			fee_estimator,
			payment_store,
			payment_store_update_pending: AtomicBool::new(false),
			config,
			kv_store,
			seed_bytes: Zeroizing::new(seed_bytes),
			address_type_runtime_config,
			node_metrics,
			logger,
			derived_account_lookahead,
		}
	}

	pub(crate) fn is_funding_transaction(
		&self, txid: &Txid, channel_manager: &ChannelManager,
	) -> bool {
		// Check all channels (pending and confirmed) for matching funding txid
		for channel in channel_manager.list_channels() {
			if let Some(funding_txo) = channel.funding_txo {
				if funding_txo.txid == *txid {
					log_debug!(
						self.logger,
						"Transaction {} is a funding transaction for channel {}",
						txid,
						channel.channel_id
					);
					return true;
				}
			}
		}
		false
	}

	pub(crate) fn estimate_fee_rate(&self, target: ConfirmationTarget) -> FeeRate {
		self.fee_estimator.estimate_fee_rate(target)
	}

	pub(crate) fn get_full_scan_request(&self) -> FullScanRequest<BdkKeychainKind> {
		self.inner.lock().unwrap().start_full_scan().build()
	}

	pub(crate) fn get_incremental_sync_request(&self) -> SyncRequest<(BdkKeychainKind, u32)> {
		self.inner.lock().unwrap().start_sync_with_revealed_spks().build()
	}

	pub(crate) fn get_wallet_full_scan_request(
		&self, wallet_account: OnchainWalletAccount,
	) -> Result<FullScanRequest<BdkKeychainKind>, bdk_wallet_aggregate::Error> {
		self.inner.lock().unwrap().wallet_full_scan_request(&wallet_account)
	}

	pub(crate) fn get_wallet_incremental_sync_request(
		&self, wallet_account: OnchainWalletAccount,
	) -> Result<SyncRequest<(BdkKeychainKind, u32)>, bdk_wallet_aggregate::Error> {
		let aggregate = self.inner.lock().unwrap();
		if wallet_account.account_index == 0 {
			aggregate.wallet_incremental_sync_request(&wallet_account)
		} else {
			aggregate.wallet_incremental_sync_request_with_lookahead(&wallet_account)
		}
	}

	pub(crate) fn get_cached_txs(&self) -> Vec<Arc<Transaction>> {
		self.inner.lock().unwrap().cached_txs()
	}

	pub(crate) fn get_unconfirmed_txids(&self) -> Vec<Txid> {
		self.inner.lock().unwrap().unconfirmed_txids()
	}

	pub(crate) fn get_unconfirmed_txids_with_last_seen(&self) -> Vec<(Txid, u64)> {
		self.inner.lock().unwrap().unconfirmed_txids_with_last_seen()
	}

	pub(crate) fn transaction_confirmations(&self) -> HashMap<Txid, ConfirmationBlockTime> {
		self.inner.lock().unwrap().transaction_confirmations()
	}

	pub(crate) fn current_best_block(&self) -> BestBlock {
		let (block_hash, height) = self.inner.lock().unwrap().current_best_block();
		BestBlock { block_hash, height }
	}

	/// Returns an account generation and matching chain-tip snapshot for Bitcoind sync.
	pub(crate) fn account_sync_snapshot(&self) -> (u64, Vec<(OnchainWalletAccount, BestBlock)>) {
		let _operation = self.operation_lock.lock().unwrap();
		let generation = self.account_generation.load(Ordering::Acquire);
		let tips = self
			.inner
			.lock()
			.unwrap()
			.chain_tips_by_key()
			.into_iter()
			.map(|(account, block_hash, height)| (account, BestBlock { block_hash, height }))
			.collect();
		(generation, tips)
	}

	pub(crate) fn lock_account_operations(&self) -> MutexGuard<'_, ()> {
		self.operation_lock.lock().unwrap()
	}

	pub(crate) fn account_generation(&self) -> u64 {
		self.account_generation.load(Ordering::Acquire)
	}

	pub(crate) fn rewind_wallet_account(
		&self, account: OnchainWalletAccount, checkpoint: BestBlock,
	) -> Result<(), Error> {
		let _operation = self.operation_lock.lock().unwrap();
		let xprv =
			Xpriv::new_master(self.config.network, self.seed_bytes.as_slice()).map_err(|e| {
				log_error!(
					self.logger,
					"Failed to derive master secret while rewinding wallet: {}",
					e
				);
				Error::WalletOperationFailed
			})?;
		let mut aggregate = self.inner.lock().unwrap();
		if aggregate.wallet(&account).is_none() {
			return Err(Error::WalletOperationFailed);
		}
		aggregate.persist_all().map_err(|e| {
			log_error!(self.logger, "Failed to persist {:?} before wallet rewind: {}", account, e);
			Error::PersistenceFailed
		})?;
		let mut persister = KVStoreWalletPersister::new(
			Arc::clone(&self.kv_store),
			Arc::clone(&self.logger),
			account,
		);
		let checkpoint_id =
			bdk_chain::BlockId { height: checkpoint.height, hash: checkpoint.block_hash };
		persister.rewind_to_checkpoint(checkpoint_id).map_err(|e| {
			log_error!(self.logger, "Failed to persist wallet rewind for {:?}: {}", account, e);
			Error::PersistenceFailed
		})?;

		let (wallet, loaded_from_store) = crate::builder::get_or_create_wallet_for_account(
			account,
			xprv,
			self.config.network,
			&mut persister,
			(account.account_index != 0).then_some(self.derived_account_lookahead),
		)
		.map_err(|e| {
			log_error!(self.logger, "Failed to reload wallet {:?} after rewind: {}", account, e);
			Error::WalletOperationFailed
		})?;
		if !loaded_from_store || wallet.latest_checkpoint().block_id() != checkpoint_id {
			log_error!(
				self.logger,
				"Wallet {:?} did not reload at the requested checkpoint",
				account
			);
			return Err(Error::WalletOperationFailed);
		}

		aggregate.wallets_mut().insert(account, wallet);
		aggregate.persisters_mut().insert(account, persister);
		self.payment_store_update_pending.store(true, Ordering::Release);
		Ok(())
	}

	pub(crate) fn apply_block_to_account(
		&self, account: OnchainWalletAccount, block: &bitcoin::Block, height: u32,
	) -> Result<(), Error> {
		let _intent = self.broadcast_intent_lock.lock().unwrap();
		let pending_intents = self.read_all_broadcast_intents()?;
		let block_txids = block.txdata.iter().map(|tx| tx.compute_txid()).collect::<HashSet<_>>();
		let resolutions = observed_broadcast_intents(&pending_intents, &block_txids, &block_txids);
		let mut locked = self.inner.lock().unwrap();
		self.payment_store_update_pending.store(true, Ordering::Release);
		locked.apply_block_to(&account, block, height).map_err(|e| {
			log_error!(
				self.logger,
				"Failed to apply block {} at height {} to {:?}: {}",
				block.block_hash(),
				height,
				account,
				e
			);
			match e {
				bdk_wallet_aggregate::Error::PersistenceFailed => Error::PersistenceFailed,
				_ => Error::WalletOperationFailed,
			}
		})?;
		drop(locked);
		self.resolve_broadcast_intents(resolutions)
	}

	pub(crate) fn finish_pending_sync(&self, refresh_payment_store: bool) -> Result<(), Error> {
		let _intent = self.broadcast_intent_lock.lock().unwrap();
		let mut locked = self.inner.lock().unwrap();
		locked.persist_all().map_err(|e| {
			log_error!(self.logger, "Failed to persist pending wallet changes: {}", e);
			Error::PersistenceFailed
		})?;
		if refresh_payment_store || self.payment_store_update_pending.load(Ordering::Acquire) {
			self.update_payment_store(&locked).map_err(|e| {
				log_error!(
					self.logger,
					"Failed to update payment store after wallet persistence: {}",
					e
				);
				e
			})?;
		}
		drop(locked);
		Ok(())
	}

	// Get a drain script for change outputs.
	pub(crate) fn get_drain_script(&self) -> Result<ScriptBuf, Error> {
		let locked_wallet = self.inner.lock().unwrap();
		let change_address = locked_wallet.peek_address(BdkKeychainKind::Internal, 0);
		Ok(change_address.address.script_pubkey())
	}

	/// Returns the list of all loaded account-0 address types (primary + monitored).
	pub(crate) fn get_loaded_address_types(&self) -> Vec<AddressType> {
		self.inner
			.lock()
			.unwrap()
			.loaded_keys()
			.into_iter()
			.filter(|account| account.account_index == 0)
			.map(|account| account.address_type)
			.collect()
	}

	/// Returns all loaded on-chain wallet accounts, including derived accounts.
	pub(crate) fn get_loaded_onchain_wallet_accounts(&self) -> Vec<OnchainWalletAccount> {
		self.inner.lock().unwrap().loaded_keys()
	}

	pub(crate) fn has_onchain_wallet_account(&self, account: OnchainWalletAccount) -> bool {
		self.inner.lock().unwrap().wallet(&account).is_some()
	}

	pub(crate) fn has_synced_derived_account(&self, account: OnchainWalletAccount) -> bool {
		debug_assert_ne!(account.account_index, 0);
		self.synced_derived_accounts.lock().unwrap().contains(&account)
	}

	pub(crate) fn mark_derived_account_synced(&self, account: OnchainWalletAccount) {
		debug_assert_ne!(account.account_index, 0);
		self.synced_derived_accounts.lock().unwrap().insert(account);
	}

	/// Adds an address type to the monitored set, creating its wallet if not already loaded.
	///
	/// Only account `0` address types can be managed through this API. Derived accounts must
	/// use [`Self::add_onchain_wallet_account`].
	pub(crate) fn add_monitored_address_type(
		&self, address_type: AddressType, seed_bytes: &[u8; WALLET_KEYS_SEED_LEN],
	) -> Result<(), Error> {
		let _op = self.operation_lock.lock().unwrap();
		let wallet_account = OnchainWalletAccount::account_zero(address_type);

		{
			let runtime_config = self.address_type_runtime_config.read().unwrap();
			if runtime_config.primary == address_type {
				return Err(Error::AddressTypeIsPrimary);
			}
			if runtime_config.monitored.contains(&address_type) {
				return Err(Error::AddressTypeAlreadyMonitored);
			}
		}

		let (wallet, persister) = create_wallet_for_account(
			seed_bytes,
			self.config.network,
			wallet_account,
			self.current_best_block(),
			Arc::clone(&self.kv_store),
			Arc::clone(&self.logger),
			None,
		)?;

		{
			let mut aggregate = self.inner.lock().unwrap();
			aggregate.add_wallet(wallet_account, wallet, persister).map_err(|e| {
				log_error!(self.logger, "Failed to add wallet for {:?}: {}", wallet_account, e);
				Error::WalletOperationFailed
			})?;
			self.account_generation.fetch_add(1, Ordering::AcqRel);
			self.address_type_runtime_config.write().unwrap().monitored.push(address_type);
		}

		log_info!(self.logger, "Added address type {:?} to monitor", address_type);
		Ok(())
	}

	/// Removes an address type from monitoring and unloads its wallet.
	/// Persisted state is retained so re-adding recovers funds on the next sync.
	pub(crate) fn remove_monitored_address_type(
		&self, address_type: AddressType,
	) -> Result<(), Error> {
		let _op = self.operation_lock.lock().unwrap();
		let wallet_account = OnchainWalletAccount::account_zero(address_type);

		{
			let runtime_config = self.address_type_runtime_config.read().unwrap();
			if runtime_config.primary == address_type {
				return Err(Error::AddressTypeIsPrimary);
			}
			if !runtime_config.monitored.contains(&address_type) {
				return Err(Error::AddressTypeNotMonitored);
			}
		}

		let mut removed = false;
		{
			let mut aggregate = self.inner.lock().unwrap();
			match aggregate.remove_wallet(wallet_account) {
				Ok(()) => removed = true,
				Err(bdk_wallet_aggregate::Error::CannotRemovePrimary) => {
					return Err(Error::AddressTypeIsPrimary);
				},
				Err(bdk_wallet_aggregate::Error::WalletNotFound) => {
					log_debug!(
						self.logger,
						"Wallet for {:?} was not in aggregate (already unloaded)",
						wallet_account
					);
				},
				Err(e) => {
					log_error!(self.logger, "Failed to remove wallet {:?}: {}", wallet_account, e);
					return Err(Error::WalletOperationFailed);
				},
			}
			self.address_type_runtime_config
				.write()
				.unwrap()
				.monitored
				.retain(|&at| at != address_type);
			if removed {
				self.account_generation.fetch_add(1, Ordering::AcqRel);
			}
		}

		log_info!(self.logger, "Removed address type {:?} from monitor", address_type);
		Ok(())
	}

	/// Sets the primary address type for account `0`, creating its wallet if not already loaded.
	/// The previous primary is demoted to the monitored set.
	pub(crate) fn set_primary_address_type(
		&self, address_type: AddressType, seed_bytes: &[u8; WALLET_KEYS_SEED_LEN],
	) -> Result<(), Error> {
		let _op = self.operation_lock.lock().unwrap();
		let wallet_account = OnchainWalletAccount::account_zero(address_type);

		let old_primary = self.address_type_runtime_config.read().unwrap().primary;
		if address_type == old_primary {
			return Ok(());
		}

		let already_loaded = self.inner.lock().unwrap().loaded_keys().contains(&wallet_account);

		let new_wallet = if !already_loaded {
			Some(create_wallet_for_account(
				seed_bytes,
				self.config.network,
				wallet_account,
				self.current_best_block(),
				Arc::clone(&self.kv_store),
				Arc::clone(&self.logger),
				None,
			)?)
		} else {
			None
		};

		{
			let mut aggregate = self.inner.lock().unwrap();
			if let Some((wallet, persister)) = new_wallet {
				aggregate.add_wallet(wallet_account, wallet, persister).map_err(|e| {
					log_error!(self.logger, "Failed to add wallet for {:?}: {}", wallet_account, e);
					Error::WalletOperationFailed
				})?;
				self.account_generation.fetch_add(1, Ordering::AcqRel);
			}

			aggregate.set_primary(wallet_account).map_err(|e| {
				log_error!(
					self.logger,
					"Failed to set primary address type to {:?}: {}",
					address_type,
					e
				);
				Error::WalletOperationFailed
			})?;

			let mut runtime_config = self.address_type_runtime_config.write().unwrap();
			runtime_config.primary = address_type;
			runtime_config.monitored.retain(|&at| at != address_type);
			if !runtime_config.monitored.contains(&old_primary) {
				runtime_config.monitored.push(old_primary);
			}
		}

		// Clear primary sync timestamp for never-synced types so the next cycle does a full
		// scan. Additional wallets have independent per-account timestamps in node_metrics.
		let needs_full_scan =
			self.node_metrics.read().unwrap().get_wallet_sync_timestamp(wallet_account).is_none();
		if needs_full_scan {
			self.node_metrics.write().unwrap().latest_onchain_wallet_sync_timestamp = None;
		}

		log_info!(self.logger, "Set primary address type to {:?}", address_type);
		Ok(())
	}

	/// Exports the account-level xpub for `(address_type, account_index)`.
	pub(crate) fn export_onchain_wallet_account_xpub(
		&self, address_type: AddressType, account_index: u32,
	) -> Result<String, Error> {
		if account_index > crate::config::MAX_ONCHAIN_WALLET_ACCOUNT_INDEX {
			log_error!(
				self.logger,
				"Account index {} exceeds the maximum hardened BIP32 account index",
				account_index
			);
			return Err(Error::InvalidQuantity);
		}

		let xprv =
			Xpriv::new_master(self.config.network, self.seed_bytes.as_slice()).map_err(|e| {
				log_error!(self.logger, "Failed to derive master secret: {}", e);
				Error::InvalidSeedBytes
			})?;
		crate::builder::derive_account_xpub_string(
			xprv,
			self.config.network,
			address_type,
			account_index,
		)
		.map_err(|e| {
			log_error!(
				self.logger,
				"Failed to export xpub for {:?}/{}: {}",
				address_type,
				account_index,
				e
			);
			Error::InvalidSeedBytes
		})
	}

	/// Registers a derived on-chain wallet account (`account_index >= 1`).
	///
	/// Runtime registration is not persisted. Idempotent for the same xpub.
	pub(crate) fn add_onchain_wallet_account(
		&self, address_type: AddressType, account_index: u32, xpub: &str,
	) -> Result<(), Error> {
		if account_index == 0 || account_index > crate::config::MAX_ONCHAIN_WALLET_ACCOUNT_INDEX {
			log_error!(
				self.logger,
				"Derived account index must be in 1..={}",
				crate::config::MAX_ONCHAIN_WALLET_ACCOUNT_INDEX
			);
			return Err(Error::InvalidQuantity);
		}

		let wallet_account = OnchainWalletAccount { address_type, account_index };

		let expected_xpub = self.export_onchain_wallet_account_xpub(address_type, account_index)?;
		let provided = xpub.trim();
		if provided != expected_xpub {
			log_error!(
				self.logger,
				"Provided xpub does not match the node's master seed for {:?}/{}",
				address_type,
				account_index
			);
			return Err(Error::InvalidSeedBytes);
		}

		let _op = self.operation_lock.lock().unwrap();

		{
			let mut aggregate = self.inner.lock().unwrap();
			if aggregate.loaded_keys().contains(&wallet_account) {
				aggregate.persist_all().map_err(|e| {
					log_error!(
						self.logger,
						"Failed to persist derived account {:?}: {}",
						wallet_account,
						e
					);
					Error::PersistenceFailed
				})?;
				drop(aggregate);
				let mut runtime_config = self.address_type_runtime_config.write().unwrap();
				if !runtime_config.derived_accounts.contains(&wallet_account) {
					runtime_config.derived_accounts.push(wallet_account);
				}
				log_info!(self.logger, "Derived account {:?} already loaded", wallet_account);
				return Ok(());
			}
		}

		let (wallet, persister) = create_wallet_for_account(
			&*self.seed_bytes,
			self.config.network,
			wallet_account,
			self.current_best_block(),
			Arc::clone(&self.kv_store),
			Arc::clone(&self.logger),
			Some(self.derived_account_lookahead),
		)?;

		{
			let mut aggregate = self.inner.lock().unwrap();
			aggregate.add_wallet_and_persist(wallet_account, wallet, persister).map_err(|e| {
				log_error!(
					self.logger,
					"Failed to add derived account {:?}: {}",
					wallet_account,
					e
				);
				match e {
					bdk_wallet_aggregate::Error::PersistenceFailed => Error::PersistenceFailed,
					_ => Error::WalletOperationFailed,
				}
			})?;
			self.account_generation.fetch_add(1, Ordering::AcqRel);
			let mut runtime_config = self.address_type_runtime_config.write().unwrap();
			if !runtime_config.derived_accounts.contains(&wallet_account) {
				runtime_config.derived_accounts.push(wallet_account);
			}
		}

		log_info!(self.logger, "Registered derived on-chain wallet account {:?}", wallet_account);
		Ok(())
	}

	/// Unloads a derived on-chain wallet account while retaining its persisted state.
	pub(crate) fn remove_onchain_wallet_account(
		&self, address_type: AddressType, account_index: u32,
	) -> Result<(), Error> {
		if account_index == 0 || account_index > crate::config::MAX_ONCHAIN_WALLET_ACCOUNT_INDEX {
			return Err(Error::InvalidQuantity);
		}

		let wallet_account = OnchainWalletAccount { address_type, account_index };
		let _op = self.operation_lock.lock().unwrap();
		if !self
			.address_type_runtime_config
			.read()
			.unwrap()
			.derived_accounts
			.contains(&wallet_account)
		{
			return Err(Error::OnchainWalletAccountNotRegistered);
		}

		let removed = {
			let mut aggregate = self.inner.lock().unwrap();
			match aggregate.remove_wallet(wallet_account) {
				Ok(()) => true,
				Err(bdk_wallet_aggregate::Error::WalletNotFound) => false,
				Err(e) => {
					log_error!(
						self.logger,
						"Failed to remove derived account {:?}: {}",
						wallet_account,
						e
					);
					return Err(Error::WalletOperationFailed);
				},
			}
		};

		self.address_type_runtime_config
			.write()
			.unwrap()
			.derived_accounts
			.retain(|account| *account != wallet_account);
		self.synced_derived_accounts.lock().unwrap().remove(&wallet_account);
		if removed {
			self.account_generation.fetch_add(1, Ordering::AcqRel);
		}

		log_info!(self.logger, "Removed derived on-chain wallet account {:?}", wallet_account);
		Ok(())
	}

	/// Callers are responsible for calling `update_payment_store_for_all_transactions`
	/// after all updates have been applied.
	pub(crate) fn apply_update(
		&self, update: impl Into<Update>,
	) -> Result<Vec<WalletEvent>, Error> {
		let _intent = self.broadcast_intent_lock.lock().unwrap();
		let update = update.into();
		let authoritative_txids = backend_observed_txids(&update);
		let confirmed_txids = backend_confirmed_txids(&update);
		let pending_intents = self.read_all_broadcast_intents()?;
		let resolutions =
			observed_broadcast_intents(&pending_intents, &authoritative_txids, &confirmed_txids);
		let observed_intent_keys =
			resolutions.iter().map(|(key, _, _)| *key).collect::<HashSet<_>>();
		let unresolved_txs = pending_intents
			.into_iter()
			.filter(|(key, intent)| {
				intent.has_pending_transaction() && !observed_intent_keys.contains(key)
			})
			.map(|(_, intent)| intent.active_transaction().clone())
			.collect();
		let mut locked_wallet = self.inner.lock().unwrap();
		match locked_wallet.apply_update(update) {
			Ok((events, _txids)) => {
				self.reapply_unresolved_broadcasts(&mut locked_wallet, unresolved_txs)?;
				drop(locked_wallet);
				self.resolve_broadcast_intents(resolutions)?;
				Ok(events)
			},
			Err(e) => {
				log_error!(self.logger, "Sync failed due to chain connection error: {}", e);
				Err(match e {
					bdk_wallet_aggregate::Error::PersistenceFailed
					| bdk_wallet_aggregate::Error::PersisterNotFound => Error::PersistenceFailed,
					_ => Error::WalletOperationFailed,
				})
			},
		}
	}

	/// Callers are responsible for calling `update_payment_store_for_all_transactions`
	/// after all updates have been applied.
	pub(crate) fn apply_update_for_wallet_account(
		&self, wallet_account: OnchainWalletAccount, update: impl Into<Update>,
	) -> Result<Option<Vec<WalletEvent>>, Error> {
		let _op = self.operation_lock.lock().unwrap();
		let _intent = self.broadcast_intent_lock.lock().unwrap();
		let update = update.into();
		let authoritative_txids = backend_observed_txids(&update);
		let confirmed_txids = backend_confirmed_txids(&update);
		let pending_intents = self.read_all_broadcast_intents()?;
		let resolutions =
			observed_broadcast_intents(&pending_intents, &authoritative_txids, &confirmed_txids);
		let observed_intent_keys =
			resolutions.iter().map(|(key, _, _)| *key).collect::<HashSet<_>>();
		let unresolved_txs = pending_intents
			.into_iter()
			.filter(|(key, intent)| {
				intent.has_pending_transaction() && !observed_intent_keys.contains(key)
			})
			.map(|(_, intent)| intent.active_transaction().clone())
			.collect();
		let mut locked_wallet = self.inner.lock().unwrap();
		if locked_wallet.wallet(&wallet_account).is_none() {
			return Ok(None);
		}
		match locked_wallet.apply_update_to_wallet(wallet_account, update) {
			Ok((events, _txids)) => {
				self.reapply_unresolved_broadcasts(&mut locked_wallet, unresolved_txs)?;
				drop(locked_wallet);
				self.resolve_broadcast_intents(resolutions)?;
				Ok(Some(events))
			},
			Err(e) => {
				log_error!(
					self.logger,
					"Failed to apply update for wallet account {:?}: {}",
					wallet_account,
					e
				);
				Err(match e {
					bdk_wallet_aggregate::Error::PersistenceFailed
					| bdk_wallet_aggregate::Error::PersisterNotFound => Error::PersistenceFailed,
					_ => Error::WalletOperationFailed,
				})
			},
		}
	}

	pub(crate) fn apply_mempool_txs(
		&self, unconfirmed_txs: Vec<(Transaction, u64)>, evicted_txids: Vec<(Txid, u64)>,
	) -> Result<(), Error> {
		let _intent = self.broadcast_intent_lock.lock().unwrap();
		let authoritative_txids =
			unconfirmed_txs.iter().map(|(tx, _)| tx.compute_txid()).collect::<HashSet<_>>();
		let pending_intents = self.read_all_broadcast_intents()?;
		let resolutions =
			observed_broadcast_intents(&pending_intents, &authoritative_txids, &HashSet::new());
		let observed_intent_keys =
			resolutions.iter().map(|(key, _, _)| *key).collect::<HashSet<_>>();
		let unresolved_txs = pending_intents
			.into_iter()
			.filter(|(key, intent)| {
				intent.has_pending_transaction() && !observed_intent_keys.contains(key)
			})
			.map(|(_, intent)| intent.active_transaction().clone())
			.collect();
		let mut locked_wallet = self.inner.lock().unwrap();
		self.payment_store_update_pending.store(true, Ordering::Release);
		locked_wallet.apply_mempool_txs(unconfirmed_txs, evicted_txids).map_err(|e| {
			log_error!(self.logger, "Failed to apply mempool txs: {}", e);
			Error::PersistenceFailed
		})?;
		self.reapply_unresolved_broadcasts(&mut locked_wallet, unresolved_txs)?;
		self.update_payment_store(&locked_wallet)?;
		drop(locked_wallet);
		self.resolve_broadcast_intents(resolutions)
	}

	/// Durably reserve and index a signed transaction before backend dispatch.
	pub(crate) fn prepare_pending_broadcast(&self, tx: &Transaction) -> Result<(), Error> {
		let _intent = self.broadcast_intent_lock.lock().unwrap();
		let txid = tx.compute_txid();
		self.write_broadcast_intent(&BroadcastIntent::new(tx.clone()))?;
		let mut locked_wallet = self.inner.lock().unwrap();
		let last_seen = Self::next_broadcast_timestamp(&locked_wallet, &[txid])?;
		self.payment_store_update_pending.store(true, Ordering::Release);
		if let Err(e) = locked_wallet.apply_mempool_txs(vec![(tx.clone(), last_seen)], Vec::new()) {
			log_error!(self.logger, "Failed to reserve pending transaction {}: {}", txid, e);
			return Err(Error::OnchainTxBroadcastFailed { txid });
		}
		if let Err(e) = self.update_payment_store(&locked_wallet) {
			log_error!(self.logger, "Failed to index pending transaction {}: {}", txid, e);
			return Err(Error::OnchainTxBroadcastFailed { txid });
		}
		Ok(())
	}

	/// Release and forget a transaction only after a conclusive initial-send outcome.
	pub(crate) fn abandon_broadcast_intent(&self, tx: &Transaction) -> Result<(), Error> {
		self.abandon_broadcast_intent_by_txid(&tx.compute_txid())
	}

	/// Release every transaction in an intent after conclusive external reconciliation.
	pub(crate) fn abandon_broadcast_intent_by_txid(&self, txid: &Txid) -> Result<(), Error> {
		let _intent = self.broadcast_intent_lock.lock().unwrap();
		let (intent_key, intent) =
			self.find_broadcast_intent_by_active_txid(txid).ok_or(Error::TransactionNotFound)?;
		if !intent.has_pending_transaction() {
			return Err(Error::TransactionNotFound);
		}
		let txids = intent.transactions.iter().map(|tx| tx.compute_txid()).collect::<Vec<_>>();
		let mut locked_wallet = self.inner.lock().unwrap();
		let abandoned_at = Self::next_broadcast_timestamp(&locked_wallet, &txids)?;
		locked_wallet.abandon_txs(&intent.transactions, abandoned_at).map_err(|e| {
			log_error!(self.logger, "Failed to abandon pending transaction {}: {}", txid, e);
			Error::PersistenceFailed
		})?;
		drop(locked_wallet);
		for intent_txid in txids {
			self.payment_store.remove(&PaymentId(intent_txid.to_byte_array()))?;
		}
		self.remove_broadcast_intent(&intent_key)
	}

	/// Recover and reserve the exact durable transaction bytes for an explicit rebroadcast.
	pub(crate) fn recover_pending_broadcast(
		&self, txid: &Txid,
	) -> Result<Option<Transaction>, Error> {
		let _intent = self.broadcast_intent_lock.lock().unwrap();
		let Some((_, intent)) = self.find_broadcast_intent_by_active_txid(txid) else {
			return Ok(None);
		};
		if !intent.has_pending_transaction() {
			return Ok(None);
		}
		let tx = intent.active_transaction().clone();
		let mut locked_wallet = self.inner.lock().unwrap();
		let last_seen = Self::next_broadcast_timestamp(&locked_wallet, &[*txid])?;
		self.payment_store_update_pending.store(true, Ordering::Release);
		if let Err(e) = locked_wallet.apply_mempool_txs(vec![(tx.clone(), last_seen)], Vec::new()) {
			log_error!(self.logger, "Failed to restore pending transaction {}: {}", txid, e);
			return Err(Error::OnchainTxBroadcastFailed { txid: *txid });
		}
		if let Err(e) = self.update_payment_store(&locked_wallet) {
			log_error!(self.logger, "Failed to restore payment index for {}: {}", txid, e);
			return Err(Error::OnchainTxBroadcastFailed { txid: *txid });
		}
		Ok(Some(tx))
	}

	/// List transaction IDs whose backend acceptance still requires reconciliation.
	pub(crate) fn list_pending_broadcasts(&self) -> Result<Vec<Txid>, Error> {
		let _intent = self.broadcast_intent_lock.lock().unwrap();
		Ok(self
			.read_all_broadcast_intents()?
			.into_iter()
			.filter(|(_, intent)| intent.has_pending_transaction())
			.map(|(_, intent)| intent.active_txid())
			.collect())
	}

	/// Resolve an accepted broadcast while retaining multi-hop RBF lineage until confirmation.
	pub(crate) fn clear_broadcast_intent(&self, txid: &Txid) -> Result<(), Error> {
		let _intent = self.broadcast_intent_lock.lock().unwrap();
		let (intent_key, intent) =
			self.find_broadcast_intent_by_active_txid(txid).ok_or(Error::TransactionNotFound)?;
		self.resolve_broadcast_intent(intent_key, intent, *txid, false)
	}

	/// Restore unresolved intent reservations after loading the wallet from persistent storage.
	pub(crate) fn restore_pending_broadcasts(&self) -> Result<(), Error> {
		let _intent = self.broadcast_intent_lock.lock().unwrap();
		let pending_intents = self.load_broadcast_intents_from_store()?;
		self.broadcast_intents.lock().unwrap().extend(pending_intents.iter().cloned());
		if pending_intents.is_empty() {
			return Ok(());
		}

		let mut locked_wallet = self.inner.lock().unwrap();
		let confirmed_txids =
			locked_wallet.transaction_confirmations().keys().copied().collect::<HashSet<_>>();
		let confirmed_intent_keys = pending_intents
			.iter()
			.filter(|(_, intent)| {
				intent.transactions.iter().any(|tx| confirmed_txids.contains(&tx.compute_txid()))
			})
			.map(|(key, _)| *key)
			.collect::<HashSet<_>>();
		let unresolved_txs = pending_intents
			.into_iter()
			.filter(|(key, intent)| {
				intent.has_pending_transaction() && !confirmed_intent_keys.contains(key)
			})
			.map(|(_, intent)| intent.active_transaction().clone())
			.collect();
		self.reapply_unresolved_broadcasts(&mut locked_wallet, unresolved_txs)?;
		self.update_payment_store(&locked_wallet)?;
		drop(locked_wallet);
		self.remove_broadcast_intents(confirmed_intent_keys)
	}

	fn reapply_unresolved_broadcasts(
		&self, locked_wallet: &mut AggregateWallet<OnchainWalletAccount, KVStoreWalletPersister>,
		transactions: Vec<Transaction>,
	) -> Result<(), Error> {
		if transactions.is_empty() {
			return Ok(());
		}
		let txids = transactions.iter().map(|tx| tx.compute_txid()).collect::<Vec<_>>();
		let last_seen = Self::next_broadcast_timestamp(locked_wallet, &txids)?;
		let unconfirmed_txs = transactions.into_iter().map(|tx| (tx, last_seen)).collect();
		locked_wallet.apply_mempool_txs(unconfirmed_txs, Vec::new()).map_err(|e| {
			log_error!(self.logger, "Failed to preserve unresolved broadcast intents: {}", e);
			Error::PersistenceFailed
		})
	}

	fn next_broadcast_timestamp(
		locked_wallet: &AggregateWallet<OnchainWalletAccount, KVStoreWalletPersister>,
		txids: &[Txid],
	) -> Result<u64, Error> {
		let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
		let tracked_txids = txids.iter().copied().collect::<HashSet<_>>();
		let latest_seen = locked_wallet
			.unconfirmed_txids_with_last_seen()
			.into_iter()
			.filter(|(txid, _)| tracked_txids.contains(txid))
			.map(|(_, last_seen)| last_seen)
			.max()
			.unwrap_or(0);
		let latest_evicted = locked_wallet
			.wallets()
			.values()
			.flat_map(|wallet| {
				txids.iter().filter_map(|txid| wallet.tx_graph().get_last_evicted(*txid))
			})
			.max()
			.unwrap_or(0);
		now.max(latest_seen).max(latest_evicted).checked_add(1).ok_or(Error::WalletOperationFailed)
	}

	fn write_broadcast_intent(&self, intent: &BroadcastIntent) -> Result<(), Error> {
		let intent_key = intent.key();
		let mut bytes = vec![ONCHAIN_BROADCAST_INTENT_SERIALIZATION_VERSION];
		bytes.extend(serialize(&(intent.first_pending_index, &intent.transactions)));
		KVStoreSync::write(
			&*self.kv_store,
			ONCHAIN_BROADCAST_INTENT_PRIMARY_NAMESPACE,
			ONCHAIN_BROADCAST_INTENT_SECONDARY_NAMESPACE,
			&intent_key.to_string(),
			bytes,
		)
		.map_err(|e| {
			log_error!(self.logger, "Failed to persist broadcast intent {}: {}", intent_key, e);
			Error::PersistenceFailed
		})?;
		self.broadcast_intents.lock().unwrap().insert(intent_key, intent.clone());
		Ok(())
	}

	fn find_broadcast_intent_by_active_txid(&self, txid: &Txid) -> Option<(Txid, BroadcastIntent)> {
		self.broadcast_intents
			.lock()
			.unwrap()
			.iter()
			.find(|(_, intent)| intent.active_txid() == *txid)
			.map(|(key, intent)| (*key, intent.clone()))
	}

	fn read_broadcast_intent_from_store(
		&self, intent_key: &Txid,
	) -> Result<Option<BroadcastIntent>, Error> {
		let bytes = match KVStoreSync::read(
			&*self.kv_store,
			ONCHAIN_BROADCAST_INTENT_PRIMARY_NAMESPACE,
			ONCHAIN_BROADCAST_INTENT_SECONDARY_NAMESPACE,
			&intent_key.to_string(),
		) {
			Ok(bytes) => bytes,
			Err(e) if e.kind() == lightning::io::ErrorKind::NotFound => return Ok(None),
			Err(e) => {
				log_error!(self.logger, "Failed to read broadcast intent {}: {}", intent_key, e);
				return Err(Error::PersistenceFailed);
			},
		};

		let intent = match bytes.first() {
			Some(&LEGACY_ONCHAIN_BROADCAST_INTENT_SERIALIZATION_VERSION) => {
				let tx = deserialize::<Transaction>(&bytes[1..]).map_err(|e| {
					log_error!(
						self.logger,
						"Failed to decode broadcast intent {}: {}",
						intent_key,
						e
					);
					Error::PersistenceFailed
				})?;
				BroadcastIntent::new(tx)
			},
			Some(&LEGACY_RBF_BROADCAST_INTENT_SERIALIZATION_VERSION) => {
				let transactions = deserialize::<Vec<Transaction>>(&bytes[1..]).map_err(|e| {
					log_error!(
						self.logger,
						"Failed to decode broadcast intent {}: {}",
						intent_key,
						e
					);
					Error::PersistenceFailed
				})?;
				if transactions.is_empty() {
					log_error!(self.logger, "Broadcast intent {} has no transactions", intent_key);
					return Err(Error::PersistenceFailed);
				}
				BroadcastIntent { transactions, first_pending_index: 0 }
			},
			Some(&ONCHAIN_BROADCAST_INTENT_SERIALIZATION_VERSION) => {
				let (first_pending_index, transactions) =
					deserialize::<(u32, Vec<Transaction>)>(&bytes[1..]).map_err(|e| {
						log_error!(
							self.logger,
							"Failed to decode broadcast intent {}: {}",
							intent_key,
							e
						);
						Error::PersistenceFailed
					})?;
				if transactions.is_empty() || first_pending_index as usize > transactions.len() {
					log_error!(
						self.logger,
						"Broadcast intent {} has an invalid pending index",
						intent_key
					);
					return Err(Error::PersistenceFailed);
				}
				BroadcastIntent { transactions, first_pending_index }
			},
			_ => {
				log_error!(self.logger, "Unsupported broadcast intent version for {}", intent_key);
				return Err(Error::PersistenceFailed);
			},
		};
		if intent.key() != *intent_key {
			log_error!(
				self.logger,
				"Broadcast intent key does not match transaction {}",
				intent_key
			);
			return Err(Error::PersistenceFailed);
		}
		for pair in intent.transactions.windows(2) {
			let replaces_previous = pair[1].input.iter().any(|replacement_input| {
				pair[0].input.iter().any(|previous_input| {
					previous_input.previous_output == replacement_input.previous_output
				})
			});
			if !replaces_previous {
				log_error!(
					self.logger,
					"Broadcast intent {} has an invalid replacement chain",
					intent_key
				);
				return Err(Error::PersistenceFailed);
			}
		}
		Ok(Some(intent))
	}

	fn read_all_broadcast_intents(&self) -> Result<Vec<(Txid, BroadcastIntent)>, Error> {
		let mut intents = self
			.broadcast_intents
			.lock()
			.unwrap()
			.iter()
			.map(|(key, intent)| (*key, intent.clone()))
			.collect::<Vec<_>>();
		intents.sort_unstable_by_key(|(key, _)| *key);
		Ok(intents)
	}

	fn load_broadcast_intents_from_store(&self) -> Result<Vec<(Txid, BroadcastIntent)>, Error> {
		let mut keys = KVStoreSync::list(
			&*self.kv_store,
			ONCHAIN_BROADCAST_INTENT_PRIMARY_NAMESPACE,
			ONCHAIN_BROADCAST_INTENT_SECONDARY_NAMESPACE,
		)
		.map_err(|e| {
			log_error!(self.logger, "Failed to list broadcast intents: {}", e);
			Error::PersistenceFailed
		})?;
		keys.sort_unstable();
		keys.into_iter()
			.map(|key| {
				let txid = Txid::from_str(&key).map_err(|e| {
					log_error!(self.logger, "Invalid broadcast intent key {}: {}", key, e);
					Error::PersistenceFailed
				})?;
				let intent = self
					.read_broadcast_intent_from_store(&txid)?
					.ok_or(Error::PersistenceFailed)?;
				Ok((txid, intent))
			})
			.collect()
	}

	fn remove_broadcast_intent(&self, txid: &Txid) -> Result<(), Error> {
		KVStoreSync::remove(
			&*self.kv_store,
			ONCHAIN_BROADCAST_INTENT_PRIMARY_NAMESPACE,
			ONCHAIN_BROADCAST_INTENT_SECONDARY_NAMESPACE,
			&txid.to_string(),
			false,
		)
		.map_err(|e| {
			log_error!(self.logger, "Failed to remove broadcast intent {}: {}", txid, e);
			Error::PersistenceFailed
		})?;
		self.broadcast_intents.lock().unwrap().remove(txid);
		Ok(())
	}

	fn remove_broadcast_intents(&self, txids: impl IntoIterator<Item = Txid>) -> Result<(), Error> {
		for txid in txids {
			self.remove_broadcast_intent(&txid)?;
		}
		Ok(())
	}

	fn resolve_broadcast_intents(
		&self, resolutions: impl IntoIterator<Item = (Txid, Txid, bool)>,
	) -> Result<(), Error> {
		for (intent_key, observed_txid, confirmed) in resolutions {
			let intent = self
				.broadcast_intents
				.lock()
				.unwrap()
				.get(&intent_key)
				.cloned()
				.ok_or(Error::TransactionNotFound)?;
			self.resolve_broadcast_intent(intent_key, intent, observed_txid, confirmed)?;
		}
		Ok(())
	}

	fn resolve_broadcast_intent(
		&self, intent_key: Txid, mut intent: BroadcastIntent, observed_txid: Txid, confirmed: bool,
	) -> Result<(), Error> {
		let observed_index = intent
			.transactions
			.iter()
			.position(|tx| tx.compute_txid() == observed_txid)
			.ok_or(Error::TransactionNotFound)?;
		for tx in &intent.transactions {
			let txid = tx.compute_txid();
			if txid != observed_txid {
				self.payment_store.remove(&PaymentId(txid.to_byte_array()))?;
			}
		}

		if confirmed {
			return self.remove_broadcast_intent(&intent_key);
		}

		intent.transactions.truncate(observed_index + 1);
		intent.mark_resolved()?;
		if intent.transactions.len() == 1 {
			self.remove_broadcast_intent(&intent_key)
		} else {
			self.write_broadcast_intent(&intent)
		}
	}

	/// Builds, persists, and reserves an RBF replacement before backend dispatch.
	///
	/// If `txid` has unresolved or accepted replacement lineage, the new replacement is appended to
	/// the same persisted intent record. One store write then atomically makes the replacement the
	/// only transaction exposed for retry while retaining its predecessors until confirmation.
	pub(crate) fn prepare_rbf_broadcast(
		&self, txid: &Txid, fee_rate: FeeRate, channel_manager: &ChannelManager,
	) -> Result<Transaction, Error> {
		// Check if this is a funding transaction
		if self.is_funding_transaction(txid, channel_manager) {
			log_error!(
				self.logger,
				"Cannot RBF transaction {}: it is a channel funding transaction",
				txid
			);
			return Err(Error::CannotRbfFundingTransaction);
		}

		let _intent = self.broadcast_intent_lock.lock().unwrap();
		let existing_intent = self.find_broadcast_intent_by_active_txid(txid);
		let mut locked_wallet = self.inner.lock().unwrap();

		let (tx, original_fee) = locked_wallet.build_rbf(*txid, fee_rate).map_err(|e| match e {
			bdk_wallet_aggregate::Error::TransactionNotFound => {
				log_error!(self.logger, "Transaction not found in any wallet: {}", txid);
				Error::TransactionNotFound
			},
			bdk_wallet_aggregate::Error::TransactionAlreadyConfirmed => {
				log_error!(self.logger, "Cannot replace confirmed transaction: {}", txid);
				Error::TransactionAlreadyConfirmed
			},
			bdk_wallet_aggregate::Error::InvalidFeeRate => {
				log_error!(self.logger, "RBF rejected: new fee rate is not higher");
				Error::InvalidFeeRate
			},
			bdk_wallet_aggregate::Error::InsufficientFunds => {
				log_error!(self.logger, "Insufficient funds for RBF fee bump of {}", txid);
				Error::InsufficientFunds
			},
			bdk_wallet_aggregate::Error::UtxoNotFoundLocally(outpoint) => {
				log_error!(
					self.logger,
					"Cannot calculate fee for RBF of {}: input UTXO {} not found locally. \
					 Try syncing the wallet first.",
					txid,
					outpoint,
				);
				Error::OnchainTxCreationFailed
			},
			other => {
				log_error!(self.logger, "Failed to build RBF for {}: {}", txid, other);
				Error::OnchainTxCreationFailed
			},
		})?;

		let new_txid = tx.compute_txid();
		let original_tx = locked_wallet.find_tx(*txid).ok_or(Error::TransactionNotFound)?;
		let replacement_intent = BroadcastIntent::replacement(
			existing_intent.map(|(_, intent)| intent),
			original_tx,
			tx.clone(),
		)?;
		self.write_broadcast_intent(&replacement_intent)?;

		let lineage_txids =
			replacement_intent.transactions.iter().map(|tx| tx.compute_txid()).collect::<Vec<_>>();
		let last_seen = Self::next_broadcast_timestamp(&locked_wallet, &lineage_txids)?;
		self.payment_store_update_pending.store(true, Ordering::Release);
		if let Err(e) = locked_wallet.apply_mempool_txs(vec![(tx.clone(), last_seen)], Vec::new()) {
			log_error!(self.logger, "Failed to reserve RBF replacement {}: {}", new_txid, e);
			return Err(Error::OnchainTxBroadcastFailed { txid: new_txid });
		}
		if let Err(e) = self.update_payment_store(&locked_wallet) {
			log_error!(self.logger, "Failed to index RBF replacement {}: {}", new_txid, e);
			return Err(Error::OnchainTxBroadcastFailed { txid: new_txid });
		}

		// Calculate and log the actual fee increase achieved
		let new_fee = locked_wallet.calculate_tx_fee(&tx).unwrap_or(Amount::ZERO);
		let actual_fee_rate = new_fee / tx.weight();

		log_info!(self.logger, "RBF transaction created successfully!");
		log_info!(self.logger, "  Original: {} ({} sats fee)", txid, original_fee.to_sat());
		log_info!(
			self.logger,
			"  Replacement: {} ({} sat/vB, {} sats fee)",
			new_txid,
			actual_fee_rate.to_sat_per_vb_ceil(),
			new_fee.to_sat()
		);
		log_info!(
			self.logger,
			"  Additional fee paid: {} sats",
			new_fee.to_sat().saturating_sub(original_fee.to_sat())
		);

		Ok(tx)
	}

	/// Roll back a conclusively rejected or undispatched RBF replacement.
	pub(crate) fn reject_rbf_broadcast(&self, replacement_txid: &Txid) -> Result<(), Error> {
		let _intent = self.broadcast_intent_lock.lock().unwrap();
		let (intent_key, mut intent) = self
			.find_broadcast_intent_by_active_txid(replacement_txid)
			.ok_or(Error::TransactionNotFound)?;
		let rejected_tx = intent.transactions.pop().ok_or(Error::TransactionNotFound)?;
		let predecessor_tx = intent.transactions.last().cloned();
		let restored_pending = intent.has_pending_transaction();
		let retain_lineage = restored_pending || intent.transactions.len() > 1;

		if retain_lineage {
			self.write_broadcast_intent(&intent)?;
		}

		let mut locked_wallet = self.inner.lock().unwrap();
		let mut lineage_txids =
			intent.transactions.iter().map(|tx| tx.compute_txid()).collect::<Vec<_>>();
		lineage_txids.push(*replacement_txid);
		let rejected_at = Self::next_broadcast_timestamp(&locked_wallet, &lineage_txids)?;
		locked_wallet.abandon_txs(std::slice::from_ref(&rejected_tx), rejected_at).map_err(
			|e| {
				log_error!(
					self.logger,
					"Failed to reject RBF replacement {}: {}",
					replacement_txid,
					e
				);
				Error::PersistenceFailed
			},
		)?;
		if let Some(predecessor_tx) = predecessor_tx {
			let restored_txid = predecessor_tx.compute_txid();
			let restored_at = Self::next_broadcast_timestamp(&locked_wallet, &[restored_txid])?;
			locked_wallet
				.apply_mempool_txs(vec![(predecessor_tx, restored_at)], Vec::new())
				.map_err(|e| {
					log_error!(
						self.logger,
						"Failed to restore superseded transaction {}: {}",
						restored_txid,
						e
					);
					Error::PersistenceFailed
				})?;
			self.update_payment_store(&locked_wallet)?;
		}
		drop(locked_wallet);
		self.payment_store.remove(&PaymentId(replacement_txid.to_byte_array()))?;
		if !retain_lineage {
			self.remove_broadcast_intent(&intent_key)?;
		}
		Ok(())
	}

	// Accelerates confirmation of a transaction using Child-Pays-For-Parent (CPFP).
	// Returns the txid of the child transaction if successful.
	pub(crate) fn accelerate_by_cpfp(
		&self, txid: &Txid, fee_rate: FeeRate, destination_address: Option<Address>,
	) -> Result<Txid, Error> {
		let destination_script = match destination_address {
			Some(ref addr) => {
				self.parse_and_validate_address(addr)?;
				Some(addr.script_pubkey())
			},
			None => None,
		};

		let mut locked_wallet = self.inner.lock().unwrap();

		let (tx, parent_fee, parent_fee_rate) =
			locked_wallet.build_cpfp(*txid, fee_rate, destination_script).map_err(|e| {
				log_error!(self.logger, "Failed to build CPFP for {}: {}", txid, e);
				match e {
					bdk_wallet_aggregate::Error::TransactionNotFound => Error::TransactionNotFound,
					bdk_wallet_aggregate::Error::TransactionAlreadyConfirmed => {
						Error::TransactionAlreadyConfirmed
					},
					bdk_wallet_aggregate::Error::NoSpendableOutputs => Error::NoSpendableOutputs,
					_ => Error::OnchainTxCreationFailed,
				}
			})?;

		// Persist wallet changes
		locked_wallet.persist_all().map_err(|e| {
			log_error!(self.logger, "Failed to persist wallet: {}", e);
			Error::PersistenceFailed
		})?;

		// Extract and broadcast the transaction
		self.broadcaster.broadcast_transactions(&[&tx]);

		let child_txid = tx.compute_txid();

		// Calculate and log the actual results
		let child_fee = locked_wallet.calculate_tx_fee(&tx).unwrap_or(Amount::ZERO);
		let actual_child_fee_rate = child_fee / tx.weight();

		log_info!(self.logger, "CPFP transaction created successfully!");
		log_info!(
			self.logger,
			"  Parent: {} ({} sat/vB, {} sats fee)",
			txid,
			parent_fee_rate.to_sat_per_vb_ceil(),
			parent_fee.to_sat()
		);
		log_info!(
			self.logger,
			"  Child: {} ({} sat/vB, {} sats fee)",
			child_txid,
			actual_child_fee_rate.to_sat_per_vb_ceil(),
			child_fee.to_sat()
		);

		Ok(child_txid)
	}

	// Calculates an appropriate fee rate for a CPFP transaction.
	pub(crate) fn calculate_cpfp_fee_rate(
		&self, parent_txid: &Txid, urgent: bool,
	) -> Result<FeeRate, Error> {
		let target = if urgent {
			ConfirmationTarget::Lightning(
				lightning::chain::chaininterface::ConfirmationTarget::MaximumFeeEstimate,
			)
		} else {
			ConfirmationTarget::OnchainPayment
		};
		let target_fee_rate = self.fee_estimator.estimate_fee_rate(target);

		let locked_wallet = self.inner.lock().unwrap();
		locked_wallet.calculate_cpfp_fee_rate(*parent_txid, target_fee_rate).map_err(|e| {
			log_error!(self.logger, "Failed to calculate CPFP fee rate for {}: {}", parent_txid, e);
			match e {
				bdk_wallet_aggregate::Error::TransactionNotFound => Error::TransactionNotFound,
				bdk_wallet_aggregate::Error::TransactionAlreadyConfirmed => {
					Error::TransactionAlreadyConfirmed
				},
				_ => Error::WalletOperationFailed,
			}
		})
	}

	pub(crate) fn update_payment_store_for_all_transactions(&self) -> Result<(), Error> {
		let _intent = self.broadcast_intent_lock.lock().unwrap();
		let locked_wallet = self.inner.lock().unwrap();
		self.update_payment_store(&locked_wallet)?;
		drop(locked_wallet);
		Ok(())
	}

	fn update_payment_store(
		&self, locked_wallet: &AggregateWallet<OnchainWalletAccount, KVStoreWalletPersister>,
	) -> Result<(), Error> {
		self.payment_store_update_pending.store(true, Ordering::Release);
		let result = self.update_payment_store_inner(locked_wallet);
		if result.is_ok() {
			self.payment_store_update_pending.store(false, Ordering::Release);
		}
		result
	}

	fn update_payment_store_inner(
		&self, locked_wallet: &AggregateWallet<OnchainWalletAccount, KVStoreWalletPersister>,
	) -> Result<(), Error> {
		let mut seen_txids = std::collections::HashSet::new();
		let cur_height = locked_wallet.current_best_block().1;
		let transaction_confirmations = locked_wallet.transaction_confirmations();

		for wallet in locked_wallet.wallets().values() {
			for wtx in wallet.transactions() {
				let txid = wtx.tx_node.txid;
				if !seen_txids.insert(txid) {
					continue;
				}

				let id = PaymentId(txid.to_byte_array());
				let (payment_status, confirmation_status) =
					if let Some(anchor) = transaction_confirmations.get(&txid).copied() {
						let confirmation_height = anchor.block_id.height;
						let payment_status =
							if cur_height >= confirmation_height + ANTI_REORG_DELAY - 1 {
								PaymentStatus::Succeeded
							} else {
								PaymentStatus::Pending
							};
						let confirmation_status = ConfirmationStatus::Confirmed {
							block_hash: anchor.block_id.hash,
							height: confirmation_height,
							timestamp: anchor.confirmation_time,
						};
						(payment_status, confirmation_status)
					} else {
						(PaymentStatus::Pending, ConfirmationStatus::Unconfirmed)
					};
				// TODO: It would be great to introduce additional variants for
				// `ChannelFunding` and `ChannelClosing`. For the former, we could just
				// take a reference to `ChannelManager` here and check against
				// `list_channels`. But for the latter the best approach is much less
				// clear: for force-closes/HTLC spends we should be good querying
				// `OutputSweeper::tracked_spendable_outputs`, but regular channel closes
				// (i.e., `SpendableOutputDescriptor::StaticOutput` variants) are directly
				// spent to a wallet address. The only solution I can come up with is to
				// create and persist a list of 'static pending outputs' that we could use
				// here to determine the `PaymentKind`, but that's not really satisfactory, so
				// we're punting on it until we can come up with a better solution.
				let kind =
					crate::payment::PaymentKind::Onchain { txid, status: confirmation_status };

				let fee = locked_wallet.calculate_tx_fee(&wtx.tx_node.tx).unwrap_or(Amount::ZERO);
				let (sent_sat, received_sat) =
					locked_wallet.sent_and_received(txid).unwrap_or((0, 0));
				let fee_sat = fee.to_sat();

				let (direction, amount_msat) = if sent_sat > received_sat {
					let direction = PaymentDirection::Outbound;
					let amount_msat =
						Some(sent_sat.saturating_sub(fee_sat).saturating_sub(received_sat) * 1000);
					(direction, amount_msat)
				} else {
					let direction = PaymentDirection::Inbound;
					let amount_msat =
						Some(received_sat.saturating_sub(sent_sat.saturating_sub(fee_sat)) * 1000);
					(direction, amount_msat)
				};

				let fee_paid_msat = Some(fee_sat * 1000);

				let payment = PaymentDetails::new(
					id,
					kind,
					amount_msat,
					fee_paid_msat,
					direction,
					payment_status,
				);

				self.payment_store.insert_or_update(payment)?;
			}
		}

		// Only the canonical member of an unresolved replacement chain is user-visible. Keeping the
		// other raw transactions in the intent is necessary for backend reconciliation, but keeping
		// their payment records would expose one logical payment more than once.
		self.remove_noncanonical_broadcast_payments(&seen_txids)?;

		Ok(())
	}

	fn remove_noncanonical_broadcast_payments(
		&self, canonical_txids: &HashSet<Txid>,
	) -> Result<(), Error> {
		for (_, intent) in self.read_all_broadcast_intents()? {
			for tx in intent.transactions {
				let txid = tx.compute_txid();
				if !canonical_txids.contains(&txid) {
					self.payment_store.remove(&PaymentId(txid.to_byte_array()))?;
				}
			}
		}
		Ok(())
	}

	#[allow(deprecated)]
	pub(crate) fn create_funding_transaction(
		&self, output_script: ScriptBuf, amount: Amount, confirmation_target: ConfirmationTarget,
		locktime: LockTime,
	) -> Result<Transaction, Error> {
		let fee_rate = self.fee_estimator.estimate_fee_rate(confirmation_target);
		let mut locked_wallet = self.inner.lock().unwrap();
		let primary_account = locked_wallet.primary_key();

		let tx = if primary_account.address_type == AddressType::Legacy {
			log_info!(
				self.logger,
				"Primary is Legacy, using best account-0 SegWit wallet for channel funding"
			);
			locked_wallet.build_and_sign_tx_with_best_wallet(
				output_script,
				amount,
				fee_rate,
				locktime,
				|k| k.account_index == 0 && k.address_type != AddressType::Legacy,
			)
		} else {
			locked_wallet.build_and_sign_funding_tx(output_script, amount, fee_rate, locktime)
		};

		tx.map_err(|e| {
			log_error!(self.logger, "Failed to create funding transaction: {}", e);
			match e {
				bdk_wallet_aggregate::Error::InsufficientFunds => Error::InsufficientFunds,
				bdk_wallet_aggregate::Error::OnchainTxSigningFailed => {
					Error::OnchainTxSigningFailed
				},
				bdk_wallet_aggregate::Error::PersistenceFailed => Error::PersistenceFailed,
				_ => Error::OnchainTxCreationFailed,
			}
		})
	}

	pub(crate) fn get_new_address(&self) -> Result<bitcoin::Address, Error> {
		self.get_new_address_info().map(|address_info| address_info.address)
	}

	pub(crate) fn get_new_address_info(&self) -> Result<AddressInfo, Error> {
		let mut locked_wallet = self.inner.lock().unwrap();
		locked_wallet.new_address_info().map_err(|e| {
			log_error!(self.logger, "Failed to get new address: {}", e);
			Error::WalletOperationFailed
		})
	}

	pub(crate) fn get_new_address_for_type(
		&self, address_type: AddressType,
	) -> Result<bitcoin::Address, Error> {
		self.get_new_address_info_for_type(address_type).map(|address_info| address_info.address)
	}

	pub(crate) fn get_new_address_info_for_type(
		&self, address_type: AddressType,
	) -> Result<AddressInfo, Error> {
		self.get_new_address_info_for_account(OnchainWalletAccount::account_zero(address_type))
	}

	pub(crate) fn get_new_address_info_for_account(
		&self, wallet_account: OnchainWalletAccount,
	) -> Result<AddressInfo, Error> {
		let mut locked_wallet = self.inner.lock().unwrap();
		locked_wallet.new_address_info_for(&wallet_account).map_err(|e| {
			log_error!(
				self.logger,
				"Failed to get new address for account {:?}: {}",
				wallet_account,
				e
			);
			map_wallet_account_error(wallet_account, e)
		})
	}

	pub(crate) fn get_new_address_for_account(
		&self, wallet_account: OnchainWalletAccount,
	) -> Result<bitcoin::Address, Error> {
		self.get_new_address_info_for_account(wallet_account)
			.map(|address_info| address_info.address)
	}

	pub(crate) fn get_address_info_for_type_at_index(
		&self, address_type: AddressType, keychain: KeychainKind, index: u32,
	) -> Result<AddressInfo, Error> {
		self.get_address_info_for_account_at_index(
			OnchainWalletAccount::account_zero(address_type),
			keychain,
			index,
		)
	}

	pub(crate) fn get_address_info_for_account_at_index(
		&self, wallet_account: OnchainWalletAccount, keychain: KeychainKind, index: u32,
	) -> Result<AddressInfo, Error> {
		validate_derivation_index(index)?;

		let locked_wallet = self.inner.lock().unwrap();
		locked_wallet.address_info_for(&wallet_account, keychain.into(), index).map_err(|e| {
			log_error!(
				self.logger,
				"Failed to get address info for account {:?} keychain {:?} at index {}: {}",
				wallet_account,
				keychain,
				index,
				e
			);
			map_wallet_account_error(wallet_account, e)
		})
	}

	pub(crate) fn get_address_infos_for_type(
		&self, address_type: AddressType, keychain: KeychainKind, start_index: u32, count: u32,
	) -> Result<Vec<AddressInfo>, Error> {
		self.get_address_infos_for_account(
			OnchainWalletAccount::account_zero(address_type),
			keychain,
			start_index,
			count,
		)
	}

	pub(crate) fn get_address_infos_for_account(
		&self, wallet_account: OnchainWalletAccount, keychain: KeychainKind, start_index: u32,
		count: u32,
	) -> Result<Vec<AddressInfo>, Error> {
		validate_derivation_range(start_index, count)?;

		let locked_wallet = self.inner.lock().unwrap();
		locked_wallet
			.address_infos_for(&wallet_account, keychain.into(), start_index, count)
			.map_err(|e| {
				log_error!(
					self.logger,
					"Failed to get address infos for account {:?} keychain {:?} at indexes {}..{}: {}",
					wallet_account,
					keychain,
					start_index,
					start_index.saturating_add(count),
					e
				);
				map_wallet_account_error(wallet_account, e)
			})
	}

	pub(crate) fn reveal_receive_addresses_to(
		&self, address_type: AddressType, index: u32,
	) -> Result<(), Error> {
		self.reveal_receive_addresses_to_account(
			OnchainWalletAccount::account_zero(address_type),
			index,
		)
	}

	pub(crate) fn reveal_receive_addresses_to_account(
		&self, wallet_account: OnchainWalletAccount, index: u32,
	) -> Result<(), Error> {
		validate_derivation_index(index)?;
		let mut locked_wallet = self.inner.lock().unwrap();
		locked_wallet.reveal_addresses_to(&wallet_account, index).map_err(|e| {
			log_error!(
				self.logger,
				"Failed to reveal receive addresses through {} for account {:?}: {}",
				index,
				wallet_account,
				e
			);
			map_wallet_account_error(wallet_account, e)
		})
	}

	// Returns a native witness address for Lightning channel scripts.
	// Falls back to a loaded NativeSegwit/Taproot wallet if the primary is not one.
	pub(crate) fn get_new_witness_address(&self) -> Result<bitcoin::Address, Error> {
		let locked_wallet = self.inner.lock().unwrap();
		let primary = locked_wallet.primary_key();

		if primary.account_index == 0 && primary.address_type.is_native_witness() {
			drop(locked_wallet);
			return self.get_new_address();
		}

		let witness_key = locked_wallet
			.loaded_keys()
			.into_iter()
			.find(|k| k.account_index == 0 && k.address_type.is_native_witness());
		drop(locked_wallet);

		match witness_key {
			Some(key) => self.get_new_address_for_type(key.address_type),
			None => {
				log_error!(self.logger, "No native witness wallet loaded for Lightning operations");
				Err(Error::WalletOperationFailed)
			},
		}
	}

	pub(crate) fn get_new_internal_address(&self) -> Result<bitcoin::Address, Error> {
		let mut locked_wallet = self.inner.lock().unwrap();
		locked_wallet.new_internal_address().map_err(|e| {
			log_error!(self.logger, "Failed to get new internal address: {}", e);
			Error::WalletOperationFailed
		})
	}

	pub(crate) fn cancel_tx(&self, tx: &Transaction) -> Result<(), Error> {
		let mut locked_wallet = self.inner.lock().unwrap();
		locked_wallet.cancel_tx(tx).map_err(|e| {
			log_error!(self.logger, "Failed to cancel transaction: {}", e);
			Error::PersistenceFailed
		})
	}

	pub(crate) fn get_balances(
		&self, total_anchor_channels_reserve_sats: u64,
	) -> Result<(u64, u64), Error> {
		let balance = self.inner.lock().unwrap().balance();

		// Make sure `list_confirmed_utxos` returns at least one `Utxo` we could use to spend/bump
		// Anchors if we have any confirmed amounts.
		#[cfg(debug_assertions)]
		if balance.confirmed != Amount::ZERO {
			debug_assert!(
				self.list_confirmed_utxos_inner().map_or(false, |v| !v.is_empty()),
				"Confirmed amounts should always be available for Anchor spending"
			);
		}

		self.get_balances_inner(&balance, total_anchor_channels_reserve_sats)
	}

	pub(crate) fn get_balance_for_address_type(
		&self, address_type: AddressType,
	) -> Result<(u64, u64), Error> {
		self.get_balance_for_onchain_wallet_account(OnchainWalletAccount::account_zero(
			address_type,
		))
	}

	pub(crate) fn get_balance_for_onchain_wallet_account(
		&self, wallet_account: OnchainWalletAccount,
	) -> Result<(u64, u64), Error> {
		let locked_wallet = self.inner.lock().unwrap();
		let balance = locked_wallet.balance_for(&wallet_account).map_err(|e| {
			log_error!(self.logger, "Failed to get balance for {:?}: {}", wallet_account, e);
			map_wallet_account_error(wallet_account, e)
		})?;

		self.get_balances_inner(&balance, 0)
	}

	fn get_balances_inner(
		&self, balance: &Balance, total_anchor_channels_reserve_sats: u64,
	) -> Result<(u64, u64), Error> {
		let spendable_base = if self.config.include_untrusted_pending_in_spendable {
			balance.trusted_spendable().to_sat() + balance.untrusted_pending.to_sat()
		} else {
			balance.trusted_spendable().to_sat()
		};

		let (total, spendable) = (
			balance.total().to_sat(),
			spendable_base.saturating_sub(total_anchor_channels_reserve_sats),
		);

		Ok((total, spendable))
	}

	pub(crate) fn get_spendable_amount_sats(
		&self, total_anchor_channels_reserve_sats: u64,
	) -> Result<u64, Error> {
		self.get_balances(total_anchor_channels_reserve_sats).map(|(_, s)| s)
	}

	pub(crate) fn get_witness_spendable_amount_sats(
		&self, total_anchor_channels_reserve_sats: u64,
	) -> Result<u64, Error> {
		let locked_wallet = self.inner.lock().unwrap();
		// Legacy-primary funding needs an account-0 non-Legacy wallet as the builder/change
		// wallet. Derived SegWit UTXOs can still be selected as foreign inputs once that builder
		// exists, so count all non-Legacy funds only when such a builder is loaded.
		let has_account_zero_segwit_builder = locked_wallet
			.loaded_keys()
			.iter()
			.any(|k| k.account_index == 0 && k.address_type != AddressType::Legacy);
		if !has_account_zero_segwit_builder {
			return Ok(0);
		}
		let balance = locked_wallet.balance_filtered(|k| k.address_type != AddressType::Legacy);
		self.get_balances_inner(&balance, total_anchor_channels_reserve_sats).map(|(_, s)| s)
	}

	// Get transaction details including inputs, outputs, and net amount.
	// Returns None if the transaction is not found in any wallet.
	pub(crate) fn get_tx_details(&self, txid: &Txid) -> Option<(i64, Vec<TxInput>, Vec<TxOutput>)> {
		let locked_wallet = self.inner.lock().unwrap();
		let (sent_sat, received_sat) = locked_wallet.sent_and_received(*txid)?;
		let tx = locked_wallet.find_tx(*txid)?;
		let net_amount = received_sat as i64 - sent_sat as i64;

		let inputs: Vec<TxInput> = tx.input.iter().map(TxInput::from_tx_input).collect();

		let outputs: Vec<TxOutput> = tx
			.output
			.iter()
			.enumerate()
			.map(|(index, tx_output)| {
				TxOutput::from_tx_output(tx_output, index as u32, self.config.network)
			})
			.collect();

		Some((net_amount, inputs, outputs))
	}

	pub(crate) fn parse_and_validate_address(&self, address: &Address) -> Result<Address, Error> {
		Address::<NetworkUnchecked>::from_str(address.to_string().as_str())
			.map_err(|_| Error::InvalidAddress)?
			.require_network(self.config.network)
			.map_err(|_| Error::InvalidAddress)
	}

	// Returns all UTXOs that are safe to spend (excluding channel funding transactions).
	pub fn get_spendable_utxos(
		&self, channel_manager: &ChannelManager,
	) -> Result<Vec<LocalOutput>, Error> {
		let locked_wallet = self.inner.lock().unwrap();

		let all_utxos: Vec<LocalOutput> = locked_wallet.list_unspent();
		let total_count = all_utxos.len();

		// Filter out channel funding transactions
		let spendable_utxos: Vec<LocalOutput> = all_utxos
			.into_iter()
			.filter(|utxo| {
				// Check if this UTXO's transaction is a channel funding transaction
				if self.is_funding_transaction(&utxo.outpoint.txid, channel_manager) {
					log_debug!(
						self.logger,
						"Filtering out UTXO {:?} as it's part of a channel funding transaction",
						utxo.outpoint
					);
					false
				} else {
					true
				}
			})
			.collect();

		log_debug!(
			self.logger,
			"Found {} spendable UTXOs out of {} total UTXOs",
			spendable_utxos.len(),
			total_count
		);

		Ok(spendable_utxos)
	}

	// Select UTXOs using a specific coin selection algorithm.
	// Returns selected UTXOs that meet the target amount plus fees, excluding channel funding txs.
	pub fn select_utxos_with_algorithm(
		&self, target_amount: u64, available_utxos: Vec<LocalOutput>, fee_rate: FeeRate,
		algorithm: CoinSelectionAlgorithm, drain_script: &Script, channel_manager: &ChannelManager,
	) -> Result<Vec<OutPoint>, Error> {
		let excluded_outpoints: Vec<OutPoint> = available_utxos
			.iter()
			.filter(|utxo| self.is_funding_transaction(&utxo.outpoint.txid, channel_manager))
			.map(|utxo| utxo.outpoint)
			.collect();

		let locked_wallet = self.inner.lock().unwrap();
		let algo = match algorithm {
			CoinSelectionAlgorithm::BranchAndBound => {
				bdk_wallet_aggregate::CoinSelectionAlgorithm::BranchAndBound
			},
			CoinSelectionAlgorithm::LargestFirst => {
				bdk_wallet_aggregate::CoinSelectionAlgorithm::LargestFirst
			},
			CoinSelectionAlgorithm::OldestFirst => {
				bdk_wallet_aggregate::CoinSelectionAlgorithm::OldestFirst
			},
			CoinSelectionAlgorithm::SingleRandomDraw => {
				bdk_wallet_aggregate::CoinSelectionAlgorithm::SingleRandomDraw
			},
		};

		locked_wallet
			.select_utxos(
				target_amount,
				available_utxos,
				fee_rate,
				algo,
				drain_script,
				&excluded_outpoints,
			)
			.map_err(|e| {
				log_error!(self.logger, "Coin selection failed: {}", e);
				Error::CoinSelectionFailed
			})
	}

	// Helper that builds a transaction PSBT with shared logic for send_to_address
	// and calculate_transaction_fee.
	// Supports cross-wallet spending: unified coin selection pools UTXOs from all
	// loaded wallets and selects optimally across the full set.
	fn build_transaction_psbt(
		&self, address: &Address, send_amount: OnchainSendAmount, fee_rate: FeeRate,
		utxos_to_spend: Option<Vec<OutPoint>>, channel_manager: &ChannelManager,
	) -> Result<
		(Psbt, MutexGuard<'_, AggregateWallet<OnchainWalletAccount, KVStoreWalletPersister>>),
		Error,
	> {
		let mut locked_wallet = self.inner.lock().unwrap();

		let all_utxos = locked_wallet.list_unspent();

		// Validate and check UTXOs if provided
		if let Some(ref outpoints) = utxos_to_spend {
			let all_utxo_set: std::collections::HashSet<_> =
				all_utxos.iter().map(|u| u.outpoint).collect();

			for outpoint in outpoints {
				if !all_utxo_set.contains(outpoint) {
					log_error!(self.logger, "UTXO {:?} not found in any wallet", outpoint);
					return Err(Error::WalletOperationFailed);
				}
				if self.is_funding_transaction(&outpoint.txid, channel_manager) {
					log_error!(
						self.logger,
						"UTXO {:?} is part of a channel funding transaction and cannot be spent",
						outpoint
					);
					return Err(Error::WalletOperationFailed);
				}
			}

			// Calculate total value of selected UTXOs
			let selected_value: u64 = all_utxos
				.iter()
				.filter(|u| outpoints.contains(&u.outpoint))
				.map(|u| u.txout.value.to_sat())
				.sum();

			// For exact amounts, ensure we have enough value
			if let OnchainSendAmount::ExactRetainingReserve { amount_sats, .. } = send_amount {
				// Calculate a fee buffer based on fee rate
				// Assume a typical tx with 1 input and 2 outputs (~200 vbytes)
				let typical_tx_weight = Weight::from_vb(200).expect("Valid weight");
				let fee_buffer =
					fee_rate.fee_wu(typical_tx_weight).expect("Valid fee calculation").to_sat();
				// Use at least 1000 sats as minimum buffer
				let min_fee_buffer = fee_buffer.max(1000);
				let min_required = amount_sats.saturating_add(min_fee_buffer);
				if selected_value < min_required {
					log_error!(
						self.logger,
						"Selected UTXOs have insufficient value. Have: {}sats, Need at least: {}sats",
						selected_value,
						min_required
					);
					return Err(Error::InsufficientFunds);
				}
			}

			log_debug!(
				self.logger,
				"Using {} manually selected UTXOs with total value: {}sats",
				outpoints.len(),
				selected_value
			);
		}

		let funding_txids: std::collections::HashSet<Txid> = all_utxos
			.iter()
			.filter(|u| self.is_funding_transaction(&u.outpoint.txid, channel_manager))
			.map(|u| u.outpoint.txid)
			.collect();

		let non_primary_utxo_infos: Option<Vec<bdk_wallet_aggregate::UtxoPsbtInfo>> =
			match (&utxos_to_spend, send_amount) {
				(Some(_), _) => None,
				(None, OnchainSendAmount::AllDrainingReserve)
				| (None, OnchainSendAmount::AllRetainingReserve { .. }) => {
					let infos =
						locked_wallet.non_primary_foreign_utxos(&funding_txids).map_err(|e| {
							log_error!(self.logger, "Failed to prepare non-primary UTXOs: {}", e);
							Error::WalletOperationFailed
						})?;
					(!infos.is_empty()).then_some(infos)
				},
				(None, OnchainSendAmount::ExactRetainingReserve { .. }) => None,
			};

		let manual_utxo_infos: Option<Vec<bdk_wallet_aggregate::UtxoPsbtInfo>> =
			if let Some(ref outpoints) = utxos_to_spend {
				Some(locked_wallet.prepare_outpoints_for_psbt(outpoints).map_err(|e| {
					log_error!(self.logger, "Failed to prepare manually selected UTXOs: {}", e);
					Error::WalletOperationFailed
				})?)
			} else {
				None
			};

		let aggregate_balance = locked_wallet.balance();

		// Prepare the tx_builder. We properly check the reserve requirements (again) further down.
		let mut tx_builder = match send_amount {
			OnchainSendAmount::ExactRetainingReserve { amount_sats, .. } => {
				let primary = locked_wallet.primary_wallet_mut();
				let mut tx_builder = primary.build_tx();
				let amount = Amount::from_sat(amount_sats);
				tx_builder.add_recipient(address.script_pubkey(), amount).fee_rate(fee_rate);
				tx_builder
			},
			OnchainSendAmount::AllRetainingReserve { cur_anchor_reserve_sats }
				if cur_anchor_reserve_sats > DUST_LIMIT_SATS =>
			{
				let change_address_info =
					locked_wallet.primary_wallet().peek_address(BdkKeychainKind::Internal, 0);
				let spendable_amount_sats = self
					.get_balances_inner(&aggregate_balance, cur_anchor_reserve_sats)
					.map(|(_, s)| s)
					.unwrap_or(0);
				let tmp_psbt = {
					let primary = locked_wallet.primary_wallet_mut();
					let mut tmp_tx_builder = primary.build_tx();
					tmp_tx_builder
						.drain_wallet()
						.drain_to(address.script_pubkey())
						.add_recipient(
							change_address_info.address.script_pubkey(),
							Amount::from_sat(cur_anchor_reserve_sats),
						)
						.fee_rate(fee_rate);

					if let Some(ref infos) = manual_utxo_infos {
						bdk_wallet_aggregate::utxo::add_utxos_to_tx_builder(
							&mut tmp_tx_builder,
							infos,
						)
						.map_err(|e| {
							log_error!(self.logger, "Failed to add UTXOs to temp tx: {}", e);
							Error::OnchainTxCreationFailed
						})?;
						tmp_tx_builder.manually_selected_only();
					}

					match tmp_tx_builder.finish() {
						Ok(psbt) => psbt,
						Err(err) => {
							log_error!(
								self.logger,
								"Failed to create temporary transaction: {}",
								err
							);
							return Err(err.into());
						},
					}
				};

				// Cancel the temp tx to free up the change address.
				locked_wallet.cancel_dry_run_tx(&tmp_psbt.unsigned_tx);

				let base_fee = locked_wallet.calculate_fee_from_psbt(&tmp_psbt).map_err(|e| {
					log_error!(
						self.logger,
						"Failed to calculate fee of temporary transaction: {}",
						e
					);
					Error::WalletOperationFailed
				})?;
				let base_fee = Amount::from_sat(base_fee);

				// Adjust the fee estimate for non-primary inputs that will be
				// added to the actual tx (the temp tx only used primary UTXOs).
				let extra_input_weight = non_primary_utxo_infos
					.as_deref()
					.map(additional_input_weight)
					.transpose()?
					.unwrap_or(Weight::ZERO);
				let extra_input_fee =
					fee_rate.fee_wu(extra_input_weight).ok_or(Error::InvalidFeeRate)?;
				let estimated_tx_fee =
					base_fee.checked_add(extra_input_fee).ok_or(Error::InvalidFeeRate)?;

				let estimated_spendable_amount = Amount::from_sat(
					spendable_amount_sats.saturating_sub(estimated_tx_fee.to_sat()),
				);

				if estimated_spendable_amount < Amount::from_sat(DUST_LIMIT_SATS) {
					log_error!(self.logger,
					"Unable to send payment without infringing on Anchor reserves. Available: {}sats, estimated fee required: {}sats.",
					spendable_amount_sats,
					estimated_tx_fee,
				);
					return Err(Error::InsufficientFunds);
				}

				let primary = locked_wallet.primary_wallet_mut();
				let mut tx_builder = primary.build_tx();
				tx_builder
					.add_recipient(address.script_pubkey(), estimated_spendable_amount)
					.fee_absolute(estimated_tx_fee);
				tx_builder
			},
			OnchainSendAmount::AllDrainingReserve
			| OnchainSendAmount::AllRetainingReserve { cur_anchor_reserve_sats: _ } => {
				let primary = locked_wallet.primary_wallet_mut();
				let mut tx_builder = primary.build_tx();
				tx_builder.drain_wallet().drain_to(address.script_pubkey()).fee_rate(fee_rate);
				tx_builder
			},
		};

		if let Some(ref utxo_infos) = manual_utxo_infos {
			bdk_wallet_aggregate::utxo::add_utxos_to_tx_builder(&mut tx_builder, utxo_infos)
				.map_err(|e| {
					log_error!(self.logger, "Failed to add manually selected UTXOs: {}", e);
					Error::OnchainTxCreationFailed
				})?;
			tx_builder.manually_selected_only();
		}

		if let Some(ref infos) = non_primary_utxo_infos {
			bdk_wallet_aggregate::utxo::add_utxos_to_tx_builder(&mut tx_builder, infos).map_err(
				|e| {
					log_error!(self.logger, "Failed to add cross-wallet UTXOs: {}", e);
					Error::OnchainTxCreationFailed
				},
			)?;
		}

		let psbt = match tx_builder.finish() {
			Ok(psbt) => {
				log_trace!(self.logger, "Created PSBT: {:?}", psbt);
				psbt
			},
			Err(err) => {
				let can_retry =
					matches!(send_amount, OnchainSendAmount::ExactRetainingReserve { .. })
						&& manual_utxo_infos.is_none()
						&& non_primary_utxo_infos.is_none();

				if can_retry {
					let amount_sats = match send_amount {
						OnchainSendAmount::ExactRetainingReserve { amount_sats, .. } => amount_sats,
						_ => unreachable!(),
					};
					locked_wallet
						.build_psbt_with_cross_wallet_fallback(
							address.script_pubkey(),
							Amount::from_sat(amount_sats),
							fee_rate,
							&funding_txids,
							bdk_wallet_aggregate::CoinSelectionAlgorithm::BranchAndBound,
						)
						.map_err(|e| {
							log_error!(self.logger, "Failed to create transaction: {}", e);
							match e {
								bdk_wallet_aggregate::Error::InsufficientFunds => {
									Error::InsufficientFunds
								},
								_ => Error::OnchainTxCreationFailed,
							}
						})?
				} else {
					log_error!(self.logger, "Failed to create transaction: {}", err);
					return Err(err.into());
				}
			},
		};

		// Check the reserve requirements (again) and return an error if they aren't met.
		// Cancel the PSBT before each early return to free up the change address.
		match send_amount {
			OnchainSendAmount::ExactRetainingReserve { amount_sats, cur_anchor_reserve_sats } => {
				let spendable_amount_sats = self
					.get_balances_inner(&aggregate_balance, cur_anchor_reserve_sats)
					.map(|(_, s)| s)
					.unwrap_or(0);
				let fee_result = locked_wallet.calculate_fee_with_fallback(&psbt);
				let tx_fee_sats = match fee_result {
					Ok(fee) => fee,
					Err(e) => {
						log_error!(
							self.logger,
							"Failed to calculate fee of candidate transaction: {}",
							e
						);
						locked_wallet.cancel_dry_run_tx(&psbt.unsigned_tx);
						return Err(Error::WalletOperationFailed);
					},
				};
				if spendable_amount_sats < amount_sats.saturating_add(tx_fee_sats) {
					log_error!(self.logger,
						"Unable to send payment due to insufficient funds. Available: {}sats, Required: {}sats + {}sats fee",
						spendable_amount_sats,
						amount_sats,
						tx_fee_sats,
					);
					locked_wallet.cancel_dry_run_tx(&psbt.unsigned_tx);
					return Err(Error::InsufficientFunds);
				}
			},
			OnchainSendAmount::AllRetainingReserve { cur_anchor_reserve_sats } => {
				let spendable_amount_sats = self
					.get_balances_inner(&aggregate_balance, cur_anchor_reserve_sats)
					.map(|(_, s)| s)
					.unwrap_or(0);
				let drain_amount = locked_wallet.drain_amount_from_psbt(&psbt);
				if spendable_amount_sats < drain_amount {
					log_error!(self.logger,
						"Unable to send payment due to insufficient funds. Available: {}sats, Required: {}sats",
						spendable_amount_sats,
						drain_amount,
					);
					locked_wallet.cancel_dry_run_tx(&psbt.unsigned_tx);
					return Err(Error::InsufficientFunds);
				}
			},
			_ => {},
		}

		Ok((psbt, locked_wallet))
	}

	pub(crate) fn calculate_transaction_fee(
		&self, address: &Address, send_amount: OnchainSendAmount, fee_rate: Option<FeeRate>,
		utxos_to_spend: Option<Vec<OutPoint>>, channel_manager: &ChannelManager,
	) -> Result<u64, Error> {
		self.parse_and_validate_address(&address)?;

		// Use the set fee_rate or default to fee estimation.
		let confirmation_target = ConfirmationTarget::OnchainPayment;
		let fee_rate =
			fee_rate.unwrap_or_else(|| self.fee_estimator.estimate_fee_rate(confirmation_target));

		let (psbt, mut locked_wallet) = self.build_transaction_psbt(
			address,
			send_amount,
			fee_rate,
			utxos_to_spend,
			channel_manager,
		)?;

		let fee_result = locked_wallet.calculate_fee_with_fallback(&psbt);

		// Cancel the dry-run PSBT to free up the change address.
		locked_wallet.cancel_dry_run_tx(&psbt.unsigned_tx);

		let calculated_fee = fee_result.map_err(|e| {
			log_error!(self.logger, "Failed to calculate transaction fee: {}", e);
			Error::WalletOperationFailed
		})?;

		log_info!(
			self.logger,
			"Calculated transaction fee: {}sats for sending to address {}",
			calculated_fee,
			address
		);

		Ok(calculated_fee)
	}

	#[allow(deprecated)]
	pub(crate) fn create_send_to_address_transaction(
		&self, address: &Address, send_amount: OnchainSendAmount, fee_rate: Option<FeeRate>,
		utxos_to_spend: Option<Vec<OutPoint>>, channel_manager: &ChannelManager,
	) -> Result<Transaction, Error> {
		self.parse_and_validate_address(&address)?;

		// Use the set fee_rate or default to fee estimation.
		let confirmation_target = ConfirmationTarget::OnchainPayment;
		let fee_rate =
			fee_rate.unwrap_or_else(|| self.fee_estimator.estimate_fee_rate(confirmation_target));

		let is_drain_all = match send_amount {
			OnchainSendAmount::AllDrainingReserve => true,
			OnchainSendAmount::AllRetainingReserve { cur_anchor_reserve_sats } => {
				cur_anchor_reserve_sats <= DUST_LIMIT_SATS
			},
			_ => false,
		};

		let tx = if is_drain_all && utxos_to_spend.is_none() {
			let mut locked_wallet = self.inner.lock().unwrap();
			let tx = locked_wallet
				.build_and_sign_drain(address.script_pubkey(), fee_rate)
				.map_err(|e| {
					log_error!(self.logger, "Failed to drain wallets: {}", e);
					Error::OnchainTxCreationFailed
				})?;
			locked_wallet.persist_all().map_err(|e| {
				log_error!(self.logger, "Failed to persist wallet: {}", e);
				Error::PersistenceFailed
			})?;
			tx
		} else {
			let (psbt, mut locked_wallet) = self.build_transaction_psbt(
				address,
				send_amount,
				fee_rate,
				utxos_to_spend,
				channel_manager,
			)?;

			let tx = locked_wallet.sign_psbt_all(psbt).map_err(|e| {
				log_error!(self.logger, "Failed to sign transaction: {}", e);
				Error::OnchainTxSigningFailed
			})?;

			locked_wallet.persist_all().map_err(|e| {
				log_error!(self.logger, "Failed to persist wallet: {}", e);
				Error::PersistenceFailed
			})?;
			tx
		};

		let txid = tx.compute_txid();

		match send_amount {
			OnchainSendAmount::ExactRetainingReserve { amount_sats, .. } => {
				log_info!(
					self.logger,
					"Created new transaction {} sending {}sats on-chain to address {}",
					txid,
					amount_sats,
					address
				);
			},
			OnchainSendAmount::AllRetainingReserve { cur_anchor_reserve_sats } => {
				log_info!(
                self.logger,
                "Created new transaction {} sending available on-chain funds retaining a reserve of {}sats to address {}",
                txid,
                cur_anchor_reserve_sats,
                address,
            );
			},
			OnchainSendAmount::AllDrainingReserve => {
				log_info!(
					self.logger,
					"Created new transaction {} sending all available on-chain funds to address {}",
					txid,
					address
				);
			},
		}

		Ok(tx)
	}

	pub(crate) fn select_confirmed_utxos(
		&self, must_spend: Vec<Input>, must_pay_to: &[TxOut], fee_rate: FeeRate,
	) -> Result<Vec<FundingTxInput>, ()> {
		let mut locked_wallet = self.inner.lock().unwrap();

		// Splicing requires native witness (P2WPKH/P2TR) primary because
		// FundingTxInput only supports native witness script types.
		if !locked_wallet.primary_key().address_type.is_native_witness() {
			log_error!(
				self.logger,
				"Splicing requires a native witness primary wallet (NativeSegwit or Taproot)"
			);
			return Err(());
		}

		let mut tx_builder = locked_wallet.build_tx();
		tx_builder.only_witness_utxo();

		for input in &must_spend {
			let psbt_input = psbt::Input {
				witness_utxo: Some(input.previous_utxo.clone()),
				..Default::default()
			};
			let weight = Weight::from_wu(input.satisfaction_weight);
			tx_builder.add_foreign_utxo(input.outpoint, psbt_input, weight).map_err(|_| ())?;
		}

		for output in must_pay_to {
			tx_builder.add_recipient(output.script_pubkey.clone(), output.value);
		}

		tx_builder.fee_rate(fee_rate);
		tx_builder.exclude_unconfirmed();

		let psbt = tx_builder.finish().map_err(|e| {
			log_error!(self.logger, "Failed to select confirmed UTXOs: {}", e);
		})?;

		let result = psbt
			.unsigned_tx
			.input
			.iter()
			.filter(|txin| must_spend.iter().all(|input| input.outpoint != txin.previous_output))
			.filter_map(|txin| {
				locked_wallet
					.tx_details(txin.previous_output.txid)
					.map(|tx_details| tx_details.tx.deref().clone())
					.map(|prevtx| FundingTxInput::new_p2wpkh(prevtx, txin.previous_output.vout))
			})
			.collect::<Result<Vec<_>, ()>>();

		// Cancel the dry-run PSBT to free up the change address.
		locked_wallet.cancel_dry_run_tx(&psbt.unsigned_tx);

		result
	}

	fn list_confirmed_utxos_inner(&self) -> Result<Vec<Utxo>, ()> {
		let locked_wallet = self.inner.lock().unwrap();
		let mut utxos = Vec::new();

		for u in locked_wallet.list_confirmed_unspent() {
			let script_pubkey = u.txout.script_pubkey.clone();
			match script_pubkey.witness_version() {
				Some(version @ WitnessVersion::V0) => {
					// According to the SegWit rules of [BIP 141] a witness program is defined as:
					// > A scriptPubKey (or redeemScript as defined in BIP16/P2SH) that consists of
					// > a 1-byte push opcode (one of OP_0,OP_1,OP_2,.. .,OP_16) followed by a direct
					// > data push between 2 and 40 bytes gets a new special meaning. The value of
					// > the first push is called the "version byte". The following byte vector
					// > pushed is called the "witness program"."
					//
					// We therefore skip the first byte we just read via `witness_version` and use
					// the rest (i.e., the data push) as the raw bytes to construct the
					// `WitnessProgram` below.
					//
					// [BIP 141]: https://github.com/bitcoin/bips/blob/master/bip-0141.mediawiki#witness-program
					let witness_bytes = &script_pubkey.as_bytes()[2..];
					let witness_program =
						WitnessProgram::new(version, witness_bytes).map_err(|e| {
							log_error!(self.logger, "Failed to retrieve script payload: {}", e);
						})?;

					let wpkh = WPubkeyHash::from_slice(&witness_program.program().as_bytes())
						.map_err(|e| {
							log_error!(self.logger, "Failed to retrieve script payload: {}", e);
						})?;
					let utxo = Utxo::new_v0_p2wpkh(u.outpoint, u.txout.value, &wpkh);
					utxos.push(utxo);
				},
				Some(version @ WitnessVersion::V1) => {
					// According to the SegWit rules of [BIP 141] a witness program is defined as:
					// > A scriptPubKey (or redeemScript as defined in BIP16/P2SH) that consists of
					// > a 1-byte push opcode (one of OP_0,OP_1,OP_2,.. .,OP_16) followed by a direct
					// > data push between 2 and 40 bytes gets a new special meaning. The value of
					// > the first push is called the "version byte". The following byte vector
					// > pushed is called the "witness program"."
					//
					// We therefore skip the first byte we just read via `witness_version` and use
					// the rest (i.e., the data push) as the raw bytes to construct the
					// `WitnessProgram` below.
					//
					// [BIP 141]: https://github.com/bitcoin/bips/blob/master/bip-0141.mediawiki#witness-program
					let witness_bytes = &script_pubkey.as_bytes()[2..];
					let witness_program =
						WitnessProgram::new(version, witness_bytes).map_err(|e| {
							log_error!(self.logger, "Failed to retrieve script payload: {}", e);
						})?;

					XOnlyPublicKey::from_slice(&witness_program.program().as_bytes()).map_err(
						|e| {
							log_error!(self.logger, "Failed to retrieve script payload: {}", e);
						},
					)?;

					let utxo = Utxo {
						outpoint: u.outpoint,
						output: TxOut {
							value: u.txout.value,
							script_pubkey: ScriptBuf::new_witness_program(&witness_program),
						},
						satisfaction_weight: 1 /* empty script_sig */ * WITNESS_SCALE_FACTOR as u64 +
							1 /* witness items */ + 1 /* schnorr sig len */ + 64, // schnorr sig
					};
					utxos.push(utxo);
				},
				Some(version) => {
					log_error!(self.logger, "Unexpected witness version: {}", version,);
				},
				None => {
					let script_bytes = script_pubkey.as_bytes();
					if script_pubkey.is_p2pkh() {
						let pkh = PubkeyHash::from_slice(&script_bytes[3..23]).map_err(|e| {
							log_error!(self.logger, "Failed to extract PubkeyHash: {}", e);
						})?;
						utxos.push(Utxo::new_p2pkh(u.outpoint, u.txout.value, &pkh));
					} else if script_pubkey.is_p2sh() {
						if let Some(wpkh) = locked_wallet.derive_wpkh_for_p2sh(&u) {
							utxos.push(Utxo::new_nested_p2wpkh(u.outpoint, u.txout.value, &wpkh));
						} else {
							log_debug!(
								self.logger,
								"Skipping P2SH UTXO {:?}: could not derive inner WPubkeyHash",
								u.outpoint
							);
						}
					} else {
						log_debug!(
							self.logger,
							"Skipping non-standard non-witness UTXO {:?}",
							u.outpoint
						);
					}
				},
			}
		}

		Ok(utxos)
	}

	#[allow(deprecated)]
	fn get_change_script_inner(&self) -> Result<ScriptBuf, ()> {
		let mut locked_wallet = self.inner.lock().unwrap();
		locked_wallet.new_internal_address().map(|addr| addr.script_pubkey()).map_err(|e| {
			log_error!(self.logger, "Failed to get change script: {}", e);
		})
	}

	pub(crate) fn sign_owned_inputs(&self, unsigned_tx: Transaction) -> Result<Transaction, ()> {
		let mut locked_wallet = self.inner.lock().unwrap();
		locked_wallet.sign_owned_inputs(unsigned_tx).map_err(|e| {
			log_error!(self.logger, "Failed to sign transaction: {}", e);
		})
	}

	fn sign_psbt_inner(&self, psbt: Psbt) -> Result<Transaction, ()> {
		let mut locked_wallet = self.inner.lock().unwrap();
		locked_wallet.sign_psbt_all(psbt).map_err(|e| {
			log_error!(self.logger, "Failed to sign PSBT: {}", e);
		})
	}
}

impl Listen for Wallet {
	fn filtered_block_connected(
		&self, _header: &bitcoin::block::Header,
		_txdata: &lightning::chain::transaction::TransactionData, _height: u32,
	) {
		debug_assert!(false, "Syncing filtered blocks is currently not supported");
		// As far as we can tell this would be a no-op anyways as we don't have to tell BDK about
		// the header chain of intermediate blocks. According to the BDK team, it's sufficient to
		// only connect full blocks starting from the last point of disagreement.
	}

	fn block_connected(&self, block: &bitcoin::Block, height: u32) {
		let _intent = self.broadcast_intent_lock.lock().unwrap();
		let pending_intents = match self.read_all_broadcast_intents() {
			Ok(intents) => intents,
			Err(e) => {
				log_error!(
					self.logger,
					"Failed to read broadcast intents before block update: {}",
					e
				);
				return;
			},
		};
		let block_txids = block.txdata.iter().map(|tx| tx.compute_txid()).collect::<HashSet<_>>();
		let resolutions = observed_broadcast_intents(&pending_intents, &block_txids, &block_txids);
		let mut locked_wallet = self.inner.lock().unwrap();

		let pre_checkpoint = locked_wallet.latest_checkpoint();
		if height > 0
			&& (pre_checkpoint.height() != height - 1
				|| pre_checkpoint.hash() != block.header.prev_blockhash)
		{
			log_debug!(
				self.logger,
				"Detected reorg while applying a connected block to on-chain wallet: new block with hash {} at height {}",
				block.header.block_hash(),
				height
			);
		}

		match locked_wallet.apply_block(block, height) {
			Ok(_all_txids) => {
				if let Err(e) = self.update_payment_store(&locked_wallet) {
					log_error!(self.logger, "Failed to update payment store: {}", e);
					return;
				}
				drop(locked_wallet);
				if let Err(e) = self.resolve_broadcast_intents(resolutions) {
					log_error!(self.logger, "Failed to reconcile confirmed broadcasts: {}", e);
				}
			},
			Err(e) => {
				log_error!(
					self.logger,
					"Failed to apply connected block to on-chain wallet: {}",
					e
				);
				return;
			},
		};
	}

	fn blocks_disconnected(&self, _fork_point_block: BestBlock) {
		// This is a no-op as we don't have to tell BDK about disconnections. According to the BDK
		// team, it's sufficient in case of a reorg to always connect blocks starting from the last
		// point of disagreement.
	}
}

fn create_wallet_for_account(
	seed_bytes: &[u8; WALLET_KEYS_SEED_LEN], network: Network,
	wallet_account: OnchainWalletAccount, chain_tip: BestBlock, kv_store: Arc<DynStore>,
	logger: Arc<Logger>, lookahead: Option<u32>,
) -> Result<(PersistedWallet<KVStoreWalletPersister>, KVStoreWalletPersister), Error> {
	let xprv = Xpriv::new_master(network, seed_bytes).map_err(|e| {
		log_error!(logger, "Failed to derive master secret: {}", e);
		Error::WalletOperationFailed
	})?;

	let mut persister =
		KVStoreWalletPersister::new(Arc::clone(&kv_store), Arc::clone(&logger), wallet_account);

	let (mut wallet, loaded_from_store) = crate::builder::get_or_create_wallet_for_account(
		wallet_account,
		xprv,
		network,
		&mut persister,
		lookahead,
	)
	.map_err(|e| {
		log_error!(logger, "Failed to setup wallet for {:?}: {}", wallet_account, e);
		Error::WalletOperationFailed
	})?;

	// Only advance brand-new wallets to the current tip. Re-loaded wallets keep their persisted
	// checkpoint so per-account Bitcoind sync can replay every block after the older tip.
	//
	// New derived wallets still start at the current tip: Bitcoind has no account full-scan, so
	// confirmed pre-registration history remains undiscoverable there. Esplora and Electrum recover
	// it with a full scan regardless of the local checkpoint.
	if !loaded_from_store {
		let block_id = bdk_chain::BlockId { height: chain_tip.height, hash: chain_tip.block_hash };
		let mut cp = wallet.latest_checkpoint();
		cp = cp.insert(block_id);
		let update = bdk_wallet::Update { chain: Some(cp), ..Default::default() };
		wallet.apply_update(update).map_err(|e| {
			log_error!(logger, "Failed to apply checkpoint for {:?}: {}", wallet_account, e);
			Error::WalletOperationFailed
		})?;
	}

	Ok((wallet, persister))
}

impl WalletSource for Wallet {
	fn list_confirmed_utxos<'a>(
		&'a self,
	) -> Pin<Box<dyn Future<Output = Result<Vec<Utxo>, ()>> + Send + 'a>> {
		Box::pin(async move { self.list_confirmed_utxos_inner() })
	}

	fn get_change_script<'a>(
		&'a self,
	) -> Pin<Box<dyn Future<Output = Result<ScriptBuf, ()>> + Send + 'a>> {
		Box::pin(async move { self.get_change_script_inner() })
	}

	fn sign_psbt<'a>(
		&'a self, psbt: Psbt,
	) -> Pin<Box<dyn Future<Output = Result<Transaction, ()>> + Send + 'a>> {
		Box::pin(async move { self.sign_psbt_inner(psbt) })
	}
}

/// Similar to [`KeysManager`], but overrides the destination and shutdown scripts so they are
/// directly spendable by the BDK wallet.
pub(crate) struct WalletKeysManager {
	inner: KeysManager,
	wallet: Arc<Wallet>,
	logger: Arc<Logger>,
}

impl WalletKeysManager {
	/// Constructs a `WalletKeysManager` that overrides the destination and shutdown scripts.
	///
	/// See [`KeysManager::new`] for more information on `seed`, `starting_time_secs`, and
	/// `starting_time_nanos`.
	pub fn new(
		seed: &[u8; 32], starting_time_secs: u64, starting_time_nanos: u32, wallet: Arc<Wallet>,
		logger: Arc<Logger>,
	) -> Self {
		let inner = KeysManager::new(seed, starting_time_secs, starting_time_nanos, true);
		Self { inner, wallet, logger }
	}

	pub fn sign_message(&self, msg: &[u8]) -> String {
		message_signing::sign(msg, &self.inner.get_node_secret_key())
	}

	pub fn get_node_secret_key(&self) -> SecretKey {
		self.inner.get_node_secret_key()
	}

	pub fn verify_signature(&self, msg: &[u8], sig: &str, pkey: &PublicKey) -> bool {
		message_signing::verify(msg, sig, pkey)
	}
}

impl NodeSigner for WalletKeysManager {
	fn get_node_id(&self, recipient: Recipient) -> Result<PublicKey, ()> {
		self.inner.get_node_id(recipient)
	}

	fn ecdh(
		&self, recipient: Recipient, other_key: &PublicKey, tweak: Option<&Scalar>,
	) -> Result<SharedSecret, ()> {
		self.inner.ecdh(recipient, other_key, tweak)
	}

	fn get_expanded_key(&self) -> ExpandedKey {
		self.inner.get_expanded_key()
	}

	fn get_peer_storage_key(&self) -> PeerStorageKey {
		self.inner.get_peer_storage_key()
	}

	fn get_receive_auth_key(&self) -> lightning::sign::ReceiveAuthKey {
		self.inner.get_receive_auth_key()
	}

	fn sign_invoice(
		&self, invoice: &RawBolt11Invoice, recipient: Recipient,
	) -> Result<RecoverableSignature, ()> {
		self.inner.sign_invoice(invoice, recipient)
	}

	fn sign_gossip_message(&self, msg: UnsignedGossipMessage<'_>) -> Result<Signature, ()> {
		self.inner.sign_gossip_message(msg)
	}

	fn sign_bolt12_invoice(
		&self, invoice: &lightning::offers::invoice::UnsignedBolt12Invoice,
	) -> Result<bitcoin::secp256k1::schnorr::Signature, ()> {
		self.inner.sign_bolt12_invoice(invoice)
	}
	fn sign_message(&self, msg: &[u8]) -> Result<String, ()> {
		self.inner.sign_message(msg)
	}
}

impl OutputSpender for WalletKeysManager {
	/// See [`KeysManager::spend_spendable_outputs`] for documentation on this method.
	fn spend_spendable_outputs(
		&self, descriptors: &[&SpendableOutputDescriptor], outputs: Vec<TxOut>,
		change_destination_script: ScriptBuf, feerate_sat_per_1000_weight: u32,
		locktime: Option<LockTime>, secp_ctx: &Secp256k1<All>,
	) -> Result<Transaction, ()> {
		self.inner.spend_spendable_outputs(
			descriptors,
			outputs,
			change_destination_script,
			feerate_sat_per_1000_weight,
			locktime,
			secp_ctx,
		)
	}
}

impl EntropySource for WalletKeysManager {
	fn get_secure_random_bytes(&self) -> [u8; 32] {
		self.inner.get_secure_random_bytes()
	}
}

impl SignerProvider for WalletKeysManager {
	type EcdsaSigner = InMemorySigner;

	fn generate_channel_keys_id(&self, inbound: bool, user_channel_id: u128) -> [u8; 32] {
		self.inner.generate_channel_keys_id(inbound, user_channel_id)
	}

	fn derive_channel_signer(&self, channel_keys_id: [u8; 32]) -> Self::EcdsaSigner {
		self.inner.derive_channel_signer(channel_keys_id)
	}

	fn get_destination_script(&self, _channel_keys_id: [u8; 32]) -> Result<ScriptBuf, ()> {
		let address = self.wallet.get_new_witness_address().map_err(|e| {
			log_error!(self.logger, "Failed to retrieve new witness address from wallet: {}", e);
		})?;
		Ok(address.script_pubkey())
	}

	fn get_shutdown_scriptpubkey(&self) -> Result<ShutdownScript, ()> {
		let address = self.wallet.get_new_witness_address().map_err(|e| {
			log_error!(self.logger, "Failed to retrieve new witness address from wallet: {}", e);
		})?;

		match address.witness_program() {
			Some(program) => ShutdownScript::new_witness_program(&program).map_err(|e| {
				log_error!(self.logger, "Invalid shutdown script: {:?}", e);
			}),
			_ => {
				log_error!(
					self.logger,
					"get_shutdown_scriptpubkey received a non-native-witness address. \
					 This is a bug in get_new_witness_address."
				);
				Err(())
			},
		}
	}
}

impl ChangeDestinationSource for WalletKeysManager {
	fn get_change_destination_script<'a>(
		&'a self,
	) -> Pin<Box<dyn Future<Output = Result<ScriptBuf, ()>> + Send + 'a>> {
		let wallet = Arc::clone(&self.wallet);
		let logger = Arc::clone(&self.logger);
		Box::pin(async move {
			wallet
				.get_new_internal_address()
				.map_err(|e| {
					log_error!(logger, "Failed to retrieve new address from wallet: {}", e);
				})
				.map(|addr| addr.script_pubkey())
				.map_err(|_| ())
		})
	}
}

#[cfg(test)]
mod tests {
	use super::{
		additional_input_weight, map_wallet_account_error, validate_derivation_index,
		validate_derivation_range, BroadcastIntent, BIP32_MAX_NORMAL_INDEX,
		LEGACY_ONCHAIN_BROADCAST_INTENT_SERIALIZATION_VERSION, MAX_ADDRESS_INFO_BATCH_COUNT,
	};
	use crate::builder::NodeBuilder;
	use crate::config::{AddressType, OnchainWalletAccount};
	use crate::io::{
		test_utils::InMemoryStore, ONCHAIN_BROADCAST_INTENT_PRIMARY_NAMESPACE,
		ONCHAIN_BROADCAST_INTENT_SECONDARY_NAMESPACE, PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
		PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
	};
	use crate::payment::{
		ConfirmationStatus, PaymentDetails, PaymentDirection, PaymentKind, PaymentStatus,
	};
	use crate::types::DynStore;
	use crate::Error;
	use bdk_wallet_aggregate::UtxoPsbtInfo;
	use bitcoin::absolute::LockTime;
	use bitcoin::consensus::serialize;
	use bitcoin::hashes::Hash;
	use bitcoin::transaction::Version;
	use bitcoin::{psbt, Amount, Network, OutPoint, Transaction, TxIn, TxOut, Weight};
	use lightning::ln::channelmanager::PaymentId;
	use lightning::util::persist::KVStoreSync;
	use std::collections::HashSet;
	use std::sync::Arc;
	use std::time::{SystemTime, UNIX_EPOCH};

	fn replacement_test_transaction(lock_time: u32) -> Transaction {
		Transaction {
			version: Version::TWO,
			lock_time: LockTime::from_consensus(lock_time),
			input: vec![TxIn { previous_output: OutPoint::null(), ..TxIn::default() }],
			output: vec![],
		}
	}

	fn replacement_test_node(store: Arc<DynStore>) -> crate::Node {
		let config = crate::Config { network: Network::Regtest, ..crate::Config::default() };
		let mut builder = NodeBuilder::from_config(config);
		builder.set_chain_source_esplora("http://127.0.0.1:1".to_string(), None);
		builder.set_entropy_seed_bytes([44u8; 64]);
		builder.set_log_facade_logger();
		builder.build_with_store(store).unwrap()
	}

	fn replacement_test_payment(txid: bitcoin::Txid) -> PaymentDetails {
		PaymentDetails::new(
			PaymentId(txid.to_byte_array()),
			PaymentKind::Onchain { txid, status: ConfirmationStatus::Unconfirmed },
			Some(1_000),
			Some(100),
			PaymentDirection::Outbound,
			PaymentStatus::Pending,
		)
	}

	#[test]
	fn derivation_index_validation_rejects_hardened_range() {
		assert_eq!(validate_derivation_index(BIP32_MAX_NORMAL_INDEX), Ok(()));
		assert_eq!(
			validate_derivation_index(BIP32_MAX_NORMAL_INDEX + 1),
			Err(Error::InvalidQuantity)
		);
	}

	#[test]
	fn derivation_range_validation_rejects_overflow_into_hardened_range() {
		assert_eq!(validate_derivation_range(BIP32_MAX_NORMAL_INDEX - 1, 2), Ok(()));
		assert_eq!(
			validate_derivation_range(BIP32_MAX_NORMAL_INDEX - 1, 3),
			Err(Error::InvalidQuantity)
		);
		assert_eq!(validate_derivation_range(u32::MAX, 1), Err(Error::InvalidQuantity));
	}

	#[test]
	fn derivation_range_validation_rejects_oversized_batches() {
		assert_eq!(validate_derivation_range(0, MAX_ADDRESS_INFO_BATCH_COUNT), Ok(()));
		assert_eq!(
			validate_derivation_range(0, MAX_ADDRESS_INFO_BATCH_COUNT + 1),
			Err(Error::InvalidQuantity)
		);
	}

	#[test]
	fn derivation_range_validation_allows_empty_ranges_at_valid_start() {
		assert_eq!(validate_derivation_range(BIP32_MAX_NORMAL_INDEX, 0), Ok(()));
	}

	#[test]
	fn missing_derived_wallet_maps_to_not_registered() {
		let derived =
			OnchainWalletAccount { address_type: AddressType::NativeSegwit, account_index: 1 };
		let account_zero = OnchainWalletAccount::account_zero(AddressType::NativeSegwit);

		assert_eq!(
			map_wallet_account_error(derived, bdk_wallet_aggregate::Error::WalletNotFound),
			Error::OnchainWalletAccountNotRegistered
		);
		assert_eq!(
			map_wallet_account_error(account_zero, bdk_wallet_aggregate::Error::WalletNotFound),
			Error::WalletOperationFailed
		);
		assert_eq!(
			map_wallet_account_error(derived, bdk_wallet_aggregate::Error::PersistenceFailed),
			Error::WalletOperationFailed
		);
	}

	#[test]
	fn additional_input_weight_includes_the_base_input_weight() {
		let satisfaction_weight = Weight::from_wu(107);
		let utxo = UtxoPsbtInfo {
			outpoint: OutPoint::null(),
			psbt_input: psbt::Input::default(),
			weight: satisfaction_weight,
			is_primary: false,
		};

		assert_eq!(
			additional_input_weight(&[utxo]).unwrap(),
			TxIn::default().segwit_weight() + satisfaction_weight
		);
	}

	#[test]
	fn rbf_supersession_preserves_the_intent_key_and_changes_only_the_active_txid() {
		let original = replacement_test_transaction(1);
		let replacement = replacement_test_transaction(2);
		let original_txid = original.compute_txid();
		let replacement_txid = replacement.compute_txid();
		let mut intent = BroadcastIntent::new(original.clone());

		intent.supersede(replacement.clone()).unwrap();

		assert_eq!(intent.key(), original_txid);
		assert_eq!(intent.active_txid(), replacement_txid);
		assert_eq!(intent.transactions, vec![original, replacement]);
	}

	#[test]
	fn ordinary_rbf_retains_the_accepted_original_only_for_reconciliation() {
		let original = replacement_test_transaction(8);
		let replacement = replacement_test_transaction(9);
		let original_txid = original.compute_txid();
		let replacement_txid = replacement.compute_txid();

		let intent =
			BroadcastIntent::replacement(None, original.clone(), replacement.clone()).unwrap();

		assert_eq!(intent.key(), original_txid);
		assert_eq!(intent.active_txid(), replacement_txid);
		assert_eq!(intent.first_pending_index, 1);
		assert_eq!(intent.transactions, vec![original, replacement]);
	}

	#[test]
	fn same_second_rbf_timestamp_makes_the_replacement_canonical() {
		let store: Arc<DynStore> = Arc::new(InMemoryStore::new());
		let node = replacement_test_node(store);
		let script_pubkey = node.onchain_payment().new_address().unwrap().script_pubkey();
		let mut original = replacement_test_transaction(14);
		original.input[0].previous_output.vout = 1;
		original
			.output
			.push(TxOut { value: Amount::from_sat(10_000), script_pubkey: script_pubkey.clone() });
		let mut replacement = replacement_test_transaction(15);
		replacement.input[0].previous_output.vout = 1;
		replacement.output.push(TxOut { value: Amount::from_sat(9_000), script_pubkey });
		let original_txid = original.compute_txid();
		let replacement_txid = replacement.compute_txid();
		let original_seen =
			SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs().saturating_add(1);
		let mut wallet = node.wallet.inner.lock().unwrap();
		wallet.apply_mempool_txs(vec![(original.clone(), original_seen)], Vec::new()).unwrap();
		let intent =
			BroadcastIntent::replacement(None, original.clone(), replacement.clone()).unwrap();
		let lineage_txids =
			intent.transactions.iter().map(|tx| tx.compute_txid()).collect::<Vec<_>>();

		let replacement_seen =
			super::Wallet::next_broadcast_timestamp(&wallet, &lineage_txids).unwrap();
		wallet
			.apply_mempool_txs(vec![(replacement.clone(), replacement_seen)], Vec::new())
			.unwrap();
		assert_eq!(wallet.find_tx(replacement_txid), Some(replacement));
		assert_eq!(wallet.find_tx(original_txid), None);
		node.wallet.update_payment_store(&wallet).unwrap();
		drop(wallet);

		assert!(replacement_seen > original_seen);
		assert!(node.payment(&PaymentId(replacement_txid.to_byte_array())).is_some());
		assert!(node.payment(&PaymentId(original_txid.to_byte_array())).is_none());
	}

	#[test]
	fn rbf_supersession_rejects_a_transaction_that_does_not_conflict() {
		let original = replacement_test_transaction(1);
		let mut replacement = replacement_test_transaction(2);
		replacement.input[0].previous_output.vout = 1;
		let mut intent = BroadcastIntent::new(original.clone());

		assert_eq!(intent.supersede(replacement), Err(Error::OnchainTxCreationFailed));
		assert_eq!(intent.transactions, vec![original]);
	}

	#[test]
	fn rejected_rbf_replacement_atomically_restores_the_previous_retry_target() {
		let store: Arc<DynStore> = Arc::new(InMemoryStore::new());
		let node = replacement_test_node(Arc::clone(&store));
		let original = replacement_test_transaction(3);
		let replacement = replacement_test_transaction(4);
		let original_txid = original.compute_txid();
		let replacement_txid = replacement.compute_txid();
		let mut intent = BroadcastIntent::new(original.clone());
		intent.supersede(replacement).unwrap();
		node.wallet.write_broadcast_intent(&intent).unwrap();

		node.wallet.reject_rbf_broadcast(&replacement_txid).unwrap();

		assert_eq!(node.wallet.list_pending_broadcasts().unwrap(), vec![original_txid]);
		assert_eq!(node.wallet.recover_pending_broadcast(&original_txid).unwrap(), Some(original));
		drop(node);
		let restarted = replacement_test_node(store);
		assert_eq!(restarted.wallet.list_pending_broadcasts().unwrap(), vec![original_txid]);
	}

	#[test]
	fn observing_an_rbf_predecessor_resolves_the_active_replacement_intent() {
		let store: Arc<DynStore> = Arc::new(InMemoryStore::new());
		let node = replacement_test_node(store);
		let original = replacement_test_transaction(5);
		let replacement = replacement_test_transaction(6);
		let intent = BroadcastIntent::replacement(None, original.clone(), replacement).unwrap();
		node.wallet.write_broadcast_intent(&intent).unwrap();

		node.wallet.apply_mempool_txs(vec![(original, 1)], Vec::new()).unwrap();

		assert!(node.wallet.list_pending_broadcasts().unwrap().is_empty());
	}

	#[test]
	fn rejected_ordinary_rbf_does_not_turn_the_accepted_original_into_a_retry_target() {
		let store: Arc<DynStore> = Arc::new(InMemoryStore::new());
		let node = replacement_test_node(store);
		let original = replacement_test_transaction(10);
		let replacement = replacement_test_transaction(11);
		let replacement_txid = replacement.compute_txid();
		let intent = BroadcastIntent::replacement(None, original, replacement).unwrap();
		node.wallet.write_broadcast_intent(&intent).unwrap();

		node.wallet.reject_rbf_broadcast(&replacement_txid).unwrap();

		assert!(node.wallet.list_pending_broadcasts().unwrap().is_empty());
	}

	#[test]
	fn rbf_payment_history_persists_only_the_canonical_replacement() {
		let store: Arc<DynStore> = Arc::new(InMemoryStore::new());
		let node = replacement_test_node(Arc::clone(&store));
		let original = replacement_test_transaction(12);
		let replacement = replacement_test_transaction(13);
		let original_txid = original.compute_txid();
		let replacement_txid = replacement.compute_txid();
		let intent = BroadcastIntent::replacement(None, original, replacement).unwrap();
		node.wallet.write_broadcast_intent(&intent).unwrap();
		node.payment_store.insert(replacement_test_payment(original_txid)).unwrap();
		node.payment_store.insert(replacement_test_payment(replacement_txid)).unwrap();

		node.wallet
			.remove_noncanonical_broadcast_payments(&HashSet::from([replacement_txid]))
			.unwrap();

		assert!(node.payment(&PaymentId(original_txid.to_byte_array())).is_none());
		assert!(node.payment(&PaymentId(replacement_txid.to_byte_array())).is_some());
		let original_key = crate::hex_utils::to_string(&original_txid.to_byte_array());
		let replacement_key = crate::hex_utils::to_string(&replacement_txid.to_byte_array());
		assert!(KVStoreSync::read(
			&*store,
			PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
			PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
			&original_key
		)
		.is_err());
		assert!(KVStoreSync::read(
			&*store,
			PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
			PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
			&replacement_key
		)
		.is_ok());
	}

	#[test]
	fn accepted_rbf_ancestors_survive_restart_until_a_lineage_member_confirms() {
		let store: Arc<DynStore> = Arc::new(InMemoryStore::new());
		let node = replacement_test_node(Arc::clone(&store));
		let original = replacement_test_transaction(16);
		let first_replacement = replacement_test_transaction(17);
		let second_replacement = replacement_test_transaction(18);
		let original_txid = original.compute_txid();
		let first_replacement_txid = first_replacement.compute_txid();
		let second_replacement_txid = second_replacement.compute_txid();
		let first_intent =
			BroadcastIntent::replacement(None, original.clone(), first_replacement.clone())
				.unwrap();
		node.wallet.write_broadcast_intent(&first_intent).unwrap();
		node.wallet.clear_broadcast_intent(&first_replacement_txid).unwrap();
		assert!(node.wallet.list_pending_broadcasts().unwrap().is_empty());
		drop(node);

		let restarted = replacement_test_node(Arc::clone(&store));
		let (_, accepted_lineage) =
			restarted.wallet.find_broadcast_intent_by_active_txid(&first_replacement_txid).unwrap();
		assert_eq!(
			accepted_lineage.transactions,
			vec![original.clone(), first_replacement.clone()]
		);
		assert!(!accepted_lineage.has_pending_transaction());
		let second_intent = BroadcastIntent::replacement(
			Some(accepted_lineage),
			first_replacement.clone(),
			second_replacement.clone(),
		)
		.unwrap();
		restarted.wallet.write_broadcast_intent(&second_intent).unwrap();
		assert_eq!(
			restarted.wallet.list_pending_broadcasts().unwrap(),
			vec![second_replacement_txid]
		);
		drop(restarted);

		let restarted_again = replacement_test_node(store);
		let (_, uncertain_lineage) = restarted_again
			.wallet
			.find_broadcast_intent_by_active_txid(&second_replacement_txid)
			.unwrap();
		assert_eq!(
			uncertain_lineage.transactions,
			vec![original, first_replacement, second_replacement]
		);
		restarted_again
			.wallet
			.resolve_broadcast_intents([(original_txid, original_txid, true)])
			.unwrap();
		assert!(restarted_again.wallet.list_pending_broadcasts().unwrap().is_empty());
		assert!(restarted_again.wallet.read_all_broadcast_intents().unwrap().is_empty());
	}

	#[test]
	fn legacy_single_transaction_broadcast_intent_is_restored() {
		let store = Arc::new(InMemoryStore::new());
		let tx = replacement_test_transaction(7);
		let txid = tx.compute_txid();
		let mut bytes = vec![LEGACY_ONCHAIN_BROADCAST_INTENT_SERIALIZATION_VERSION];
		bytes.extend(serialize(&tx));
		KVStoreSync::write(
			&*store,
			ONCHAIN_BROADCAST_INTENT_PRIMARY_NAMESPACE,
			ONCHAIN_BROADCAST_INTENT_SECONDARY_NAMESPACE,
			&txid.to_string(),
			bytes,
		)
		.unwrap();
		let dyn_store: Arc<DynStore> = store;

		let node = replacement_test_node(dyn_store);

		assert_eq!(node.wallet.list_pending_broadcasts().unwrap(), vec![txid]);
		assert_eq!(node.wallet.recover_pending_broadcast(&txid).unwrap(), Some(tx));
	}
}

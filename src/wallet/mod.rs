// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::HashMap;
use std::future::Future;
use std::ops::Deref;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use bdk_chain::spk_client::{FullScanRequest, SyncRequest};
pub use bdk_wallet::coin_selection::CoinSelectionAlgorithm as BdkCoinSelectionAlgorithm;
use bdk_wallet::coin_selection::{
	BranchAndBoundCoinSelection, Excess, LargestFirstCoinSelection, OldestFirstCoinSelection,
	SingleRandomDraw,
};
use bdk_wallet::event::WalletEvent;
#[allow(deprecated)]
use bdk_wallet::SignOptions;
use bdk_wallet::{Balance, KeychainKind, LocalOutput, PersistedWallet, Update, WeightedUtxo};
use bip39::rand::rngs::OsRng;
use bitcoin::address::NetworkUnchecked;
use bitcoin::blockdata::constants::WITNESS_SCALE_FACTOR;
use bitcoin::blockdata::locktime::absolute::LockTime;
use bitcoin::hashes::Hash;
use bitcoin::key::XOnlyPublicKey;
use bitcoin::psbt::{self, Psbt};
use bitcoin::secp256k1::ecdh::SharedSecret;
use bitcoin::secp256k1::ecdsa::{RecoverableSignature, Signature};
use bitcoin::secp256k1::{All, PublicKey, Scalar, Secp256k1, SecretKey};
use bitcoin::{
	Address, Amount, FeeRate, OutPoint, Script, ScriptBuf, Transaction, TxOut, Txid, WPubkeyHash,
	Weight, WitnessProgram, WitnessVersion,
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
use lightning::log_warn;
use lightning::sign::{
	ChangeDestinationSource, EntropySource, InMemorySigner, KeysManager, NodeSigner, OutputSpender,
	PeerStorageKey, Recipient, SignerProvider, SpendableOutputDescriptor,
};
use lightning::util::message_signing;
use lightning_invoice::RawBolt11Invoice;
use persist::KVStoreWalletPersister;

use crate::config::{AddressType, Config};
use crate::event::{TxInput, TxOutput};
use crate::fee_estimator::{ConfirmationTarget, FeeEstimator, OnchainFeeEstimator};
use crate::logger::{log_debug, log_error, log_info, log_trace, LdkLogger, Logger};
use crate::payment::store::ConfirmationStatus;
use crate::payment::{PaymentDetails, PaymentDirection, PaymentStatus};
use crate::types::{Broadcaster, ChannelManager, PaymentStore};
use crate::Error;

// Minimum economical output value (dust limit)
const DUST_LIMIT_SATS: u64 = 546;

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

/// UTXO info for multi-wallet PSBT building.
#[derive(Clone)]
pub(crate) struct UtxoPsbtInfo {
	pub outpoint: OutPoint,
	pub psbt_input: psbt::Input,
	pub weight: Weight,
	pub is_primary: bool,
}

pub(crate) struct Wallet {
	// Multiple BDK on-chain wallets, one per address type being monitored.
	// The primary wallet (for generating new addresses) is stored under config.address_type.
	wallets: Mutex<HashMap<AddressType, PersistedWallet<KVStoreWalletPersister>>>,
	persisters: Mutex<HashMap<AddressType, KVStoreWalletPersister>>,
	broadcaster: Arc<Broadcaster>,
	fee_estimator: Arc<OnchainFeeEstimator>,
	payment_store: Arc<PaymentStore>,
	config: Arc<Config>,
	logger: Arc<Logger>,
	// Optional chain source and runtime for fetching transactions when not in wallet history
	chain_source: Option<Arc<crate::chain::ChainSource>>,
	runtime: Option<Arc<crate::runtime::Runtime>>,
}

impl Wallet {
	pub(crate) fn new(
		primary_wallet: bdk_wallet::PersistedWallet<KVStoreWalletPersister>,
		primary_persister: KVStoreWalletPersister,
		additional_wallets: Vec<(
			AddressType,
			bdk_wallet::PersistedWallet<KVStoreWalletPersister>,
			KVStoreWalletPersister,
		)>,
		broadcaster: Arc<Broadcaster>, fee_estimator: Arc<OnchainFeeEstimator>,
		payment_store: Arc<PaymentStore>, config: Arc<Config>, logger: Arc<Logger>,
		chain_source: Option<Arc<crate::chain::ChainSource>>,
		runtime: Option<Arc<crate::runtime::Runtime>>,
	) -> Self {
		let mut wallets = HashMap::new();
		let mut persisters = HashMap::new();

		// Add primary wallet
		wallets.insert(config.address_type, primary_wallet);
		persisters.insert(config.address_type, primary_persister);

		// Add additional wallets for monitoring
		for (address_type, wallet, persister) in additional_wallets {
			wallets.insert(address_type, wallet);
			persisters.insert(address_type, persister);
		}

		Self {
			wallets: Mutex::new(wallets),
			persisters: Mutex::new(persisters),
			broadcaster,
			fee_estimator,
			payment_store,
			config,
			logger,
			chain_source,
			runtime,
		}
	}

	fn with_wallet_mut<T, F>(&self, address_type: AddressType, f: F) -> Result<T, Error>
	where
		F: FnOnce(
			&mut PersistedWallet<KVStoreWalletPersister>,
			&mut KVStoreWalletPersister,
		) -> Result<T, Error>,
	{
		let mut wallets = self.wallets.lock().unwrap();
		let mut persisters = self.persisters.lock().unwrap();

		let wallet = wallets.get_mut(&address_type).ok_or_else(|| {
			log_error!(self.logger, "Wallet not found for address type {:?}", address_type);
			Error::WalletOperationFailed
		})?;

		let persister = persisters.get_mut(&address_type).ok_or_else(|| {
			log_error!(self.logger, "Persister not found for address type {:?}", address_type);
			Error::WalletOperationFailed
		})?;

		f(wallet, persister)
	}

	fn with_primary_wallet_mut<T, F>(&self, f: F) -> Result<T, Error>
	where
		F: FnOnce(
			&mut PersistedWallet<KVStoreWalletPersister>,
			&mut KVStoreWalletPersister,
		) -> Result<T, Error>,
	{
		self.with_wallet_mut(self.config.address_type, f)
	}

	fn collect_from_wallets<T, F>(&self, f: F) -> Vec<T>
	where
		F: Fn(&PersistedWallet<KVStoreWalletPersister>) -> Vec<T>,
	{
		let wallets = self.wallets.lock().unwrap();
		wallets.values().flat_map(|w| f(w)).collect()
	}

	fn calculate_utxo_weight(script_pubkey: &ScriptBuf) -> Weight {
		match script_pubkey.witness_version() {
			Some(bitcoin::WitnessVersion::V0) => Weight::from_wu(272), // P2WPKH
			Some(bitcoin::WitnessVersion::V1) => Weight::from_wu(230), // P2TR
			None => {
				// Check if P2SH-wrapped SegWit (nested segwit)
				let script_bytes = script_pubkey.as_bytes();
				if script_bytes.len() == 23 && script_bytes[0] == 0xa9 && script_bytes[21] == 0x87 {
					Weight::from_wu(360) // P2SH-wrapped P2WPKH
				} else {
					Weight::from_wu(588) // P2PKH (legacy)
				}
			},
			_ => Weight::from_wu(272), // Fallback to P2WPKH weight
		}
	}

	fn find_wallet_for_tx(
		wallets: &HashMap<AddressType, PersistedWallet<KVStoreWalletPersister>>, txid: Txid,
	) -> Option<AddressType> {
		wallets.iter().find_map(|(addr_type, wallet)| wallet.get_tx(txid).map(|_| *addr_type))
	}

	fn find_tx_in_wallets(
		wallets: &HashMap<AddressType, PersistedWallet<KVStoreWalletPersister>>, txid: Txid,
	) -> Option<Transaction> {
		for wallet in wallets.values() {
			if let Some(tx_node) = wallet.get_tx(txid) {
				return Some((*tx_node.tx_node.tx).clone());
			}
		}
		None
	}

	fn get_aggregate_balance(&self) -> Balance {
		let wallets = self.wallets.lock().unwrap();
		Self::get_aggregate_balance_from_wallets(&wallets)
	}

	fn get_aggregate_balance_from_wallets(
		wallets: &HashMap<AddressType, PersistedWallet<KVStoreWalletPersister>>,
	) -> Balance {
		let mut total = Balance::default();
		for wallet in wallets.values() {
			let balance = wallet.balance();
			total.confirmed += balance.confirmed;
			total.trusted_pending += balance.trusted_pending;
			total.untrusted_pending += balance.untrusted_pending;
		}
		total
	}

	fn calculate_fee_from_psbt(
		&self, psbt: &Psbt, wallets: &HashMap<AddressType, PersistedWallet<KVStoreWalletPersister>>,
	) -> Result<u64, Error> {
		let mut total_input_value = 0u64;

		for (i, txin) in psbt.unsigned_tx.input.iter().enumerate() {
			if let Some(psbt_input) = psbt.inputs.get(i) {
				if let Some(witness_utxo) = &psbt_input.witness_utxo {
					total_input_value += witness_utxo.value.to_sat();
				} else if let Some(non_witness_tx) = &psbt_input.non_witness_utxo {
					if let Some(txout) =
						non_witness_tx.output.get(txin.previous_output.vout as usize)
					{
						total_input_value += txout.value.to_sat();
					} else {
						log_error!(
							self.logger,
							"Could not find output {} in non_witness_utxo for input {:?}",
							txin.previous_output.vout,
							txin.previous_output
						);
						return Err(Error::OnchainTxCreationFailed);
					}
				} else {
					// Fallback: try to find the UTXO in any wallet
					let mut found = false;
					for wallet in wallets.values() {
						if let Some(local_utxo) = wallet.get_utxo(txin.previous_output) {
							total_input_value += local_utxo.txout.value.to_sat();
							found = true;
							break;
						}
					}
					if !found {
						log_error!(
							self.logger,
							"Could not find TxOut for input {:?} in PSBT or any wallet",
							txin.previous_output
						);
						return Err(Error::OnchainTxCreationFailed);
					}
				}
			} else {
				// PSBT input not found, try to find in wallets
				let mut found = false;
				for wallet in wallets.values() {
					if let Some(local_utxo) = wallet.get_utxo(txin.previous_output) {
						total_input_value += local_utxo.txout.value.to_sat();
						found = true;
						break;
					}
				}
				if !found {
					log_error!(
						self.logger,
						"Could not find TxOut for input {:?} in PSBT or any wallet",
						txin.previous_output
					);
					return Err(Error::OnchainTxCreationFailed);
				}
			}
		}

		let total_output_value: u64 =
			psbt.unsigned_tx.output.iter().map(|txout| txout.value.to_sat()).sum();

		Ok(total_input_value.saturating_sub(total_output_value))
	}

	fn prepare_utxos_for_psbt(
		&self, utxos: &[LocalOutput],
		wallets: &HashMap<AddressType, PersistedWallet<KVStoreWalletPersister>>,
	) -> Result<Vec<UtxoPsbtInfo>, Error> {
		let mut result = Vec::new();

		for utxo in utxos {
			let mut found_wallet: Option<(AddressType, &PersistedWallet<KVStoreWalletPersister>)> =
				None;
			for (addr_type, w) in wallets.iter() {
				if w.get_utxo(utxo.outpoint).is_some() {
					found_wallet = Some((*addr_type, w));
					break;
				}
			}

			let (addr_type, wallet) = match found_wallet {
				Some(w) => w,
				None => {
					log_error!(self.logger, "UTXO {:?} not found in any wallet", utxo.outpoint);
					return Err(Error::OnchainTxCreationFailed);
				},
			};

			let local_utxo = match wallet.get_utxo(utxo.outpoint) {
				Some(u) => u,
				None => {
					log_error!(self.logger, "UTXO {:?} disappeared from wallet", utxo.outpoint);
					return Err(Error::OnchainTxCreationFailed);
				},
			};

			let mut psbt_input = match wallet.get_psbt_input(local_utxo.clone(), None, true) {
				Ok(input) => input,
				Err(e) => {
					log_error!(
						self.logger,
						"Failed to get PSBT input for UTXO {:?}: {}",
						utxo.outpoint,
						e
					);
					return Err(Error::OnchainTxCreationFailed);
				},
			};

			let is_primary = addr_type == self.config.address_type;
			let weight = Self::calculate_utxo_weight(&local_utxo.txout.script_pubkey);

			if !is_primary {
				// BDK requires non_witness_utxo for foreign UTXOs
				if psbt_input.non_witness_utxo.is_none() {
					let found_tx = Self::find_tx_in_wallets(wallets, utxo.outpoint.txid);

					if let Some(tx) = found_tx {
						psbt_input.non_witness_utxo = Some(tx);
						log_debug!(
							self.logger,
							"Set non_witness_utxo for foreign UTXO {:?} from wallet",
							utxo.outpoint
						);
					} else {
						log_info!(
							self.logger,
							"Transaction {:?} not found in any wallet, attempting chain source...",
							utxo.outpoint.txid
						);
						if let (Some(chain_source), Some(runtime)) =
							(&self.chain_source, &self.runtime)
						{
							match runtime
								.block_on(chain_source.get_transaction(&utxo.outpoint.txid))
							{
								Ok(Some(tx)) => {
									psbt_input.non_witness_utxo = Some(tx.clone());
									log_info!(
										self.logger,
										"Fetched transaction {:?} from chain source",
										utxo.outpoint.txid
									);
								},
								Ok(None) => {
									log_error!(
										self.logger,
										"Transaction {:?} not found in chain source",
										utxo.outpoint.txid
									);
									return Err(Error::OnchainTxCreationFailed);
								},
								Err(e) => {
									log_error!(
										self.logger,
										"Failed to fetch transaction {:?}: {}",
										utxo.outpoint.txid,
										e
									);
									return Err(Error::OnchainTxCreationFailed);
								},
							}
						} else {
							log_error!(
								self.logger,
								"Cannot get transaction for foreign UTXO {:?}: no chain source",
								utxo.outpoint
							);
							return Err(Error::OnchainTxCreationFailed);
						}
					}
				}
				if psbt_input.witness_utxo.is_none() {
					psbt_input.witness_utxo = Some(local_utxo.txout.clone());
				}
			} else if psbt_input.witness_utxo.is_none() {
				psbt_input.witness_utxo = Some(local_utxo.txout.clone());
			}

			result.push(UtxoPsbtInfo { outpoint: utxo.outpoint, psbt_input, weight, is_primary });
		}

		Ok(result)
	}

	fn prepare_outpoints_for_psbt(
		&self, outpoints: &[OutPoint],
		wallets: &HashMap<AddressType, PersistedWallet<KVStoreWalletPersister>>,
	) -> Result<Vec<UtxoPsbtInfo>, Error> {
		let mut utxos = Vec::new();
		for outpoint in outpoints {
			let mut found = false;
			for wallet in wallets.values() {
				if let Some(local_utxo) = wallet.get_utxo(*outpoint) {
					utxos.push(local_utxo);
					found = true;
					break;
				}
			}
			if !found {
				log_error!(self.logger, "UTXO {:?} not found in any wallet", outpoint);
				return Err(Error::OnchainTxCreationFailed);
			}
		}
		self.prepare_utxos_for_psbt(&utxos, wallets)
	}

	fn add_utxos_to_tx_builder<Cs>(
		&self, tx_builder: &mut bdk_wallet::TxBuilder<'_, Cs>, utxo_infos: &[UtxoPsbtInfo],
	) -> Result<(), Error>
	where
		Cs: bdk_wallet::coin_selection::CoinSelectionAlgorithm,
	{
		for info in utxo_infos {
			if info.is_primary {
				tx_builder.add_utxo(info.outpoint).map_err(|e| {
					log_error!(self.logger, "Failed to add UTXO {:?}: {}", info.outpoint, e);
					Error::OnchainTxCreationFailed
				})?;
			} else {
				tx_builder
					.add_foreign_utxo(info.outpoint, info.psbt_input.clone(), info.weight)
					.map_err(|e| {
						log_error!(
							self.logger,
							"Failed to add foreign UTXO {:?}: {}",
							info.outpoint,
							e
						);
						Error::OnchainTxCreationFailed
					})?;
			}
		}
		Ok(())
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

	pub(crate) fn get_full_scan_request(&self) -> FullScanRequest<KeychainKind> {
		let wallets = self.wallets.lock().unwrap();
		wallets.get(&self.config.address_type).unwrap().start_full_scan().build()
	}

	pub(crate) fn get_incremental_sync_request(&self) -> SyncRequest<(KeychainKind, u32)> {
		let wallets = self.wallets.lock().unwrap();
		wallets.get(&self.config.address_type).unwrap().start_sync_with_revealed_spks().build()
	}

	pub(crate) fn apply_update_to_wallet(
		&self, address_type: AddressType, update: impl Into<Update>,
	) -> Result<Vec<WalletEvent>, Error> {
		let update = update.into();
		let mut wallets = self.wallets.lock().unwrap();
		let mut persisters = self.persisters.lock().unwrap();

		let events_result = if let (Some(wallet), Some(persister)) =
			(wallets.get_mut(&address_type), persisters.get_mut(&address_type))
		{
			match wallet.apply_update_events(update) {
				Ok(events) => {
					wallet.persist(persister).map_err(|e| {
						log_error!(
							self.logger,
							"Failed to persist wallet for {:?}: {}",
							address_type,
							e
						);
						Error::PersistenceFailed
					})?;

					let txids: Vec<Txid> =
						wallet.transactions().map(|wtx| wtx.tx_node.txid).collect();
					Ok((events, txids))
				},
				Err(e) => {
					log_error!(
						self.logger,
						"Failed to apply update to wallet {:?}: {}",
						address_type,
						e
					);
					Err(Error::WalletOperationFailed)
				},
			}
		} else {
			log_error!(
				self.logger,
				"Wallet or persister not found for address type {:?}",
				address_type
			);
			Err(Error::WalletOperationFailed)
		};

		drop(wallets);
		drop(persisters);

		match events_result {
			Ok((events, txids)) => {
				if let Err(e) = self.update_payment_store_for_txids(txids) {
					log_error!(
						self.logger,
						"Failed to update payment store for wallet {:?}: {}",
						address_type,
						e
					);
				}
				Ok(events)
			},
			Err(e) => Err(e),
		}
	}

	pub(crate) fn get_wallet_sync_request(
		&self, address_type: AddressType,
	) -> Result<(FullScanRequest<KeychainKind>, SyncRequest<(KeychainKind, u32)>), Error> {
		let wallets = self.wallets.lock().unwrap();
		let wallet = wallets.get(&address_type).ok_or_else(|| {
			let loaded_types: Vec<_> = wallets.keys().copied().collect();
			log_error!(
				self.logger,
				"Wallet not found for address type {:?}. Expected in address_types_to_monitor: {:?}, Primary: {:?}, Loaded wallets: {:?}",
				address_type,
				self.config.address_types_to_monitor,
				self.config.address_type,
				loaded_types
			);
			Error::WalletOperationFailed
		})?;

		let full_scan = wallet.start_full_scan().build();
		let incremental_sync = wallet.start_sync_with_revealed_spks().build();
		Ok((full_scan, incremental_sync))
	}

	pub(crate) fn get_cached_txs(&self) -> Vec<Arc<Transaction>> {
		self.collect_from_wallets(|wallet| {
			wallet.tx_graph().full_txs().map(|tx_node| tx_node.tx).collect()
		})
	}

	pub(crate) fn get_unconfirmed_txids(&self) -> Vec<Txid> {
		self.collect_from_wallets(|wallet| {
			wallet
				.transactions()
				.filter(|t| t.chain_position.is_unconfirmed())
				.map(|t| t.tx_node.txid)
				.collect()
		})
	}

	pub(crate) fn is_tx_confirmed(&self, txid: &Txid) -> bool {
		// Check all wallets
		let wallets = self.wallets.lock().unwrap();
		for wallet in wallets.values() {
			if let Some(tx_node) = wallet.get_tx(*txid) {
				if tx_node.chain_position.is_confirmed() {
					return true;
				}
			}
		}
		false
	}

	pub(crate) fn current_best_block(&self) -> BestBlock {
		// Use primary wallet's checkpoint
		let wallets = self.wallets.lock().unwrap();
		let checkpoint = wallets.get(&self.config.address_type).unwrap().latest_checkpoint();
		BestBlock { block_hash: checkpoint.hash(), height: checkpoint.height() }
	}

	// Get a drain script for change outputs.
	pub(crate) fn get_drain_script(&self) -> Result<ScriptBuf, Error> {
		// Use primary wallet for change addresses
		let wallets = self.wallets.lock().unwrap();
		let wallet = wallets.get(&self.config.address_type).unwrap();
		let change_address = wallet.peek_address(KeychainKind::Internal, 0);
		Ok(change_address.address.script_pubkey())
	}

	pub(crate) fn apply_update(
		&self, update: impl Into<Update>,
	) -> Result<Vec<WalletEvent>, Error> {
		let update = update.into();
		let mut wallets = self.wallets.lock().unwrap();
		let mut persisters = self.persisters.lock().unwrap();
		let mut all_events = Vec::new();

		let (primary_events_result, all_txids) =
			if let Some(wallet) = wallets.get_mut(&self.config.address_type) {
				let events_result = match wallet.apply_update_events(update) {
					Ok(events) => {
						if let Some(persister) = persisters.get_mut(&self.config.address_type) {
							wallet.persist(persister).map_err(|e| {
								log_error!(self.logger, "Failed to persist wallet: {}", e);
								Error::PersistenceFailed
							})?;
						}
						Ok(events)
					},
					Err(e) => {
						log_error!(self.logger, "Sync failed due to chain connection error: {}", e);
						Err(Error::WalletOperationFailed)
					},
				};

				let mut all_txids_set = std::collections::HashSet::new();
				for wallet in wallets.values() {
					for wtx in wallet.transactions() {
						all_txids_set.insert(wtx.tx_node.txid);
					}
				}
				(events_result, all_txids_set.into_iter().collect())
			} else {
				(Err(Error::WalletOperationFailed), Vec::new())
			};

		drop(wallets);
		drop(persisters);

		match primary_events_result {
			Ok(events) => {
				all_events.extend(events);
				if !all_txids.is_empty() {
					self.update_payment_store_for_txids(all_txids).map_err(|e| {
						log_error!(self.logger, "Failed to update payment store: {}", e);
						Error::PersistenceFailed
					})?;
				}
			},
			Err(e) => return Err(e),
		}

		Ok(all_events)
	}

	pub(crate) fn apply_mempool_txs(
		&self, unconfirmed_txs: Vec<(Transaction, u64)>, evicted_txids: Vec<(Txid, u64)>,
	) -> Result<(), Error> {
		// Apply mempool updates to all wallets
		let mut wallets = self.wallets.lock().unwrap();
		let mut persisters = self.persisters.lock().unwrap();

		for (address_type, wallet) in wallets.iter_mut() {
			wallet.apply_unconfirmed_txs(unconfirmed_txs.clone());
			wallet.apply_evicted_txs(evicted_txids.clone());

			if let Some(persister) = persisters.get_mut(address_type) {
				wallet.persist(persister).map_err(|e| {
					log_error!(self.logger, "Failed to persist wallet {:?}: {}", address_type, e);
					Error::PersistenceFailed
				})?;
			}
		}

		Ok(())
	}

	// Bumps the fee of an existing transaction using Replace-By-Fee (RBF).
	// Supports both single-wallet and cross-wallet transactions.
	pub(crate) fn bump_fee_by_rbf(
		&self, txid: &Txid, fee_rate: FeeRate, channel_manager: &ChannelManager,
	) -> Result<Txid, Error> {
		// Check if this is a funding transaction
		if self.is_funding_transaction(txid, channel_manager) {
			log_error!(
				self.logger,
				"Cannot RBF transaction {}: it is a channel funding transaction",
				txid
			);
			return Err(Error::CannotRbfFundingTransaction);
		}

		// Find which wallet contains this transaction
		let mut wallets = self.wallets.lock().unwrap();
		let mut persisters = self.persisters.lock().unwrap();

		// First, find the transaction to get its inputs
		let tx = Self::find_tx_in_wallets(&wallets, *txid).ok_or_else(|| {
			log_error!(self.logger, "Transaction not found in any wallet: {}", txid);
			Error::TransactionNotFound
		})?;

		// Check if this is a cross-wallet transaction by seeing which wallets own inputs
		let mut wallets_with_inputs: Vec<AddressType> = Vec::new();
		for (addr_type, wallet) in wallets.iter() {
			for input in &tx.input {
				if let Some(prev_tx_node) = wallet.get_tx(input.previous_output.txid) {
					// This wallet has the previous tx, check if the output belongs to this wallet
					if let Some(prev_output) =
						prev_tx_node.tx_node.tx.output.get(input.previous_output.vout as usize)
					{
						if wallet.is_mine(prev_output.script_pubkey.clone()) {
							if !wallets_with_inputs.contains(addr_type) {
								wallets_with_inputs.push(*addr_type);
							}
							break;
						}
					}
				}
			}
		}

		let is_cross_wallet = wallets_with_inputs.len() > 1;

		// Get transaction details from any wallet that has it
		let first_wallet_type = Self::find_wallet_for_tx(&wallets, *txid).ok_or_else(|| {
			log_error!(self.logger, "Transaction not found in any wallet: {}", txid);
			Error::TransactionNotFound
		})?;

		let (original_tx, original_fee, original_fee_rate, is_confirmed) = {
			let wallet = wallets.get(&first_wallet_type).unwrap();
			let tx_node = wallet.get_tx(*txid).unwrap();

			let is_confirmed = tx_node.chain_position.is_confirmed();
			let original_tx = (*tx_node.tx_node.tx).clone();
			let original_fee = wallet.calculate_fee(&original_tx).map_err(|e| {
				log_error!(self.logger, "Failed to calculate original fee: {}", e);
				Error::WalletOperationFailed
			})?;
			let original_fee_rate = original_fee / original_tx.weight();

			(original_tx, original_fee, original_fee_rate, is_confirmed)
		};

		// Check if transaction is confirmed - can't replace confirmed transactions
		if is_confirmed {
			log_error!(self.logger, "Cannot replace confirmed transaction: {}", txid);
			return Err(Error::TransactionAlreadyConfirmed);
		}

		// Log detailed information for debugging
		log_info!(self.logger, "RBF Analysis for transaction {}", txid);
		log_info!(self.logger, "  Original fee: {} sats", original_fee.to_sat());
		log_info!(
			self.logger,
			"  Original weight: {} WU ({} vB)",
			original_tx.weight().to_wu(),
			original_tx.weight().to_vbytes_ceil()
		);
		log_info!(
			self.logger,
			"  Original fee rate: {} sat/kwu ({} sat/vB)",
			original_fee_rate.to_sat_per_kwu(),
			original_fee_rate.to_sat_per_vb_ceil()
		);
		log_info!(
			self.logger,
			"  Requested fee rate: {} sat/kwu ({} sat/vB)",
			fee_rate.to_sat_per_kwu(),
			fee_rate.to_sat_per_vb_ceil()
		);
		if is_cross_wallet {
			log_info!(
				self.logger,
				"  Cross-wallet transaction: inputs from {:?}",
				wallets_with_inputs
			);
		}

		// Essential validation: new fee rate must be higher than original
		if fee_rate <= original_fee_rate {
			log_error!(
				self.logger,
				"RBF rejected: New fee rate ({} sat/vB) must be higher than original fee rate ({} sat/vB)",
				fee_rate.to_sat_per_vb_ceil(),
				original_fee_rate.to_sat_per_vb_ceil()
			);
			return Err(Error::InvalidFeeRate);
		}

		log_info!(
			self.logger,
			"RBF approved: Fee rate increase from {} to {} sat/vB",
			original_fee_rate.to_sat_per_vb_ceil(),
			fee_rate.to_sat_per_vb_ceil()
		);

		let replacement_tx = if is_cross_wallet {
			// For cross-wallet transactions, we need to build the RBF manually
			// because BDK's build_fee_bump only operates on a single wallet
			self.build_cross_wallet_rbf(
				&wallets,
				&mut persisters,
				&original_tx,
				fee_rate,
				original_fee,
			)?
		} else {
			// For single-wallet transactions, use BDK's build_fee_bump
			let address_type = first_wallet_type;
			let wallet = wallets.get_mut(&address_type).unwrap();
			let persister = persisters.get_mut(&address_type).unwrap();

			let mut tx_builder = wallet.build_fee_bump(*txid).map_err(|e| {
				log_error!(self.logger, "Failed to create fee bump builder: {}", e);
				Error::OnchainTxCreationFailed
			})?;

			tx_builder.fee_rate(fee_rate);

			let mut psbt = match tx_builder.finish() {
				Ok(psbt) => {
					log_trace!(self.logger, "Created RBF PSBT: {:?}", psbt);
					psbt
				},
				Err(err) => {
					log_error!(self.logger, "Failed to create RBF transaction: {}", err);
					return Err(Error::OnchainTxCreationFailed);
				},
			};

			match wallet.sign(&mut psbt, SignOptions::default()) {
				Ok(finalized) => {
					if !finalized {
						log_error!(self.logger, "Failed to finalize RBF transaction");
						return Err(Error::OnchainTxSigningFailed);
					}
				},
				Err(err) => {
					log_error!(self.logger, "Failed to sign RBF transaction: {}", err);
					return Err(Error::OnchainTxSigningFailed);
				},
			}

			wallet.persist(persister).map_err(|e| {
				log_error!(self.logger, "Failed to persist wallet: {}", e);
				Error::PersistenceFailed
			})?;

			psbt.extract_tx().map_err(|e| {
				log_error!(self.logger, "Failed to extract transaction: {}", e);
				Error::OnchainTxCreationFailed
			})?
		};

		self.broadcaster.broadcast_transactions(&[&replacement_tx]);

		let new_txid = replacement_tx.compute_txid();

		// Calculate and log the actual fee increase achieved
		let new_weight = replacement_tx.weight();
		let new_fee_sats = fee_rate.to_sat_per_kwu() as u64 * new_weight.to_wu() / 1000;
		let actual_fee_rate = FeeRate::from_sat_per_kwu(new_fee_sats * 1000 / new_weight.to_wu());

		log_info!(self.logger, "RBF transaction created successfully!");
		log_info!(
			self.logger,
			"  Original: {} ({} sat/vB, {} sats fee)",
			txid,
			original_fee_rate.to_sat_per_vb_ceil(),
			original_fee.to_sat()
		);
		log_info!(
			self.logger,
			"  Replacement: {} (~{} sat/vB)",
			new_txid,
			actual_fee_rate.to_sat_per_vb_ceil()
		);

		Ok(new_txid)
	}

	// Builds an RBF transaction for cross-wallet transactions.
	// Reduces change output or adds new inputs as needed to cover the higher fee.
	fn build_cross_wallet_rbf(
		&self, wallets: &HashMap<AddressType, PersistedWallet<KVStoreWalletPersister>>,
		_persisters: &mut HashMap<AddressType, KVStoreWalletPersister>, original_tx: &Transaction,
		new_fee_rate: FeeRate, original_fee: Amount,
	) -> Result<Transaction, Error> {
		// BIP 125 check: Verify original transaction signals RBF
		// At least one input must have nSequence < 0xfffffffe
		let signals_rbf = original_tx.input.iter().any(|input| input.sequence.0 < 0xfffffffe);

		if !signals_rbf {
			log_error!(
				self.logger,
				"Original transaction does not signal RBF (no input has nSequence < 0xfffffffe)"
			);
			return Err(Error::OnchainTxCreationFailed);
		}

		// Calculate the new fee needed (rough estimate)
		let tx_weight = original_tx.weight();
		let new_fee_sats = new_fee_rate.to_sat_per_kwu() as u64 * tx_weight.to_wu() / 1000;

		// BIP 125 check: Replacement must pay higher ABSOLUTE fee
		if new_fee_sats <= original_fee.to_sat() {
			log_error!(
				self.logger,
				"BIP 125 violation: Replacement fee ({} sats) must be higher than original ({} sats)",
				new_fee_sats,
				original_fee.to_sat()
			);
			return Err(Error::InvalidFeeRate);
		}

		let additional_fee_needed = new_fee_sats - original_fee.to_sat();

		log_info!(
			self.logger,
			"Cross-wallet RBF: need ~{} additional sats for fee bump",
			additional_fee_needed
		);

		// Identify the change output (belongs to a wallet we control)
		// and the recipient outputs (external addresses)
		let primary_wallet = wallets.get(&self.config.address_type).ok_or_else(|| {
			log_error!(self.logger, "Primary wallet not found");
			Error::WalletOperationFailed
		})?;

		let mut recipient_outputs: Vec<TxOut> = Vec::new();
		let mut change_outputs: Vec<TxOut> = Vec::new();

		for output in &original_tx.output {
			// OP_RETURN outputs (data carriers) should be preserved as-is
			if output.script_pubkey.is_op_return() {
				recipient_outputs.push(output.clone());
				continue;
			}

			// Check if this output belongs to any of our wallets
			let is_ours = wallets.values().any(|w| w.is_mine(output.script_pubkey.clone()));

			if is_ours {
				// This is a change output (could be multiple in edge cases)
				change_outputs.push(output.clone());
				log_info!(
					self.logger,
					"Found change output with value {} sats",
					output.value.to_sat()
				);
			} else {
				// This is a recipient output - we must preserve it exactly
				recipient_outputs.push(output.clone());
			}
		}

		// Calculate total change available
		let total_change_value: u64 = change_outputs.iter().map(|o| o.value.to_sat()).sum();
		let has_change = !change_outputs.is_empty();

		// First approach: Try to just reduce the change output(s)
		if has_change && total_change_value > additional_fee_needed + 546 {
			let new_change_value = total_change_value - additional_fee_needed;

			log_info!(
				self.logger,
				"Cross-wallet RBF: reducing change from {} to {} sats",
				total_change_value,
				new_change_value
			);

			// Rebuild the transaction with reduced change
			let mut new_outputs = recipient_outputs.clone();

			// Get change address from primary wallet
			let change_script =
				primary_wallet.peek_address(KeychainKind::Internal, 0).address.script_pubkey();
			new_outputs.push(TxOut {
				value: Amount::from_sat(new_change_value),
				script_pubkey: change_script,
			});

			// Create clean unsigned inputs (preserve outpoint and sequence, clear script/witness)
			let unsigned_inputs: Vec<bitcoin::TxIn> = original_tx
				.input
				.iter()
				.map(|input| bitcoin::TxIn {
					previous_output: input.previous_output,
					script_sig: ScriptBuf::new(),
					sequence: input.sequence,
					witness: bitcoin::Witness::new(),
				})
				.collect();

			// Create unsigned transaction
			let unsigned_tx = Transaction {
				version: original_tx.version,
				lock_time: original_tx.lock_time,
				input: unsigned_inputs,
				output: new_outputs,
			};

			// Sign with all wallets that own inputs
			let signed_tx = self.sign_owned_inputs(unsigned_tx).map_err(|_| {
				log_error!(self.logger, "Failed to sign cross-wallet RBF transaction");
				Error::OnchainTxSigningFailed
			})?;

			return Ok(signed_tx);
		}

		// Second approach: Need to add more inputs
		// Collect the original inputs as outpoints
		let original_outpoints: Vec<OutPoint> =
			original_tx.input.iter().map(|i| i.previous_output).collect();

		log_info!(self.logger, "Cross-wallet RBF: change insufficient, adding more inputs");

		// Prepare UTXO info for original inputs
		let original_utxo_infos = self.prepare_outpoints_for_psbt(&original_outpoints, wallets)?;

		// Get additional available UTXOs from all wallets (excluding ones already used)
		let additional_utxos: Vec<LocalOutput> = wallets
			.values()
			.flat_map(|w| w.list_unspent())
			.filter(|utxo| !original_outpoints.contains(&utxo.outpoint))
			.collect();

		// Calculate total from original inputs (try witness_utxo first, then non_witness_utxo)
		let original_input_value: u64 = original_utxo_infos
			.iter()
			.zip(original_outpoints.iter())
			.filter_map(|(u, outpoint)| {
				// First try witness_utxo
				if let Some(txout) = &u.psbt_input.witness_utxo {
					return Some(txout.value.to_sat());
				}
				// For legacy inputs, get value from non_witness_utxo
				if let Some(tx) = &u.psbt_input.non_witness_utxo {
					if let Some(txout) = tx.output.get(outpoint.vout as usize) {
						return Some(txout.value.to_sat());
					}
				}
				log_error!(self.logger, "Could not determine value for input {:?}", outpoint);
				None
			})
			.sum();

		// Calculate recipient value
		let recipient_value: u64 = recipient_outputs.iter().map(|o| o.value.to_sat()).sum();

		// Select additional UTXOs if needed, accounting for their weight contribution to fees.
		// Each input we add increases tx weight, which increases required fee.
		let mut selected_additional: Vec<LocalOutput> = Vec::new();
		let mut additional_value: u64 = 0;
		let mut additional_weight = Weight::ZERO;

		// Calculate how much we need: value shortfall + fee for additional weight
		let base_shortfall = (recipient_value + new_fee_sats).saturating_sub(original_input_value);

		if base_shortfall > 0 && additional_utxos.is_empty() {
			log_error!(
				self.logger,
				"Need {} more sats but no additional UTXOs available",
				base_shortfall
			);
			return Err(Error::InsufficientFunds);
		}

		// Greedy selection accounting for input weight
		for utxo in additional_utxos {
			// Calculate fee contribution of this input
			let input_weight = Self::calculate_utxo_weight(&utxo.txout.script_pubkey);
			let input_fee_cost = new_fee_rate.to_sat_per_kwu() as u64 * input_weight.to_wu() / 1000;

			// Total needed = base shortfall + fee for all additional inputs so far
			let total_additional_fee =
				new_fee_rate.to_sat_per_kwu() as u64 * additional_weight.to_wu() / 1000;
			let total_needed = base_shortfall + total_additional_fee + input_fee_cost;

			if additional_value >= total_needed {
				break;
			}

			additional_value += utxo.txout.value.to_sat();
			additional_weight = additional_weight + input_weight;
			selected_additional.push(utxo);
		}

		// Final check: do we have enough?
		let final_additional_fee =
			new_fee_rate.to_sat_per_kwu() as u64 * additional_weight.to_wu() / 1000;
		let final_needed = base_shortfall + final_additional_fee;

		if additional_value < final_needed {
			log_error!(
				self.logger,
				"Insufficient funds: need {} more (including {} for input fees), only have {}",
				final_needed,
				final_additional_fee,
				additional_value
			);
			return Err(Error::InsufficientFunds);
		}

		// Update total fee to include additional input weight
		let new_fee_sats = new_fee_sats + final_additional_fee;

		// Build new transaction with all inputs
		// Create clean unsigned inputs (preserve outpoint and sequence, clear script/witness)
		let mut all_inputs: Vec<bitcoin::TxIn> = original_tx
			.input
			.iter()
			.map(|input| bitcoin::TxIn {
				previous_output: input.previous_output,
				script_sig: ScriptBuf::new(),
				sequence: input.sequence,
				witness: bitcoin::Witness::new(),
			})
			.collect();

		// Add additional inputs with RBF-signaling sequence
		for utxo in &selected_additional {
			all_inputs.push(bitcoin::TxIn {
				previous_output: utxo.outpoint,
				script_sig: ScriptBuf::new(),
				sequence: bitcoin::Sequence::ENABLE_RBF_NO_LOCKTIME,
				witness: bitcoin::Witness::new(),
			});
		}

		// Calculate new change
		let total_input = original_input_value + additional_value;
		let new_change = total_input.saturating_sub(recipient_value + new_fee_sats);

		// Build outputs
		let mut new_outputs = recipient_outputs;
		if new_change >= 546 {
			let change_script =
				primary_wallet.peek_address(KeychainKind::Internal, 0).address.script_pubkey();
			new_outputs
				.push(TxOut { value: Amount::from_sat(new_change), script_pubkey: change_script });
		}

		let unsigned_tx = Transaction {
			version: original_tx.version,
			lock_time: original_tx.lock_time,
			input: all_inputs,
			output: new_outputs,
		};

		log_info!(
			self.logger,
			"Cross-wallet RBF: built tx with {} inputs, {} outputs, ~{} change",
			unsigned_tx.input.len(),
			unsigned_tx.output.len(),
			new_change
		);

		// Sign with all wallets that own inputs
		let signed_tx = self.sign_owned_inputs(unsigned_tx).map_err(|_| {
			log_error!(self.logger, "Failed to sign cross-wallet RBF transaction");
			Error::OnchainTxSigningFailed
		})?;

		Ok(signed_tx)
	}

	// Accelerates confirmation of a transaction using Child-Pays-For-Parent (CPFP).
	// Returns the txid of the child transaction if successful.
	//
	// For cross-wallet transactions, change typically goes to the primary wallet,
	// so this method searches all wallets for spendable outputs from the parent transaction.
	pub(crate) fn accelerate_by_cpfp(
		&self, txid: &Txid, fee_rate: FeeRate, destination_address: Option<Address>,
	) -> Result<Txid, Error> {
		let mut wallets = self.wallets.lock().unwrap();
		let mut persisters = self.persisters.lock().unwrap();

		// Find the transaction in any wallet to get its details
		let tx_wallet_type = Self::find_wallet_for_tx(&wallets, *txid).ok_or_else(|| {
			log_error!(self.logger, "Transaction not found in any wallet: {}", txid);
			Error::TransactionNotFound
		})?;

		// Get transaction info first (read-only)
		let (parent_tx, parent_fee, parent_fee_rate) = {
			let wallet_ref = wallets.get(&tx_wallet_type).unwrap();
			let parent_tx_node = wallet_ref.get_tx(*txid).unwrap();

			// Check if transaction is confirmed - can't accelerate confirmed transactions
			if parent_tx_node.chain_position.is_confirmed() {
				log_error!(self.logger, "Cannot accelerate confirmed transaction: {}", txid);
				return Err(Error::TransactionAlreadyConfirmed);
			}

			let parent_tx = &parent_tx_node.tx_node.tx;
			let parent_fee = wallet_ref.calculate_fee(parent_tx).map_err(|e| {
				log_error!(self.logger, "Failed to calculate parent fee: {}", e);
				Error::WalletOperationFailed
			})?;
			let parent_fee_rate = parent_fee / parent_tx.weight();

			(parent_tx.clone(), parent_fee, parent_fee_rate)
		};

		// Search ALL wallets for spendable outputs from the parent transaction
		// Change typically goes to the primary wallet, but we check all to be robust
		let mut utxos: Vec<LocalOutput> = Vec::new();
		let mut utxo_wallet_type: Option<AddressType> = None;

		// Check primary wallet first (change usually goes here)
		if let Some(primary_wallet) = wallets.get(&self.config.address_type) {
			let primary_utxos: Vec<_> =
				primary_wallet.list_unspent().filter(|utxo| utxo.outpoint.txid == *txid).collect();
			if !primary_utxos.is_empty() {
				utxos = primary_utxos;
				utxo_wallet_type = Some(self.config.address_type);
			}
		}

		// If no UTXOs found in primary, check other wallets
		if utxos.is_empty() {
			for (addr_type, wallet) in wallets.iter() {
				let wallet_utxos: Vec<_> =
					wallet.list_unspent().filter(|utxo| utxo.outpoint.txid == *txid).collect();
				if !wallet_utxos.is_empty() {
					utxos = wallet_utxos;
					utxo_wallet_type = Some(*addr_type);
					break;
				}
			}
		}

		let address_type = utxo_wallet_type.ok_or_else(|| {
			log_error!(self.logger, "No spendable outputs found for transaction: {}", txid);
			Error::NoSpendableOutputs
		})?;

		// Log detailed information for debugging
		log_info!(self.logger, "CPFP Analysis for transaction {}", txid);
		log_info!(self.logger, "  Parent fee: {} sats", parent_fee.to_sat());
		log_info!(
			self.logger,
			"  Parent weight: {} WU ({} vB)",
			parent_tx.weight().to_wu(),
			parent_tx.weight().to_vbytes_ceil()
		);
		log_info!(
			self.logger,
			"  Parent fee rate: {} sat/kwu ({} sat/vB)",
			parent_fee_rate.to_sat_per_kwu(),
			parent_fee_rate.to_sat_per_vb_ceil()
		);
		log_info!(
			self.logger,
			"  Child fee rate: {} sat/kwu ({} sat/vB)",
			fee_rate.to_sat_per_kwu(),
			fee_rate.to_sat_per_vb_ceil()
		);

		// Validate that child fee rate is higher than parent (for effective acceleration)
		if fee_rate <= parent_fee_rate {
			log_info!(
				self.logger,
				"CPFP warning: Child fee rate ({} sat/vB) is not higher than parent fee rate ({} sat/vB). This may not effectively accelerate confirmation.",
				fee_rate.to_sat_per_vb_ceil(),
				parent_fee_rate.to_sat_per_vb_ceil()
			);
			// Note: We warn but don't reject - CPFP can still work in some cases
		} else {
			let acceleration_ratio =
				fee_rate.to_sat_per_kwu() as f64 / parent_fee_rate.to_sat_per_kwu() as f64;
			log_info!(
				self.logger,
				"CPFP acceleration: Child fee rate is {:.1}x higher than parent ({} vs {} sat/vB)",
				acceleration_ratio,
				fee_rate.to_sat_per_vb_ceil(),
				parent_fee_rate.to_sat_per_vb_ceil()
			);
		}

		// utxos is guaranteed non-empty at this point (handled by address_type check above)
		log_info!(self.logger, "Found {} spendable output(s) from parent transaction", utxos.len());
		log_info!(self.logger, "  UTXOs found in {:?} wallet", address_type);
		let total_input_value: u64 = utxos.iter().map(|utxo| utxo.txout.value.to_sat()).sum();
		log_info!(self.logger, "  Total input value: {} sats", total_input_value);

		// Determine where to send the funds
		let script_pubkey = match destination_address {
			Some(addr) => {
				log_info!(self.logger, "  Destination: {} (user-specified)", addr);
				self.parse_and_validate_address(&addr)?;
				addr.script_pubkey()
			},
			None => {
				// Create a new address to send the funds to (use primary wallet for change)
				// Need to release current lock and get primary wallet
				drop(wallets);
				drop(persisters);
				let mut wallets_primary = self.wallets.lock().unwrap();
				let mut persisters_primary = self.persisters.lock().unwrap();
				let primary_wallet = wallets_primary.get_mut(&self.config.address_type).unwrap();
				let address_info = primary_wallet.next_unused_address(KeychainKind::Internal);
				primary_wallet
					.persist(persisters_primary.get_mut(&self.config.address_type).unwrap())
					.map_err(|e| {
						log_error!(self.logger, "Failed to persist wallet: {}", e);
						Error::PersistenceFailed
					})?;
				log_info!(
					self.logger,
					"  Destination: {} (wallet internal address)",
					address_info.address
				);
				let script_pubkey = address_info.address.script_pubkey();
				drop(wallets_primary);
				drop(persisters_primary);
				// Re-acquire locks for the original wallet
				wallets = self.wallets.lock().unwrap();
				persisters = self.persisters.lock().unwrap();
				script_pubkey
			},
		};

		// Now get mutable references for building the transaction
		let wallet = wallets.get_mut(&address_type).unwrap();
		let persister = persisters.get_mut(&address_type).unwrap();

		// Build a transaction that spends these UTXOs
		let mut tx_builder = wallet.build_tx();

		// Add the UTXOs explicitly
		for utxo in &utxos {
			match tx_builder.add_utxo(utxo.outpoint) {
				Ok(_) => {},
				Err(e) => {
					log_error!(self.logger, "Failed to add UTXO: {:?} - {}", utxo.outpoint, e);
					return Err(Error::OnchainTxCreationFailed);
				},
			}
		}

		// Set the fee rate for the child transaction
		tx_builder.fee_rate(fee_rate);

		// Drain all inputs to the destination
		tx_builder.drain_to(script_pubkey);

		// Finalize the transaction
		let mut psbt = match tx_builder.finish() {
			Ok(psbt) => {
				log_trace!(self.logger, "Created CPFP PSBT: {:?}", psbt);
				psbt
			},
			Err(err) => {
				log_error!(self.logger, "Failed to create CPFP transaction: {}", err);
				return Err(Error::OnchainTxCreationFailed);
			},
		};

		// Sign the transaction
		match wallet.sign(&mut psbt, SignOptions::default()) {
			Ok(finalized) => {
				if !finalized {
					log_error!(self.logger, "Failed to finalize CPFP transaction");
					return Err(Error::OnchainTxSigningFailed);
				}
			},
			Err(err) => {
				log_error!(self.logger, "Failed to sign CPFP transaction: {}", err);
				return Err(Error::OnchainTxSigningFailed);
			},
		}

		// Persist wallet changes
		wallet.persist(persister).map_err(|e| {
			log_error!(self.logger, "Failed to persist wallet: {}", e);
			Error::PersistenceFailed
		})?;

		// Extract and broadcast the transaction
		let tx = psbt.extract_tx().map_err(|e| {
			log_error!(self.logger, "Failed to extract transaction: {}", e);
			Error::OnchainTxCreationFailed
		})?;

		self.broadcaster.broadcast_transactions(&[&tx]);

		let child_txid = tx.compute_txid();

		// Calculate and log the actual results
		let child_fee = wallet.calculate_fee(&tx).unwrap_or(Amount::ZERO);
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
		log_info!(
			self.logger,
			"  Combined package fee rate: approximately {:.1} sat/vB",
			((parent_fee.to_sat() + child_fee.to_sat()) as f64)
				/ ((parent_tx.weight().to_vbytes_ceil() + tx.weight().to_vbytes_ceil()) as f64)
		);

		Ok(child_txid)
	}

	// Calculates an appropriate fee rate for a CPFP transaction.
	pub(crate) fn calculate_cpfp_fee_rate(
		&self, parent_txid: &Txid, urgent: bool,
	) -> Result<FeeRate, Error> {
		// Find which wallet contains this transaction
		let wallets = self.wallets.lock().unwrap();
		let wallet = wallets
			.values()
			.find_map(|wallet| wallet.get_tx(*parent_txid).map(|_| wallet))
			.ok_or_else(|| {
				log_error!(self.logger, "Transaction not found in any wallet: {}", parent_txid);
				Error::TransactionNotFound
			})?;

		// Get the parent transaction
		let parent_tx_node = wallet.get_tx(*parent_txid).ok_or_else(|| {
			log_error!(self.logger, "Transaction not found in wallet: {}", parent_txid);
			Error::TransactionNotFound
		})?;

		// Make sure it's not confirmed
		if parent_tx_node.chain_position.is_confirmed() {
			log_error!(self.logger, "Transaction is already confirmed: {}", parent_txid);
			return Err(Error::TransactionAlreadyConfirmed);
		}

		let parent_tx = &parent_tx_node.tx_node.tx;

		// Calculate parent fee and fee rate using accurate method
		let parent_fee = wallet.calculate_fee(parent_tx).map_err(|e| {
			log_error!(self.logger, "Failed to calculate parent fee: {}", e);
			Error::WalletOperationFailed
		})?;

		// Use Bitcoin crate's built-in fee rate calculation for accuracy
		let parent_fee_rate = parent_fee / parent_tx.weight();

		// Get current mempool fee rates from fee estimator based on urgency
		let target = if urgent {
			ConfirmationTarget::Lightning(
				lightning::chain::chaininterface::ConfirmationTarget::MaximumFeeEstimate,
			)
		} else {
			ConfirmationTarget::OnchainPayment
		};

		let target_fee_rate = self.fee_estimator.estimate_fee_rate(target);

		log_info!(self.logger, "CPFP Fee Rate Calculation for transaction {}", parent_txid);
		log_info!(self.logger, "  Parent fee: {} sats", parent_fee.to_sat());
		log_info!(
			self.logger,
			"  Parent weight: {} WU ({} vB)",
			parent_tx.weight().to_wu(),
			parent_tx.weight().to_vbytes_ceil()
		);
		log_info!(
			self.logger,
			"  Parent fee rate: {} sat/kwu ({} sat/vB)",
			parent_fee_rate.to_sat_per_kwu(),
			parent_fee_rate.to_sat_per_vb_ceil()
		);
		log_info!(
			self.logger,
			"  Target fee rate: {} sat/kwu ({} sat/vB)",
			target_fee_rate.to_sat_per_kwu(),
			target_fee_rate.to_sat_per_vb_ceil()
		);
		log_info!(self.logger, "  Urgency level: {}", if urgent { "HIGH" } else { "NORMAL" });

		// If parent fee rate is already sufficient, return a slightly higher one
		if parent_fee_rate >= target_fee_rate {
			let recommended_rate =
				FeeRate::from_sat_per_kwu(parent_fee_rate.to_sat_per_kwu() + 250); // +1 sat/vB
			log_info!(
				self.logger,
				"Parent fee rate is already sufficient. Recommending slightly higher rate: {} sat/vB",
				recommended_rate.to_sat_per_vb_ceil()
			);
			return Ok(recommended_rate);
		}

		// Estimate child transaction size (weight units)
		// Conservative estimate for a typical 1-input, 1-output transaction
		let estimated_child_weight_units = 480; // ~120 vbytes * 4 = 480 wu
		let estimated_child_vbytes = estimated_child_weight_units / 4;

		// Calculate the fee deficit for the parent (in sats)
		// let parent_weight_units = parent_tx.weight().to_wu();
		let parent_vbytes = parent_tx.weight().to_vbytes_ceil();
		let parent_fee_deficit = (target_fee_rate.to_sat_per_vb_ceil()
			- parent_fee_rate.to_sat_per_vb_ceil())
			* parent_vbytes;

		// Calculate what the child needs to pay to cover both transactions
		let base_child_fee = target_fee_rate.to_sat_per_vb_ceil() * estimated_child_vbytes;
		let total_child_fee = base_child_fee + parent_fee_deficit;

		// Calculate the effective fee rate for the child
		let child_fee_rate_sat_vb = total_child_fee / estimated_child_vbytes;
		let child_fee_rate = FeeRate::from_sat_per_vb(child_fee_rate_sat_vb)
			.unwrap_or(FeeRate::from_sat_per_kwu(child_fee_rate_sat_vb * 250));

		log_info!(self.logger, "CPFP Calculation Results:");
		log_info!(self.logger, "  Parent fee deficit: {} sats", parent_fee_deficit);
		log_info!(self.logger, "  Base child fee needed: {} sats", base_child_fee);
		log_info!(self.logger, "  Total child fee needed: {} sats", total_child_fee);
		log_info!(
			self.logger,
			"  Recommended child fee rate: {} sat/vB",
			child_fee_rate.to_sat_per_vb_ceil()
		);
		log_info!(
			self.logger,
			"  Combined package rate: ~{} sat/vB",
			((parent_fee.to_sat() + total_child_fee) / (parent_vbytes + estimated_child_vbytes))
		);

		Ok(child_fee_rate)
	}

	pub(crate) fn update_payment_store_for_txids(&self, txids: Vec<Txid>) -> Result<(), Error> {
		// Get read access to all wallets to aggregate amounts
		let wallets_read = self.wallets.lock().unwrap();

		// Get the maximum chain height across all wallets to ensure consistent confirmation status
		// This prevents issues where non-primary wallets might be behind in sync
		let max_chain_height =
			wallets_read.values().map(|w| w.latest_checkpoint().height()).max().unwrap_or(0);

		for txid in txids {
			// Find the transaction in any wallet to get transaction data
			// Check ALL wallets that have this transaction to get the most up-to-date confirmation status
			// This is important because a transaction can exist in multiple wallets (different address types)
			// and we want to use the confirmed status if ANY wallet shows it as confirmed
			let mut tx_clone_opt = None;
			let mut confirmation_status_opt = None;
			let mut payment_status_opt = None;

			// First pass: find the transaction and collect confirmation status from all wallets
			// Prefer confirmed status over unconfirmed if any wallet shows it as confirmed
			let mut found_in_wallet_count = 0;
			for (addr_type, wallet) in wallets_read.iter() {
				if let Some(tx_node) = wallet.get_tx(txid) {
					found_in_wallet_count += 1;
					let is_confirmed = tx_node.chain_position.is_confirmed();
					log_debug!(
						self.logger,
						"Transaction {} found in wallet {:?}: confirmed={}, max_chain_height={}",
						txid,
						addr_type,
						is_confirmed,
						max_chain_height
					);

					// Get transaction data (only need to do this once)
					if tx_clone_opt.is_none() {
						tx_clone_opt = Some((*tx_node.tx_node.tx).clone());
					}

					// Check confirmation status from this wallet
					match tx_node.chain_position {
						bdk_chain::ChainPosition::Confirmed { anchor, .. } => {
							// If we already have a confirmed status, keep the one with the lower height (earlier confirmation)
							// Otherwise, use this confirmed status
							let confirmation_height = anchor.block_id.height;
							// Use the max chain height across all wallets for consistent status determination
							let cur_height = max_chain_height;

							match &confirmation_status_opt {
								Some(ConfirmationStatus::Confirmed {
									height: existing_height,
									..
								}) => {
									// Keep the earlier confirmation (lower height)
									if confirmation_height < *existing_height {
										payment_status_opt = Some(
											if cur_height
												>= confirmation_height + ANTI_REORG_DELAY - 1
											{
												PaymentStatus::Succeeded
											} else {
												PaymentStatus::Pending
											},
										);
										confirmation_status_opt =
											Some(ConfirmationStatus::Confirmed {
												block_hash: anchor.block_id.hash,
												height: confirmation_height,
												timestamp: anchor.confirmation_time,
											});
									}
								},
								Some(ConfirmationStatus::Unconfirmed) => {
									// Upgrade from unconfirmed to confirmed
									payment_status_opt = Some(
										if cur_height >= confirmation_height + ANTI_REORG_DELAY - 1
										{
											PaymentStatus::Succeeded
										} else {
											PaymentStatus::Pending
										},
									);
									confirmation_status_opt = Some(ConfirmationStatus::Confirmed {
										block_hash: anchor.block_id.hash,
										height: confirmation_height,
										timestamp: anchor.confirmation_time,
									});
								},
								None => {
									// First time seeing this transaction
									payment_status_opt = Some(
										if cur_height >= confirmation_height + ANTI_REORG_DELAY - 1
										{
											PaymentStatus::Succeeded
										} else {
											PaymentStatus::Pending
										},
									);
									confirmation_status_opt = Some(ConfirmationStatus::Confirmed {
										block_hash: anchor.block_id.hash,
										height: confirmation_height,
										timestamp: anchor.confirmation_time,
									});
								},
							}
						},
						bdk_chain::ChainPosition::Unconfirmed { .. } => {
							// Only set unconfirmed if we don't already have a confirmed status
							if confirmation_status_opt.is_none() {
								payment_status_opt = Some(PaymentStatus::Pending);
								confirmation_status_opt = Some(ConfirmationStatus::Unconfirmed);
							}
						},
					}
				}
			}

			let tx = match tx_clone_opt {
				Some(tx) => tx,
				None => continue, // Transaction not found in any wallet, skip
			};

			let (payment_status, confirmation_status) =
				match (payment_status_opt, confirmation_status_opt) {
					(Some(ps), Some(cs)) => (ps, cs),
					_ => continue, // Couldn't determine status, skip
				};

			// Aggregate sent and received amounts across ALL wallets that have this transaction
			// This is necessary because a transaction can use UTXOs from multiple wallets
			let mut total_sent = 0u64;
			let mut total_received = 0u64;

			// Get primary wallet for fee calculation
			let primary_wallet = wallets_read.get(&self.config.address_type).unwrap();
			let total_fee = primary_wallet.calculate_fee(&tx).unwrap_or(Amount::ZERO).to_sat();

			for wallet in wallets_read.values() {
				if wallet.get_tx(txid).is_some() {
					let (sent, received) = wallet.sent_and_received(&tx);
					total_sent += sent.to_sat();
					total_received += received.to_sat();
				}
			}

			let id = PaymentId(txid.to_byte_array());
			let kind = crate::payment::PaymentKind::Onchain { txid, status: confirmation_status };

			let (direction, amount_msat) = if total_sent > total_received {
				let direction = PaymentDirection::Outbound;
				let amount_msat = Some(
					total_sent.saturating_sub(total_fee).saturating_sub(total_received) * 1000,
				);
				(direction, amount_msat)
			} else {
				let direction = PaymentDirection::Inbound;
				let amount_msat = Some(
					total_received.saturating_sub(total_sent.saturating_sub(total_fee)) * 1000,
				);
				(direction, amount_msat)
			};

			let fee_paid_msat = Some(total_fee * 1000);

			log_debug!(
				self.logger,
				"Updating payment store for txid {}: status={:?}, confirmation={:?}, found_in_wallets={}",
				txid,
				payment_status,
				confirmation_status,
				found_in_wallet_count
			);

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

		drop(wallets_read);

		Ok(())
	}

	pub(crate) fn update_payment_store_for_all_transactions(&self) -> Result<(), Error> {
		let wallets_read = self.wallets.lock().unwrap();
		let mut all_txids = std::collections::HashSet::new();
		for wallet in wallets_read.values() {
			for wtx in wallet.transactions() {
				all_txids.insert(wtx.tx_node.txid);
			}
		}
		drop(wallets_read);

		if !all_txids.is_empty() {
			let txids_vec: Vec<Txid> = all_txids.into_iter().collect();
			self.update_payment_store_for_txids(txids_vec)
		} else {
			Ok(())
		}
	}

	#[allow(deprecated)]
	pub(crate) fn create_funding_transaction(
		&self, output_script: ScriptBuf, amount: Amount, confirmation_target: ConfirmationTarget,
		locktime: LockTime,
	) -> Result<Transaction, Error> {
		let fee_rate = self.fee_estimator.estimate_fee_rate(confirmation_target);

		// Support multi-wallet funding: collect UTXOs from all wallets and manually add them
		// Note: Lightning requires the funding OUTPUT to be a witness script (P2WSH),
		// but the INPUTS can be any type of UTXO (Legacy, NestedSegwit, NativeSegwit, Taproot).
		// The change output will use a witness address via ChangeDestinationSource.

		// Get all spendable UTXOs from all wallets (excluding funding transactions)
		// We need a channel_manager to filter funding transactions, but we don't have one here.
		// For funding transactions, we can skip the funding transaction filter since we're creating one.
		let wallets_read = self.wallets.lock().unwrap();
		let all_available_utxos: Vec<_> =
			wallets_read.values().flat_map(|w| w.list_unspent()).collect();
		drop(wallets_read);

		// Get drain script (change address) from primary wallet
		let drain_script = {
			let wallets_read = self.wallets.lock().unwrap();
			let primary_wallet = wallets_read.get(&self.config.address_type).unwrap();
			primary_wallet.peek_address(KeychainKind::Internal, 0).address.script_pubkey()
		};

		// Select UTXOs from all wallets using coin selection algorithm
		// We pass None for channel_manager since we're creating a funding transaction
		// and don't need to filter existing funding transactions
		let selected_outpoints = self.select_utxos_with_algorithm(
			amount.to_sat(),
			all_available_utxos,
			fee_rate,
			CoinSelectionAlgorithm::LargestFirst, // Use LargestFirst as default
			&drain_script,
			None, // No channel_manager - skip funding transaction filtering
		)?;

		// Prepare UTXO info for selected UTXOs from all wallets
		let wallets_read = self.wallets.lock().unwrap();
		let utxo_infos = self.prepare_outpoints_for_psbt(&selected_outpoints, &wallets_read)?;
		drop(wallets_read);

		// Build transaction with selected UTXOs from all wallets
		let mut wallets = self.wallets.lock().unwrap();
		let mut persisters = self.persisters.lock().unwrap();
		let wallet = wallets.get_mut(&self.config.address_type).ok_or_else(|| {
			log_error!(self.logger, "Primary wallet not found");
			Error::WalletOperationFailed
		})?;

		let mut tx_builder = wallet.build_tx();
		tx_builder.add_recipient(output_script, amount).fee_rate(fee_rate).nlocktime(locktime);

		// Add selected UTXOs using helper
		self.add_utxos_to_tx_builder(&mut tx_builder, &utxo_infos)?;
		tx_builder.manually_selected_only();

		let mut psbt = match tx_builder.finish() {
			Ok(psbt) => {
				log_trace!(self.logger, "Created funding PSBT: {:?}", psbt);
				psbt
			},
			Err(err) => {
				log_error!(self.logger, "Failed to create funding transaction: {}", err);
				return Err(err.into());
			},
		};

		// Sign with each wallet that owns inputs in the transaction
		// This is necessary because inputs might come from different wallets
		let mut wallet_inputs: HashMap<AddressType, Vec<usize>> = HashMap::new();
		for (i, txin) in psbt.unsigned_tx.input.iter().enumerate() {
			// Find which wallet owns this UTXO
			for (addr_type, w) in wallets.iter() {
				if w.get_utxo(txin.previous_output).is_some() {
					wallet_inputs.entry(*addr_type).or_insert_with(Vec::new).push(i);
					break;
				}
			}
		}

		// Sign with each wallet that has inputs
		for (addr_type, _input_indices) in wallet_inputs {
			let wallet = wallets.get_mut(&addr_type).ok_or_else(|| {
				log_error!(self.logger, "Wallet not found for address type {:?}", addr_type);
				Error::WalletOperationFailed
			})?;
			let persister = persisters.get_mut(&addr_type).ok_or_else(|| {
				log_error!(self.logger, "Persister not found for address type {:?}", addr_type);
				Error::WalletOperationFailed
			})?;

			let mut sign_options = SignOptions::default();
			sign_options.trust_witness_utxo = true;

			match wallet.sign(&mut psbt, sign_options) {
				Ok(_finalized) => {
					// Note: finalized might be false if there are other unsigned inputs, which is expected
				},
				Err(err) => {
					log_error!(
						self.logger,
						"Failed to sign funding transaction with wallet {:?}: {}",
						addr_type,
						err
					);
					return Err(Error::OnchainTxCreationFailed);
				},
			}

			// Persist the wallet after signing
			wallet.persist(persister).map_err(|e| {
				log_error!(self.logger, "Failed to persist wallet {:?}: {}", addr_type, e);
				Error::PersistenceFailed
			})?;
		}

		let tx = psbt.extract_tx().map_err(|e| {
			log_error!(self.logger, "Failed to extract transaction: {}", e);
			e
		})?;

		Ok(tx)
	}

	pub(crate) fn get_new_address(&self) -> Result<bitcoin::Address, Error> {
		self.get_new_address_for_type(self.config.address_type)
	}

	pub(crate) fn get_new_address_for_type(
		&self, address_type: AddressType,
	) -> Result<bitcoin::Address, Error> {
		self.with_wallet_mut(address_type, |wallet, persister| {
			let address_info = wallet.reveal_next_address(KeychainKind::External);
			wallet.persist(persister).map_err(|e| {
				log_error!(self.logger, "Failed to persist wallet: {}", e);
				Error::PersistenceFailed
			})?;
			Ok(address_info.address)
		})
	}

	pub(crate) fn get_new_internal_address(&self) -> Result<bitcoin::Address, Error> {
		self.with_primary_wallet_mut(|wallet, persister| {
			let address_info = wallet.next_unused_address(KeychainKind::Internal);
			wallet.persist(persister).map_err(|e| {
				log_error!(self.logger, "Failed to persist wallet: {}", e);
				Error::PersistenceFailed
			})?;
			Ok(address_info.address)
		})
	}

	// Internal helper for getting witness addresses for Lightning channel operations.
	fn get_witness_address_impl(&self, keychain: KeychainKind) -> Result<bitcoin::Address, Error> {
		let mut wallets = self.wallets.lock().unwrap();
		let mut persisters = self.persisters.lock().unwrap();

		// Helper closure to get address from a wallet
		let get_address = |wallet: &mut PersistedWallet<KVStoreWalletPersister>,
		                   persister: &mut KVStoreWalletPersister,
		                   keychain: KeychainKind|
		 -> Result<bitcoin::Address, Error> {
			let address_info = match keychain {
				KeychainKind::External => wallet.reveal_next_address(keychain),
				KeychainKind::Internal => wallet.next_unused_address(keychain),
			};
			wallet.persist(persister).map_err(|_| Error::PersistenceFailed)?;
			Ok(address_info.address)
		};

		// If primary wallet is already a witness type, use it
		if matches!(self.config.address_type, AddressType::NativeSegwit | AddressType::Taproot) {
			if let (Some(wallet), Some(persister)) = (
				wallets.get_mut(&self.config.address_type),
				persisters.get_mut(&self.config.address_type),
			) {
				return get_address(wallet, persister, keychain);
			}
		}

		// Try NativeSegwit first
		if let (Some(wallet), Some(persister)) = (
			wallets.get_mut(&AddressType::NativeSegwit),
			persisters.get_mut(&AddressType::NativeSegwit),
		) {
			return get_address(wallet, persister, keychain);
		}

		// Fall back to Taproot
		if let (Some(wallet), Some(persister)) =
			(wallets.get_mut(&AddressType::Taproot), persisters.get_mut(&AddressType::Taproot))
		{
			return get_address(wallet, persister, keychain);
		}

		// If no witness wallet is available, this is a configuration error
		log_error!(
			self.logger,
			"No witness wallet (NativeSegwit or Taproot) available for Lightning operations"
		);
		Err(Error::WalletOperationFailed)
	}

	// Get a new witness address for Lightning channel operations.
	pub(crate) fn get_new_witness_address(&self) -> Result<bitcoin::Address, Error> {
		self.get_witness_address_impl(KeychainKind::External)
	}

	// Get a new witness internal address for Lightning channel operations.
	pub(crate) fn get_new_witness_internal_address(&self) -> Result<bitcoin::Address, Error> {
		self.get_witness_address_impl(KeychainKind::Internal)
	}

	pub(crate) fn cancel_tx(&self, tx: &Transaction) -> Result<(), Error> {
		// Cancel transaction in all wallets (in case it exists in multiple)
		let mut wallets = self.wallets.lock().unwrap();
		let mut persisters = self.persisters.lock().unwrap();

		for (address_type, wallet) in wallets.iter_mut() {
			wallet.cancel_tx(tx);
			if let Some(persister) = persisters.get_mut(address_type) {
				wallet.persist(persister).map_err(|e| {
					log_error!(self.logger, "Failed to persist wallet {:?}: {}", address_type, e);
					Error::PersistenceFailed
				})?;
			}
		}

		Ok(())
	}

	pub(crate) fn get_balances(
		&self, total_anchor_channels_reserve_sats: u64,
	) -> Result<(u64, u64), Error> {
		let balance = self.get_aggregate_balance();
		self.get_balances_inner(balance, total_anchor_channels_reserve_sats)
	}

	fn get_balances_inner(
		&self, balance: Balance, total_anchor_channels_reserve_sats: u64,
	) -> Result<(u64, u64), Error> {
		// Calculate trusted_spendable manually to avoid Amount subtraction underflow panic
		// trusted_spendable = confirmed + trusted_pending - untrusted_pending
		// We use saturating operations on satoshis to avoid panics
		let confirmed_sats = balance.confirmed.to_sat();
		let trusted_pending_sats = balance.trusted_pending.to_sat();
		let untrusted_pending_sats = balance.untrusted_pending.to_sat();
		let trusted_spendable_sats =
			(confirmed_sats + trusted_pending_sats).saturating_sub(untrusted_pending_sats);

		let spendable_base = if self.config.include_untrusted_pending_in_spendable {
			trusted_spendable_sats + untrusted_pending_sats
		} else {
			trusted_spendable_sats
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

	// Get the balance for a specific address type.
	// Returns (total_sats, spendable_sats) for the specified wallet.
	pub(crate) fn get_balance_for_address_type(
		&self, address_type: AddressType,
	) -> Result<(u64, u64), Error> {
		let wallets = self.wallets.lock().unwrap();
		let wallet = wallets.get(&address_type).ok_or_else(|| {
			log_error!(self.logger, "Wallet not found for address type {:?}", address_type);
			Error::WalletOperationFailed
		})?;

		let balance = wallet.balance();
		drop(wallets);

		let confirmed_sats = balance.confirmed.to_sat();
		let trusted_pending_sats = balance.trusted_pending.to_sat();
		let untrusted_pending_sats = balance.untrusted_pending.to_sat();
		let trusted_spendable_sats =
			(confirmed_sats + trusted_pending_sats).saturating_sub(untrusted_pending_sats);

		let spendable = if self.config.include_untrusted_pending_in_spendable {
			trusted_spendable_sats + untrusted_pending_sats
		} else {
			trusted_spendable_sats
		};

		Ok((balance.total().to_sat(), spendable))
	}

	// Get all loaded address types (primary + monitored).
	pub(crate) fn get_loaded_address_types(&self) -> Vec<AddressType> {
		let wallets = self.wallets.lock().unwrap();
		wallets.keys().copied().collect()
	}

	// Get transaction details including inputs, outputs, and net amount.
	// Returns None if the transaction is not found in the wallet.
	pub(crate) fn get_tx_details(&self, txid: &Txid) -> Option<(i64, Vec<TxInput>, Vec<TxOutput>)> {
		// Check all wallets for the transaction
		let wallets = self.wallets.lock().unwrap();

		// First, find the transaction in any wallet to get the transaction data
		let mut tx_clone_opt = None;
		for wallet in wallets.values() {
			if let Some(tx_node) = wallet.get_tx(*txid) {
				tx_clone_opt = Some((*tx_node.tx_node.tx).clone());
				break;
			}
		}

		let tx = tx_clone_opt?;

		// Aggregate sent and received amounts across ALL wallets that have this transaction
		// This is necessary because a transaction can use UTXOs from multiple wallets
		let mut total_sent = 0u64;
		let mut total_received = 0u64;
		for wallet in wallets.values() {
			if wallet.get_tx(*txid).is_some() {
				let (sent, received) = wallet.sent_and_received(&tx);
				total_sent += sent.to_sat();
				total_received += received.to_sat();
			}
		}

		let net_amount = total_received as i64 - total_sent as i64;

		let inputs: Vec<TxInput> =
			tx.input.iter().map(|tx_input| TxInput::from_tx_input(tx_input)).collect();

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
		// Collect UTXOs from all wallets
		let wallets = self.wallets.lock().unwrap();
		let mut all_utxos = Vec::new();
		for wallet in wallets.values() {
			let wallet_utxos: Vec<LocalOutput> = wallet.list_unspent().collect();
			all_utxos.extend(wallet_utxos);
		}
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
		algorithm: CoinSelectionAlgorithm, drain_script: &Script,
		channel_manager: Option<&ChannelManager>,
	) -> Result<Vec<OutPoint>, Error> {
		// First, filter out any funding transactions for safety (if channel_manager is provided)
		let safe_utxos: Vec<LocalOutput> = if let Some(channel_manager) = channel_manager {
			available_utxos
				.into_iter()
				.filter(|utxo| {
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
				.collect()
		} else {
			// No channel_manager provided, skip filtering (e.g., for funding transactions)
			available_utxos
		};

		if safe_utxos.is_empty() {
			log_error!(
				self.logger,
				"No spendable UTXOs available after filtering funding transactions"
			);
			return Err(Error::NoSpendableOutputs);
		}

		// Use the improved weight calculation from the second implementation
		// Use primary wallet for weight calculation (all wallets should have similar weight)
		let wallets = self.wallets.lock().unwrap();
		let primary_wallet = wallets.get(&self.config.address_type).unwrap();
		let weighted_utxos: Vec<WeightedUtxo> = safe_utxos
			.iter()
			.map(|utxo| {
				// Use BDK's descriptor to calculate satisfaction weight
				let descriptor = primary_wallet.public_descriptor(utxo.keychain);
				let satisfaction_weight = descriptor.max_weight_to_satisfy().unwrap_or_else(|_| {
					// Fallback to manual calculation if BDK method fails
					log_debug!(
						self.logger,
						"Failed to calculate descriptor weight, using fallback for UTXO {:?}",
						utxo.outpoint
					);
					match utxo.txout.script_pubkey.witness_version() {
						Some(WitnessVersion::V0) => {
							// P2WPKH input weight calculation:
							// Non-witness data: 32 (txid) + 4 (vout) + 1 (script_sig length) + 4 (sequence) = 41 bytes
							// Witness data: 1 (item count) + 1 (sig length) + 72 (sig) + 1 (pubkey length) + 33 (pubkey) = 108 bytes
							// Total weight = 41 * 4 + 108 = 272 WU
							Weight::from_wu(272)
						},
						Some(WitnessVersion::V1) => {
							// P2TR key-path spend weight calculation:
							// Non-witness data: 32 + 4 + 1 + 4 = 41 bytes
							// Witness data: 1 (item count) + 1 (sig length) + 64 (schnorr sig) = 66 bytes
							// Total weight = 41 * 4 + 66 = 230 WU
							Weight::from_wu(230)
						},
						None => {
							// Non-witness script (P2PKH or P2SH)
							// Check if it's P2SH-wrapped SegWit (nested segwit)
							// P2SH scripts start with OP_HASH160 (0xa9) followed by 20-byte hash and OP_EQUAL (0x87)
							let script_bytes = utxo.txout.script_pubkey.as_bytes();
							if script_bytes.len() == 23
								&& script_bytes[0] == 0xa9
								&& script_bytes[21] == 0x87
							{
								// P2SH-wrapped P2WPKH (nested segwit):
								// Non-witness data: 32 + 4 + 23 (redeem script) + 4 = 63 bytes
								// Witness data: 1 + 1 + 72 + 1 + 33 = 108 bytes
								// Total weight = 63 * 4 + 108 = 360 WU
								Weight::from_wu(360)
							} else {
								// P2PKH (legacy):
								// Non-witness data: 32 + 4 + 107 (script_sig: 1 + 72 + 1 + 33) + 4 = 147 bytes
								// No witness data
								// Total weight = 147 * 4 = 588 WU
								Weight::from_wu(588)
							}
						},
						_ => {
							// Unknown witness version (future witness versions)
							log_warn!(
								self.logger,
								"Unknown witness version for UTXO {:?}, using conservative weight",
								utxo.outpoint
							);
							Weight::from_wu(272)
						},
					}
				});

				WeightedUtxo { satisfaction_weight, utxo: bdk_wallet::Utxo::Local(utxo.clone()) }
			})
			.collect();

		let target = Amount::from_sat(target_amount);
		let mut rng = OsRng;

		// Run coin selection based on the algorithm
		let result =
			match algorithm {
				CoinSelectionAlgorithm::BranchAndBound => {
					BranchAndBoundCoinSelection::<SingleRandomDraw>::default().coin_select(
						vec![], // required UTXOs
						weighted_utxos,
						fee_rate,
						target,
						drain_script,
						&mut rng,
					)
				},
				CoinSelectionAlgorithm::LargestFirst => LargestFirstCoinSelection::default()
					.coin_select(vec![], weighted_utxos, fee_rate, target, drain_script, &mut rng),
				CoinSelectionAlgorithm::OldestFirst => OldestFirstCoinSelection::default()
					.coin_select(vec![], weighted_utxos, fee_rate, target, drain_script, &mut rng),
				CoinSelectionAlgorithm::SingleRandomDraw => SingleRandomDraw::default()
					.coin_select(vec![], weighted_utxos, fee_rate, target, drain_script, &mut rng),
			}
			.map_err(|e| {
				log_error!(self.logger, "Coin selection failed: {}", e);
				Error::CoinSelectionFailed
			})?;

		// Validate change amount is not dust
		if let Excess::Change { amount, .. } = result.excess {
			if amount.to_sat() > 0 && amount.to_sat() < DUST_LIMIT_SATS {
				return Err(Error::CoinSelectionFailed);
			}
		}

		// Extract the selected outputs
		let selected_outputs: Vec<LocalOutput> = result
			.selected
			.into_iter()
			.filter_map(|utxo| match utxo {
				bdk_wallet::Utxo::Local(local) => Some(local),
				_ => None,
			})
			.collect();

		log_debug!(
			self.logger,
			"Selected {} UTXOs using {:?} algorithm for target {} sats",
			selected_outputs.len(),
			algorithm,
			target_amount,
		);
		Ok(selected_outputs.into_iter().map(|u| u.outpoint).collect())
	}

	// Helper that builds a transaction PSBT with shared logic for send_to_address
	// and calculate_transaction_fee.
	fn build_transaction_psbt(
		&self, address: &Address, send_amount: OnchainSendAmount, fee_rate: FeeRate,
		utxos_to_spend: Option<Vec<OutPoint>>, channel_manager: &ChannelManager,
	) -> Result<Psbt, Error> {
		// Validate and check UTXOs if provided - do this BEFORE acquiring mutable lock
		let wallet_utxos: Vec<_> = if utxos_to_spend.is_some() {
			let wallets_read = self.wallets.lock().unwrap();
			wallets_read.values().flat_map(|w| w.list_unspent()).collect()
		} else {
			Vec::new()
		};

		// Prepare the tx_builder. We properly check the reserve requirements (again) further down.

		// For AllRetainingReserve, we need balance and UTXOs from all wallets - collect BEFORE getting mutable wallet
		let (_balance_for_all, change_address_info_for_all, all_utxos_for_tmp) = if matches!(send_amount, OnchainSendAmount::AllRetainingReserve { cur_anchor_reserve_sats: reserve } if reserve > DUST_LIMIT_SATS)
		{
			let wallets_read = self.wallets.lock().unwrap();
			let total_balance = Self::get_aggregate_balance_from_wallets(&wallets_read);
			let change_address_info = wallets_read
				.get(&self.config.address_type)
				.unwrap()
				.peek_address(KeychainKind::Internal, 0);
			let all_unspent_utxos: Vec<_> =
				wallets_read.values().flat_map(|w| w.list_unspent()).collect();
			(Some(total_balance), Some(change_address_info), Some(all_unspent_utxos))
		} else {
			(None, None, None)
		};

		// For AllDrainingReserve, collect UTXOs from all wallets
		let all_utxos_for_drain: Option<Vec<_>> = if matches!(
			send_amount,
			OnchainSendAmount::AllDrainingReserve | OnchainSendAmount::AllRetainingReserve { .. }
		) {
			let wallets_read = self.wallets.lock().unwrap();
			Some(wallets_read.values().flat_map(|w| w.list_unspent()).collect())
		} else {
			None
		};

		// Validate and check UTXOs if provided - do this BEFORE acquiring mutable lock
		if let Some(ref outpoints) = utxos_to_spend {
			let wallet_utxo_set: std::collections::HashSet<_> =
				wallet_utxos.iter().map(|u| u.outpoint).collect();

			// Validate all requested UTXOs exist and are safe to spend
			for outpoint in outpoints {
				if !wallet_utxo_set.contains(outpoint) {
					log_error!(self.logger, "UTXO {:?} not found in wallet", outpoint);
					return Err(Error::WalletOperationFailed);
				}

				// Check if this UTXO's transaction is a channel funding transaction
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
			let selected_value: u64 = wallet_utxos
				.iter()
				.filter(|u| outpoints.contains(&u.outpoint))
				.map(|u| u.txout.value.to_sat())
				.sum();

			// Note: We don't do a pre-check here for insufficient funds when utxos_to_spend is provided.
			// The actual transaction building and reserve check (later in this function) will correctly
			// validate if there are sufficient funds, including proper fee calculation that accounts
			// for the actual transaction weight and any foreign UTXOs.

			log_debug!(
				self.logger,
				"Using {} manually selected UTXOs with total value: {}sats",
				outpoints.len(),
				selected_value
			);
		}

		// Use primary wallet for building transactions
		// We need to handle utxos_to_spend before building, so let's collect that info first
		let utxo_info_for_manual = if let Some(ref outpoints) = utxos_to_spend {
			log_info!(
				self.logger,
				"build_transaction_psbt: Processing {} manually selected UTXOs",
				outpoints.len()
			);
			let wallets_read = self.wallets.lock().unwrap();
			let utxo_infos = self.prepare_outpoints_for_psbt(outpoints, &wallets_read)?;
			Some(utxo_infos)
		} else {
			None
		};

		// Now build the transaction
		// We need to handle different cases - some need to collect data before acquiring mutable lock
		// For cases where utxos_to_spend is provided, we finish the transaction inside the match arm
		// to avoid lifetime issues with the wallets lock
		let psbt = if utxos_to_spend.is_some() {
			// Handle case where utxos_to_spend is provided - finish transaction inside match arm
			let mut wallets = self.wallets.lock().unwrap();
			let wallet = wallets.get_mut(&self.config.address_type).ok_or_else(|| {
				log_error!(self.logger, "Primary wallet not found");
				Error::WalletOperationFailed
			})?;
			let mut tx_builder = wallet.build_tx();

			// Configure transaction based on send_amount type
			match send_amount {
				OnchainSendAmount::ExactRetainingReserve { amount_sats, .. } => {
					let amount = Amount::from_sat(amount_sats);
					tx_builder.add_recipient(address.script_pubkey(), amount).fee_rate(fee_rate);
				},
				OnchainSendAmount::AllDrainingReserve => {
					// For drain operations with utxos_to_spend, drain to the address
					tx_builder.drain_to(address.script_pubkey()).fee_rate(fee_rate);
				},
				OnchainSendAmount::AllRetainingReserve { .. } => {
					// This case shouldn't typically use utxos_to_spend, but handle it if needed
					// We'll drain to the address (reserve will be handled by the reserve check later)
					tx_builder.drain_to(address.script_pubkey()).fee_rate(fee_rate);
				},
			}

			// Add specified UTXOs using helper
			if let Some(ref utxo_infos) = utxo_info_for_manual {
				self.add_utxos_to_tx_builder(&mut tx_builder, utxo_infos)?;
				tx_builder.manually_selected_only();
			}

			tx_builder.finish().map_err(|e| {
				log_error!(self.logger, "Failed to create transaction: {}", e);
				e
			})?
		} else {
			// Handle cases where utxos_to_spend is not provided - finish transaction in each match arm
			match send_amount {
				OnchainSendAmount::ExactRetainingReserve { amount_sats, .. } => {
					// For ExactRetainingReserve without utxos_to_spend, manually select UTXOs from all wallets
					// using our coin selection algorithm, then add them explicitly to the transaction builder

					// Get all spendable UTXOs from all wallets
					let all_available_utxos = self.get_spendable_utxos(channel_manager)?;

					// Get drain script (change address) from primary wallet
					// Need to acquire lock for this, but we'll drop it before building tx_builder
					let drain_script = {
						let wallets_read = self.wallets.lock().unwrap();
						let primary_wallet = wallets_read.get(&self.config.address_type).unwrap();
						primary_wallet
							.peek_address(KeychainKind::Internal, 0)
							.address
							.script_pubkey()
					};

					// Select UTXOs from all wallets using coin selection algorithm
					let selected_outpoints = self.select_utxos_with_algorithm(
						amount_sats,
						all_available_utxos,
						fee_rate,
						CoinSelectionAlgorithm::LargestFirst, // Use LargestFirst as default
						&drain_script,
						Some(channel_manager),
					)?;

					// Prepare UTXO info for selected UTXOs using helper
					let wallets_read = self.wallets.lock().unwrap();
					let utxo_infos =
						self.prepare_outpoints_for_psbt(&selected_outpoints, &wallets_read)?;
					drop(wallets_read);

					// Re-acquire mutable lock for building transaction
					let mut wallets = self.wallets.lock().unwrap();
					let wallet = wallets.get_mut(&self.config.address_type).ok_or_else(|| {
						log_error!(self.logger, "Primary wallet not found");
						Error::WalletOperationFailed
					})?;

					// Build transaction with selected UTXOs from all wallets
					let mut tx_builder = wallet.build_tx();
					let amount = Amount::from_sat(amount_sats);
					tx_builder.add_recipient(address.script_pubkey(), amount).fee_rate(fee_rate);

					// Add selected UTXOs using helper
					self.add_utxos_to_tx_builder(&mut tx_builder, &utxo_infos)?;
					tx_builder.manually_selected_only();

					// Finish transaction while lock is held
					tx_builder.finish().map_err(|e| {
						log_error!(self.logger, "Failed to create transaction: {}", e);
						e
					})?
				},
				OnchainSendAmount::AllRetainingReserve { cur_anchor_reserve_sats }
					if cur_anchor_reserve_sats > DUST_LIMIT_SATS =>
				{
					let change_address_info = change_address_info_for_all.unwrap();
					// Calculate spendable amount from actual UTXOs that will be used, not from balance
					// This ensures the displayed max matches what will actually be sent
					let total_utxo_value = if let Some(ref all_utxos) = all_utxos_for_tmp {
						all_utxos.iter().map(|utxo| utxo.txout.value.to_sat()).sum::<u64>()
					} else {
						0
					};
					let tmp_tx = {
						// Prepare UTXO info using helper
						let wallets_read = self.wallets.lock().unwrap();
						let utxo_infos = if let Some(ref all_utxos) = all_utxos_for_tmp {
							self.prepare_utxos_for_psbt(all_utxos, &wallets_read)?
						} else {
							Vec::new()
						};
						drop(wallets_read);

						// Acquire mutable lock for building transaction
						let mut wallets = self.wallets.lock().unwrap();
						let wallet =
							wallets.get_mut(&self.config.address_type).ok_or_else(|| {
								log_error!(self.logger, "Primary wallet not found");
								Error::WalletOperationFailed
							})?;

						let mut tmp_tx_builder = wallet.build_tx();
						// Add UTXOs using helper (ignoring errors for temp tx)
						for info in &utxo_infos {
							if info.is_primary {
								if let Err(e) = tmp_tx_builder.add_utxo(info.outpoint) {
									log_warn!(
										self.logger,
										"Failed to add UTXO {:?} to temp tx: {}",
										info.outpoint,
										e
									);
								}
							} else {
								if let Err(e) = tmp_tx_builder.add_foreign_utxo(
									info.outpoint,
									info.psbt_input.clone(),
									info.weight,
								) {
									log_warn!(
										self.logger,
										"Failed to add foreign UTXO {:?} to temp tx: {}",
										info.outpoint,
										e
									);
								}
							}
						}
						tmp_tx_builder
							.drain_to(address.script_pubkey())
							.add_recipient(
								change_address_info.address.script_pubkey(),
								Amount::from_sat(cur_anchor_reserve_sats),
							)
							.fee_rate(fee_rate);

						// Add manual UTXOs to temporary transaction if specified
						if let Some(ref outpoints) = utxos_to_spend {
							for outpoint in outpoints {
								tmp_tx_builder.add_utxo(*outpoint).map_err(|e| {
									log_error!(
										self.logger,
										"Failed to add UTXO {:?} to temp tx: {}",
										outpoint,
										e
									);
									Error::OnchainTxCreationFailed
								})?;
							}
							tmp_tx_builder.manually_selected_only();
						}

						match tmp_tx_builder.finish() {
							Ok(psbt) => psbt.unsigned_tx,
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

					// Get primary wallet for cancellation
					// Calculate fee manually from transaction inputs and outputs
					// This is necessary because tmp_tx may include foreign UTXOs
					let estimated_tx_fee = {
						let wallets_read = self.wallets.lock().unwrap();
						let mut total_input_value = 0u64;
						for txin in &tmp_tx.input {
							// Try to find the UTXO in any wallet
							let mut found = false;
							for wallet in wallets_read.values() {
								if let Some(local_utxo) = wallet.get_utxo(txin.previous_output) {
									total_input_value += local_utxo.txout.value.to_sat();
									found = true;
									break;
								}
							}
							if !found {
								log_error!(
									self.logger,
									"Could not find TxOut for input {:?} in temporary transaction",
									txin.previous_output
								);
								return Err(Error::OnchainTxCreationFailed);
							}
						}
						let total_output_value: u64 =
							tmp_tx.output.iter().map(|txout| txout.value.to_sat()).sum();
						total_input_value.saturating_sub(total_output_value)
					};

					// Cancel the transaction to free up any used change addresses
					let mut wallets = self.wallets.lock().unwrap();
					let wallet = wallets.get_mut(&self.config.address_type).ok_or_else(|| {
						log_error!(self.logger, "Primary wallet not found");
						Error::WalletOperationFailed
					})?;
					let estimated_tx_fee = Amount::from_sat(estimated_tx_fee);

					// 'cancel' the transaction to free up any used change addresses
					wallet.cancel_tx(&tmp_tx);

					// Calculate spendable amount from actual UTXOs: total UTXO value - anchor reserve - fee
					// This ensures the displayed max matches what will actually be sent
					let estimated_spendable_amount = Amount::from_sat(
						total_utxo_value
							.saturating_sub(cur_anchor_reserve_sats)
							.saturating_sub(estimated_tx_fee.to_sat()),
					);

					if estimated_spendable_amount < Amount::from_sat(DUST_LIMIT_SATS) {
						log_error!(self.logger,
						"Unable to send payment without infringing on Anchor reserves. Total UTXO value: {}sats, anchor reserve: {}sats, estimated fee required: {}sats.",
						total_utxo_value,
						cur_anchor_reserve_sats,
						estimated_tx_fee,
					);
						return Err(Error::InsufficientFunds);
					}

					let mut tx_builder = wallet.build_tx();
					tx_builder
						.add_recipient(address.script_pubkey(), estimated_spendable_amount)
						.fee_absolute(estimated_tx_fee);

					// Finish transaction while lock is held
					tx_builder.finish().map_err(|e| {
						log_error!(self.logger, "Failed to create transaction: {}", e);
						e
					})?
				},
				OnchainSendAmount::AllDrainingReserve
				| OnchainSendAmount::AllRetainingReserve { cur_anchor_reserve_sats: _ } => {
					// Prepare UTXO info using helper
					let wallets_read = self.wallets.lock().unwrap();
					let utxo_infos = if let Some(ref all_utxos) = all_utxos_for_drain {
						self.prepare_utxos_for_psbt(all_utxos, &wallets_read)?
					} else {
						Vec::new()
					};
					drop(wallets_read);

					// Re-acquire mutable lock for building transaction
					let mut wallets = self.wallets.lock().unwrap();
					let wallet = wallets.get_mut(&self.config.address_type).ok_or_else(|| {
						log_error!(self.logger, "Primary wallet not found");
						Error::WalletOperationFailed
					})?;

					// Build transaction with all UTXOs from all wallets
					let mut tx_builder = wallet.build_tx();
					// Add UTXOs (ignoring errors for drain operations)
					for info in &utxo_infos {
						if info.is_primary {
							if let Err(e) = tx_builder.add_utxo(info.outpoint) {
								log_warn!(
									self.logger,
									"Failed to add UTXO {:?} from primary wallet: {}",
									info.outpoint,
									e
								);
							}
						} else {
							if let Err(e) = tx_builder.add_foreign_utxo(
								info.outpoint,
								info.psbt_input.clone(),
								info.weight,
							) {
								log_warn!(
									self.logger,
									"Failed to add foreign UTXO {:?} from another wallet: {}",
									info.outpoint,
									e
								);
							}
						}
					}
					tx_builder.drain_to(address.script_pubkey()).fee_rate(fee_rate);

					// Finish transaction while lock is held
					tx_builder.finish().map_err(|e| {
						log_error!(self.logger, "Failed to create transaction: {}", e);
						e
					})?
				},
			}
		};

		// Check the reserve requirements (again) and return an error if they aren't met.
		match send_amount {
			OnchainSendAmount::ExactRetainingReserve { amount_sats, cur_anchor_reserve_sats } => {
				// Get balance and fee using helpers
				let (balance, tx_fee_sats) = {
					let wallets_read = self.wallets.lock().unwrap();
					let balance = Self::get_aggregate_balance_from_wallets(&wallets_read);
					let tx_fee_sats = self.calculate_fee_from_psbt(&psbt, &wallets_read)?;
					(balance, tx_fee_sats)
				};
				let spendable_amount_sats = self
					.get_balances_inner(balance, cur_anchor_reserve_sats)
					.map(|(_, s)| s)
					.unwrap_or(0);
				if spendable_amount_sats < amount_sats.saturating_add(tx_fee_sats) {
					log_error!(self.logger,
						"Unable to send payment due to insufficient funds. Available: {}sats, Required: {}sats + {}sats fee",
						spendable_amount_sats,
						amount_sats,
						tx_fee_sats,
					);
					return Err(Error::InsufficientFunds);
				}
			},
			OnchainSendAmount::AllRetainingReserve { cur_anchor_reserve_sats } => {
				// Get balance from all wallets using helper
				let balance = self.get_aggregate_balance();
				let spendable_amount_sats = self
					.get_balances_inner(balance, cur_anchor_reserve_sats)
					.map(|(_, s)| s)
					.unwrap_or(0);
				// Get primary wallet for sent_and_received calculation
				let wallets_read = self.wallets.lock().unwrap();
				let primary_wallet = wallets_read.get(&self.config.address_type).unwrap();
				let (sent, received) = primary_wallet.sent_and_received(&psbt.unsigned_tx);
				drop(wallets_read);
				// Use saturating_sub on satoshis to avoid panic if received > sent (shouldn't happen for drain, but be safe)
				let drain_amount =
					Amount::from_sat(sent.to_sat().saturating_sub(received.to_sat()));
				if spendable_amount_sats < drain_amount.to_sat() {
					log_error!(self.logger,
						"Unable to send payment due to insufficient funds. Available: {}sats, Required: {}",
						spendable_amount_sats,
						drain_amount,
					);
					return Err(Error::InsufficientFunds);
				}
			},
			_ => {},
		}

		// Return just the PSBT - callers will need to lock wallets themselves when signing
		Ok(psbt)
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

		let psbt = self.build_transaction_psbt(
			address,
			send_amount,
			fee_rate,
			utxos_to_spend,
			channel_manager,
		)?;

		// Calculate the final fee using helper
		let wallets_read = self.wallets.lock().unwrap();
		let calculated_fee = self.calculate_fee_from_psbt(&psbt, &wallets_read)?;
		drop(wallets_read);

		Ok(calculated_fee)
	}

	#[allow(deprecated)]
	pub(crate) fn send_to_address(
		&self, address: &Address, send_amount: OnchainSendAmount, fee_rate: Option<FeeRate>,
		utxos_to_spend: Option<Vec<OutPoint>>, channel_manager: &ChannelManager,
	) -> Result<Txid, Error> {
		self.parse_and_validate_address(&address)?;

		// Use the set fee_rate or default to fee estimation.
		let confirmation_target = ConfirmationTarget::OnchainPayment;
		let fee_rate =
			fee_rate.unwrap_or_else(|| self.fee_estimator.estimate_fee_rate(confirmation_target));

		let mut psbt = self.build_transaction_psbt(
			address,
			send_amount,
			fee_rate,
			utxos_to_spend,
			channel_manager,
		)?;

		// Sign the transaction - each wallet signs its own UTXOs
		// This is necessary because each wallet has a different descriptor (Bip44, Bip49, Bip84, Bip86)
		// and BDK's sign() method only signs inputs that match the wallet's descriptor
		let mut wallets = self.wallets.lock().unwrap();
		let mut persisters = self.persisters.lock().unwrap();
		// Identify which wallet owns each input in the PSBT
		let mut wallet_inputs: HashMap<AddressType, Vec<usize>> = HashMap::new();
		let mut unsigned_inputs = Vec::new();
		for (i, txin) in psbt.unsigned_tx.input.iter().enumerate() {
			// Find which wallet owns this UTXO
			let mut found = false;
			for (addr_type, wallet) in wallets.iter() {
				if wallet.get_utxo(txin.previous_output).is_some() {
					wallet_inputs.entry(*addr_type).or_insert_with(Vec::new).push(i);
					found = true;
					break;
				}
			}
			if !found {
				unsigned_inputs.push(i);
				log_warn!(
					self.logger,
					"Input {} (UTXO {:?}) not found in any wallet",
					i,
					txin.previous_output
				);
			}
		}

		// If we have inputs that aren't in any wallet, that's an error
		if !unsigned_inputs.is_empty() {
			log_error!(
				self.logger,
				"Some inputs are not owned by any wallet: {:?}",
				unsigned_inputs
			);
			return Err(Error::OnchainTxSigningFailed);
		}

		// If no inputs found at all, that's also an error
		if wallet_inputs.is_empty() {
			log_error!(self.logger, "No inputs found in any wallet for transaction");
			return Err(Error::OnchainTxSigningFailed);
		}

		// Sign with each wallet that has inputs in the transaction
		for (addr_type, input_indices) in wallet_inputs {
			let wallet = wallets.get_mut(&addr_type).ok_or_else(|| {
				log_error!(self.logger, "Wallet not found for address type {:?}", addr_type);
				Error::WalletOperationFailed
			})?;
			let persister = persisters.get_mut(&addr_type).ok_or_else(|| {
				log_error!(self.logger, "Persister not found for address type {:?}", addr_type);
				Error::WalletOperationFailed
			})?;

			// Create sign options for this wallet
			let mut sign_options = SignOptions::default();
			sign_options.trust_witness_utxo = true;

			// Sign inputs owned by this wallet
			log_debug!(
				self.logger,
				"Attempting to sign {} inputs for address type {:?}",
				input_indices.len(),
				addr_type
			);
			match wallet.sign(&mut psbt, sign_options) {
				Ok(finalized) => {
					// Note: finalized might be false if there are other unsigned inputs, which is expected
					// We'll verify all inputs are signed when we call extract_tx() below
					log_debug!(self.logger, "Signing completed for address type {:?} (finalized={}, expected {} inputs)", 
						addr_type, finalized, input_indices.len());
				},
				Err(err) => {
					log_error!(
						self.logger,
						"Failed to sign inputs for address type {:?}: {}",
						addr_type,
						err
					);
					return Err(Error::OnchainTxSigningFailed);
				},
			}

			// Persist the wallet after signing
			wallet.persist(persister).map_err(|e| {
				log_error!(self.logger, "Failed to persist wallet for {:?}: {}", addr_type, e);
				Error::PersistenceFailed
			})?;
		}

		// Extract the transaction
		// Note: psbt.extract_tx() will fail if not all inputs are signed, which is what we want
		let tx = psbt.extract_tx().map_err(|e| {
			log_error!(self.logger, "Failed to extract transaction: {}", e);
			Error::OnchainTxSigningFailed
		})?;

		self.broadcaster.broadcast_transactions(&[&tx]);

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

		Ok(txid)
	}

	pub(crate) fn select_confirmed_utxos(
		&self, must_spend: Vec<Input>, must_pay_to: &[TxOut], fee_rate: FeeRate,
	) -> Result<Vec<FundingTxInput>, ()> {
		// Note: Lightning requires the funding OUTPUT to be a witness script (P2WSH).
		// Funding INPUTS must be spendable by LDK and are restricted to witness types (P2WPKH/P2TR).
		let mut wallets = self.wallets.lock().unwrap();
		let funding_wallet_type =
			if matches!(self.config.address_type, AddressType::NativeSegwit | AddressType::Taproot)
			{
				self.config.address_type
			} else if wallets.contains_key(&AddressType::NativeSegwit) {
				AddressType::NativeSegwit
			} else if wallets.contains_key(&AddressType::Taproot) {
				AddressType::Taproot
			} else {
				log_error!(
				self.logger,
				"No witness wallet available for channel funding. Primary: {:?}, Monitored: {:?}",
				self.config.address_type,
				self.config.address_types_to_monitor
			);
				return Err(());
			};
		let wallet = wallets.get_mut(&funding_wallet_type).ok_or(())?;

		let mut tx_builder = wallet.build_tx();
		// Use witness wallet UTXOs to ensure compatibility with FundingTxInput constructors.

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

		// Keep wallets locked for tx_details lookup
		let result: Result<Vec<_>, ()> = tx_builder
			.finish()
			.map_err(|e| {
				log_error!(self.logger, "Failed to select confirmed UTXOs: {}", e);
			})?
			.unsigned_tx
			.input
			.iter()
			.filter(|txin| must_spend.iter().all(|input| input.outpoint != txin.previous_output))
			.map(|txin| {
				let prevtx = wallets
					.values()
					.find_map(|w| w.tx_details(txin.previous_output.txid))
					.map(|tx_details| tx_details.tx.deref().clone())
					.ok_or_else(|| {
						log_error!(
							self.logger,
							"Failed to find previous transaction for {:?}",
							txin.previous_output
						);
					})?;

				let vout = txin.previous_output.vout;
				let script_pubkey = prevtx
					.output
					.get(vout as usize)
					.map(|output| &output.script_pubkey)
					.ok_or_else(|| {
						log_error!(
							self.logger,
							"Missing output {} for previous transaction {:?}",
							vout,
							txin.previous_output.txid
						);
					})?;

				if script_pubkey.is_p2wpkh() {
					FundingTxInput::new_p2wpkh(prevtx, vout).map_err(|_| {
						log_error!(
							self.logger,
							"Failed to create P2WPKH funding input for {:?}",
							txin.previous_output
						);
					})
				} else if script_pubkey.is_p2tr() {
					FundingTxInput::new_p2tr_key_spend(prevtx, vout).map_err(|_| {
						log_error!(
							self.logger,
							"Failed to create P2TR funding input for {:?}",
							txin.previous_output
						);
					})
				} else {
					log_error!(
						self.logger,
						"Unsupported funding input script for {:?}: {:?}",
						txin.previous_output,
						script_pubkey
					);
					Err(())
				}
			})
			.collect();
		result
	}

	fn list_confirmed_utxos_inner(&self) -> Result<Vec<Utxo>, ()> {
		// Collect confirmed UTXOs from all wallets
		let wallets = self.wallets.lock().unwrap();
		let mut utxos = Vec::new();
		let mut all_confirmed_txs = Vec::new();
		let mut all_unspent_utxos = Vec::new();

		for wallet in wallets.values() {
			let confirmed_txs: Vec<Txid> = wallet
				.transactions()
				.filter(|t| t.chain_position.is_confirmed())
				.map(|t| t.tx_node.txid)
				.collect();
			all_confirmed_txs.extend(confirmed_txs);
			all_unspent_utxos.extend(wallet.list_unspent());
		}

		let confirmed_txs_set: std::collections::HashSet<_> =
			all_confirmed_txs.into_iter().collect();
		let unspent_confirmed_utxos =
			all_unspent_utxos.into_iter().filter(|u| confirmed_txs_set.contains(&u.outpoint.txid));

		for u in unspent_confirmed_utxos {
			let script_pubkey = u.txout.script_pubkey;
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
				Some(_version) => {
					// Unsupported witness version, skip
				},
				None => {
					// Non-witness UTXO (Legacy), skip for Lightning operations
				},
			}
		}

		Ok(utxos)
	}

	#[allow(deprecated)]
	fn get_change_script_inner(&self) -> Result<ScriptBuf, ()> {
		// Use primary wallet for change addresses
		let mut wallets = self.wallets.lock().unwrap();
		let mut persisters = self.persisters.lock().unwrap();

		let wallet = wallets.get_mut(&self.config.address_type).ok_or(())?;
		let persister = persisters.get_mut(&self.config.address_type).ok_or(())?;

		let address_info = wallet.next_unused_address(KeychainKind::Internal);
		wallet.persist(persister).map_err(|e| {
			log_error!(self.logger, "Failed to persist wallet: {}", e);
			()
		})?;
		Ok(address_info.address.script_pubkey())
	}

	#[allow(deprecated)]
	pub(crate) fn sign_owned_inputs(&self, unsigned_tx: Transaction) -> Result<Transaction, ()> {
		let mut psbt = Psbt::from_unsigned_tx(unsigned_tx).map_err(|e| {
			log_error!(self.logger, "Failed to construct PSBT: {}", e);
		})?;

		// Track which wallet owns each input so we can sign with the correct wallet
		// Each wallet has a different descriptor (Bip44, Bip49, Bip84, Bip86) and can only sign its own inputs
		let mut wallet_inputs: HashMap<AddressType, Vec<usize>> = HashMap::new();

		{
			// First pass: populate PSBT inputs and track which wallet owns each
			let wallets = self.wallets.lock().unwrap();
			for (i, txin) in psbt.unsigned_tx.input.iter().enumerate() {
				let mut found = false;
				for (addr_type, wallet) in wallets.iter() {
					if let Some(utxo) = wallet.get_utxo(txin.previous_output) {
						debug_assert!(!utxo.is_spent);
						psbt.inputs[i] = wallet.get_psbt_input(utxo, None, true).map_err(|e| {
							log_error!(self.logger, "Failed to construct PSBT input: {}", e);
						})?;
						wallet_inputs.entry(*addr_type).or_insert_with(Vec::new).push(i);
						found = true;
						break;
					}
				}
				if !found {
					log_error!(
						self.logger,
						"UTXO {:?} not found in any wallet",
						txin.previous_output
					);
				}
			}
		} // Release read lock

		// Second pass: sign with each wallet that owns inputs
		let mut wallets = self.wallets.lock().unwrap();
		let mut sign_options = SignOptions::default();
		sign_options.trust_witness_utxo = true;

		for (addr_type, _input_indices) in &wallet_inputs {
			if let Some(wallet) = wallets.get_mut(addr_type) {
				match wallet.sign(&mut psbt, sign_options.clone()) {
					Ok(_finalized) => {
						// finalized may be false if there are inputs from other wallets
					},
					Err(e) => {
						log_error!(
							self.logger,
							"Failed to sign owned inputs for wallet {:?}: {}",
							addr_type,
							e
						);
						return Err(());
					},
				}
			}
		}

		match psbt.extract_tx() {
			Ok(tx) => Ok(tx),
			Err(bitcoin::psbt::ExtractTxError::MissingInputValue { tx }) => Ok(tx),
			Err(e) => {
				log_error!(self.logger, "Failed to extract transaction: {}", e);
				Err(())
			},
		}
	}

	#[allow(deprecated)]
	fn sign_psbt_inner(&self, mut psbt: Psbt) -> Result<Transaction, ()> {
		// Use primary wallet for signing
		let mut wallets = self.wallets.lock().unwrap();
		let wallet = wallets.get_mut(&self.config.address_type).ok_or(())?;

		// While BDK populates both `witness_utxo` and `non_witness_utxo` fields, LDK does not. As
		// BDK by default doesn't trust the witness UTXO to account for the Segwit bug, we must
		// disable it here as otherwise we fail to sign.
		let mut sign_options = SignOptions::default();
		sign_options.trust_witness_utxo = true;

		match wallet.sign(&mut psbt, sign_options) {
			Ok(_finalized) => {
				// BDK will fail to finalize for all LDK-provided inputs of the PSBT. Unfortunately
				// we can't check more fine grained if it succeeded for all the other inputs here,
				// so we just ignore the returned `finalized` bool.
			},
			Err(err) => {
				log_error!(self.logger, "Failed to sign transaction: {}", err);
				return Err(());
			},
		}

		let tx = psbt.extract_tx().map_err(|e| {
			log_error!(self.logger, "Failed to extract transaction: {}", e);
			()
		})?;

		Ok(tx)
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
		// Apply block to all wallets
		let mut wallets = self.wallets.lock().unwrap();
		let mut persisters = self.persisters.lock().unwrap();

		// Use primary wallet's checkpoint for reorg detection
		let primary_wallet = wallets.get(&self.config.address_type).unwrap();
		let pre_checkpoint = primary_wallet.latest_checkpoint();
		if pre_checkpoint.height() != height - 1
			|| pre_checkpoint.hash() != block.header.prev_blockhash
		{
			log_debug!(
				self.logger,
				"Detected reorg while applying a connected block to on-chain wallet: new block with hash {} at height {}",
				block.header.block_hash(),
				height
			);
		}

		// Apply block to all wallets
		for (address_type, wallet) in wallets.iter_mut() {
			match wallet.apply_block(block, height) {
				Ok(()) => {
					if let Some(persister) = persisters.get_mut(address_type) {
						if let Err(e) = wallet.persist(persister) {
							log_error!(
								self.logger,
								"Failed to persist wallet {:?}: {}",
								address_type,
								e
							);
							return;
						}
					}
				},
				Err(e) => {
					log_error!(
						self.logger,
						"Failed to apply connected block to wallet {:?}: {}",
						address_type,
						e
					);
					return;
				},
			};
		}

		let mut all_txids = std::collections::HashSet::new();
		for wallet in wallets.values() {
			for wtx in wallet.transactions() {
				all_txids.insert(wtx.tx_node.txid);
			}
		}
		drop(wallets);
		drop(persisters);

		if !all_txids.is_empty() {
			let txids_vec: Vec<Txid> = all_txids.into_iter().collect();
			if let Err(e) = self.update_payment_store_for_txids(txids_vec) {
				log_error!(
					self.logger,
					"Failed to update payment store after block connected: {}",
					e
				);
			}
		}
	}

	fn blocks_disconnected(&self, _fork_point_block: BestBlock) {
		// This is a no-op as we don't have to tell BDK about disconnections. According to the BDK
		// team, it's sufficient in case of a reorg to always connect blocks starting from the last
		// point of disagreement.
	}
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
		// Lightning channels require witness addresses, so use get_new_witness_address
		let address = self.wallet.get_new_witness_address().map_err(|e| {
			log_error!(self.logger, "Failed to retrieve new witness address from wallet: {}", e);
		})?;
		Ok(address.script_pubkey())
	}

	fn get_shutdown_scriptpubkey(&self) -> Result<ShutdownScript, ()> {
		// Lightning channels require witness addresses, so use get_new_witness_address
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
					"Tried to use a non-witness address. This must never happen. Address: {}",
					address
				);
				panic!("Tried to use a non-witness address. This must never happen.");
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
			// Lightning channels require witness addresses for change outputs
			wallet
				.get_new_witness_internal_address()
				.map_err(|e| {
					log_error!(
						logger,
						"Failed to retrieve new witness internal address from wallet: {}",
						e
					);
				})
				.map(|addr| addr.script_pubkey())
				.map_err(|_| ())
		})
	}
}

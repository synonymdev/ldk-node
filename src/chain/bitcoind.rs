// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::prelude::BASE64_STANDARD;
use base64::Engine;
use bitcoin::{BlockHash, FeeRate, Network, Transaction, Txid};
use lightning::chain::chaininterface::ConfirmationTarget as LdkConfirmationTarget;
use lightning::chain::{BestBlock, Listen};
use lightning::util::ser::Writeable;
use lightning_block_sync::gossip::UtxoSource;
use lightning_block_sync::http::{HttpEndpoint, JsonResponse};
use lightning_block_sync::init::{synchronize_listeners, validate_best_block_header};
use lightning_block_sync::poll::ValidatedBlockHeader;
use lightning_block_sync::rest::RestClient;
use lightning_block_sync::rpc::{RpcClient, RpcError};
use lightning_block_sync::{
	AsyncBlockSourceResult, BlockData, BlockHeaderData, BlockSource, Cache,
};
use serde::Serialize;

use super::{periodically_archive_fully_resolved_monitors, WalletSyncStatus};
use crate::config::{
	BitcoindRestClientConfig, Config, OnchainWalletAccount, FEE_RATE_CACHE_UPDATE_TIMEOUT_SECS,
	TX_BROADCAST_TIMEOUT_SECS,
};
use crate::fee_estimator::{
	apply_post_estimation_adjustments, get_all_conf_targets, get_num_block_defaults_for_target,
	ConfirmationTarget, OnchainFeeEstimator,
};
use crate::io::utils::write_node_metrics;
use crate::logger::{log_bytes, log_error, log_info, log_trace, LdkLogger, Logger};
use crate::types::{ChainMonitor, ChannelManager, DynStore, Sweeper, Wallet};
use crate::{Error, NodeMetrics};

const CHAIN_POLLING_INTERVAL_SECS: u64 = 2;
const CHAIN_POLLING_TIMEOUT_SECS: u64 = 10;

enum ListenerSyncOutcome {
	Complete(u64),
	AccountSetChanged,
}

pub(super) struct BitcoindChainSource {
	api_client: Arc<BitcoindClient>,
	header_cache: tokio::sync::Mutex<BoundedHeaderCache>,
	wallet_polling_status: Mutex<WalletSyncStatus>,
	last_mempool_account_generation: AtomicU64,
	fee_estimator: Arc<OnchainFeeEstimator>,
	pub(super) kv_store: Arc<DynStore>,
	pub(super) config: Arc<Config>,
	logger: Arc<Logger>,
	pub(super) node_metrics: Arc<RwLock<NodeMetrics>>,
}

impl BitcoindChainSource {
	pub(crate) fn new_rpc(
		rpc_host: String, rpc_port: u16, rpc_user: String, rpc_password: String,
		fee_estimator: Arc<OnchainFeeEstimator>, kv_store: Arc<DynStore>, config: Arc<Config>,
		logger: Arc<Logger>, node_metrics: Arc<RwLock<NodeMetrics>>,
	) -> Self {
		let api_client = Arc::new(BitcoindClient::new_rpc(
			rpc_host.clone(),
			rpc_port.clone(),
			rpc_user.clone(),
			rpc_password.clone(),
		));

		let header_cache = tokio::sync::Mutex::new(BoundedHeaderCache::new());
		let wallet_polling_status = Mutex::new(WalletSyncStatus::Completed);
		Self {
			api_client,
			header_cache,
			wallet_polling_status,
			last_mempool_account_generation: AtomicU64::new(u64::MAX),
			fee_estimator,
			kv_store,
			config,
			logger: Arc::clone(&logger),
			node_metrics,
		}
	}

	pub(crate) fn new_rest(
		rpc_host: String, rpc_port: u16, rpc_user: String, rpc_password: String,
		fee_estimator: Arc<OnchainFeeEstimator>, kv_store: Arc<DynStore>, config: Arc<Config>,
		rest_client_config: BitcoindRestClientConfig, logger: Arc<Logger>,
		node_metrics: Arc<RwLock<NodeMetrics>>,
	) -> Self {
		let api_client = Arc::new(BitcoindClient::new_rest(
			rest_client_config.rest_host,
			rest_client_config.rest_port,
			rpc_host,
			rpc_port,
			rpc_user,
			rpc_password,
		));

		let header_cache = tokio::sync::Mutex::new(BoundedHeaderCache::new());
		let wallet_polling_status = Mutex::new(WalletSyncStatus::Completed);

		Self {
			api_client,
			header_cache,
			wallet_polling_status,
			last_mempool_account_generation: AtomicU64::new(u64::MAX),
			fee_estimator,
			kv_store,
			config,
			logger: Arc::clone(&logger),
			node_metrics,
		}
	}

	pub(super) fn as_utxo_source(&self) -> Arc<dyn UtxoSource> {
		self.api_client.utxo_source()
	}

	pub(super) async fn continuously_sync_wallets(
		&self, mut stop_sync_receiver: tokio::sync::watch::Receiver<()>,
		onchain_wallet: Arc<Wallet>, channel_manager: Arc<ChannelManager>,
		chain_monitor: Arc<ChainMonitor>, output_sweeper: Arc<Sweeper>,
	) {
		// First register for the wallet polling status to make sure `Node::sync_wallets` calls
		// wait on the result before proceeding.
		{
			let mut status_lock = self.wallet_polling_status.lock().unwrap();
			if status_lock.register_or_subscribe_pending_sync().is_some() {
				debug_assert!(false, "Sync already in progress. This should never happen.");
			}
		}

		log_info!(
			self.logger,
			"Starting initial synchronization of chain listeners. This might take a while..",
		);

		let mut backoff = CHAIN_POLLING_INTERVAL_SECS;
		const MAX_BACKOFF_SECS: u64 = 300;

		// Use the same chain+mempool path as periodic polling. `sync_wallets` waits on this
		// initial sync, so returning after headers alone would leave persisted unconfirmed
		// transactions in place when Bitcoind's mempool is empty after restart.
		loop {
			if stop_sync_receiver.has_changed().unwrap_or(true) {
				log_trace!(self.logger, "Stopping initial chain sync.");
				return;
			}

			match self
				.poll_and_update_listeners_inner(
					Arc::clone(&onchain_wallet),
					Arc::clone(&channel_manager),
					Arc::clone(&chain_monitor),
					Arc::clone(&output_sweeper),
				)
				.await
			{
				Ok(ListenerSyncOutcome::AccountSetChanged) => continue,
				Ok(ListenerSyncOutcome::Complete(account_generation)) => {
					let account_operation = onchain_wallet.lock_account_operations();
					if onchain_wallet.account_generation() != account_generation {
						log_info!(
							self.logger,
							"On-chain account set changed during initial sync; retrying."
						);
						drop(account_operation);
						continue;
					}
					self.wallet_polling_status
						.lock()
						.unwrap()
						.propagate_result_to_subscribers(Ok(()));
					drop(account_operation);
					break;
				},
				Err(e) => {
					log_error!(
						self.logger,
						"Failed initial chain/mempool sync: {:?}. Retrying in {} seconds.",
						e,
						backoff
					);
					tokio::select! {
						biased;
						_ = stop_sync_receiver.changed() => {
							log_trace!(self.logger, "Stopping initial chain sync.");
							return;
						}
						_ = tokio::time::sleep(Duration::from_secs(backoff)) => {}
					}
					backoff = (backoff * 2).min(MAX_BACKOFF_SECS);
				},
			}
		}

		let mut chain_polling_interval =
			tokio::time::interval(Duration::from_secs(CHAIN_POLLING_INTERVAL_SECS));
		// Initial sync already ran chain+mempool; skip the immediate first tick.
		chain_polling_interval.reset();
		chain_polling_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

		let mut fee_rate_update_interval =
			tokio::time::interval(Duration::from_secs(CHAIN_POLLING_INTERVAL_SECS));
		// When starting up, we just blocked on updating, so skip the first tick.
		fee_rate_update_interval.reset();
		fee_rate_update_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

		log_info!(self.logger, "Starting continuous polling for chain updates.");

		// Start the polling loop.
		let mut last_best_block_hash = None;
		loop {
			tokio::select! {
				biased;
				_ = stop_sync_receiver.changed() => {
					log_trace!(
						self.logger,
						"Stopping polling for new chain data.",
					);
					return;
				}
				_ = chain_polling_interval.tick() => {
					tokio::select! {
						biased;
						_ = stop_sync_receiver.changed() => {
							log_trace!(
								self.logger,
								"Stopping polling for new chain data.",
							);
							return;
						}
						_ = self.poll_and_update_listeners(
							Arc::clone(&onchain_wallet),
							Arc::clone(&channel_manager),
							Arc::clone(&chain_monitor),
							Arc::clone(&output_sweeper)
						) => {}
					}
				}
				_ = fee_rate_update_interval.tick() => {
					if last_best_block_hash != Some(channel_manager.current_best_block().block_hash) {
						tokio::select! {
							biased;
							_ = stop_sync_receiver.changed() => {
								log_trace!(
									self.logger,
									"Stopping polling for new chain data.",
								);
								return;
							}
							update_res = self.update_fee_rate_estimates() => {
								if update_res.is_ok() {
									last_best_block_hash = Some(channel_manager.current_best_block().block_hash);
								}
							}
						}
					}
				}
			}
		}
	}

	pub(super) async fn poll_best_block(&self) -> Result<BestBlock, Error> {
		self.poll_chain_tip().await.map(|tip| tip.to_best_block())
	}

	async fn poll_chain_tip(&self) -> Result<ValidatedBlockHeader, Error> {
		let validate_res = tokio::time::timeout(
			Duration::from_secs(CHAIN_POLLING_TIMEOUT_SECS),
			validate_best_block_header(self.api_client.as_ref()),
		)
		.await
		.map_err(|e| {
			log_error!(self.logger, "Failed to poll for chain data: {:?}", e);
			Error::TxSyncTimeout
		})?;

		match validate_res {
			Ok(tip) => Ok(tip),
			Err(e) => {
				log_error!(self.logger, "Failed to poll for chain data: {:?}", e);
				return Err(Error::TxSyncFailed);
			},
		}
	}

	pub(super) async fn poll_and_update_listeners(
		&self, onchain_wallet: Arc<Wallet>, channel_manager: Arc<ChannelManager>,
		chain_monitor: Arc<ChainMonitor>, output_sweeper: Arc<Sweeper>,
	) -> Result<(), Error> {
		let receiver_res = {
			let mut status_lock = self.wallet_polling_status.lock().unwrap();
			status_lock.register_or_subscribe_pending_sync()
		};

		if let Some(mut sync_receiver) = receiver_res {
			log_info!(self.logger, "Sync in progress, skipping.");
			return sync_receiver.recv().await.map_err(|e| {
				debug_assert!(false, "Failed to receive wallet polling result: {:?}", e);
				log_error!(self.logger, "Failed to receive wallet polling result: {:?}", e);
				Error::WalletOperationFailed
			})?;
		}

		loop {
			match self
				.poll_and_update_listeners_inner(
					Arc::clone(&onchain_wallet),
					Arc::clone(&channel_manager),
					Arc::clone(&chain_monitor),
					Arc::clone(&output_sweeper),
				)
				.await
			{
				Ok(ListenerSyncOutcome::AccountSetChanged) => continue,
				Ok(ListenerSyncOutcome::Complete(account_generation)) => {
					let _account_operation = onchain_wallet.lock_account_operations();
					if onchain_wallet.account_generation() != account_generation {
						log_info!(
							self.logger,
							"On-chain account set changed during sync; retrying."
						);
						continue;
					}
					self.wallet_polling_status
						.lock()
						.unwrap()
						.propagate_result_to_subscribers(Ok(()));
					return Ok(());
				},
				Err(e) => {
					self.wallet_polling_status
						.lock()
						.unwrap()
						.propagate_result_to_subscribers(Err(e));
					return Err(e);
				},
			}
		}
	}

	async fn poll_and_update_listeners_inner(
		&self, onchain_wallet: Arc<Wallet>, channel_manager: Arc<ChannelManager>,
		chain_monitor: Arc<ChainMonitor>, output_sweeper: Arc<Sweeper>,
	) -> Result<ListenerSyncOutcome, Error> {
		onchain_wallet.finish_pending_sync(false)?;

		let previous_lightning_tip = channel_manager.current_best_block();
		let (account_generation, account_tips) = onchain_wallet.account_sync_snapshot();
		let account_listeners: Vec<AccountChainListener> = account_tips
			.iter()
			.map(|(account, _)| AccountChainListener::new(Arc::clone(&onchain_wallet), *account))
			.collect();
		let mut chain_listeners: Vec<(BlockHash, &(dyn Listen + Send + Sync))> = account_tips
			.iter()
			.zip(account_listeners.iter())
			.map(|((_, tip), listener)| (tip.block_hash, listener as &(dyn Listen + Send + Sync)))
			.collect();
		chain_listeners.push((
			previous_lightning_tip.block_hash,
			&*channel_manager as &(dyn Listen + Send + Sync),
		));
		chain_listeners.push((
			output_sweeper.current_best_block().block_hash,
			&*output_sweeper as &(dyn Listen + Send + Sync),
		));
		if let Some(worst_channel_monitor_block_hash) = chain_monitor
			.list_monitors()
			.iter()
			.flat_map(|channel_id| chain_monitor.get_monitor(*channel_id))
			.map(|monitor| monitor.current_best_block())
			.min_by_key(|block| block.height)
			.map(|block| block.block_hash)
		{
			chain_listeners.push((
				worst_channel_monitor_block_hash,
				&*chain_monitor as &(dyn Listen + Send + Sync),
			));
		}

		let mut locked_header_cache = self.header_cache.lock().await;
		let now = SystemTime::now();
		synchronize_listeners(
			self.api_client.as_ref(),
			self.config.network,
			&mut *locked_header_cache,
			chain_listeners,
		)
		.await
		.map_err(|e| {
			log_error!(self.logger, "Failed to synchronize chain listeners: {:?}", e);
			Error::TxSyncFailed
		})?;
		drop(locked_header_cache);

		for listener in &account_listeners {
			if let Err(err) = listener.finish() {
				log_error!(
					self.logger,
					"Per-account wallet sync apply failed for {:?}: {}",
					listener.account(),
					err
				);
				return Err(err);
			}
		}
		onchain_wallet.finish_pending_sync(false)?;

		log_trace!(
			self.logger,
			"Finished synchronizing chain listeners in {}ms",
			now.elapsed().unwrap().as_millis()
		);
		// Archival and metrics persistence are best-effort: chain/mempool sync already
		// succeeded, and failing these must not block `sync_wallets` (especially during
		// initial sync, which subscribers wait on).
		if let Err(e) = periodically_archive_fully_resolved_monitors(
			Arc::clone(&channel_manager),
			Arc::clone(&chain_monitor),
			Arc::clone(&self.kv_store),
			Arc::clone(&self.logger),
			Arc::clone(&self.node_metrics),
		) {
			log_error!(
				self.logger,
				"Failed to archive fully resolved channel monitors after sync: {}",
				e
			);
		}

		let cur_height = channel_manager.current_best_block().height;

		let now = SystemTime::now();
		let bdk_unconfirmed_txids = onchain_wallet.get_unconfirmed_txids_with_last_seen();
		// A newly loaded account must see transactions emitted before it joined the aggregate.
		let replay_mempool =
			self.last_mempool_account_generation.load(Ordering::Acquire) != account_generation;
		let mempool_update = match self
			.api_client
			.get_updated_mempool_transactions(cur_height, bdk_unconfirmed_txids, replay_mempool)
			.await
		{
			Ok(update) => {
				log_trace!(
					self.logger,
					"Finished polling mempool of size {} and {} evicted transactions in {}ms",
					update.transactions.len(),
					update.evicted_txids.len(),
					now.elapsed().unwrap().as_millis()
				);
				update
			},
			Err(e) => {
				log_error!(self.logger, "Failed to poll for mempool transactions: {:?}", e);
				return Err(Error::TxSyncFailed);
			},
		};

		{
			let _account_operation = onchain_wallet.lock_account_operations();
			if onchain_wallet.account_generation() != account_generation {
				log_info!(self.logger, "On-chain account set changed during sync; retrying.");
				return Ok(ListenerSyncOutcome::AccountSetChanged);
			}
			onchain_wallet
				.apply_mempool_txs(mempool_update.transactions, mempool_update.evicted_txids)
				.map_err(|e| {
					log_error!(self.logger, "Failed to apply mempool transactions: {:?}", e);
					e
				})?;
			self.api_client.commit_mempool_timestamp(mempool_update.next_timestamp);
			self.last_mempool_account_generation.store(account_generation, Ordering::Release);
		}

		let unix_time_secs_opt =
			SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs());
		{
			let mut locked_node_metrics = self.node_metrics.write().unwrap();
			locked_node_metrics.latest_lightning_wallet_sync_timestamp = unix_time_secs_opt;
			locked_node_metrics.latest_onchain_wallet_sync_timestamp = unix_time_secs_opt;

			if let Err(e) = write_node_metrics(
				&*locked_node_metrics,
				Arc::clone(&self.kv_store),
				Arc::clone(&self.logger),
			) {
				log_error!(self.logger, "Failed to persist node metrics after sync: {}", e);
			}
		}

		Ok(ListenerSyncOutcome::Complete(account_generation))
	}

	pub(super) async fn update_fee_rate_estimates(&self) -> Result<(), Error> {
		macro_rules! get_fee_rate_update {
			($estimation_fut:expr) => {{
				let update_res = tokio::time::timeout(
					Duration::from_secs(FEE_RATE_CACHE_UPDATE_TIMEOUT_SECS),
					$estimation_fut,
				)
				.await
				.map_err(|e| {
					log_error!(self.logger, "Updating fee rate estimates timed out: {}", e);
					Error::FeerateEstimationUpdateTimeout
				})?;
				update_res
			}};
		}
		let confirmation_targets = get_all_conf_targets();

		let mut new_fee_rate_cache = HashMap::with_capacity(10);
		let now = Instant::now();
		for target in confirmation_targets {
			let fee_rate_update_res = match target {
				ConfirmationTarget::Lightning(
					LdkConfirmationTarget::MinAllowedAnchorChannelRemoteFee,
				) => {
					let estimation_fut = self.api_client.get_mempool_minimum_fee_rate();
					get_fee_rate_update!(estimation_fut)
				},
				ConfirmationTarget::Lightning(LdkConfirmationTarget::MaximumFeeEstimate) => {
					let num_blocks = get_num_block_defaults_for_target(target);
					let estimation_mode = FeeRateEstimationMode::Conservative;
					let estimation_fut =
						self.api_client.get_fee_estimate_for_target(num_blocks, estimation_mode);
					get_fee_rate_update!(estimation_fut)
				},
				ConfirmationTarget::Lightning(LdkConfirmationTarget::UrgentOnChainSweep) => {
					let num_blocks = get_num_block_defaults_for_target(target);
					let estimation_mode = FeeRateEstimationMode::Conservative;
					let estimation_fut =
						self.api_client.get_fee_estimate_for_target(num_blocks, estimation_mode);
					get_fee_rate_update!(estimation_fut)
				},
				_ => {
					// Otherwise, we default to economical block-target estimate.
					let num_blocks = get_num_block_defaults_for_target(target);
					let estimation_mode = FeeRateEstimationMode::Economical;
					let estimation_fut =
						self.api_client.get_fee_estimate_for_target(num_blocks, estimation_mode);
					get_fee_rate_update!(estimation_fut)
				},
			};

			let fee_rate = match (fee_rate_update_res, self.config.network) {
				(Ok(rate), _) => rate,
				(Err(e), Network::Bitcoin) => {
					// Strictly fail on mainnet.
					log_error!(self.logger, "Failed to retrieve fee rate estimates: {}", e);
					return Err(Error::FeerateEstimationUpdateFailed);
				},
				(Err(e), n) if n == Network::Regtest || n == Network::Signet => {
					// On regtest/signet we just fall back to the usual 1 sat/vb == 250
					// sat/kwu default.
					log_error!(
								self.logger,
								"Failed to retrieve fee rate estimates: {}. Falling back to default of 1 sat/vb.",
								e,
							);
					FeeRate::from_sat_per_kwu(250)
				},
				(Err(e), _) => {
					// On testnet `estimatesmartfee` can be unreliable so we just skip in
					// case of a failure, which will have us falling back to defaults.
					log_error!(
						self.logger,
						"Failed to retrieve fee rate estimates: {}. Falling back to defaults.",
						e,
					);
					return Ok(());
				},
			};

			// LDK 0.0.118 introduced changes to the `ConfirmationTarget` semantics that
			// require some post-estimation adjustments to the fee rates, which we do here.
			let adjusted_fee_rate = apply_post_estimation_adjustments(target, fee_rate);

			new_fee_rate_cache.insert(target, adjusted_fee_rate);

			log_trace!(
				self.logger,
				"Fee rate estimation updated for {:?}: {} sats/kwu",
				target,
				adjusted_fee_rate.to_sat_per_kwu(),
			);
		}

		if self.fee_estimator.set_fee_rate_cache(new_fee_rate_cache) {
			// We only log if the values changed, as it might be very spammy otherwise.
			log_info!(
				self.logger,
				"Fee rate cache update finished in {}ms.",
				now.elapsed().as_millis()
			);
		}

		let unix_time_secs_opt =
			SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs());
		{
			let mut locked_node_metrics = self.node_metrics.write().unwrap();
			locked_node_metrics.latest_fee_rate_cache_update_timestamp = unix_time_secs_opt;
			write_node_metrics(
				&*locked_node_metrics,
				Arc::clone(&self.kv_store),
				Arc::clone(&self.logger),
			)?;
		}

		Ok(())
	}

	pub(crate) async fn process_broadcast_package(&self, package: Vec<Transaction>) {
		// While it's a bit unclear when we'd be able to lean on Bitcoin Core >v28
		// features, we should eventually switch to use `submitpackage` via the
		// `rust-bitcoind-json-rpc` crate rather than just broadcasting individual
		// transactions.
		for tx in &package {
			let txid = tx.compute_txid();
			let timeout_fut = tokio::time::timeout(
				Duration::from_secs(TX_BROADCAST_TIMEOUT_SECS),
				self.api_client.broadcast_transaction(tx),
			);
			match timeout_fut.await {
				Ok(res) => match res {
					Ok(id) => {
						debug_assert_eq!(id, txid);
						log_trace!(self.logger, "Successfully broadcast transaction {}", txid);
					},
					Err(e) => {
						log_error!(self.logger, "Failed to broadcast transaction {}: {}", txid, e);
						log_trace!(
							self.logger,
							"Failed broadcast transaction bytes: {}",
							log_bytes!(tx.encode())
						);
					},
				},
				Err(e) => {
					log_error!(
						self.logger,
						"Failed to broadcast transaction due to timeout {}: {}",
						txid,
						e
					);
					log_trace!(
						self.logger,
						"Failed broadcast transaction bytes: {}",
						log_bytes!(tx.encode())
					);
				},
			}
		}
	}
}

struct MempoolUpdate {
	transactions: Vec<(Transaction, u64)>,
	evicted_txids: Vec<(Txid, u64)>,
	next_timestamp: Option<u64>,
}

fn should_emit_mempool_entry(
	entry_time: u64, entry_height: u32, best_processed_height: u32, watermark: u64,
	replay_all: bool,
) -> bool {
	replay_all || entry_height > best_processed_height || entry_time >= watermark
}

pub enum BitcoindClient {
	Rpc {
		rpc_client: Arc<RpcClient>,
		latest_mempool_timestamp: AtomicU64,
		mempool_entries_cache: tokio::sync::Mutex<HashMap<Txid, MempoolEntry>>,
		mempool_txs_cache: tokio::sync::Mutex<HashMap<Txid, (Transaction, u64)>>,
	},
	Rest {
		rest_client: Arc<RestClient>,
		rpc_client: Arc<RpcClient>,
		latest_mempool_timestamp: AtomicU64,
		mempool_entries_cache: tokio::sync::Mutex<HashMap<Txid, MempoolEntry>>,
		mempool_txs_cache: tokio::sync::Mutex<HashMap<Txid, (Transaction, u64)>>,
	},
}

impl BitcoindClient {
	/// Creates a new RPC API client for the chain interactions with Bitcoin Core.
	pub(crate) fn new_rpc(host: String, port: u16, rpc_user: String, rpc_password: String) -> Self {
		let http_endpoint = endpoint(host, port);
		let rpc_credentials = rpc_credentials(rpc_user, rpc_password);

		let rpc_client = Arc::new(RpcClient::new(&rpc_credentials, http_endpoint));

		let latest_mempool_timestamp = AtomicU64::new(0);

		let mempool_entries_cache = tokio::sync::Mutex::new(HashMap::new());
		let mempool_txs_cache = tokio::sync::Mutex::new(HashMap::new());
		Self::Rpc { rpc_client, latest_mempool_timestamp, mempool_entries_cache, mempool_txs_cache }
	}

	/// Creates a new, primarily REST API client for the chain interactions
	/// with Bitcoin Core.
	///
	/// Aside the required REST host and port, we provide RPC configuration
	/// options for necessary calls not supported by the REST interface.
	pub(crate) fn new_rest(
		rest_host: String, rest_port: u16, rpc_host: String, rpc_port: u16, rpc_user: String,
		rpc_password: String,
	) -> Self {
		let rest_endpoint = endpoint(rest_host, rest_port).with_path("/rest".to_string());
		let rest_client = Arc::new(RestClient::new(rest_endpoint));

		let rpc_endpoint = endpoint(rpc_host, rpc_port);
		let rpc_credentials = rpc_credentials(rpc_user, rpc_password);
		let rpc_client = Arc::new(RpcClient::new(&rpc_credentials, rpc_endpoint));

		let latest_mempool_timestamp = AtomicU64::new(0);

		let mempool_entries_cache = tokio::sync::Mutex::new(HashMap::new());
		let mempool_txs_cache = tokio::sync::Mutex::new(HashMap::new());

		Self::Rest {
			rest_client,
			rpc_client,
			latest_mempool_timestamp,
			mempool_entries_cache,
			mempool_txs_cache,
		}
	}

	pub(crate) fn utxo_source(&self) -> Arc<dyn UtxoSource> {
		match self {
			BitcoindClient::Rpc { rpc_client, .. } => Arc::clone(rpc_client) as Arc<dyn UtxoSource>,
			BitcoindClient::Rest { rest_client, .. } => {
				Arc::clone(rest_client) as Arc<dyn UtxoSource>
			},
		}
	}

	/// Broadcasts the provided transaction.
	pub(crate) async fn broadcast_transaction(&self, tx: &Transaction) -> std::io::Result<Txid> {
		match self {
			BitcoindClient::Rpc { rpc_client, .. } => {
				Self::broadcast_transaction_inner(Arc::clone(rpc_client), tx).await
			},
			BitcoindClient::Rest { rpc_client, .. } => {
				// Bitcoin Core's REST interface does not support broadcasting transactions
				// so we use the RPC client.
				Self::broadcast_transaction_inner(Arc::clone(rpc_client), tx).await
			},
		}
	}

	async fn broadcast_transaction_inner(
		rpc_client: Arc<RpcClient>, tx: &Transaction,
	) -> std::io::Result<Txid> {
		let tx_serialized = bitcoin::consensus::encode::serialize_hex(tx);
		let tx_json = serde_json::json!(tx_serialized);
		rpc_client.call_method::<Txid>("sendrawtransaction", &[tx_json]).await
	}

	/// Retrieve the fee estimate needed for a transaction to begin
	/// confirmation within the provided `num_blocks`.
	pub(crate) async fn get_fee_estimate_for_target(
		&self, num_blocks: usize, estimation_mode: FeeRateEstimationMode,
	) -> std::io::Result<FeeRate> {
		match self {
			BitcoindClient::Rpc { rpc_client, .. } => {
				Self::get_fee_estimate_for_target_inner(
					Arc::clone(rpc_client),
					num_blocks,
					estimation_mode,
				)
				.await
			},
			BitcoindClient::Rest { rpc_client, .. } => {
				// We rely on the internal RPC client to make this call, as this
				// operation is not supported by Bitcoin Core's REST interface.
				Self::get_fee_estimate_for_target_inner(
					Arc::clone(rpc_client),
					num_blocks,
					estimation_mode,
				)
				.await
			},
		}
	}

	/// Estimate the fee rate for the provided target number of blocks.
	async fn get_fee_estimate_for_target_inner(
		rpc_client: Arc<RpcClient>, num_blocks: usize, estimation_mode: FeeRateEstimationMode,
	) -> std::io::Result<FeeRate> {
		let num_blocks_json = serde_json::json!(num_blocks);
		let estimation_mode_json = serde_json::json!(estimation_mode);
		rpc_client
			.call_method::<FeeResponse>(
				"estimatesmartfee",
				&[num_blocks_json, estimation_mode_json],
			)
			.await
			.map(|resp| resp.0)
	}

	/// Gets the mempool minimum fee rate.
	pub(crate) async fn get_mempool_minimum_fee_rate(&self) -> std::io::Result<FeeRate> {
		match self {
			BitcoindClient::Rpc { rpc_client, .. } => {
				Self::get_mempool_minimum_fee_rate_rpc(Arc::clone(rpc_client)).await
			},
			BitcoindClient::Rest { rest_client, .. } => {
				Self::get_mempool_minimum_fee_rate_rest(Arc::clone(rest_client)).await
			},
		}
	}

	/// Get the mempool minimum fee rate via RPC interface.
	async fn get_mempool_minimum_fee_rate_rpc(
		rpc_client: Arc<RpcClient>,
	) -> std::io::Result<FeeRate> {
		rpc_client
			.call_method::<MempoolMinFeeResponse>("getmempoolinfo", &[])
			.await
			.map(|resp| resp.0)
	}

	/// Get the mempool minimum fee rate via REST interface.
	async fn get_mempool_minimum_fee_rate_rest(
		rest_client: Arc<RestClient>,
	) -> std::io::Result<FeeRate> {
		rest_client
			.request_resource::<JsonResponse, MempoolMinFeeResponse>("mempool/info.json")
			.await
			.map(|resp| resp.0)
	}

	/// Gets the raw transaction for the provided transaction ID. Returns `None` if not found.
	pub(crate) async fn get_raw_transaction(
		&self, txid: &Txid,
	) -> std::io::Result<Option<Transaction>> {
		match self {
			BitcoindClient::Rpc { rpc_client, .. } => {
				Self::get_raw_transaction_rpc(Arc::clone(rpc_client), txid).await
			},
			BitcoindClient::Rest { rest_client, .. } => {
				Self::get_raw_transaction_rest(Arc::clone(rest_client), txid).await
			},
		}
	}

	/// Retrieve raw transaction for provided transaction ID via the RPC interface.
	async fn get_raw_transaction_rpc(
		rpc_client: Arc<RpcClient>, txid: &Txid,
	) -> std::io::Result<Option<Transaction>> {
		let txid_hex = txid.to_string();
		let txid_json = serde_json::json!(txid_hex);
		match rpc_client
			.call_method::<GetRawTransactionResponse>("getrawtransaction", &[txid_json])
			.await
		{
			Ok(resp) => Ok(Some(resp.0)),
			Err(e) => match e.into_inner() {
				Some(inner) => {
					let rpc_error_res: Result<Box<RpcError>, _> = inner.downcast();

					match rpc_error_res {
						Ok(rpc_error) => {
							// Check if it's the 'not found' error code.
							if rpc_error.code == -5 {
								Ok(None)
							} else {
								Err(std::io::Error::new(std::io::ErrorKind::Other, rpc_error))
							}
						},
						Err(_) => Err(std::io::Error::new(
							std::io::ErrorKind::Other,
							"Failed to process getrawtransaction response",
						)),
					}
				},
				None => Err(std::io::Error::new(
					std::io::ErrorKind::Other,
					"Failed to process getrawtransaction response",
				)),
			},
		}
	}

	/// Retrieve raw transaction for provided transaction ID via the REST interface.
	async fn get_raw_transaction_rest(
		rest_client: Arc<RestClient>, txid: &Txid,
	) -> std::io::Result<Option<Transaction>> {
		let txid_hex = txid.to_string();
		let tx_path = format!("tx/{}.json", txid_hex);
		match rest_client
			.request_resource::<JsonResponse, GetRawTransactionResponse>(&tx_path)
			.await
		{
			Ok(resp) => Ok(Some(resp.0)),
			Err(e) => match e.kind() {
				std::io::ErrorKind::Other => {
					match e.into_inner() {
						Some(inner) => {
							let http_error_res: Result<Box<HttpError>, _> = inner.downcast();
							match http_error_res {
								Ok(http_error) => {
									// Check if it's the HTTP NOT_FOUND error code.
									if &http_error.status_code == "404" {
										Ok(None)
									} else {
										Err(std::io::Error::new(
											std::io::ErrorKind::Other,
											http_error,
										))
									}
								},
								Err(_) => {
									let error_msg =
										format!("Failed to process {} response.", tx_path);
									Err(std::io::Error::new(
										std::io::ErrorKind::Other,
										error_msg.as_str(),
									))
								},
							}
						},
						None => {
							let error_msg = format!("Failed to process {} response.", tx_path);
							Err(std::io::Error::new(std::io::ErrorKind::Other, error_msg.as_str()))
						},
					}
				},
				_ => {
					let error_msg = format!("Failed to process {} response.", tx_path);
					Err(std::io::Error::new(std::io::ErrorKind::Other, error_msg.as_str()))
				},
			},
		}
	}

	/// Retrieves the raw mempool.
	pub(crate) async fn get_raw_mempool(&self) -> std::io::Result<Vec<Txid>> {
		match self {
			BitcoindClient::Rpc { rpc_client, .. } => {
				Self::get_raw_mempool_rpc(Arc::clone(rpc_client)).await
			},
			BitcoindClient::Rest { rest_client, .. } => {
				Self::get_raw_mempool_rest(Arc::clone(rest_client)).await
			},
		}
	}

	/// Retrieves the raw mempool via the RPC interface.
	async fn get_raw_mempool_rpc(rpc_client: Arc<RpcClient>) -> std::io::Result<Vec<Txid>> {
		let verbose_flag_json = serde_json::json!(false);
		rpc_client
			.call_method::<GetRawMempoolResponse>("getrawmempool", &[verbose_flag_json])
			.await
			.map(|resp| resp.0)
	}

	/// Retrieves the raw mempool via the REST interface.
	async fn get_raw_mempool_rest(rest_client: Arc<RestClient>) -> std::io::Result<Vec<Txid>> {
		rest_client
			.request_resource::<JsonResponse, GetRawMempoolResponse>(
				"mempool/contents.json?verbose=false",
			)
			.await
			.map(|resp| resp.0)
	}

	/// Retrieves an entry from the mempool if it exists, else return `None`.
	pub(crate) async fn get_mempool_entry(
		&self, txid: Txid,
	) -> std::io::Result<Option<MempoolEntry>> {
		match self {
			BitcoindClient::Rpc { rpc_client, .. } => {
				Self::get_mempool_entry_inner(Arc::clone(rpc_client), txid).await
			},
			BitcoindClient::Rest { rpc_client, .. } => {
				Self::get_mempool_entry_inner(Arc::clone(rpc_client), txid).await
			},
		}
	}

	/// Retrieves the mempool entry of the provided transaction ID.
	async fn get_mempool_entry_inner(
		client: Arc<RpcClient>, txid: Txid,
	) -> std::io::Result<Option<MempoolEntry>> {
		let txid_hex = txid.to_string();
		let txid_json = serde_json::json!(txid_hex);

		match client.call_method::<GetMempoolEntryResponse>("getmempoolentry", &[txid_json]).await {
			Ok(resp) => Ok(Some(MempoolEntry { txid, time: resp.time, height: resp.height })),
			Err(e) => match e.into_inner() {
				Some(inner) => {
					let rpc_error_res: Result<Box<RpcError>, _> = inner.downcast();

					match rpc_error_res {
						Ok(rpc_error) => {
							// Check if it's the 'not found' error code.
							if rpc_error.code == -5 {
								Ok(None)
							} else {
								Err(std::io::Error::new(std::io::ErrorKind::Other, rpc_error))
							}
						},
						Err(_) => Err(std::io::Error::new(
							std::io::ErrorKind::Other,
							"Failed to process getmempoolentry response",
						)),
					}
				},
				None => Err(std::io::Error::new(
					std::io::ErrorKind::Other,
					"Failed to process getmempoolentry response",
				)),
			},
		}
	}

	pub(crate) async fn update_mempool_entries_cache(&self) -> std::io::Result<()> {
		match self {
			BitcoindClient::Rpc { mempool_entries_cache, .. } => {
				self.update_mempool_entries_cache_inner(mempool_entries_cache).await
			},
			BitcoindClient::Rest { mempool_entries_cache, .. } => {
				self.update_mempool_entries_cache_inner(mempool_entries_cache).await
			},
		}
	}

	async fn update_mempool_entries_cache_inner(
		&self, mempool_entries_cache: &tokio::sync::Mutex<HashMap<Txid, MempoolEntry>>,
	) -> std::io::Result<()> {
		let mempool_txids = self.get_raw_mempool().await?;

		let mut mempool_entries_cache = mempool_entries_cache.lock().await;
		mempool_entries_cache.retain(|txid, _| mempool_txids.contains(txid));

		if let Some(difference) = mempool_txids.len().checked_sub(mempool_entries_cache.capacity())
		{
			mempool_entries_cache.reserve(difference)
		}

		for txid in mempool_txids {
			if mempool_entries_cache.contains_key(&txid) {
				continue;
			}

			if let Some(entry) = self.get_mempool_entry(txid).await? {
				mempool_entries_cache.insert(txid, entry.clone());
			}
		}

		mempool_entries_cache.shrink_to_fit();

		Ok(())
	}

	/// Collects mempool changes without advancing the committed emission timestamp.
	async fn get_updated_mempool_transactions(
		&self, best_processed_height: u32, bdk_unconfirmed_txids: Vec<(Txid, u64)>,
		replay_all: bool,
	) -> std::io::Result<MempoolUpdate> {
		let (transactions, next_timestamp) = self
			.get_mempool_transactions_and_timestamp_at_height(best_processed_height, replay_all)
			.await?;
		let sync_timestamp = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.map(|duration| duration.as_secs())
			.unwrap_or_else(|_| self.mempool_timestamp());
		let eviction_timestamp =
			next_timestamp.unwrap_or_else(|| self.mempool_timestamp()).max(sync_timestamp);
		let evicted_txids = self
			.get_evicted_mempool_txids_and_timestamp(bdk_unconfirmed_txids, eviction_timestamp)
			.await?;
		Ok(MempoolUpdate { transactions, evicted_txids, next_timestamp })
	}

	/// Get mempool transactions, alongside their first-seen unix timestamps.
	///
	/// This method is an adapted version of `bdk_bitcoind_rpc::Emitter::mempool`. It emits each
	/// transaction only once, unless we cannot assume the transaction's ancestors are already
	/// emitted.
	async fn get_mempool_transactions_and_timestamp_at_height(
		&self, best_processed_height: u32, replay_all: bool,
	) -> std::io::Result<(Vec<(Transaction, u64)>, Option<u64>)> {
		match self {
			BitcoindClient::Rpc {
				latest_mempool_timestamp,
				mempool_entries_cache,
				mempool_txs_cache,
				..
			} => {
				self.get_mempool_transactions_and_timestamp_at_height_inner(
					latest_mempool_timestamp,
					mempool_entries_cache,
					mempool_txs_cache,
					best_processed_height,
					replay_all,
				)
				.await
			},
			BitcoindClient::Rest {
				latest_mempool_timestamp,
				mempool_entries_cache,
				mempool_txs_cache,
				..
			} => {
				self.get_mempool_transactions_and_timestamp_at_height_inner(
					latest_mempool_timestamp,
					mempool_entries_cache,
					mempool_txs_cache,
					best_processed_height,
					replay_all,
				)
				.await
			},
		}
	}

	async fn get_mempool_transactions_and_timestamp_at_height_inner(
		&self, latest_mempool_timestamp: &AtomicU64,
		mempool_entries_cache: &tokio::sync::Mutex<HashMap<Txid, MempoolEntry>>,
		mempool_txs_cache: &tokio::sync::Mutex<HashMap<Txid, (Transaction, u64)>>,
		best_processed_height: u32, replay_all: bool,
	) -> std::io::Result<(Vec<(Transaction, u64)>, Option<u64>)> {
		let prev_mempool_time = latest_mempool_timestamp.load(Ordering::Relaxed);
		let mut latest_time = prev_mempool_time;

		self.update_mempool_entries_cache().await?;

		let mempool_entries_cache = mempool_entries_cache.lock().await;
		let mut mempool_txs_cache = mempool_txs_cache.lock().await;
		mempool_txs_cache.retain(|txid, _| mempool_entries_cache.contains_key(txid));

		if let Some(difference) =
			mempool_entries_cache.len().checked_sub(mempool_txs_cache.capacity())
		{
			mempool_txs_cache.reserve(difference)
		}

		let mut txs_to_emit = Vec::with_capacity(mempool_entries_cache.len());
		for (txid, entry) in mempool_entries_cache.iter() {
			if entry.time > latest_time {
				latest_time = entry.time;
			}

			// Avoid emitting transactions that are already emitted if we can guarantee
			// blocks containing ancestors are already emitted. The bitcoind rpc interface
			// provides us with the block height that the tx is introduced to the mempool.
			// If we have already emitted the block of height, we can assume that all
			// ancestor txs have been processed by the receiver.
			// Re-emit the watermark boundary because Bitcoin Core timestamps entries to the second.
			if !should_emit_mempool_entry(
				entry.time,
				entry.height,
				best_processed_height,
				prev_mempool_time,
				replay_all,
			) {
				continue;
			}

			if let Some((cached_tx, cached_time)) = mempool_txs_cache.get(txid) {
				txs_to_emit.push((cached_tx.clone(), *cached_time));
				continue;
			}

			match self.get_raw_transaction(&entry.txid).await {
				Ok(Some(tx)) => {
					mempool_txs_cache.insert(entry.txid, (tx.clone(), entry.time));
					txs_to_emit.push((tx, entry.time));
				},
				Ok(None) => {
					continue;
				},
				Err(e) => return Err(e),
			};
		}

		let next_timestamp = (!txs_to_emit.is_empty()).then_some(latest_time);
		Ok((txs_to_emit, next_timestamp))
	}

	// Retrieve a list of Txids that have been evicted from the mempool.
	//
	// To this end, we first update our local mempool_entries_cache and then return all unconfirmed
	// wallet `Txid`s that don't appear in the mempool still.
	async fn get_evicted_mempool_txids_and_timestamp(
		&self, bdk_unconfirmed_txids: Vec<(Txid, u64)>, timestamp: u64,
	) -> std::io::Result<Vec<(Txid, u64)>> {
		match self {
			BitcoindClient::Rpc { mempool_entries_cache, .. } => {
				Self::get_evicted_mempool_txids_and_timestamp_inner(
					mempool_entries_cache,
					bdk_unconfirmed_txids,
					timestamp,
				)
				.await
			},
			BitcoindClient::Rest { mempool_entries_cache, .. } => {
				Self::get_evicted_mempool_txids_and_timestamp_inner(
					mempool_entries_cache,
					bdk_unconfirmed_txids,
					timestamp,
				)
				.await
			},
		}
	}

	async fn get_evicted_mempool_txids_and_timestamp_inner(
		mempool_entries_cache: &tokio::sync::Mutex<HashMap<Txid, MempoolEntry>>,
		bdk_unconfirmed_txids: Vec<(Txid, u64)>, timestamp: u64,
	) -> std::io::Result<Vec<(Txid, u64)>> {
		let mempool_entries_cache = mempool_entries_cache.lock().await;
		let evicted_txids = bdk_unconfirmed_txids
			.into_iter()
			.filter(|(txid, _)| !mempool_entries_cache.contains_key(txid))
			.map(|(txid, last_seen)| (txid, timestamp.max(last_seen)))
			.collect();
		Ok(evicted_txids)
	}

	fn mempool_timestamp(&self) -> u64 {
		match self {
			Self::Rpc { latest_mempool_timestamp, .. }
			| Self::Rest { latest_mempool_timestamp, .. } => {
				latest_mempool_timestamp.load(Ordering::Acquire)
			},
		}
	}

	fn commit_mempool_timestamp(&self, timestamp: Option<u64>) {
		let Some(timestamp) = timestamp else {
			return;
		};
		match self {
			Self::Rpc { latest_mempool_timestamp, .. }
			| Self::Rest { latest_mempool_timestamp, .. } => {
				latest_mempool_timestamp.fetch_max(timestamp, Ordering::AcqRel);
			},
		}
	}
}

impl BlockSource for BitcoindClient {
	fn get_header<'a>(
		&'a self, header_hash: &'a bitcoin::BlockHash, height_hint: Option<u32>,
	) -> AsyncBlockSourceResult<'a, BlockHeaderData> {
		match self {
			BitcoindClient::Rpc { rpc_client, .. } => {
				Box::pin(async move { rpc_client.get_header(header_hash, height_hint).await })
			},
			BitcoindClient::Rest { rest_client, .. } => {
				Box::pin(async move { rest_client.get_header(header_hash, height_hint).await })
			},
		}
	}

	fn get_block<'a>(
		&'a self, header_hash: &'a bitcoin::BlockHash,
	) -> AsyncBlockSourceResult<'a, BlockData> {
		match self {
			BitcoindClient::Rpc { rpc_client, .. } => {
				Box::pin(async move { rpc_client.get_block(header_hash).await })
			},
			BitcoindClient::Rest { rest_client, .. } => {
				Box::pin(async move { rest_client.get_block(header_hash).await })
			},
		}
	}

	fn get_best_block(&self) -> AsyncBlockSourceResult<'_, (bitcoin::BlockHash, Option<u32>)> {
		match self {
			BitcoindClient::Rpc { rpc_client, .. } => {
				Box::pin(async move { rpc_client.get_best_block().await })
			},
			BitcoindClient::Rest { rest_client, .. } => {
				Box::pin(async move { rest_client.get_best_block().await })
			},
		}
	}
}

pub(crate) struct FeeResponse(pub FeeRate);

impl TryInto<FeeResponse> for JsonResponse {
	type Error = std::io::Error;
	fn try_into(self) -> std::io::Result<FeeResponse> {
		if !self.0["errors"].is_null() {
			return Err(std::io::Error::new(
				std::io::ErrorKind::Other,
				self.0["errors"].to_string(),
			));
		}
		let fee_rate_btc_per_kvbyte = self.0["feerate"]
			.as_f64()
			.ok_or(std::io::Error::new(std::io::ErrorKind::Other, "Failed to parse fee rate"))?;
		// Bitcoin Core gives us a feerate in BTC/KvB.
		// Thus, we multiply by 25_000_000 (10^8 / 4) to get satoshis/kwu.
		let fee_rate = {
			let fee_rate_sat_per_kwu = (fee_rate_btc_per_kvbyte * 25_000_000.0).round() as u64;
			FeeRate::from_sat_per_kwu(fee_rate_sat_per_kwu)
		};
		Ok(FeeResponse(fee_rate))
	}
}

pub(crate) struct MempoolMinFeeResponse(pub FeeRate);

impl TryInto<MempoolMinFeeResponse> for JsonResponse {
	type Error = std::io::Error;
	fn try_into(self) -> std::io::Result<MempoolMinFeeResponse> {
		let fee_rate_btc_per_kvbyte = self.0["mempoolminfee"]
			.as_f64()
			.ok_or(std::io::Error::new(std::io::ErrorKind::Other, "Failed to parse fee rate"))?;
		// Bitcoin Core gives us a feerate in BTC/KvB.
		// Thus, we multiply by 25_000_000 (10^8 / 4) to get satoshis/kwu.
		let fee_rate = {
			let fee_rate_sat_per_kwu = (fee_rate_btc_per_kvbyte * 25_000_000.0).round() as u64;
			FeeRate::from_sat_per_kwu(fee_rate_sat_per_kwu)
		};
		Ok(MempoolMinFeeResponse(fee_rate))
	}
}

pub(crate) struct GetRawTransactionResponse(pub Transaction);

impl TryInto<GetRawTransactionResponse> for JsonResponse {
	type Error = std::io::Error;
	fn try_into(self) -> std::io::Result<GetRawTransactionResponse> {
		let tx = self
			.0
			.as_str()
			.ok_or(std::io::Error::new(
				std::io::ErrorKind::Other,
				"Failed to parse getrawtransaction response",
			))
			.and_then(|s| {
				bitcoin::consensus::encode::deserialize_hex(s).map_err(|_| {
					std::io::Error::new(
						std::io::ErrorKind::Other,
						"Failed to parse getrawtransaction response",
					)
				})
			})?;

		Ok(GetRawTransactionResponse(tx))
	}
}

pub struct GetRawMempoolResponse(Vec<Txid>);

impl TryInto<GetRawMempoolResponse> for JsonResponse {
	type Error = std::io::Error;
	fn try_into(self) -> std::io::Result<GetRawMempoolResponse> {
		let res = self.0.as_array().ok_or(std::io::Error::new(
			std::io::ErrorKind::Other,
			"Failed to parse getrawmempool response",
		))?;

		let mut mempool_transactions = Vec::with_capacity(res.len());

		for hex in res {
			let txid = if let Some(hex_str) = hex.as_str() {
				match hex_str.parse::<Txid>() {
					Ok(txid) => txid,
					Err(_) => {
						return Err(std::io::Error::new(
							std::io::ErrorKind::Other,
							"Failed to parse getrawmempool response",
						));
					},
				}
			} else {
				return Err(std::io::Error::new(
					std::io::ErrorKind::Other,
					"Failed to parse getrawmempool response",
				));
			};

			mempool_transactions.push(txid);
		}

		Ok(GetRawMempoolResponse(mempool_transactions))
	}
}

pub struct GetMempoolEntryResponse {
	time: u64,
	height: u32,
}

impl TryInto<GetMempoolEntryResponse> for JsonResponse {
	type Error = std::io::Error;
	fn try_into(self) -> std::io::Result<GetMempoolEntryResponse> {
		let res = self.0.as_object().ok_or(std::io::Error::new(
			std::io::ErrorKind::Other,
			"Failed to parse getmempoolentry response",
		))?;

		let time = match res["time"].as_u64() {
			Some(time) => time,
			None => {
				return Err(std::io::Error::new(
					std::io::ErrorKind::Other,
					"Failed to parse getmempoolentry response",
				));
			},
		};

		let height = match res["height"].as_u64().and_then(|h| h.try_into().ok()) {
			Some(height) => height,
			None => {
				return Err(std::io::Error::new(
					std::io::ErrorKind::Other,
					"Failed to parse getmempoolentry response",
				));
			},
		};

		Ok(GetMempoolEntryResponse { time, height })
	}
}

#[derive(Debug, Clone)]
pub(crate) struct MempoolEntry {
	/// The transaction id
	txid: Txid,
	/// Local time transaction entered pool in seconds since 1 Jan 1970 GMT
	time: u64,
	/// Block height when transaction entered pool
	height: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub(crate) enum FeeRateEstimationMode {
	Economical,
	Conservative,
}

const MAX_HEADER_CACHE_ENTRIES: usize = 100;

pub(crate) struct BoundedHeaderCache {
	header_map: HashMap<BlockHash, ValidatedBlockHeader>,
	recently_seen: VecDeque<BlockHash>,
}

impl BoundedHeaderCache {
	pub(crate) fn new() -> Self {
		let header_map = HashMap::new();
		let recently_seen = VecDeque::new();
		Self { header_map, recently_seen }
	}
}

impl Cache for BoundedHeaderCache {
	fn look_up(&self, block_hash: &BlockHash) -> Option<&ValidatedBlockHeader> {
		self.header_map.get(block_hash)
	}

	fn block_connected(&mut self, block_hash: BlockHash, block_header: ValidatedBlockHeader) {
		self.recently_seen.push_back(block_hash);
		self.header_map.insert(block_hash, block_header);

		if self.header_map.len() >= MAX_HEADER_CACHE_ENTRIES {
			// Keep dropping old entries until we've actually removed a header entry.
			while let Some(oldest_entry) = self.recently_seen.pop_front() {
				if self.header_map.remove(&oldest_entry).is_some() {
					break;
				}
			}
		}
	}

	fn block_disconnected(&mut self, block_hash: &BlockHash) -> Option<ValidatedBlockHeader> {
		self.recently_seen.retain(|e| e != block_hash);
		self.header_map.remove(block_hash)
	}
}

/// Listen adapter that synchronizes one aggregate account from its own checkpoint.
struct AccountChainListener {
	wallet: Arc<Wallet>,
	account: OnchainWalletAccount,
	fork_point: Mutex<Option<BestBlock>>,
	reorg_blocks: Mutex<Vec<(bitcoin::Block, u32)>>,
	apply_error: Mutex<Option<Error>>,
}

impl AccountChainListener {
	fn new(wallet: Arc<Wallet>, account: OnchainWalletAccount) -> Self {
		Self {
			wallet,
			account,
			fork_point: Mutex::new(None),
			reorg_blocks: Mutex::new(Vec::new()),
			apply_error: Mutex::new(None),
		}
	}

	fn account(&self) -> OnchainWalletAccount {
		self.account
	}

	fn finish(&self) -> Result<(), Error> {
		if let Some(error) = self.apply_error.lock().unwrap().take() {
			return Err(error);
		}

		let Some(fork_point) = self.fork_point.lock().unwrap().take() else {
			return Ok(());
		};
		let blocks: Vec<_> = self.reorg_blocks.lock().unwrap().drain(..).collect();
		self.wallet.apply_reorg_blocks_to_account(self.account, fork_point, &blocks)
	}
}

impl Listen for AccountChainListener {
	fn filtered_block_connected(
		&self, _header: &bitcoin::block::Header,
		_txdata: &lightning::chain::transaction::TransactionData, _height: u32,
	) {
		debug_assert!(false, "Syncing filtered blocks is currently not supported");
	}

	fn block_connected(&self, block: &bitcoin::Block, height: u32) {
		if self.apply_error.lock().unwrap().is_some() {
			return;
		}
		if self.fork_point.lock().unwrap().is_some() {
			self.reorg_blocks.lock().unwrap().push((block.clone(), height));
			return;
		}
		if let Err(e) = self.wallet.apply_block_to_account(self.account, block, height) {
			*self.apply_error.lock().unwrap() = Some(e);
		}
	}

	fn blocks_disconnected(&self, fork_point: BestBlock) {
		*self.fork_point.lock().unwrap() = Some(fork_point);
		self.reorg_blocks.lock().unwrap().clear();
	}
}

pub(crate) fn rpc_credentials(rpc_user: String, rpc_password: String) -> String {
	BASE64_STANDARD.encode(format!("{}:{}", rpc_user, rpc_password))
}

pub(crate) fn endpoint(host: String, port: u16) -> HttpEndpoint {
	HttpEndpoint::for_host(host).with_port(port)
}

#[derive(Debug)]
pub struct HttpError {
	pub(crate) status_code: String,
	pub(crate) contents: Vec<u8>,
}

impl std::error::Error for HttpError {}

impl std::fmt::Display for HttpError {
	fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
		let contents = String::from_utf8_lossy(&self.contents);
		write!(f, "status_code: {}, contents: {}", self.status_code, contents)
	}
}

#[cfg(test)]
mod tests {
	use bitcoin::hashes::Hash;
	use bitcoin::{FeeRate, OutPoint, ScriptBuf, Transaction, TxIn, TxOut, Txid, Witness};
	use lightning_block_sync::http::JsonResponse;
	use proptest::arbitrary::any;
	use proptest::collection::vec;
	use proptest::{prop_assert_eq, prop_compose, proptest};
	use serde_json::json;

	use crate::chain::bitcoind::{
		should_emit_mempool_entry, BitcoindClient, FeeResponse, GetMempoolEntryResponse,
		GetRawMempoolResponse, GetRawTransactionResponse, MempoolMinFeeResponse, MempoolUpdate,
	};

	#[test]
	fn mempool_timestamp_advances_only_after_commit() {
		let client = BitcoindClient::new_rpc(
			"127.0.0.1".to_string(),
			18443,
			"user".to_string(),
			"password".to_string(),
		);
		let update = MempoolUpdate {
			transactions: Vec::new(),
			evicted_txids: Vec::new(),
			next_timestamp: Some(42),
		};

		assert_eq!(client.mempool_timestamp(), 0);
		client.commit_mempool_timestamp(update.next_timestamp);
		assert_eq!(client.mempool_timestamp(), 42);
		client.commit_mempool_timestamp(Some(21));
		assert_eq!(client.mempool_timestamp(), 42);
	}

	#[test]
	fn mempool_watermark_replays_same_second_entries() {
		assert!(should_emit_mempool_entry(42, 100, 100, 42, false));
		assert!(!should_emit_mempool_entry(41, 100, 100, 42, false));
		assert!(should_emit_mempool_entry(41, 101, 100, 42, false));
		assert!(should_emit_mempool_entry(41, 100, 100, 42, true));
	}

	#[tokio::test]
	async fn mempool_eviction_never_predates_last_seen() {
		let txid = Txid::from_byte_array([42; 32]);
		let entries = tokio::sync::Mutex::new(std::collections::HashMap::new());
		let evicted = BitcoindClient::get_evicted_mempool_txids_and_timestamp_inner(
			&entries,
			vec![(txid, 42)],
			0,
		)
		.await
		.unwrap();

		assert_eq!(evicted, vec![(txid, 42)]);
	}

	prop_compose! {
		fn arbitrary_witness()(
			witness_elements in vec(vec(any::<u8>(), 0..100), 0..20)
		) -> Witness {
			let mut witness = Witness::new();
			for element in witness_elements {
				witness.push(element);
			}
			witness
		}
	}

	prop_compose! {
		fn arbitrary_txin()(
			outpoint_hash in any::<[u8; 32]>(),
			outpoint_vout in any::<u32>(),
			script_bytes in vec(any::<u8>(), 0..100),
			witness in arbitrary_witness(),
			sequence in any::<u32>()
		) -> TxIn {
			TxIn {
				previous_output: OutPoint {
					txid: Txid::from_byte_array(outpoint_hash),
					vout: outpoint_vout,
				},
				script_sig: ScriptBuf::from_bytes(script_bytes),
				sequence: bitcoin::Sequence::from_consensus(sequence),
				witness,
			}
		}
	}

	prop_compose! {
		fn arbitrary_txout()(
			value in 0u64..21_000_000_00_000_000u64,
			script_bytes in vec(any::<u8>(), 0..100)
		) -> TxOut {
			TxOut {
				value: bitcoin::Amount::from_sat(value),
				script_pubkey: ScriptBuf::from_bytes(script_bytes),
			}
		}
	}

	prop_compose! {
		fn arbitrary_transaction()(
			version in any::<i32>(),
			inputs in vec(arbitrary_txin(), 1..20),
			outputs in vec(arbitrary_txout(), 1..20),
			lock_time in any::<u32>()
		) -> Transaction {
			Transaction {
				version: bitcoin::transaction::Version(version),
				input: inputs,
				output: outputs,
				lock_time: bitcoin::absolute::LockTime::from_consensus(lock_time),
			}
		}
	}

	proptest! {
		#![proptest_config(proptest::test_runner::Config::with_cases(20))]

		#[test]
		fn prop_get_raw_mempool_response_roundtrip(txids in vec(any::<[u8;32]>(), 0..10)) {
			let txid_vec: Vec<Txid> = txids.into_iter().map(Txid::from_byte_array).collect();
			let original = GetRawMempoolResponse(txid_vec.clone());

			let json_vec: Vec<String> = txid_vec.iter().map(|t| t.to_string()).collect();
			let json_val = serde_json::Value::Array(json_vec.iter().map(|s| json!(s)).collect());

			let resp = JsonResponse(json_val);
			let decoded: GetRawMempoolResponse = resp.try_into().unwrap();

			prop_assert_eq!(original.0.len(), decoded.0.len());

			prop_assert_eq!(original.0, decoded.0);
		}

		#[test]
		fn prop_get_mempool_entry_response_roundtrip(
			time in any::<u64>(),
			height in any::<u32>()
		) {
			let json_val = json!({
				"time": time,
				"height": height
			});

			let resp = JsonResponse(json_val);
			let decoded: GetMempoolEntryResponse = resp.try_into().unwrap();

			prop_assert_eq!(decoded.time, time);
			prop_assert_eq!(decoded.height, height);
		}

		#[test]
		fn prop_get_raw_transaction_response_roundtrip(tx in arbitrary_transaction()) {
			let hex = bitcoin::consensus::encode::serialize_hex(&tx);
			let json_val = serde_json::Value::String(hex.clone());

			let resp = JsonResponse(json_val);
			let decoded: GetRawTransactionResponse = resp.try_into().unwrap();

			prop_assert_eq!(decoded.0.compute_txid(), tx.compute_txid());
			prop_assert_eq!(decoded.0.compute_wtxid(), tx.compute_wtxid());

			prop_assert_eq!(decoded.0, tx);
		}

		#[test]
		fn prop_fee_response_roundtrip(fee_rate in any::<f64>()) {
			let fee_rate = fee_rate.abs();
			let json_val = json!({
				"feerate": fee_rate,
				"errors": serde_json::Value::Null
			});

			let resp = JsonResponse(json_val);
			let decoded: FeeResponse = resp.try_into().unwrap();

			let expected = {
				let fee_rate_sat_per_kwu = (fee_rate * 25_000_000.0).round() as u64;
				FeeRate::from_sat_per_kwu(fee_rate_sat_per_kwu)
			};
			prop_assert_eq!(decoded.0, expected);
		}

		#[test]
		fn prop_mempool_min_fee_response_roundtrip(fee_rate in any::<f64>()) {
			let fee_rate = fee_rate.abs();
			let json_val = json!({
				"mempoolminfee": fee_rate
			});

			let resp = JsonResponse(json_val);
			let decoded: MempoolMinFeeResponse = resp.try_into().unwrap();

			let expected = {
				let fee_rate_sat_per_kwu = (fee_rate * 25_000_000.0).round() as u64;
				FeeRate::from_sat_per_kwu(fee_rate_sat_per_kwu)
			};
			prop_assert_eq!(decoded.0, expected);
		}

	}
}

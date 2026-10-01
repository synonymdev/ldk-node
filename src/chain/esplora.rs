// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bdk_esplora::EsploraAsyncExt;
use bitcoin::{FeeRate, Network, Script, Transaction, Txid};
use esplora_client::AsyncClient as EsploraAsyncClient;
use lightning::chain::{Confirm, Filter, WatchedOutput};
use lightning::log_warn;
use lightning::util::ser::Writeable;
use lightning_transaction_sync::EsploraSyncClient;

use super::{
	non_final_rejection, periodically_archive_fully_resolved_monitors, BroadcastResponse,
	WalletSyncStatus,
};
use crate::config::{
	AddressTypeRuntimeConfig, Config, EsploraSyncConfig, BDK_CLIENT_CONCURRENCY,
	BDK_CLIENT_STOP_GAP, BDK_WALLET_SYNC_TIMEOUT_SECS, DEFAULT_ESPLORA_CLIENT_TIMEOUT_SECS,
	FEE_RATE_CACHE_UPDATE_TIMEOUT_SECS, LDK_WALLET_SYNC_TIMEOUT_SECS, TX_BROADCAST_TIMEOUT_SECS,
};
use crate::fee_estimator::{
	apply_post_estimation_adjustments, get_all_conf_targets, get_num_block_defaults_for_target,
	OnchainFeeEstimator,
};
use crate::io::utils::write_node_metrics;
use crate::logger::{log_bytes, log_error, log_info, log_trace, LdkLogger, Logger};
use crate::types::{ChainMonitor, ChannelManager, DynStore, Sweeper, Wallet};
use crate::{Error, NodeMetrics};

pub(super) struct EsploraChainSource {
	pub(super) sync_config: EsploraSyncConfig,
	esplora_client: EsploraAsyncClient,
	onchain_wallet_sync_status: Mutex<WalletSyncStatus>,
	tx_sync: Arc<EsploraSyncClient<Arc<Logger>>>,
	lightning_wallet_sync_status: Mutex<WalletSyncStatus>,
	fee_estimator: Arc<OnchainFeeEstimator>,
	pub(super) kv_store: Arc<DynStore>,
	pub(super) config: Arc<Config>,
	address_type_runtime_config: Arc<RwLock<AddressTypeRuntimeConfig>>,
	logger: Arc<Logger>,
	pub(super) node_metrics: Arc<RwLock<NodeMetrics>>,
}

fn classify_esplora_broadcast(result: Result<(), esplora_client::Error>) -> BroadcastResponse {
	match result {
		Ok(()) => BroadcastResponse::Accepted,
		Err(esplora_client::Error::HttpResponse { status: 400, message }) => {
			// Esplora wraps Bitcoin Core's structured sendrawtransaction error in HTTP 400.
			let rpc = message
				.strip_prefix("sendrawtransaction RPC error: ")
				.and_then(|body| serde_json::from_str::<serde_json::Value>(body).ok());
			match rpc {
				Some(rpc)
					if rpc.get("code").and_then(|c| c.as_i64()) == Some(-26)
						&& rpc
							.get("message")
							.and_then(|m| m.as_str())
							.is_some_and(|m| non_final_rejection(-26, m)) =>
				{
					BroadcastResponse::Rejected(message)
				},
				_ => BroadcastResponse::Unknown,
			}
		},
		_ => BroadcastResponse::Unknown,
	}
}

impl EsploraChainSource {
	pub(crate) fn new(
		server_url: String, headers: HashMap<String, String>, sync_config: EsploraSyncConfig,
		fee_estimator: Arc<OnchainFeeEstimator>, kv_store: Arc<DynStore>, config: Arc<Config>,
		address_type_runtime_config: Arc<RwLock<AddressTypeRuntimeConfig>>, logger: Arc<Logger>,
		node_metrics: Arc<RwLock<NodeMetrics>>,
	) -> Self {
		let mut client_builder = esplora_client::Builder::new(&server_url);
		client_builder = client_builder.timeout(DEFAULT_ESPLORA_CLIENT_TIMEOUT_SECS);

		for (header_name, header_value) in &headers {
			client_builder = client_builder.header(header_name, header_value);
		}

		let esplora_client = client_builder.build_async().unwrap();
		let tx_sync =
			Arc::new(EsploraSyncClient::from_client(esplora_client.clone(), Arc::clone(&logger)));

		let onchain_wallet_sync_status = Mutex::new(WalletSyncStatus::Completed);
		let lightning_wallet_sync_status = Mutex::new(WalletSyncStatus::Completed);
		Self {
			sync_config,
			esplora_client,
			onchain_wallet_sync_status,
			tx_sync,
			lightning_wallet_sync_status,
			fee_estimator,
			kv_store,
			config,
			address_type_runtime_config,
			logger,
			node_metrics,
		}
	}

	pub(super) async fn sync_onchain_wallet(
		&self, onchain_wallet: Arc<Wallet>,
	) -> super::WalletSyncOutcome {
		let receiver_res = {
			let mut status_lock = self.onchain_wallet_sync_status.lock().unwrap();
			status_lock.register_or_subscribe_pending_sync()
		};
		if let Some(mut sync_receiver) = receiver_res {
			log_info!(self.logger, "Sync in progress, skipping.");
			match sync_receiver.recv().await {
				Ok(Ok(())) => return super::WalletSyncOutcome::new(Vec::new(), None),
				Ok(Err(e)) => return super::WalletSyncOutcome::failed(e),
				Err(e) => {
					debug_assert!(false, "Failed to receive wallet sync result: {:?}", e);
					log_error!(self.logger, "Failed to receive wallet sync result: {:?}", e);
					return super::WalletSyncOutcome::failed(Error::WalletOperationFailed);
				},
			}
		}

		let outcome = self
			.sync_onchain_wallet_inner(onchain_wallet)
			.await
			.unwrap_or_else(super::WalletSyncOutcome::failed);

		self.onchain_wallet_sync_status
			.lock()
			.unwrap()
			.propagate_result_to_subscribers(outcome.result());

		outcome
	}

	async fn sync_onchain_wallet_inner(
		&self, onchain_wallet: Arc<Wallet>,
	) -> Result<super::WalletSyncOutcome, Error> {
		let primary_incremental =
			self.node_metrics.read().unwrap().latest_onchain_wallet_sync_timestamp.is_some();

		let additional_accounts =
			self.address_type_runtime_config.read().unwrap().additional_wallet_accounts();
		let additional_sync_requests = super::collect_additional_sync_requests(
			&additional_accounts,
			&onchain_wallet,
			&self.node_metrics,
			&self.logger,
		)?;

		let primary_request: super::WalletSyncRequest = if primary_incremental {
			super::WalletSyncRequest::Incremental(onchain_wallet.get_incremental_sync_request())
		} else {
			super::WalletSyncRequest::FullScan(onchain_wallet.get_full_scan_request())
		};

		// Primary wallet is identified by address_type = None in the JoinSet results.
		let now = Instant::now();
		type EsploraSyncResult = (
			Option<crate::config::OnchainWalletAccount>,
			Result<
				Result<bdk_wallet::Update, Box<esplora_client::Error>>,
				tokio::time::error::Elapsed,
			>,
		);
		let mut join_set: tokio::task::JoinSet<EsploraSyncResult> = tokio::task::JoinSet::new();

		let client = self.esplora_client.clone();
		match primary_request {
			super::WalletSyncRequest::Incremental(req) => {
				join_set.spawn(async move {
					let result = tokio::time::timeout(
						Duration::from_secs(BDK_WALLET_SYNC_TIMEOUT_SECS),
						client.sync(req, BDK_CLIENT_CONCURRENCY),
					)
					.await
					.map(|r| r.map(|u| bdk_wallet::Update::from(u)));
					(None, result)
				});
			},
			super::WalletSyncRequest::FullScan(req) => {
				join_set.spawn(async move {
					let result = tokio::time::timeout(
						Duration::from_secs(BDK_WALLET_SYNC_TIMEOUT_SECS),
						client.full_scan(req, BDK_CLIENT_STOP_GAP, BDK_CLIENT_CONCURRENCY),
					)
					.await
					.map(|r| r.map(|u| bdk_wallet::Update::from(u)));
					(None, result)
				});
			},
		}

		for (wallet_account, sync_req) in additional_sync_requests {
			let client = self.esplora_client.clone();
			match sync_req {
				super::WalletSyncRequest::Incremental(req) => {
					join_set.spawn(async move {
						let result = tokio::time::timeout(
							Duration::from_secs(BDK_WALLET_SYNC_TIMEOUT_SECS),
							client.sync(req, BDK_CLIENT_CONCURRENCY),
						)
						.await
						.map(|r| r.map(|u| bdk_wallet::Update::from(u)));
						(Some(wallet_account), result)
					});
				},
				super::WalletSyncRequest::FullScan(req) => {
					join_set.spawn(async move {
						let result = tokio::time::timeout(
							Duration::from_secs(BDK_WALLET_SYNC_TIMEOUT_SECS),
							client.full_scan(req, BDK_CLIENT_STOP_GAP, BDK_CLIENT_CONCURRENCY),
						)
						.await
						.map(|r| r.map(|u| bdk_wallet::Update::from(u)));
						(Some(wallet_account), result)
					});
				},
			}
		}

		let mut primary_update: Option<bdk_wallet::Update> = None;
		let mut primary_error: Option<Error> = None;
		let mut additional_results = Vec::new();
		let mut task_error = None;

		while let Some(join_result) = join_set.join_next().await {
			match join_result {
				Ok((None, Ok(Ok(update)))) => {
					primary_update = Some(update);
				},
				Ok((None, Ok(Err(e)))) => {
					match *e {
						esplora_client::Error::Reqwest(ref he) => {
							if let Some(status_code) = he.status() {
								log_error!(
									self.logger,
									"{} of primary on-chain wallet failed due to HTTP {} error: {}",
									if primary_incremental {
										"Incremental sync"
									} else {
										"Full sync"
									},
									status_code,
									he,
								);
							} else {
								log_error!(
									self.logger,
									"{} of primary on-chain wallet failed due to HTTP error: {}",
									if primary_incremental {
										"Incremental sync"
									} else {
										"Full sync"
									},
									he,
								);
							}
						},
						_ => {
							log_error!(
								self.logger,
								"{} of primary on-chain wallet failed due to Esplora error: {}",
								if primary_incremental { "Incremental sync" } else { "Full sync" },
								e
							);
						},
					}
					primary_error = Some(Error::WalletOperationFailed);
				},
				Ok((None, Err(e))) => {
					log_error!(
						self.logger,
						"{} of primary on-chain wallet timed out: {}",
						if primary_incremental { "Incremental sync" } else { "Full sync" },
						e
					);
					primary_error = Some(Error::WalletOperationTimeout);
				},
				Ok((Some(wallet_account), Ok(Ok(update)))) => {
					additional_results.push((wallet_account, Ok(update)));
				},
				Ok((Some(wallet_account), Ok(Err(e)))) => {
					log_warn!(self.logger, "Failed to sync wallet {:?}: {}", wallet_account, e);
					additional_results.push((wallet_account, Err(Error::WalletOperationFailed)));
				},
				Ok((Some(wallet_account), Err(_))) => {
					log_warn!(self.logger, "Sync timeout for wallet {:?}", wallet_account);
					additional_results.push((wallet_account, Err(Error::WalletOperationTimeout)));
				},
				Err(e) => {
					log_warn!(self.logger, "Wallet sync task panicked: {}", e);
					task_error = Some(Error::WalletOperationFailed);
				},
			};
		}

		if primary_update.is_none() && primary_error.is_none() {
			log_error!(self.logger, "Primary wallet sync task failed unexpectedly");
			primary_error = Some(Error::WalletOperationFailed);
		}
		let mut outcome = super::apply_wallet_sync_results(
			primary_update,
			primary_error,
			task_error,
			additional_results,
			&onchain_wallet,
			&self.node_metrics,
			&self.logger,
		);
		if outcome.primary_applied {
			log_info!(
				self.logger,
				"{} of primary on-chain wallet finished in {}ms.",
				if primary_incremental { "Incremental sync" } else { "Full sync" },
				now.elapsed().as_millis()
			);
		}

		if outcome.any_applied {
			if let Err(e) = onchain_wallet.update_payment_store_for_all_transactions() {
				log_error!(self.logger, "Failed to update payment store after wallet syncs: {}", e);
				outcome.error.get_or_insert(e);
			}

			let locked_node_metrics = self.node_metrics.read().unwrap();
			if let Err(e) = write_node_metrics(
				&*locked_node_metrics,
				Arc::clone(&self.kv_store),
				Arc::clone(&self.logger),
			) {
				log_error!(self.logger, "Failed to persist node metrics: {}", e);
			}
		}

		Ok(super::WalletSyncOutcome::new(outcome.events, outcome.error))
	}

	pub(super) async fn sync_lightning_wallet(
		&self, channel_manager: Arc<ChannelManager>, chain_monitor: Arc<ChainMonitor>,
		output_sweeper: Arc<Sweeper>,
	) -> Result<(), Error> {
		let receiver_res = {
			let mut status_lock = self.lightning_wallet_sync_status.lock().unwrap();
			status_lock.register_or_subscribe_pending_sync()
		};
		if let Some(mut sync_receiver) = receiver_res {
			log_info!(self.logger, "Sync in progress, skipping.");
			return sync_receiver.recv().await.map_err(|e| {
				debug_assert!(false, "Failed to receive wallet sync result: {:?}", e);
				log_error!(self.logger, "Failed to receive wallet sync result: {:?}", e);
				Error::WalletOperationFailed
			})?;
		}

		let res =
			self.sync_lightning_wallet_inner(channel_manager, chain_monitor, output_sweeper).await;

		self.lightning_wallet_sync_status.lock().unwrap().propagate_result_to_subscribers(res);

		res
	}

	async fn sync_lightning_wallet_inner(
		&self, channel_manager: Arc<ChannelManager>, chain_monitor: Arc<ChainMonitor>,
		output_sweeper: Arc<Sweeper>,
	) -> Result<(), Error> {
		let sync_cman = Arc::clone(&channel_manager);
		let sync_cmon = Arc::clone(&chain_monitor);
		let sync_sweeper = Arc::clone(&output_sweeper);
		let confirmables = vec![
			&*sync_cman as &(dyn Confirm + Sync + Send),
			&*sync_cmon as &(dyn Confirm + Sync + Send),
			&*sync_sweeper as &(dyn Confirm + Sync + Send),
		];

		let timeout_fut = tokio::time::timeout(
			Duration::from_secs(LDK_WALLET_SYNC_TIMEOUT_SECS),
			self.tx_sync.sync(confirmables),
		);
		let now = Instant::now();
		match timeout_fut.await {
			Ok(res) => match res {
				Ok(()) => {
					log_info!(
						self.logger,
						"Sync of Lightning wallet finished in {}ms.",
						now.elapsed().as_millis()
					);

					let unix_time_secs_opt =
						SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs());
					{
						let mut locked_node_metrics = self.node_metrics.write().unwrap();
						locked_node_metrics.latest_lightning_wallet_sync_timestamp =
							unix_time_secs_opt;
						write_node_metrics(
							&*locked_node_metrics,
							Arc::clone(&self.kv_store),
							Arc::clone(&self.logger),
						)?;
					}

					periodically_archive_fully_resolved_monitors(
						Arc::clone(&channel_manager),
						Arc::clone(&chain_monitor),
						Arc::clone(&self.kv_store),
						Arc::clone(&self.logger),
						Arc::clone(&self.node_metrics),
					)?;
					Ok(())
				},
				Err(e) => {
					log_error!(self.logger, "Sync of Lightning wallet failed: {}", e);
					Err(e.into())
				},
			},
			Err(e) => {
				log_error!(self.logger, "Lightning wallet sync timed out: {}", e);
				Err(Error::TxSyncTimeout)
			},
		}
	}

	pub(crate) async fn update_fee_rate_estimates(&self) -> Result<(), Error> {
		let now = Instant::now();
		let estimates = tokio::time::timeout(
			Duration::from_secs(FEE_RATE_CACHE_UPDATE_TIMEOUT_SECS),
			self.esplora_client.get_fee_estimates(),
		)
		.await
		.map_err(|e| {
			log_error!(self.logger, "Updating fee rate estimates timed out: {}", e);
			Error::FeerateEstimationUpdateTimeout
		})?
		.map_err(|e| {
			log_error!(self.logger, "Failed to retrieve fee rate estimates: {}", e);
			Error::FeerateEstimationUpdateFailed
		})?;

		if estimates.is_empty() && self.config.network == Network::Bitcoin {
			// Ensure we fail if we didn't receive any estimates.
			log_error!(
						self.logger,
						"Failed to retrieve fee rate estimates: empty fee estimates are dissallowed on Mainnet.",
					);
			return Err(Error::FeerateEstimationUpdateFailed);
		}

		let confirmation_targets = get_all_conf_targets();

		let mut new_fee_rate_cache = HashMap::with_capacity(10);
		for target in confirmation_targets {
			let num_blocks = get_num_block_defaults_for_target(target);

			// Convert the retrieved fee rate and fall back to 1 sat/vb if we fail or it
			// yields less than that. This is mostly necessary to continue on
			// `signet`/`regtest` where we might not get estimates (or bogus values).
			let converted_estimate_sat_vb =
				esplora_client::convert_fee_rate(num_blocks, estimates.clone())
					.map_or(1.0, |converted| converted.max(1.0));

			let fee_rate = FeeRate::from_sat_per_kwu((converted_estimate_sat_vb * 250.0) as u64);

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

		self.fee_estimator.set_fee_rate_cache(new_fee_rate_cache);

		log_info!(
			self.logger,
			"Fee rate cache update finished in {}ms.",
			now.elapsed().as_millis()
		);
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

	async fn broadcast_transaction_checked(
		&self, tx: &Transaction,
	) -> Result<(), esplora_client::Error> {
		// esplora-client's broadcast discards the success body. Reuse its configured client
		// and URL so acknowledgement validation preserves headers and transport settings.
		let response = self
			.esplora_client
			.client()
			.post(format!("{}/tx", self.esplora_client.url()))
			.body(bitcoin::consensus::encode::serialize_hex(tx))
			.send()
			.await?;
		let status = response.status();
		let body = response.text().await?;
		if !status.is_success() {
			return Err(esplora_client::Error::HttpResponse {
				status: status.as_u16(),
				message: body,
			});
		}
		if body.trim().parse::<Txid>().ok() != Some(tx.compute_txid()) {
			return Err(esplora_client::Error::InvalidResponse);
		}
		Ok(())
	}

	pub(crate) async fn broadcast_transaction_with_result(
		&self, tx: &Transaction,
	) -> BroadcastResponse {
		let txid = tx.compute_txid();
		match tokio::time::timeout(
			Duration::from_secs(TX_BROADCAST_TIMEOUT_SECS),
			self.broadcast_transaction_checked(tx),
		)
		.await
		{
			Ok(Ok(())) => {
				log_trace!(self.logger, "Successfully broadcast transaction {}", txid);
				BroadcastResponse::Accepted
			},
			Ok(Err(error)) => {
				match &error {
					esplora_client::Error::HttpResponse { status: 400, message } => {
						// An already-known transaction commonly produces HTTP 400.
						log_trace!(
							self.logger,
							"Failed to broadcast due to HTTP connection error: {}",
							message
						);
					},
					esplora_client::Error::HttpResponse { status, message } => {
						log_error!(
							self.logger,
							"Failed to broadcast due to HTTP connection error: {} - {}",
							status,
							message
						);
					},
					_ => log_error!(
						self.logger,
						"Failed to broadcast transaction {}: {}",
						txid,
						error
					),
				}
				log_trace!(
					self.logger,
					"Failed broadcast transaction bytes: {}",
					log_bytes!(tx.encode())
				);
				classify_esplora_broadcast(Err(error))
			},
			Err(error) => {
				log_error!(
					self.logger,
					"Failed to broadcast transaction due to timeout {}: {}",
					txid,
					error
				);
				log_trace!(
					self.logger,
					"Failed broadcast transaction bytes: {}",
					log_bytes!(tx.encode())
				);
				BroadcastResponse::Unknown
			},
		}
	}

	pub(crate) async fn process_broadcast_package(&self, package: Vec<Transaction>) {
		for tx in &package {
			let _ = self.broadcast_transaction_with_result(tx).await;
		}
	}

	pub(super) async fn get_address_balance(&self, address: &bitcoin::Address) -> Option<u64> {
		let script = address.script_pubkey();
		match self.esplora_client.scripthash_txs(&script, None).await {
			Ok(txs) => {
				let mut balance = 0i64;
				for tx in txs {
					for output in &tx.vout {
						if output.scriptpubkey == script {
							balance += output.value as i64;
						}
					}
					for input in &tx.vin {
						if let Some(prevout) = &input.prevout {
							if prevout.scriptpubkey == script {
								balance -= prevout.value as i64;
							}
						}
					}
				}
				Some(balance.max(0) as u64)
			},
			Err(_) => None,
		}
	}
}

impl Filter for EsploraChainSource {
	fn register_tx(&self, txid: &Txid, script_pubkey: &Script) {
		self.tx_sync.register_tx(txid, script_pubkey);
	}
	fn register_output(&self, output: WatchedOutput) {
		self.tx_sync.register_output(output);
	}
}

#[cfg(test)]
mod broadcast_tests {
	use std::io::{BufRead, BufReader, Read, Write};
	use std::net::TcpListener;
	use std::thread;

	use crate::io::test_utils::InMemoryStore;
	use crate::runtime::Runtime;

	use super::*;

	#[test]
	fn esplora_broadcast_http_requires_matching_txid_acknowledgement() {
		let tx = Transaction {
			version: bitcoin::transaction::Version::TWO,
			lock_time: bitcoin::absolute::LockTime::ZERO,
			input: vec![],
			output: vec![],
		};
		let txid = tx.compute_txid();
		let refusal = "sendrawtransaction RPC error: {\"code\":-26,\"message\":\"non-final\"}";
		for (name, status, body, extra_length, expected) in [
			("matching", 200, txid.to_string(), 0, BroadcastResponse::Accepted),
			("matching with newline", 201, format!("{txid}\n"), 0, BroadcastResponse::Accepted),
			("empty", 200, String::new(), 0, BroadcastResponse::Unknown),
			("no content", 204, String::new(), 0, BroadcastResponse::Unknown),
			("malformed", 200, "<html>success</html>".to_owned(), 0, BroadcastResponse::Unknown),
			("mismatched", 200, "00".repeat(32), 0, BroadcastResponse::Unknown),
			("unreadable", 200, txid.to_string(), 1, BroadcastResponse::Unknown),
			(
				"refused",
				400,
				refusal.to_owned(),
				0,
				BroadcastResponse::Rejected(refusal.to_owned()),
			),
			("unstructured refusal", 400, "non-final".to_owned(), 0, BroadcastResponse::Unknown),
			("server failure", 503, refusal.to_owned(), 0, BroadcastResponse::Unknown),
		] {
			let listener = TcpListener::bind("127.0.0.1:0").unwrap();
			let url = format!("http://{}/api", listener.local_addr().unwrap());
			let expected_body = bitcoin::consensus::encode::serialize_hex(&tx);
			let server = thread::spawn(move || {
				let (mut stream, _) = listener.accept().unwrap();
				stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
				let mut reader = BufReader::new(stream.try_clone().unwrap());
				let mut line = String::new();
				reader.read_line(&mut line).unwrap();
				assert_eq!(line, "POST /api/tx HTTP/1.1\r\n");
				let mut content_length = None;
				let mut fixture_header = false;
				loop {
					line.clear();
					reader.read_line(&mut line).unwrap();
					if line == "\r\n" {
						break;
					}
					let header = line.to_ascii_lowercase();
					if let Some(length) = header.strip_prefix("content-length: ") {
						content_length = Some(length.trim().parse::<usize>().unwrap());
					}
					fixture_header |= header == "x-broadcast-fixture: preserved\r\n";
				}
				assert!(fixture_header, "configured client headers must be preserved");
				let mut request_body = vec![0; content_length.unwrap()];
				reader.read_exact(&mut request_body).unwrap();
				assert_eq!(request_body, expected_body.as_bytes());
				write!(
					stream,
					"HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
					body.len() + extra_length
				)
				.unwrap();
				listener
			});
			let logger = Arc::new(Logger::new_log_facade());
			let runtime = Runtime::new(Arc::clone(&logger)).unwrap();
			let config = Arc::new(Config::default());
			let source = EsploraChainSource::new(
				url,
				HashMap::from([("X-Broadcast-Fixture".to_owned(), "preserved".to_owned())]),
				EsploraSyncConfig::default(),
				Arc::new(OnchainFeeEstimator::new()),
				Arc::new(InMemoryStore::new()),
				Arc::clone(&config),
				Arc::new(RwLock::new(AddressTypeRuntimeConfig::from_config(&config, vec![]))),
				logger,
				Arc::new(RwLock::new(NodeMetrics::default())),
			);
			let result = runtime.block_on(source.broadcast_transaction_with_result(&tx));
			let listener = server.join().unwrap();
			listener.set_nonblocking(true).unwrap();
			assert!(listener.accept().is_err(), "{name}: must submit only once");
			assert_eq!(result, expected, "{name}");
		}
	}

	#[test]
	fn esplora_broadcast_requires_structured_non_final_refusal() {
		assert_eq!(classify_esplora_broadcast(Ok(())), BroadcastResponse::Accepted);
		for (status, message, expected) in [
			(
				400,
				"sendrawtransaction RPC error: {\"code\":-26,\"message\":\"non-final\"}",
				BroadcastResponse::Rejected(
					"sendrawtransaction RPC error: {\"code\":-26,\"message\":\"non-final\"}"
						.to_owned(),
				),
			),
			(400, "non-final", BroadcastResponse::Unknown),
			(
				400,
				"sendrawtransaction RPC error: {\"code\":-25,\"message\":\"non-final\"}",
				BroadcastResponse::Unknown,
			),
			(
				400,
				"sendrawtransaction RPC error: {\"code\":-26,\"message\":\"missing inputs\"}",
				BroadcastResponse::Unknown,
			),
			(
				503,
				"sendrawtransaction RPC error: {\"code\":-26,\"message\":\"non-final\"}",
				BroadcastResponse::Unknown,
			),
		] {
			assert_eq!(
				classify_esplora_broadcast(Err(esplora_client::Error::HttpResponse {
					status,
					message: message.to_owned()
				})),
				expected
			);
		}
	}
}

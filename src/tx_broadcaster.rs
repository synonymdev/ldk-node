// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::ops::Deref;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bitcoin::{Transaction, Txid};
use lightning::chain::chaininterface::BroadcasterInterface;
use tokio::sync::{mpsc, oneshot, Mutex, MutexGuard};

use crate::config::TX_BROADCAST_TIMEOUT_SECS;
use crate::logger::{log_error, LdkLogger};

const EXPLICIT_BCAST_PACKAGE_QUEUE_SIZE: usize = 50;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TxBroadcastError {
	Rejected,
	NotDispatched,
	Failed,
	Timeout,
}

pub(crate) fn classify_rpc_broadcast_error(
	code: Option<i64>, message: &str,
) -> Result<(), TxBroadcastError> {
	let normalized_message = message.to_ascii_lowercase();
	let compact_message = normalized_message
		.chars()
		.filter(|character| !character.is_ascii_whitespace())
		.collect::<String>();
	let contains_code = |candidate| {
		code == Some(candidate)
			|| compact_message.contains(&format!("\"code\":{}", candidate))
			|| normalized_message.contains(&format!("rpc error {}", candidate))
	};

	if contains_code(-27)
		|| [
			"already in block chain",
			"already in blockchain",
			"already in mempool",
			"transaction already known",
			"txn-already-known",
		]
		.iter()
		.any(|marker| normalized_message.contains(marker))
	{
		return Ok(());
	}

	if contains_code(-25)
		|| contains_code(-26)
		|| [
			"bad-txns-",
			"dust",
			"insufficient fee",
			"mandatory-script-verify-flag-failed",
			"mempool min fee not met",
			"min relay fee not met",
			"missing inputs",
			"non-bip68-final",
			"non-final",
			"non-mandatory-script-verify-flag",
			"txn-mempool-conflict",
		]
		.iter()
		.any(|marker| normalized_message.contains(marker))
	{
		return Err(TxBroadcastError::Rejected);
	}

	Err(TxBroadcastError::Failed)
}

pub(crate) fn validate_broadcast_txid(
	expected_txid: Txid, returned_txid: Txid,
) -> Result<(), TxBroadcastError> {
	if returned_txid == expected_txid {
		Ok(())
	} else {
		Err(TxBroadcastError::Failed)
	}
}

const EXPLICIT_BROADCAST_QUEUED: u8 = 0;
const EXPLICIT_BROADCAST_CLAIMED: u8 = 1;
const EXPLICIT_BROADCAST_CANCELLED: u8 = 2;

struct ExplicitBroadcastClaim {
	state: AtomicU8,
}

impl ExplicitBroadcastClaim {
	fn new() -> Self {
		Self { state: AtomicU8::new(EXPLICIT_BROADCAST_QUEUED) }
	}

	fn try_claim(&self) -> bool {
		self.state
			.compare_exchange(
				EXPLICIT_BROADCAST_QUEUED,
				EXPLICIT_BROADCAST_CLAIMED,
				Ordering::AcqRel,
				Ordering::Acquire,
			)
			.is_ok()
	}

	fn cancel_if_queued(&self) -> bool {
		self.state
			.compare_exchange(
				EXPLICIT_BROADCAST_QUEUED,
				EXPLICIT_BROADCAST_CANCELLED,
				Ordering::AcqRel,
				Ordering::Acquire,
			)
			.is_ok()
	}
}

struct CancelExplicitBroadcastOnDrop {
	claim: Arc<ExplicitBroadcastClaim>,
}

impl Drop for CancelExplicitBroadcastOnDrop {
	fn drop(&mut self) {
		self.claim.cancel_if_queued();
	}
}

pub(crate) struct BroadcastRequest {
	pub(crate) package: Vec<Transaction>,
	pub(crate) result_sender: Option<oneshot::Sender<Result<(), TxBroadcastError>>>,
	explicit_claim: Option<Arc<ExplicitBroadcastClaim>>,
}

impl BroadcastRequest {
	fn explicit(
		package: Vec<Transaction>, result_sender: oneshot::Sender<Result<(), TxBroadcastError>>,
	) -> (Self, Arc<ExplicitBroadcastClaim>) {
		let explicit_claim = Arc::new(ExplicitBroadcastClaim::new());
		(
			Self {
				package,
				result_sender: Some(result_sender),
				explicit_claim: Some(Arc::clone(&explicit_claim)),
			},
			explicit_claim,
		)
	}

	fn ldk(package: Vec<Transaction>) -> Self {
		Self { package, result_sender: None, explicit_claim: None }
	}

	fn try_claim(&self) -> bool {
		self.explicit_claim.as_ref().map_or(true, |claim| claim.try_claim())
	}
}

/// Separate receivers for safety-critical LDK traffic and bounded explicit user sends.
pub(crate) struct BroadcastQueueReceivers {
	ldk_receiver: mpsc::UnboundedReceiver<BroadcastRequest>,
	explicit_receiver: mpsc::Receiver<BroadcastRequest>,
}

impl BroadcastQueueReceivers {
	/// Returns the next request, prioritizing LDK traffic when both queues are ready.
	pub(crate) async fn recv(&mut self) -> Option<BroadcastRequest> {
		loop {
			let request = tokio::select! {
				biased;
				request = self.ldk_receiver.recv() => request,
				request = self.explicit_receiver.recv() => request,
			};
			match request {
				Some(request) if request.try_claim() => return Some(request),
				Some(_) => continue,
				None => return None,
			}
		}
	}

	/// Completes queued explicit requests without dispatching them when the worker stops.
	pub(crate) fn fail_queued_explicit_requests(&mut self) {
		while let Ok(request) = self.explicit_receiver.try_recv() {
			if !request.try_claim() {
				continue;
			}
			if let Some(result_sender) = request.result_sender {
				let _ = result_sender.send(Err(TxBroadcastError::NotDispatched));
			}
		}
	}
}

pub(crate) struct TransactionBroadcaster<L: Deref>
where
	L::Target: LdkLogger,
{
	ldk_sender: mpsc::UnboundedSender<BroadcastRequest>,
	explicit_sender: mpsc::Sender<BroadcastRequest>,
	queue_receivers: Mutex<BroadcastQueueReceivers>,
	explicit_broadcasts_enabled: std::sync::Mutex<bool>,
	logger: L,
}

impl<L: Deref> TransactionBroadcaster<L>
where
	L::Target: LdkLogger,
{
	pub(crate) fn new(logger: L) -> Self {
		let (ldk_sender, ldk_receiver) = mpsc::unbounded_channel();
		let (explicit_sender, explicit_receiver) = mpsc::channel(EXPLICIT_BCAST_PACKAGE_QUEUE_SIZE);
		let queue_receivers =
			Mutex::new(BroadcastQueueReceivers { ldk_receiver, explicit_receiver });
		Self {
			ldk_sender,
			explicit_sender,
			queue_receivers,
			explicit_broadcasts_enabled: std::sync::Mutex::new(true),
			logger,
		}
	}

	/// Allows explicit broadcasts for a new node run after the prior queue was drained.
	pub(crate) fn resume_explicit_broadcasts(&self) {
		*self.explicit_broadcasts_enabled.lock().unwrap() = true;
	}

	/// Prevents new explicit broadcasts from entering the current run's queue.
	pub(crate) fn pause_explicit_broadcasts(&self) {
		*self.explicit_broadcasts_enabled.lock().unwrap() = false;
	}

	/// Completes every queued explicit request after new enqueue operations have been fenced.
	pub(crate) async fn drain_explicit_broadcasts(&self) {
		let mut receivers = self.queue_receivers.lock().await;
		receivers.fail_queued_explicit_requests();
	}

	pub(crate) async fn get_broadcast_queue_receivers(
		&self,
	) -> MutexGuard<'_, BroadcastQueueReceivers> {
		self.queue_receivers.lock().await
	}

	pub(crate) async fn broadcast_transaction(
		&self, tx: Transaction,
	) -> Result<(), TxBroadcastError> {
		self.broadcast_transaction_with_timeout(tx, Duration::from_secs(TX_BROADCAST_TIMEOUT_SECS))
			.await
	}

	async fn broadcast_transaction_with_timeout(
		&self, tx: Transaction, timeout: Duration,
	) -> Result<(), TxBroadcastError> {
		let (result_sender, result_receiver) = oneshot::channel();
		let (request, explicit_claim) = BroadcastRequest::explicit(vec![tx], result_sender);
		{
			let enabled = self.explicit_broadcasts_enabled.lock().unwrap();
			if !*enabled {
				return Err(TxBroadcastError::NotDispatched);
			}
			self.explicit_sender.try_send(request).map_err(|_| TxBroadcastError::NotDispatched)?;
		}
		let _cancel_on_drop = CancelExplicitBroadcastOnDrop { claim: Arc::clone(&explicit_claim) };
		let mut result_receiver = result_receiver;
		let receiver_result = match tokio::time::timeout(timeout, &mut result_receiver).await {
			Ok(result) => result,
			Err(_) if explicit_claim.cancel_if_queued() => {
				return Err(TxBroadcastError::NotDispatched)
			},
			Err(_) => result_receiver.await,
		};
		receiver_result.map_err(|_| TxBroadcastError::Failed)?
	}
}

impl<L: Deref> BroadcasterInterface for TransactionBroadcaster<L>
where
	L::Target: LdkLogger,
{
	fn broadcast_transactions(&self, txs: &[&Transaction]) {
		let package = txs.iter().map(|&t| t.clone()).collect::<Vec<Transaction>>();
		let request = BroadcastRequest::ldk(package);
		self.ldk_sender.send(request).unwrap_or_else(|e| {
			log_error!(self.logger, "Failed to broadcast transactions: {}", e);
		});
	}
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;
	use std::time::Duration;

	use bitcoin::absolute::LockTime;
	use bitcoin::transaction::Version;
	use bitcoin::Transaction;
	use lightning::chain::chaininterface::BroadcasterInterface;
	use lightning::util::test_utils::TestLogger;

	use super::{
		classify_rpc_broadcast_error, BroadcastRequest, TransactionBroadcaster, TxBroadcastError,
		EXPLICIT_BCAST_PACKAGE_QUEUE_SIZE,
	};

	fn test_transaction() -> Transaction {
		test_transaction_with_lock_time(0)
	}

	fn test_transaction_with_lock_time(lock_time: u32) -> Transaction {
		Transaction {
			version: Version::TWO,
			lock_time: LockTime::from_consensus(lock_time),
			input: vec![],
			output: vec![],
		}
	}

	#[test]
	fn rpc_broadcast_errors_distinguish_known_rejections_and_ambiguous_failures() {
		assert_eq!(
			classify_rpc_broadcast_error(Some(-27), "Transaction already in block chain"),
			Ok(())
		);
		assert_eq!(
			classify_rpc_broadcast_error(None, r#"sendrawtransaction: {"code": -26}"#),
			Err(TxBroadcastError::Rejected)
		);
		assert_eq!(
			classify_rpc_broadcast_error(None, "non-final"),
			Err(TxBroadcastError::Rejected)
		);
		assert_eq!(
			classify_rpc_broadcast_error(Some(-28), "Loading block index"),
			Err(TxBroadcastError::Failed)
		);
	}

	#[tokio::test]
	async fn explicit_broadcast_returns_after_backend_acceptance() {
		let broadcaster = Arc::new(TransactionBroadcaster::new(Arc::new(TestLogger::new())));
		let broadcast_fut = broadcaster.broadcast_transaction(test_transaction());
		let process_fut = async {
			let mut receivers = broadcaster.get_broadcast_queue_receivers().await;
			let request = receivers.recv().await.unwrap();
			request.result_sender.unwrap().send(Ok(())).unwrap();
		};

		let (result, ()) = tokio::join!(broadcast_fut, process_fut);
		assert_eq!(result, Ok(()));
	}

	#[tokio::test]
	async fn explicit_broadcast_propagates_backend_rejection() {
		let broadcaster = Arc::new(TransactionBroadcaster::new(Arc::new(TestLogger::new())));
		let broadcast_fut = broadcaster.broadcast_transaction(test_transaction());
		let process_fut = async {
			let mut receivers = broadcaster.get_broadcast_queue_receivers().await;
			let request = receivers.recv().await.unwrap();
			request.result_sender.unwrap().send(Err(TxBroadcastError::Rejected)).unwrap();
		};

		let (result, ()) = tokio::join!(broadcast_fut, process_fut);
		assert_eq!(result, Err(TxBroadcastError::Rejected));
	}

	#[tokio::test]
	async fn explicit_broadcast_propagates_backend_failure() {
		let broadcaster = Arc::new(TransactionBroadcaster::new(Arc::new(TestLogger::new())));
		let broadcast_fut = broadcaster.broadcast_transaction(test_transaction());
		let process_fut = async {
			let mut receivers = broadcaster.get_broadcast_queue_receivers().await;
			let request = receivers.recv().await.unwrap();
			request.result_sender.unwrap().send(Err(TxBroadcastError::Failed)).unwrap();
		};

		let (result, ()) = tokio::join!(broadcast_fut, process_fut);
		assert_eq!(result, Err(TxBroadcastError::Failed));
	}

	#[tokio::test]
	async fn claimed_explicit_broadcast_waits_for_backend_result_after_queue_timeout() {
		let broadcaster = Arc::new(TransactionBroadcaster::new(Arc::new(TestLogger::new())));
		let broadcast_fut = broadcaster
			.broadcast_transaction_with_timeout(test_transaction(), Duration::from_millis(10));
		let process_fut = async {
			let mut receivers = broadcaster.get_broadcast_queue_receivers().await;
			let request = receivers.recv().await.unwrap();
			tokio::time::sleep(Duration::from_millis(20)).await;
			request.result_sender.unwrap().send(Ok(())).unwrap();
		};

		let (result, ()) = tokio::join!(broadcast_fut, process_fut);
		assert_eq!(result, Ok(()));
	}

	#[tokio::test]
	async fn queued_explicit_broadcast_is_cancelled_before_backend_claim() {
		let broadcaster = Arc::new(TransactionBroadcaster::new(Arc::new(TestLogger::new())));
		let cancelled_result = broadcaster
			.broadcast_transaction_with_timeout(test_transaction(), Duration::from_millis(10))
			.await;
		assert_eq!(cancelled_result, Err(TxBroadcastError::NotDispatched));

		let live_tx = test_transaction_with_lock_time(1);
		let broadcast_fut =
			broadcaster.broadcast_transaction_with_timeout(live_tx.clone(), Duration::from_secs(1));
		let process_fut = async {
			let mut receivers = broadcaster.get_broadcast_queue_receivers().await;
			let request = receivers.recv().await.unwrap();
			assert_eq!(request.package, vec![live_tx]);
			request.result_sender.unwrap().send(Ok(())).unwrap();
		};

		let (result, ()) = tokio::join!(broadcast_fut, process_fut);
		assert_eq!(result, Ok(()));
	}

	#[tokio::test]
	async fn dropped_explicit_broadcast_future_cancels_queued_request() {
		let broadcaster = Arc::new(TransactionBroadcaster::new(Arc::new(TestLogger::new())));
		let cancelled_broadcaster = Arc::clone(&broadcaster);
		let cancelled_task = tokio::spawn(async move {
			cancelled_broadcaster
				.broadcast_transaction_with_timeout(test_transaction(), Duration::from_secs(1))
				.await
		});
		tokio::task::yield_now().await;
		assert_eq!(broadcaster.explicit_sender.capacity(), EXPLICIT_BCAST_PACKAGE_QUEUE_SIZE - 1);
		cancelled_task.abort();
		assert!(cancelled_task.await.unwrap_err().is_cancelled());

		let live_tx = test_transaction_with_lock_time(1);
		let broadcast_fut =
			broadcaster.broadcast_transaction_with_timeout(live_tx.clone(), Duration::from_secs(1));
		let process_fut = async {
			let mut receivers = broadcaster.get_broadcast_queue_receivers().await;
			let request = receivers.recv().await.unwrap();
			assert_eq!(request.package, vec![live_tx]);
			request.result_sender.unwrap().send(Ok(())).unwrap();
		};

		let (result, ()) = tokio::join!(broadcast_fut, process_fut);
		assert_eq!(result, Ok(()));
	}

	#[tokio::test]
	async fn stopping_worker_fails_queued_explicit_broadcast_without_dispatch() {
		let broadcaster = Arc::new(TransactionBroadcaster::new(Arc::new(TestLogger::new())));
		let broadcast_fut = broadcaster
			.broadcast_transaction_with_timeout(test_transaction(), Duration::from_secs(1));
		let stop_fut = async {
			tokio::task::yield_now().await;
			let mut receivers = broadcaster.get_broadcast_queue_receivers().await;
			receivers.fail_queued_explicit_requests();
		};

		let (result, ()) = tokio::join!(broadcast_fut, stop_fut);
		assert_eq!(result, Err(TxBroadcastError::NotDispatched));
	}

	#[tokio::test]
	async fn stopped_queue_rejects_new_requests_and_does_not_replay_them_after_restart() {
		let broadcaster = Arc::new(TransactionBroadcaster::new(Arc::new(TestLogger::new())));
		broadcaster.pause_explicit_broadcasts();
		broadcaster.drain_explicit_broadcasts().await;

		assert_eq!(
			broadcaster
				.broadcast_transaction_with_timeout(test_transaction(), Duration::from_secs(1))
				.await,
			Err(TxBroadcastError::NotDispatched)
		);

		broadcaster.resume_explicit_broadcasts();
		let live_tx = test_transaction_with_lock_time(1);
		let broadcast_fut =
			broadcaster.broadcast_transaction_with_timeout(live_tx.clone(), Duration::from_secs(1));
		let process_fut = async {
			let mut receivers = broadcaster.get_broadcast_queue_receivers().await;
			let request = receivers.recv().await.unwrap();
			assert_eq!(request.package, vec![live_tx]);
			request.result_sender.unwrap().send(Ok(())).unwrap();
		};

		let (result, ()) = tokio::join!(broadcast_fut, process_fut);
		assert_eq!(result, Ok(()));
	}

	#[tokio::test]
	async fn ldk_broadcast_remains_fire_and_forget() {
		let broadcaster = TransactionBroadcaster::new(Arc::new(TestLogger::new()));
		let tx = test_transaction();
		broadcaster.broadcast_transactions(&[&tx]);

		let mut receivers = broadcaster.get_broadcast_queue_receivers().await;
		let request = receivers.recv().await.unwrap();
		assert_eq!(request.package, vec![tx]);
		assert!(request.result_sender.is_none());
	}

	#[tokio::test]
	async fn ldk_broadcast_is_not_dropped_when_explicit_queue_is_saturated() {
		let broadcaster = TransactionBroadcaster::new(Arc::new(TestLogger::new()));
		for _ in 0..EXPLICIT_BCAST_PACKAGE_QUEUE_SIZE {
			let (result_sender, _result_receiver) = tokio::sync::oneshot::channel();
			let (request, _claim) =
				BroadcastRequest::explicit(vec![test_transaction()], result_sender);
			broadcaster.explicit_sender.try_send(request).unwrap();
		}

		assert_eq!(
			broadcaster
				.broadcast_transaction_with_timeout(test_transaction(), Duration::from_secs(1))
				.await,
			Err(TxBroadcastError::NotDispatched)
		);

		let ldk_tx = test_transaction();
		broadcaster.broadcast_transactions(&[&ldk_tx]);

		let mut receivers = broadcaster.get_broadcast_queue_receivers().await;
		let request = receivers.recv().await.unwrap();
		assert_eq!(request.package, vec![ldk_tx]);
		assert!(request.result_sender.is_none());
	}
}

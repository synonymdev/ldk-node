// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::ops::Deref;
use std::time::Duration;

use bitcoin::{Transaction, Txid};
use lightning::chain::chaininterface::BroadcasterInterface;
use tokio::sync::{mpsc, oneshot, Mutex, MutexGuard};

use crate::config::TX_BROADCAST_TIMEOUT_SECS;
use crate::error::Error;
use crate::logger::{log_error, LdkLogger};

const BCAST_PACKAGE_QUEUE_SIZE: usize = 50;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TxBroadcastError {
	Rejected,
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

impl From<TxBroadcastError> for Error {
	fn from(error: TxBroadcastError) -> Self {
		match error {
			TxBroadcastError::Rejected => Error::OnchainTxBroadcastRejected,
			TxBroadcastError::Failed => Error::OnchainTxBroadcastFailed,
			TxBroadcastError::Timeout => Error::OnchainTxBroadcastTimeout,
		}
	}
}

pub(crate) struct BroadcastRequest {
	pub(crate) package: Vec<Transaction>,
	pub(crate) result_sender: Option<oneshot::Sender<Result<(), TxBroadcastError>>>,
}

pub(crate) struct TransactionBroadcaster<L: Deref>
where
	L::Target: LdkLogger,
{
	queue_sender: mpsc::Sender<BroadcastRequest>,
	queue_receiver: Mutex<mpsc::Receiver<BroadcastRequest>>,
	logger: L,
}

impl<L: Deref> TransactionBroadcaster<L>
where
	L::Target: LdkLogger,
{
	pub(crate) fn new(logger: L) -> Self {
		let (queue_sender, queue_receiver) = mpsc::channel(BCAST_PACKAGE_QUEUE_SIZE);
		Self { queue_sender, queue_receiver: Mutex::new(queue_receiver), logger }
	}

	pub(crate) async fn get_broadcast_queue(
		&self,
	) -> MutexGuard<'_, mpsc::Receiver<BroadcastRequest>> {
		self.queue_receiver.lock().await
	}

	pub(crate) async fn broadcast_transaction(&self, tx: Transaction) -> Result<(), Error> {
		self.broadcast_transaction_with_timeout(tx, Duration::from_secs(TX_BROADCAST_TIMEOUT_SECS))
			.await
	}

	async fn broadcast_transaction_with_timeout(
		&self, tx: Transaction, timeout: Duration,
	) -> Result<(), Error> {
		let (result_sender, result_receiver) = oneshot::channel();
		let request = BroadcastRequest { package: vec![tx], result_sender: Some(result_sender) };
		let result = tokio::time::timeout(timeout, async {
			self.queue_sender.send(request).await.map_err(|_| TxBroadcastError::Failed)?;
			result_receiver.await.map_err(|_| TxBroadcastError::Failed)?
		})
		.await
		.map_err(|_| TxBroadcastError::Timeout)?;
		result.map_err(Into::into)
	}
}

impl<L: Deref> BroadcasterInterface for TransactionBroadcaster<L>
where
	L::Target: LdkLogger,
{
	fn broadcast_transactions(&self, txs: &[&Transaction]) {
		let package = txs.iter().map(|&t| t.clone()).collect::<Vec<Transaction>>();
		let request = BroadcastRequest { package, result_sender: None };
		self.queue_sender.try_send(request).unwrap_or_else(|e| {
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

	use super::{classify_rpc_broadcast_error, TransactionBroadcaster, TxBroadcastError};
	use crate::Error;

	fn test_transaction() -> Transaction {
		Transaction {
			version: Version::TWO,
			lock_time: LockTime::ZERO,
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
			let mut receiver = broadcaster.get_broadcast_queue().await;
			let request = receiver.recv().await.unwrap();
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
			let mut receiver = broadcaster.get_broadcast_queue().await;
			let request = receiver.recv().await.unwrap();
			request.result_sender.unwrap().send(Err(TxBroadcastError::Rejected)).unwrap();
		};

		let (result, ()) = tokio::join!(broadcast_fut, process_fut);
		assert_eq!(result, Err(Error::OnchainTxBroadcastRejected));
	}

	#[tokio::test]
	async fn explicit_broadcast_propagates_backend_failure() {
		let broadcaster = Arc::new(TransactionBroadcaster::new(Arc::new(TestLogger::new())));
		let broadcast_fut = broadcaster.broadcast_transaction(test_transaction());
		let process_fut = async {
			let mut receiver = broadcaster.get_broadcast_queue().await;
			let request = receiver.recv().await.unwrap();
			request.result_sender.unwrap().send(Err(TxBroadcastError::Failed)).unwrap();
		};

		let (result, ()) = tokio::join!(broadcast_fut, process_fut);
		assert_eq!(result, Err(Error::OnchainTxBroadcastFailed));
	}

	#[tokio::test]
	async fn explicit_broadcast_times_out_without_backend_result() {
		let broadcaster = Arc::new(TransactionBroadcaster::new(Arc::new(TestLogger::new())));
		let broadcast_fut = broadcaster
			.broadcast_transaction_with_timeout(test_transaction(), Duration::from_millis(10));
		let process_fut = async {
			let mut receiver = broadcaster.get_broadcast_queue().await;
			let request = receiver.recv().await.unwrap();
			tokio::time::sleep(Duration::from_millis(20)).await;
			drop(request);
		};

		let (result, ()) = tokio::join!(broadcast_fut, process_fut);
		assert_eq!(result, Err(Error::OnchainTxBroadcastTimeout));
	}

	#[tokio::test]
	async fn ldk_broadcast_remains_fire_and_forget() {
		let broadcaster = TransactionBroadcaster::new(Arc::new(TestLogger::new()));
		let tx = test_transaction();
		broadcaster.broadcast_transactions(&[&tx]);

		let mut receiver = broadcaster.get_broadcast_queue().await;
		let request = receiver.recv().await.unwrap();
		assert_eq!(request.package, vec![tx]);
		assert!(request.result_sender.is_none());
	}
}

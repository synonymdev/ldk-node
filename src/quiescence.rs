// This file is licensed under the Apache License, Version 2.0 or the MIT license.

//! Native operation accounting. Completion belongs to the original owner, not its waiter.

use lightning::io;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Why the original node cannot yet certify a complete storage drain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShutdownError {
	/// Work is still running. Keep this same shutdown owner and wait again.
	TimedOut,
	/// A persistence operation failed or has an uncertain acknowledgement.
	PersistenceFailed,
	/// Native work panicked or was cancelled before reporting completion.
	TaskFailed,
}

impl std::fmt::Display for ShutdownError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(
			f,
			"{}",
			match self {
				Self::TimedOut => "original native work is still running",
				Self::PersistenceFailed => "original persistence did not complete successfully",
				Self::TaskFailed => "original native work did not complete normally",
			}
		)
	}
}

impl std::error::Error for ShutdownError {}

#[derive(Default)]
struct WorkState {
	closed: bool,
	active: usize,
	failure: Option<ShutdownError>,
}

/// The admission check and lifetime increment use the same lock as permanent closure.
#[derive(Default)]
pub(crate) struct WorkTracker {
	state: Mutex<WorkState>,
	changed: Condvar,
}

impl WorkTracker {
	pub(crate) fn admit(self: &Arc<Self>) -> io::Result<WorkPermit> {
		let mut state = self.state.lock().unwrap();
		if state.closed {
			return Err(io::Error::new(io::ErrorKind::BrokenPipe, "native admission is closed"));
		}
		state.active += 1;
		Ok(WorkPermit { tracker: Arc::clone(self), completed: false })
	}

	pub(crate) fn record_failure(&self, error: ShutdownError) {
		self.state.lock().unwrap().failure.get_or_insert(error);
	}

	pub(crate) fn close(&self) {
		self.state.lock().unwrap().closed = true;
		self.changed.notify_all();
	}

	pub(crate) fn wait(&self, timeout: Duration) -> Result<(), ShutdownError> {
		let deadline = Instant::now().checked_add(timeout).ok_or(ShutdownError::TimedOut)?;
		let mut state = self.state.lock().unwrap();
		// An idle sample is not a drain while new operations can still enter.
		if !state.closed {
			return Err(ShutdownError::TimedOut);
		}
		while state.active != 0 {
			let remaining = deadline.saturating_duration_since(Instant::now());
			if remaining.is_zero() {
				return Err(ShutdownError::TimedOut);
			}
			state = self.changed.wait_timeout(state, remaining).unwrap().0;
		}
		state.failure.map_or(Ok(()), Err)
	}
}

pub(crate) struct WorkPermit {
	tracker: Arc<WorkTracker>,
	completed: bool,
}

impl WorkPermit {
	pub(crate) fn record_failure(&self, error: ShutdownError) {
		self.tracker.record_failure(error);
	}

	pub(crate) fn complete(mut self, failure: Option<ShutdownError>) {
		if let Some(failure) = failure {
			self.tracker.state.lock().unwrap().failure.get_or_insert(failure);
		}
		self.completed = true;
	}
}

impl Drop for WorkPermit {
	fn drop(&mut self) {
		let mut state = self.tracker.state.lock().unwrap();
		if !self.completed {
			state.failure.get_or_insert(ShutdownError::TaskFailed);
		}
		state.active -= 1;
		self.tracker.changed.notify_all();
	}
}

/// Owns a node permanently removed from its public mutation APIs.
///
/// Construct this with [`crate::Node::try_into_shutdown`] after dropping all payment and
/// liquidity handles. Keep this owner across timeouts: waiting again never recreates the node
/// or replays a storage operation. A successful wait proves drainage only for this owner;
/// it is not a backup, a remote migration receipt, or permission to construct another writer.
/// Dropping this owner without success does not prove drainage.
pub struct NodeShutdown {
	node: Box<crate::Node>,
	phase: ShutdownPhase,
	failure: Option<ShutdownError>,
	#[cfg(test)]
	after_background_disconnect: Option<Box<dyn FnOnce() + Send>>,
}

enum ShutdownPhase {
	Background,
	NativeChain,
	NativePeers,
	Processor,
	Store,
	Complete,
}

impl NodeShutdown {
	pub(crate) fn begin(node: crate::Node) -> Self {
		let failure = node
			.background_processor_failed
			.load(Ordering::Acquire)
			.then_some(ShutdownError::PersistenceFailed);
		*node.is_running.write().unwrap() = false;
		node.background_processor_generation.fetch_add(1, Ordering::AcqRel);
		node.strict_shutdown.store(true, Ordering::Release);
		node.runtime.close_task_admission();
		node.connection_manager.close_admission();
		node.chain_source.begin_shutdown();
		let _ = node.stop_sender.send(());
		node.peer_manager.disconnect_all_peers();
		Self {
			node: Box::new(node),
			phase: ShutdownPhase::Background,
			failure,
			#[cfg(test)]
			after_background_disconnect: None,
		}
	}

	/// Wait up to `timeout` for the original tasks, callbacks and persistence operations.
	///
	/// Timeout never aborts native work. Persistence and task failures remain sticky even
	/// after every task exits; another wait cannot turn an unknown write into success.
	/// Call from a synchronous thread or a multithreaded Tokio runtime, as with `Node::stop`.
	/// This Rust source API is not exported by the mobile bindings.
	///
	/// ```no_run
	/// # fn example(node: ldk_node::Node) {
	/// if let Ok(mut shutdown) = node.try_into_shutdown() {
	///     let result = shutdown.wait(std::time::Duration::from_secs(30));
	///     // Retain `shutdown` if the result is a timeout. Never infer a backup from this result.
	///     let _ = result;
	/// }
	/// # }
	/// ```
	pub fn wait(&mut self, timeout: Duration) -> Result<(), ShutdownError> {
		let deadline = Instant::now().checked_add(timeout).ok_or(ShutdownError::TimedOut)?;
		loop {
			let remaining = deadline.saturating_duration_since(Instant::now());
			let result = match self.phase {
				ShutdownPhase::Background => {
					// An already admitted handshake may finish after the first disconnect.
					// Keep disconnecting only those original peers while their tasks drain.
					self.node.peer_manager.disconnect_all_peers();
					#[cfg(test)]
					if let Some(after_disconnect) = self.after_background_disconnect.take() {
						after_disconnect();
					}
					match self
						.node
						.runtime
						.drain_background(remaining.min(Duration::from_millis(10)))
					{
						Err(ShutdownError::TimedOut) if Instant::now() < deadline => continue,
						result => result,
					}
				},
				ShutdownPhase::NativeChain => self.node.chain_source.drain_native_calls(remaining),
				ShutdownPhase::NativePeers => self.node.connection_manager.drain(remaining),
				ShutdownPhase::Processor => self.node.runtime.drain_processor(remaining),
				ShutdownPhase::Store => self
					.node
					.shutdown_store
					.as_ref()
					.expect("shutdown requires its original VSS store")
					.close_and_drain(remaining),
				ShutdownPhase::Complete => {
					return self
						.failure
						.map_or_else(|| self.node.runtime.drain_background(Duration::ZERO), Err)
				},
			};
			match result {
				Err(ShutdownError::TimedOut) => return Err(ShutdownError::TimedOut),
				Err(error) => {
					self.failure.get_or_insert(error);
				},
				Ok(()) => {},
			}
			self.phase = match self.phase {
				ShutdownPhase::Background => {
					self.node.peer_manager.disconnect_all_peers();
					ShutdownPhase::NativeChain
				},
				ShutdownPhase::NativeChain => ShutdownPhase::NativePeers,
				ShutdownPhase::NativePeers => {
					self.node.chain_source.stop();
					let _ = self.node.background_processor_stop_sender.send(());
					ShutdownPhase::Processor
				},
				ShutdownPhase::Processor => ShutdownPhase::Store,
				ShutdownPhase::Store | ShutdownPhase::Complete => ShutdownPhase::Complete,
			};
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use proptest::prelude::*;

	#[test]
	fn closure_waits_for_original_work_and_refuses_new_admission() {
		let tracker = Arc::new(WorkTracker::default());
		let permit = tracker.admit().unwrap();
		assert_eq!(tracker.wait(Duration::ZERO), Err(ShutdownError::TimedOut));
		tracker.close();
		assert!(tracker.admit().is_err());
		assert_eq!(tracker.wait(Duration::ZERO), Err(ShutdownError::TimedOut));
		permit.complete(None);
		assert_eq!(tracker.wait(Duration::ZERO), Ok(()));
		assert_eq!(tracker.wait(Duration::ZERO), Ok(()));
		// A result on one lifetime never closes a replacement owner.
		assert_eq!(WorkTracker::default().wait(Duration::ZERO), Err(ShutdownError::TimedOut));
	}

	proptest! {
		#[test]
		fn interrupted_or_failed_work_never_becomes_a_success(count in 1usize..32, failed in 0usize..32, persistence in any::<bool>()) {
			let tracker = Arc::new(WorkTracker::default());
			let permits: Vec<_> = (0..count).map(|_| tracker.admit().unwrap()).collect();
			tracker.close();
			for (i, permit) in permits.into_iter().enumerate() {
				if i == failed % count {
					if persistence { permit.complete(Some(ShutdownError::PersistenceFailed)); }
					else { drop(permit); }
				} else { permit.complete(None); }
			}
			let error = if persistence { ShutdownError::PersistenceFailed } else { ShutdownError::TaskFailed };
			prop_assert_eq!(tracker.wait(Duration::ZERO), Err(error));
			prop_assert_eq!(tracker.wait(Duration::ZERO), Err(error));
		}
	}
}

#[cfg(test)]
#[path = "quiescence_tests.rs"]
mod native_tests;

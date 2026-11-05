// This file is licensed under the Apache License, Version 2.0 or the MIT license.

//! Transfer generated, reference-counted handles into the original consuming shutdown owner.

use crate::{Node, NodeShutdown, ShutdownError};
use std::sync::{Arc, Mutex, TryLockError};
use std::time::{Duration, Instant};

/// Why a retained shutdown handle cannot certify completion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShutdownHandleError {
	/// The node does not own its runtime or does not use the built-in VSS store.
	Unsupported,
	/// Original node references, calls, payment handles or prepared sends remain live.
	PendingHandles,
	/// Another waiter owns the transition. Retain the same handle and try again later.
	Busy,
	/// Native work remains live. Waiting again does not cancel or replay that work.
	TimedOut,
	/// Original persistence failed or has an uncertain acknowledgement.
	PersistenceFailed,
	/// Original native work did not complete normally.
	TaskFailed,
	/// A handoff panicked. Completion cannot be certified by this handle.
	Poisoned,
}

impl std::fmt::Display for ShutdownHandleError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(match self {
			Self::Unsupported => "shutdown requires an owned runtime and built-in VSS store",
			Self::PendingHandles => "original node handles or calls remain live",
			Self::Busy => "another original-node shutdown waiter is active",
			Self::TimedOut => "original native work is still running",
			Self::PersistenceFailed => "original persistence did not complete successfully",
			Self::TaskFailed => "original native work did not complete normally",
			Self::Poisoned => "original-node shutdown handoff panicked",
		})
	}
}

impl std::error::Error for ShutdownHandleError {}

impl From<ShutdownError> for ShutdownHandleError {
	fn from(error: ShutdownError) -> Self {
		match error {
			ShutdownError::TimedOut => Self::TimedOut,
			ShutdownError::PersistenceFailed => Self::PersistenceFailed,
			ShutdownError::TaskFailed => Self::TaskFailed,
		}
	}
}

/// Retains one original node across foreign-handle release, timeouts and native failures.
///
/// Created by [`Node::prepare_shutdown`]. Preparation does not close application admission.
/// The caller must keep its lifecycle fence closed, finish admitted work and event listeners,
/// then release all Node aliases and payment handles before waiting. Preserve genuine pending
/// sends until they finish; never discard them to obtain a shutdown result. Different foreign
/// wrappers returned by preparation share this same native owner.
///
/// Dropping this handle is not a drain receipt. Success covers only this original node, not
/// another writer, a complete backup, remote migration, or permission to start a replacement.
pub struct NodeShutdownHandle {
	// None exists only while a locked transition owns the original value. A panic poisons
	// the mutex; neither an empty slot nor poison can be mistaken for successful drainage.
	state: Mutex<Option<HandoffState>>,
}

enum HandoffState {
	Shared(Arc<Node>),
	Exclusive(Box<Node>),
	Draining(NodeShutdown),
}

impl Node {
	/// Prepare a retained shutdown handle without stopping or changing the original node.
	///
	/// Requires the built-in VSS store and a node-owned runtime. Unsupported nodes remain
	/// usable. Repeated preparation returns the same live native owner. After closing its
	/// own lifecycle admission, the caller must finish admitted work and release its Node
	/// and child aliases. Preparation and foreign wrapper destruction alone prove nothing.
	///
	/// ```no_run
	/// # fn example(node: std::sync::Arc<ldk_node::Node>) -> Result<(), ldk_node::ShutdownHandleError> {
	/// let shutdown = node.clone().prepare_shutdown()?;
	/// // Close application admission and finish listeners/operations before releasing aliases.
	/// drop(node);
	/// let result = shutdown.wait(30_000);
	/// // Retain `shutdown` and the lifecycle fence if completion is not certified.
	/// # let _ = result;
	/// # Ok(()) }
	/// ```
	pub fn prepare_shutdown(
		self: Arc<Self>,
	) -> Result<Arc<NodeShutdownHandle>, ShutdownHandleError> {
		if !self.runtime.owns_runtime() || self.shutdown_store.is_none() {
			return Err(ShutdownHandleError::Unsupported);
		}
		let mut cached = self.shutdown_handoff.lock().map_err(|_| ShutdownHandleError::Poisoned)?;
		if let Some(owner) = cached.upgrade() {
			return Ok(owner);
		}
		let owner = Arc::new(NodeShutdownHandle {
			state: Mutex::new(Some(HandoffState::Shared(Arc::clone(&self)))),
		});
		*cached = Arc::downgrade(&owner);
		Ok(owner)
	}
}

impl NodeShutdownHandle {
	/// Try exclusive ownership, then wait up to `timeout_millis` for original native work.
	///
	/// Live aliases or children return `PendingHandles` immediately without stopping the
	/// node. A concurrent waiter returns `Busy`. Neither outcome cancels work. Once exclusive,
	/// the original consuming shutdown continues on this same handle across timeouts; native
	/// failures remain sticky. Call off the UI thread, on a synchronous thread or a multithreaded
	/// Tokio runtime. The timeout inherits [`NodeShutdown::wait`]'s scheduling semantics.
	pub fn wait(&self, timeout_millis: u64) -> Result<(), ShutdownHandleError> {
		let deadline = Instant::now()
			.checked_add(Duration::from_millis(timeout_millis))
			.ok_or(ShutdownHandleError::TimedOut)?;
		let mut state = self.state.try_lock().map_err(|error| match error {
			TryLockError::WouldBlock => ShutdownHandleError::Busy,
			TryLockError::Poisoned(_) => ShutdownHandleError::Poisoned,
		})?;
		let original = state.take().ok_or(ShutdownHandleError::Poisoned)?;
		let original = match original {
			HandoffState::Shared(node) => match Arc::try_unwrap(node) {
				Ok(node) => HandoffState::Exclusive(Box::new(node)),
				Err(node) => {
					*state = Some(HandoffState::Shared(node));
					return Err(ShutdownHandleError::PendingHandles);
				},
			},
			original => original,
		};
		let owner = match original {
			HandoffState::Exclusive(node) => match node.prepare_shutdown_owner() {
				Ok(owner) => owner,
				Err(node) => {
					*state = Some(HandoffState::Exclusive(node));
					return Err(ShutdownHandleError::PendingHandles);
				},
			},
			HandoffState::Draining(owner) => owner,
			HandoffState::Shared(_) => unreachable!("shared ownership was handled above"),
		};
		*state = Some(HandoffState::Draining(owner));
		// Keep the original in the poisoned mutex if native entry or waiting unwinds.
		match state.as_mut() {
			Some(HandoffState::Draining(owner)) => {
				owner.wait(deadline.saturating_duration_since(Instant::now())).map_err(Into::into)
			},
			_ => unreachable!("original shutdown owner was retained above"),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn poisoned_handoff_never_certifies_completion() {
		let owner = Arc::new(NodeShutdownHandle { state: Mutex::new(None) });
		let waiting = owner.clone();
		assert!(std::thread::spawn(move || {
			let _state = waiting.state.lock().unwrap();
			panic!("controlled transition panic");
		})
		.join()
		.is_err());
		assert_eq!(owner.wait(0), Err(ShutdownHandleError::Poisoned));
		assert_eq!(owner.wait(100), Err(ShutdownHandleError::Poisoned));
	}
}

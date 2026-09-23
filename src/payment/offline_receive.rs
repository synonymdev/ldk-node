// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Experimental offline-receive (FFOR) requests.
//!
//! Every method returns [`Error::OfflineReceiveDisabled`] unless the builder's
//! `set_offline_receive_config` was called before the node was built. Readiness is reported
//! only after native durable readiness and publication in this process; ordinary invoices are
//! never presented as offline-capable.

use std::sync::{Arc, RwLock};

use crate::error::Error;
use crate::ffor::runtime::FforReceiverRuntime;

/// Terminal outcome of a closed offline-receive epoch, reported from the native journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OfflineReceiveOutcome {
	/// The voucher was fulfilled and the payment was credited exactly once.
	Fulfilled,
	/// The voucher was removed as a failure; nothing was credited.
	Failed,
}

/// Progress of one offline-receive request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OfflineReceiveStatus {
	/// The durable intent exists; native preparation has not completed.
	Preparing,
	/// Native setup is in progress with the settlement peer.
	AwaitingActivation,
	/// The epoch is active; witness acknowledgements or route evidence are outstanding.
	AwaitingWitnesses,
	/// The exact invoice was retained, confirmed and released in this process.
	Ready {
		/// The BOLT 11 invoice to present to the payer.
		bolt11: String,
	},
	/// The invoice or settlement window expired without a fulfilled voucher.
	Expired,
	/// The epoch closed with a journaled outcome.
	Settled {
		/// The journaled outcome.
		outcome: OfflineReceiveOutcome,
	},
	/// The request failed permanently.
	Failed {
		/// A short reason.
		reason: String,
	},
}

/// A handler for experimental offline-receive requests.
///
/// Should be retrieved by calling [`Node::offline_receive`].
///
/// [`Node::offline_receive`]: crate::Node::offline_receive
pub struct OfflineReceivePayment {
	runtime: Option<Arc<FforReceiverRuntime>>,
	is_running: Arc<RwLock<bool>>,
}

impl OfflineReceivePayment {
	pub(crate) fn new(
		runtime: Option<Arc<FforReceiverRuntime>>, is_running: Arc<RwLock<bool>>,
	) -> Self {
		Self { runtime, is_running }
	}

	fn runtime(&self) -> Result<&Arc<FforReceiverRuntime>, Error> {
		self.runtime.as_ref().ok_or(Error::OfflineReceiveDisabled)
	}

	fn running_runtime(&self) -> Result<&Arc<FforReceiverRuntime>, Error> {
		let runtime = self.runtime()?;
		if !*self.is_running.read().unwrap() {
			return Err(Error::NotRunning);
		}
		Ok(runtime)
	}

	/// Whether an exact `amount_msat` can be received offline right now: the runtime recovered,
	/// one ready channel with the configured settlement peer has enough inbound capacity, and no
	/// other request or historical epoch occupies that channel.
	pub fn can_receive(&self, amount_msat: u64) -> Result<bool, Error> {
		Ok(self.running_runtime()?.can_receive(amount_msat)?)
	}

	/// Begin or resume the request identified by `request_id`. Idempotent for identical
	/// arguments; different arguments for a known ID are refused.
	pub fn prepare(
		&self, request_id: String, amount_msat: u64, description: String,
	) -> Result<OfflineReceiveStatus, Error> {
		Ok(self.running_runtime()?.prepare(request_id, amount_msat, description)?)
	}

	/// Current status. `Ready` carries the invoice only after it was released in this process.
	pub fn status(&self, request_id: String) -> Result<OfflineReceiveStatus, Error> {
		Ok(self.runtime()?.status(&request_id)?)
	}

	/// Cancel a request. Before activation the native setup is cancelled; afterwards the epoch
	/// is closed cooperatively and its outcome is still joined.
	pub fn cancel(&self, request_id: String) -> Result<(), Error> {
		Ok(self.running_runtime()?.cancel(&request_id)?)
	}
}

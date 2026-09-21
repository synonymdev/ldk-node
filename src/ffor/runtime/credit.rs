//! The idempotent credit sequence for one fulfilled voucher.
//!
//! `Intended` is written before the payment row changes, `Credited` after the successful
//! payment write, and `Notified` only after the `PaymentReceived` event is durably queued. A
//! crash between any two writes resumes from the persisted state. If the queue already holds
//! the event when resuming from `Credited`, it is not queued again. An event handled by the
//! application between queueing and the `Notified` write can still be delivered twice; that
//! window is documented, not hidden.

use std::sync::Arc;

use lightning::ln::channelmanager::PaymentId;
use lightning_types::payment::PaymentPreimage;

use super::ledger::{IntentState, LedgerError, OutcomeIntent, OutcomeLedger};
use super::RuntimeError;
use crate::data_store::DataStoreUpdateResult;
use crate::event::{Event, EventQueue};
use crate::logger::Logger;
use crate::payment::store::{PaymentDetailsUpdate, PaymentStatus};
use crate::types::PaymentStore;

/// Advance the intent to `Credited`. Returns the event that must be queued before
/// [`mark_notified`], or `None` when nothing remains to queue.
pub(super) fn credit_intent(
	ledger: &OutcomeLedger, payments: &PaymentStore, event_queue: &EventQueue<Arc<Logger>>,
	intent: &OutcomeIntent, preimage: Option<PaymentPreimage>,
) -> Result<Option<(OutcomeIntent, Event)>, RuntimeError> {
	let mut current = match ledger.load(&intent.key)? {
		Some(existing) => {
			if existing.payment_hash != intent.payment_hash
				|| existing.amount_msat != intent.amount_msat
			{
				return Err(RuntimeError::Conflict);
			}
			existing
		},
		None => {
			let fresh = OutcomeIntent { state: IntentState::Intended, ..intent.clone() };
			ledger.write(&fresh)?;
			fresh
		},
	};
	if current.state == IntentState::Intended {
		let update = PaymentDetailsUpdate {
			preimage: preimage.map(Some),
			amount_msat: Some(Some(current.amount_msat)),
			status: Some(PaymentStatus::Succeeded),
			..PaymentDetailsUpdate::new(PaymentId(current.payment_hash.0))
		};
		match payments.update(&update) {
			Ok(DataStoreUpdateResult::Updated | DataStoreUpdateResult::Unchanged) => {},
			Ok(DataStoreUpdateResult::NotFound) => return Err(RuntimeError::NotFound),
			Err(_) => return Err(RuntimeError::Ledger(LedgerError::Storage)),
		}
		current.state = IntentState::Credited;
		ledger.write(&current)?;
	}
	if current.state == IntentState::Credited {
		if event_queue.contains_payment_received(&current.payment_hash) {
			return mark_notified(ledger, &current).map(|()| None);
		}
		let event = Event::PaymentReceived {
			payment_id: Some(PaymentId(current.payment_hash.0)),
			payment_hash: current.payment_hash,
			amount_msat: current.amount_msat,
			custom_records: Vec::new(),
		};
		return Ok(Some((current, event)));
	}
	Ok(None)
}

/// Persist the terminal `Notified` state after the event was durably queued.
pub(super) fn mark_notified(
	ledger: &OutcomeLedger, intent: &OutcomeIntent,
) -> Result<(), RuntimeError> {
	let notified = OutcomeIntent { state: IntentState::Notified, ..intent.clone() };
	ledger.write(&notified)?;
	Ok(())
}

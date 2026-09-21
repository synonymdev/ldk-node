//! Native invoice issuance joined to protected storage and exact Pending payment confirmation.
//!
//! Order for one request: durable policy reservation, native preparation, native persistence,
//! exact invoice retention in the protected record, exact Pending payment confirmation, protected
//! confirmation marker, and only then a handle whose release rechecks everything under the payment
//! store exclusion and the native monitor guard. Amount, hash, description and route terms come
//! from the record and native ownership; the caller supplies only a policy and route evidence.
//!
//! `AwaitingPersistence` covers every pending native manager or monitor write. No production
//! completer for native persistence tokens exists yet; the runtime that drives the background
//! persister remains separate work. Nothing here emits payment events or credits a payment.

use std::fmt;

use lightning::ln::ffor::{
	FFORCommitmentError, FFORInvoiceIntent, FFORReceiverError, FFORReceiverRecoveryContext,
	FFORStoredInvoice, FFORWitnessRouteEvidence,
};
use lightning_invoice::Bolt11Invoice;

use super::record::invoice::{InvoicePolicy, RetainedInvoice};
use super::record::RetainedSelector;
use super::{RequestStore, RequestStoreError, StoredRequest};
use crate::payment::store::PaymentDetails;

/// Progress of one request's invoice after a successful owner step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ffor) enum InvoiceProgress {
	/// Native retains the assignment but its manager write is not complete. Retry later.
	AwaitingPersistence,
	/// The exact invoice, its Pending payment and the confirmation marker are durable.
	Retained,
}

/// Opaque publication handle bound to one owner instance and one manager instance.
///
/// No method returns invoice bytes. Release rechecks the protected record, native state, the
/// payment row and the actual monitor before publishing once into the caller's slot.
pub(in crate::ffor) struct ConfirmedInvoice {
	owner: u64,
	client_id: String,
	selector: RetainedSelector,
	context_digest: [u8; 32],
	invoice_digest: [u8; 32],
	stored: FFORStoredInvoice,
	payment: PaymentDetails,
}

impl fmt::Debug for ConfirmedInvoice {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("ConfirmedInvoice")
			.field("client_id", &self.client_id)
			.field("payment_id", &self.payment.id)
			.field("invoice_digest", &self.invoice_digest)
			.finish_non_exhaustive()
	}
}

enum NativeInvoice {
	Awaiting,
	Missing,
	Stored(Box<FFORStoredInvoice>),
}

impl RequestStore {
	/// Reserve the policy durably, then obtain, retain and confirm the exact native invoice.
	///
	/// Retry with the same policy is idempotent. A different policy is a conflict; the reserved
	/// policy is never replaced. Fresh route evidence may be supplied on every retry because an
	/// existing native assignment is recovered before any new preparation is attempted.
	pub(in crate::ffor) fn issue_invoice(
		&mut self, client_id: &str, policy: InvoicePolicy, route: &FFORWitnessRouteEvidence,
	) -> Result<InvoiceProgress, RequestStoreError> {
		let record = self.lookup(client_id)?.ok_or(RequestStoreError::Missing)?;
		let record = self.reserve_policy(client_id, record, policy)?;
		let context = self.recovery_context(&record)?;
		let intent = native_intent(&record, policy);
		let mut prepared = None;
		if record.invoice().is_none() {
			if let NativeInvoice::Missing = self.fetch_stored(&context)? {
				match self.manager.prepare_ffor_receiver_invoice(&context, &intent, route) {
					Ok(preparation) => {
						if !self
							.manager
							.is_ffor_state_persisted(preparation.persistence_requirement())
						{
							return Ok(InvoiceProgress::AwaitingPersistence);
						}
						prepared = Some(preparation.invoice_digest());
					},
					Err(FFORReceiverError::ChannelState(FFORCommitmentError::PendingUpdates)) => {
						return Ok(InvoiceProgress::AwaitingPersistence);
					},
					Err(error) => return Err(RequestStoreError::Native(error)),
				}
			}
		}
		self.join_native(client_id, record, &context, &intent, prepared)
	}

	/// Rejoin a native assignment after restart or a lost application write, without a route.
	/// None means the record reserved no policy and native holds no assignment for it.
	pub(in crate::ffor) fn recover_invoice(
		&mut self, client_id: &str,
	) -> Result<Option<InvoiceProgress>, RequestStoreError> {
		let record = self.lookup(client_id)?.ok_or(RequestStoreError::Missing)?;
		let Some(policy) = record.invoice_policy() else {
			return Ok(None);
		};
		if record.selector().is_none() {
			return Ok(None);
		}
		let context = self.recovery_context(&record)?;
		let intent = native_intent(&record, policy);
		if record.invoice().is_none() {
			if let NativeInvoice::Missing = self.fetch_stored(&context)? {
				return Ok(None);
			}
		}
		self.join_native(client_id, record, &context, &intent, None).map(Some)
	}

	/// Mint a handle only when the record, native assignment and Pending payment all agree. The
	/// exact payment row is confirmed again through a successful write before the handle exists.
	pub(in crate::ffor) fn confirmed_invoice(
		&mut self, client_id: &str,
	) -> Result<Option<ConfirmedInvoice>, RequestStoreError> {
		let record = self.lookup(client_id)?.ok_or(RequestStoreError::Missing)?;
		let Some(policy) = record.invoice_policy() else {
			return Ok(None);
		};
		let Some(retained) = record.invoice().cloned() else {
			return Ok(None);
		};
		if !retained.payment_confirmed() {
			return Ok(None);
		}
		let context = self.recovery_context(&record)?;
		let stored = match self.fetch_stored(&context)? {
			NativeInvoice::Awaiting => return Ok(None),
			NativeInvoice::Missing => return Err(RequestStoreError::MissingNative),
			NativeInvoice::Stored(stored) => *stored,
		};
		check_agreement(&stored, &retained, &context, &native_intent(&record, policy))?;
		let record = self.confirm_pending(client_id, record)?;
		let selector = record.selector().ok_or(RequestStoreError::MissingNative)?;
		Ok(Some(ConfirmedInvoice {
			owner: self.instance,
			client_id: client_id.to_owned(),
			selector,
			context_digest: retained.context_digest(),
			invoice_digest: retained.digest(),
			stored,
			payment: retained.payment()?,
		}))
	}

	/// Publish once into `slot` under the payment store exclusion and the native monitor guard.
	///
	/// Returns Ok(false) when native declined without error. Every refusal leaves the assignment,
	/// record and payment row unchanged; the caller may retry with the same or a fresh handle.
	/// The innermost callback only moves the exact bytes into the slot and performs no I/O.
	pub(in crate::ffor) fn release_invoice(
		&mut self, handle: &ConfirmedInvoice, slot: &mut Option<String>,
	) -> Result<bool, RequestStoreError> {
		if handle.owner != self.instance {
			return Err(RequestStoreError::Conflict);
		}
		let record = self.lookup(&handle.client_id)?.ok_or(RequestStoreError::Missing)?;
		let policy = record.invoice_policy().ok_or(RequestStoreError::Unreserved)?;
		let retained = record.invoice().ok_or(RequestStoreError::Missing)?;
		if !retained.payment_confirmed()
			|| retained.digest() != handle.invoice_digest
			|| retained.context_digest() != handle.context_digest
			|| record.selector() != Some(handle.selector)
			|| retained.payment()? != handle.payment
		{
			return Err(RequestStoreError::Conflict);
		}
		let context = self.recovery_context(&record)?;
		check_agreement(&handle.stored, retained, &context, &native_intent(&record, policy))?;
		if handle.stored.invoice_digest() != handle.invoice_digest {
			return Err(RequestStoreError::Conflict);
		}
		let manager = &self.manager;
		let mut published = None;
		let outcome = self
			.payments
			.with_ffor_pending(&handle.payment, || {
				manager.release_ffor_receiver_invoice(&handle.stored, |wire| {
					published = Some(wire.to_owned());
					Ok(())
				})
			})
			.map_err(RequestStoreError::Payment)?;
		let released = outcome.map_err(RequestStoreError::Native)?;
		if released {
			*slot = published;
		}
		Ok(released)
	}

	fn reserve_policy(
		&mut self, client_id: &str, mut record: StoredRequest, policy: InvoicePolicy,
	) -> Result<StoredRequest, RequestStoreError> {
		if record.reserve_invoice(policy)? {
			self.write_record(&record)?;
			record = self.lookup(client_id)?.ok_or(RequestStoreError::Missing)?;
		}
		Ok(record)
	}

	/// The stored selector must still name the exact native intent and epoch context.
	fn recovery_context(
		&self, record: &StoredRequest,
	) -> Result<FFORReceiverRecoveryContext, RequestStoreError> {
		let selector = record.selector().ok_or(RequestStoreError::MissingNative)?;
		if self.validate_native(record)?.is_none() {
			return Err(RequestStoreError::MissingNative);
		}
		let context = self
			.manager
			.ffor_receiver_recovery_context(&selector.channel, selector.epoch)
			.map_err(RequestStoreError::Native)?;
		if context.channel_id() != record.plan().channel
			|| context.epoch_id() != selector.epoch
			|| context.settlement_node_id() != record.plan().settlement
			|| context.receiver_node_id() != self.node
		{
			return Err(RequestStoreError::Conflict);
		}
		Ok(context)
	}

	fn fetch_stored(
		&self, context: &FFORReceiverRecoveryContext,
	) -> Result<NativeInvoice, RequestStoreError> {
		match self.manager.ffor_receiver_invoice_for_storage(context) {
			Ok(Some(stored)) => Ok(NativeInvoice::Stored(Box::new(stored))),
			Ok(None) => Ok(NativeInvoice::Missing),
			Err(FFORReceiverError::ChannelState(FFORCommitmentError::PendingUpdates)) => {
				Ok(NativeInvoice::Awaiting)
			},
			Err(error) => Err(RequestStoreError::Native(error)),
		}
	}

	fn join_native(
		&mut self, client_id: &str, record: StoredRequest, context: &FFORReceiverRecoveryContext,
		intent: &FFORInvoiceIntent, prepared: Option<[u8; 32]>,
	) -> Result<InvoiceProgress, RequestStoreError> {
		let stored = match self.fetch_stored(context)? {
			NativeInvoice::Awaiting => return Ok(InvoiceProgress::AwaitingPersistence),
			NativeInvoice::Missing => return Err(RequestStoreError::MissingNative),
			NativeInvoice::Stored(stored) => *stored,
		};
		if prepared.is_some_and(|digest| digest != stored.invoice_digest()) {
			return Err(RequestStoreError::Conflict);
		}
		let retained = match record.invoice() {
			Some(retained) => retained.clone(),
			None => {
				let parsed: Bolt11Invoice =
					stored.invoice_for_storage().parse().map_err(|_| RequestStoreError::Corrupt)?;
				RetainedInvoice::new(
					context.context_digest(),
					stored.invoice_for_storage().to_owned(),
					parsed.duration_since_epoch().as_secs(),
				)?
			},
		};
		check_agreement(&stored, &retained, context, intent)?;
		let record = self.retain_native(client_id, record, retained)?;
		if retained_marker(&record) {
			// The marker proves an earlier successful exact write. Only the current row is checked
			// here; a fresh write happens when a handle is minted.
			self.payments
				.with_ffor_pending(
					&record.invoice().ok_or(RequestStoreError::Missing)?.payment()?,
					|| (),
				)
				.map_err(RequestStoreError::Payment)?;
		} else {
			self.confirm_pending(client_id, record)?;
		}
		Ok(InvoiceProgress::Retained)
	}

	fn retain_native(
		&mut self, client_id: &str, before: StoredRequest, retained: RetainedInvoice,
	) -> Result<StoredRequest, RequestStoreError> {
		let mut current = self.lookup(client_id)?.ok_or(RequestStoreError::Missing)?;
		if current.encode() != before.encode() {
			return Err(RequestStoreError::Conflict);
		}
		if current.retain_invoice(retained)? {
			self.write_record(&current)?;
		}
		Ok(current)
	}

	/// Confirm the exact Pending row through a successful write, then mark the record. The
	/// candidate stays installed across an ambiguous failure, blocking every later step.
	fn confirm_pending(
		&mut self, client_id: &str, before: StoredRequest,
	) -> Result<StoredRequest, RequestStoreError> {
		let retained = before.invoice().ok_or(RequestStoreError::Missing)?;
		let expected = retained.payment()?;
		self.confirm_payment_candidate(&expected, retained.payment_confirmed())?;
		let mut current = self.lookup(client_id)?.ok_or(RequestStoreError::Missing)?;
		if current.encode() != before.encode() {
			return Err(RequestStoreError::Conflict);
		}
		if current.confirm_invoice_payment(&expected)? {
			self.write_record(&current)?;
		}
		Ok(current)
	}
}

fn retained_marker(record: &StoredRequest) -> bool {
	record.invoice().is_some_and(RetainedInvoice::payment_confirmed)
}

fn native_intent(record: &StoredRequest, policy: InvoicePolicy) -> FFORInvoiceIntent {
	FFORInvoiceIntent {
		description: record.intent().description().to_owned(),
		expiry_seconds: policy.expiry_seconds,
		safety_margin_seconds: policy.safety_margin_seconds,
	}
}

fn check_agreement(
	stored: &FFORStoredInvoice, retained: &RetainedInvoice, context: &FFORReceiverRecoveryContext,
	intent: &FFORInvoiceIntent,
) -> Result<(), RequestStoreError> {
	if stored.invoice_for_storage() != retained.invoice()
		|| stored.invoice_digest() != retained.digest()
		|| stored.intent() != intent
		|| stored.recovery_context().context_digest() != context.context_digest()
		|| retained.context_digest() != context.context_digest()
	{
		return Err(RequestStoreError::Conflict);
	}
	Ok(())
}

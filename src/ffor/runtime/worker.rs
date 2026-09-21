//! The worker pass. Every step observes native progress and takes exactly one bounded action.
//!
//! No mutex is held across an await: the credit sequence's only asynchronous step, adding the
//! `PaymentReceived` event, runs after the state lock is released, and the durable `Notified`
//! marker is written afterwards under a fresh lock.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use lightning::ln::ffor::{
	FFORCommitmentError, FFORReceiverError, FFORReceiverProgress, FFORReceiverRecoveryContext,
	FFORVoucherOutcome, FFORWitnessReceiptProgress,
};
use lightning::ln::wire::Type;
use lightning_types::payment::{PaymentHash, PaymentPreimage};

use super::credit::{credit_intent, mark_notified};
use super::ledger::{IntentKey, IntentState, OutcomeIntent};
use super::route::witness_route_evidence;
use super::{
	CloseReason, FforReceiverRuntime, LiveRequest, RuntimeError, RuntimeState, Stage,
	MAX_PENDING_ACKS, PROVISION_TIMEOUT,
};
use crate::event::Event;
use crate::ffor::request_store::{InvoicePolicy, InvoiceProgress, RequestStoreError};
use crate::ffor::witness_owner::{
	AcknowledgementProgress, FetchProgress, ProvisioningProgress, RegistrationProgress,
	WitnessOwnerError,
};
use crate::ffor::witness_store::WitnessStorageBinding;
use crate::logger::{log_debug, log_info, LdkLogger};
use crate::payment::store::{PaymentDetailsUpdate, PaymentStatus};
use crate::payment::OfflineReceiveOutcome;

const WITNESS_ACK_TYPE: u16 = 55057;
const FETCH_RESPONSE_TYPE: u16 = 55061;
const MAX_TRANSPORT_MESSAGES_PER_PASS: usize = 64;
/// The single voucher slot of a one-amount Variant D request.
const SLOT: u16 = 1;

enum Verdict {
	Wait,
	Fail(String),
}

fn verdict_native(error: &FFORReceiverError) -> Verdict {
	match error {
		FFORReceiverError::ChannelState(
			FFORCommitmentError::PendingUpdates | FFORCommitmentError::ChannelUnavailable,
		)
		| FFORReceiverError::PersistenceUnavailable => Verdict::Wait,
		other => Verdict::Fail(format!("native: {other}")),
	}
}

fn verdict_request(error: &RequestStoreError) -> Verdict {
	match error {
		RequestStoreError::Native(native) => verdict_native(native),
		RequestStoreError::Storage
		| RequestStoreError::Uncertain
		| RequestStoreError::Payment(crate::data_store::ffor::FFORPaymentError::Storage) => Verdict::Wait,
		other => Verdict::Fail(format!("request store: {other:?}")),
	}
}

fn verdict_witness(error: &WitnessOwnerError) -> Verdict {
	match error {
		WitnessOwnerError::Native(native) => verdict_native(native),
		WitnessOwnerError::StaleConnection
		| WitnessOwnerError::Capacity
		| WitnessOwnerError::Entropy
		| WitnessOwnerError::Storage(crate::ffor::witness_store::WitnessStoreError::Storage)
		| WitnessOwnerError::Storage(crate::ffor::witness_store::WitnessStoreError::Uncertain) => {
			Verdict::Wait
		},
		other => Verdict::Fail(format!("witness owner: {other:?}")),
	}
}

fn now_secs() -> u64 {
	SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl FforReceiverRuntime {
	pub(crate) async fn tick(&self) {
		let emits = {
			let mut state = self.state.lock().unwrap();
			self.pass(&mut state)
		};
		for (intent, event) in emits {
			let queued = self.event_queue.add_event(event).await;
			let mut state = self.state.lock().unwrap();
			let outcome = queued
				.map_err(|_| ())
				.and_then(|()| mark_notified(&state.ledger, &intent).map_err(|_| ()));
			if outcome.is_err() {
				// The persisted Credited record and the queue membership check make the retry
				// idempotent; keep the intent for the next pass.
				self.log_error("notification", &"event queue or marker write failed");
				state.credits.push(intent);
			}
		}
	}

	/// One synchronous pass over transport input, recovery, every live request and pending
	/// credits. Returns the events whose intents reached `Credited` and are not yet queued.
	pub(super) fn pass(&self, state: &mut RuntimeState) -> Vec<(OutcomeIntent, Event)> {
		if let Err(error) = state.requests.recover_write() {
			self.log_error("request store recovery", &error);
		}
		if let Err(error) = state.witnesses.recover_storage() {
			self.log_error("witness store recovery", &error);
		}
		if !state.recovered {
			if let Err(error) = self.recover_locked(state) {
				self.log_error("recovery", &error);
				return Vec::new();
			}
		}
		self.drain_transport(state);
		let keys: Vec<String> = state.live.keys().cloned().collect();
		for key in keys {
			self.advance_request(state, &key);
		}
		self.run_credits(state)
	}

	fn drain_transport(&self, state: &mut RuntimeState) {
		let mut messages = std::mem::take(&mut state.pending_acks);
		while messages.len() < MAX_TRANSPORT_MESSAGES_PER_PASS {
			match self.transport.pop() {
				Some(message) => messages.push(message),
				None => break,
			}
		}
		for message in messages {
			let retain = match message.frame().type_id() {
				WITNESS_ACK_TYPE => match state.witnesses.acknowledge(&message) {
					Ok(AcknowledgementProgress::AwaitingPersistence) => true,
					Ok(_) => false,
					Err(error) => {
						log_debug!(self.logger, "Witness acknowledgement not retained: {error:?}");
						matches!(verdict_witness(&error), Verdict::Wait)
							&& !matches!(error, WitnessOwnerError::StaleConnection)
					},
				},
				FETCH_RESPONSE_TYPE => match state.witnesses.accept_fetch_page(&message) {
					Ok(_) => false,
					Err(error) => {
						log_debug!(self.logger, "Witness fetch page not retained: {error:?}");
						matches!(verdict_witness(&error), Verdict::Wait)
							&& !matches!(error, WitnessOwnerError::StaleConnection)
					},
				},
				_ => false,
			};
			if retain && state.pending_acks.len() < MAX_PENDING_ACKS {
				state.pending_acks.push(message);
			}
		}
	}

	fn tip(&self) -> u32 {
		self.manager.current_best_block().height
	}

	fn past_deadline_margin(&self, live: &LiveRequest) -> bool {
		let tip = self.tip();
		tip.saturating_add(self.config.deadline_safety_margin_blocks) >= live.settlement_deadline
			|| tip >= live.voucher_expiry
	}

	fn advance_request(&self, state: &mut RuntimeState, key: &str) {
		let stage = match state.live.get(key) {
			Some(live) if !live.stage.is_terminal() => live.stage.clone(),
			_ => return,
		};
		let result = match stage {
			Stage::Prepare => self.step_prepare(state, key),
			Stage::Setup => self.step_setup(state, key),
			Stage::Witnesses => self.step_witnesses(state, key),
			Stage::Invoice => self.step_invoice(state, key),
			Stage::Ready => self.step_ready(state, key),
			Stage::Closing(reason) => self.step_closing(state, key, reason),
			Stage::Closed(reason) => self.step_closed(state, key, reason),
			Stage::Settled(_) | Stage::Expired | Stage::Failed(_) => Ok(()),
		};
		let Some(live) = state.live.get_mut(key) else { return };
		match result {
			Ok(()) => {},
			Err(Verdict::Wait) => {},
			Err(Verdict::Fail(reason)) => {
				log_info!(
					self.logger,
					"Offline receive request on channel {} failed: {}",
					live.channel,
					reason
				);
				live.stage = Stage::Failed(reason);
			},
		}
	}

	fn set_stage(state: &mut RuntimeState, key: &str, stage: Stage) {
		if let Some(live) = state.live.get_mut(key) {
			live.stage = stage;
		}
	}

	fn step_prepare(&self, state: &mut RuntimeState, key: &str) -> Result<(), Verdict> {
		let live = state.live.get(key).ok_or(Verdict::Wait)?;
		let (client_id, settlement, local_request_id) =
			(live.client_id.clone(), live.settlement, live.local_request_id);
		if live.cancel_requested || self.past_deadline_margin(live) {
			let reason = if live.cancel_requested { "cancelled" } else { "deadline" };
			state.ledger.mark_cancelled(local_request_id).map_err(|_| Verdict::Wait)?;
			return Err(Verdict::Fail(reason.to_owned()));
		}
		let Some(connection) = self.transport.native_connection(settlement) else {
			return Err(Verdict::Wait);
		};
		match state.requests.prepare_native(&client_id, connection.native()) {
			Ok(id) => {
				let live = state.live.get_mut(key).ok_or(Verdict::Wait)?;
				live.selector = Some(id);
				live.stage = Stage::Setup;
				Ok(())
			},
			// Native refuses a book on a channel that is not yet reestablished or not empty with
			// the same error as a genuinely invalid book. Both wait; the deadline bounds the retry.
			Err(RequestStoreError::Native(FFORReceiverError::ChannelState(
				FFORCommitmentError::InvalidVoucherBook,
			))) => {
				log_debug!(self.logger, "Native preparation deferred: channel not ready");
				Err(Verdict::Wait)
			},
			Err(error) => Err(verdict_request(&error)),
		}
	}

	/// Observe Active without a peer when possible; otherwise drive native setup on the live
	/// settlement connection.
	fn step_setup(&self, state: &mut RuntimeState, key: &str) -> Result<(), Verdict> {
		let live = state.live.get(key).ok_or(Verdict::Wait)?;
		let id = live.selector.ok_or(Verdict::Wait)?;
		let settlement = live.settlement;
		if live.cancel_requested || self.past_deadline_margin(live) {
			let reason = if live.cancel_requested { "cancelled" } else { "deadline" };
			if self.transport.native_connection(settlement).is_none() {
				return Err(Verdict::Wait);
			}
			return match self.setup.cancel(settlement, &id) {
				Ok(_) => Err(Verdict::Fail(reason.to_owned())),
				Err(error) => Err(verdict_native(&error)),
			};
		}
		if self.is_active(live) {
			Self::set_stage(state, key, Stage::Witnesses);
			return Ok(());
		}
		if self.transport.native_connection(settlement).is_none() {
			return Err(Verdict::Wait);
		}
		let progress = self.setup.advance(settlement, &id).map_err(|e| verdict_native(&e))?;
		self.apply_progress(state, key, progress, Stage::Witnesses)
	}

	fn is_active(&self, live: &LiveRequest) -> bool {
		live.selector.is_some_and(|id| {
			self.manager
				.capture_ffor_receiver_active_context(
					&live.channel,
					&live.settlement,
					id.epoch_id(),
				)
				.is_ok()
		})
	}

	/// Map an observed native progress to the stage machine. `on_active` is the stage entered
	/// when native reports a durable Active epoch.
	fn apply_progress(
		&self, state: &mut RuntimeState, key: &str, progress: FFORReceiverProgress,
		on_active: Stage,
	) -> Result<(), Verdict> {
		let live = state.live.get_mut(key).ok_or(Verdict::Wait)?;
		match progress {
			FFORReceiverProgress::Active => {
				if matches!(live.stage, Stage::Setup) {
					live.stage = on_active;
				}
				Ok(())
			},
			FFORReceiverProgress::Draining => {
				if !matches!(live.stage, Stage::Closing(_)) {
					live.stage = Stage::Closing(CloseReason::Peer);
				}
				Err(Verdict::Wait)
			},
			FFORReceiverProgress::Closed => {
				let reason = match live.stage {
					Stage::Closing(reason) => reason,
					_ => CloseReason::Peer,
				};
				live.stage = Stage::Closed(reason);
				Ok(())
			},
			FFORReceiverProgress::Aborted { reason } => Err(Verdict::Fail(format!("{reason:?}"))),
			FFORReceiverProgress::ReconnectRequired | FFORReceiverProgress::ResolutionRequired => {
				if !matches!(live.stage, Stage::Setup) {
					live.needs_receipts = true;
				}
				Err(Verdict::Wait)
			},
			FFORReceiverProgress::AwaitingPersistence
			| FFORReceiverProgress::Backpressured
			| FFORReceiverProgress::AwaitingPeer
			| FFORReceiverProgress::AwaitingVoucherCommitments
			| FFORReceiverProgress::NeedsMonitorSnapshot => Err(Verdict::Wait),
		}
	}

	fn context(&self, live: &LiveRequest) -> Result<FFORReceiverRecoveryContext, Verdict> {
		let id = live.selector.ok_or(Verdict::Wait)?;
		self.manager
			.ffor_receiver_recovery_context(&live.channel, id.epoch_id())
			.map_err(|e| verdict_native(&e))
	}

	/// Shared liveness for post-activation stages: apply cancellation and deadlines, observe an
	/// unexpected close or a reconnect requirement, and recover receipts when flagged.
	fn post_activation_probe(&self, state: &mut RuntimeState, key: &str) -> Result<(), Verdict> {
		let live = state.live.get(key).ok_or(Verdict::Wait)?;
		let id = live.selector.ok_or(Verdict::Wait)?;
		let settlement = live.settlement;
		if live.cancel_requested {
			Self::set_stage(state, key, Stage::Closing(CloseReason::Cancelled));
			return Err(Verdict::Wait);
		}
		if self.past_deadline_margin(live) {
			Self::set_stage(state, key, Stage::Closing(CloseReason::Deadline));
			return Err(Verdict::Wait);
		}
		if live.needs_receipts {
			let context = self.context(live)?;
			self.recover_receipts(state, key, &context);
		}
		if !self.is_active(state.live.get(key).ok_or(Verdict::Wait)?)
			&& self.transport.native_connection(settlement).is_some()
		{
			let progress = self.setup.advance(settlement, &id).map_err(|e| verdict_native(&e))?;
			self.apply_progress(state, key, progress, Stage::Witnesses)?;
			let live = state.live.get(key).ok_or(Verdict::Wait)?;
			if matches!(live.stage, Stage::Closed(_) | Stage::Closing(_)) {
				return Err(Verdict::Wait);
			}
		}
		Ok(())
	}

	fn step_witnesses(&self, state: &mut RuntimeState, key: &str) -> Result<(), Verdict> {
		self.post_activation_probe(state, key)?;
		let live = state.live.get(key).ok_or(Verdict::Wait)?;
		let context = self.context(live)?;
		let policies = self.witness_policies(live.voucher_expiry);
		match state.witnesses.register(&context, &policies) {
			Ok(RegistrationProgress::Retained) => {},
			Ok(RegistrationProgress::AwaitingPersistence) => return Err(Verdict::Wait),
			Err(error) => return Err(verdict_witness(&error)),
		}
		let mut retained = 0usize;
		for witness in self.witness_ids() {
			let started =
				state.live.get(key).and_then(|live| live.provision_started.get(&witness).copied());
			let timed_out = started.is_some_and(|since| since.elapsed() >= PROVISION_TIMEOUT);
			let progress = if timed_out {
				state.witnesses.retry_provision(&context, witness)
			} else {
				state.witnesses.provision(&context, witness)
			};
			match progress {
				Ok(ProvisioningProgress::AcknowledgementRetained) => retained += 1,
				Ok(ProvisioningProgress::Queued) => {
					if let Some(live) = state.live.get_mut(key) {
						live.provision_started.insert(witness, Instant::now());
					}
				},
				Ok(
					ProvisioningProgress::AwaitingAcknowledgement
					| ProvisioningProgress::Backpressured
					| ProvisioningProgress::AwaitingPersistence,
				) => {},
				Err(WitnessOwnerError::StaleConnection) => {
					log_debug!(self.logger, "Witness {} is not connected", witness);
				},
				Err(error) => return Err(verdict_witness(&error)),
			}
		}
		if retained == self.config.witnesses.len() {
			Self::set_stage(state, key, Stage::Invoice);
		}
		Ok(())
	}

	fn invoice_policy(&self) -> InvoicePolicy {
		InvoicePolicy {
			expiry_seconds: self.config.invoice_expiry_seconds,
			safety_margin_seconds: self.config.invoice_safety_margin_seconds,
		}
	}

	fn step_invoice(&self, state: &mut RuntimeState, key: &str) -> Result<(), Verdict> {
		self.post_activation_probe(state, key)?;
		let live = state.live.get(key).ok_or(Verdict::Wait)?;
		let client_id = live.client_id.clone();
		let settlement = live.settlement;
		let Some(route) = self
			.witness_ids()
			.into_iter()
			.find_map(|witness| witness_route_evidence(&self.graph, witness, settlement))
		else {
			log_debug!(self.logger, "No signed witness route evidence in the network graph yet");
			return Err(Verdict::Wait);
		};
		match state.requests.issue_invoice(&client_id, self.invoice_policy(), &route) {
			Ok(InvoiceProgress::Retained) => {},
			Ok(InvoiceProgress::AwaitingPersistence) => return Err(Verdict::Wait),
			Err(error) => return Err(verdict_request(&error)),
		}
		let handle = match state.requests.confirmed_invoice(&client_id) {
			Ok(Some(handle)) => handle,
			Ok(None) => return Err(Verdict::Wait),
			Err(error) => return Err(verdict_request(&error)),
		};
		let mut slot = None;
		match state.requests.release_invoice(&handle, &mut slot) {
			Ok(true) => {},
			Ok(false) => return Err(Verdict::Wait),
			Err(error) => return Err(verdict_request(&error)),
		}
		let expires_at = state
			.requests
			.lookup(&client_id)
			.map_err(|e| verdict_request(&e))?
			.ok_or(Verdict::Wait)?
			.invoice_expires_at()
			.map_err(|e| verdict_request(&e))?;
		let live = state.live.get_mut(key).ok_or(Verdict::Wait)?;
		live.bolt11 = slot;
		live.invoice_expires_at = expires_at;
		live.stage = Stage::Ready;
		log_info!(self.logger, "Offline receive invoice ready on channel {}", live.channel);
		Ok(())
	}

	fn step_ready(&self, state: &mut RuntimeState, key: &str) -> Result<(), Verdict> {
		self.post_activation_probe(state, key)?;
		let live = state.live.get(key).ok_or(Verdict::Wait)?;
		if live.invoice_expires_at.is_some_and(|expires| now_secs() >= expires) {
			Self::set_stage(state, key, Stage::Closing(CloseReason::InvoiceExpired));
		}
		Ok(())
	}

	fn step_closing(
		&self, state: &mut RuntimeState, key: &str, reason: CloseReason,
	) -> Result<(), Verdict> {
		let live = state.live.get(key).ok_or(Verdict::Wait)?;
		let id = live.selector.ok_or(Verdict::Wait)?;
		let settlement = live.settlement;
		if live.needs_receipts {
			let context = self.context(live)?;
			self.recover_receipts(state, key, &context);
		}
		if self.transport.native_connection(settlement).is_none() {
			return Err(Verdict::Wait);
		}
		if reason != CloseReason::Peer {
			match self.setup.close(settlement, &id) {
				Ok(_) => {},
				Err(FFORReceiverError::ChannelState(FFORCommitmentError::PendingUpdates)) => {},
				Err(error) => match verdict_native(&error) {
					Verdict::Wait => return Err(Verdict::Wait),
					// A close request refused after Draining or Closed is observed by advance.
					Verdict::Fail(_) => {},
				},
			}
		}
		let progress = self.setup.advance(settlement, &id).map_err(|e| verdict_native(&e))?;
		self.apply_progress(state, key, progress, Stage::Closing(reason))
	}

	/// After Closed, join the single slot's journal outcome. Only `Fulfilled` credits, and only
	/// through the durable intent record.
	fn step_closed(
		&self, state: &mut RuntimeState, key: &str, reason: CloseReason,
	) -> Result<(), Verdict> {
		let live = state.live.get(key).ok_or(Verdict::Wait)?;
		let client_id = live.client_id.clone();
		let context = self.context(live)?;
		let record = state
			.requests
			.lookup(&client_id)
			.map_err(|e| verdict_request(&e))?
			.ok_or(Verdict::Wait)?;
		let Some(payment) = record.confirmed_payment().map_err(|e| verdict_request(&e))? else {
			Self::set_stage(state, key, Stage::Expired);
			return Ok(());
		};
		let voucher = context.setup().vouchers().first().ok_or(Verdict::Wait)?;
		let hash = PaymentHash(voucher.payment_hash);
		if payment.id.0 != hash.0 || payment.amount_msat != Some(voucher.amount_msat) {
			return Err(Verdict::Fail("voucher does not match the confirmed payment".to_owned()));
		}
		let outcome = match self.manager.ffor_receiver_voucher_outcome(
			&context,
			SLOT,
			hash,
			voucher.amount_msat,
		) {
			Ok(outcome) => outcome,
			Err(error) => return Err(verdict_native(&error)),
		};
		let stage = match outcome {
			None => Stage::Expired,
			Some(FFORVoucherOutcome::Fulfilled) => {
				let intent = OutcomeIntent {
					key: IntentKey {
						channel: context.channel_id(),
						epoch: context.epoch_id(),
						slot: SLOT,
					},
					state: IntentState::Intended,
					payment_hash: hash,
					amount_msat: voucher.amount_msat,
					client_id: client_id.clone(),
				};
				if !state.credits.iter().any(|existing| existing.key == intent.key) {
					state.credits.push(intent);
				}
				Stage::Settled(OfflineReceiveOutcome::Fulfilled)
			},
			Some(FFORVoucherOutcome::Failed) => {
				let update = PaymentDetailsUpdate {
					status: Some(PaymentStatus::Failed),
					..PaymentDetailsUpdate::new(payment.id)
				};
				if self.payments.update(&update).is_err() {
					return Err(Verdict::Wait);
				}
				Stage::Settled(OfflineReceiveOutcome::Failed)
			},
		};
		log_info!(
			self.logger,
			"Offline receive epoch on channel {} closed ({:?}) with outcome {:?}",
			context.channel_id(),
			reason,
			stage
		);
		Self::set_stage(state, key, stage);
		Ok(())
	}

	/// Fetch each witness's evidence and import the slot receipt into the original monitor.
	/// Nothing here credits a payment.
	fn recover_receipts(
		&self, state: &mut RuntimeState, key: &str, context: &FFORReceiverRecoveryContext,
	) {
		let mut complete = true;
		for witness in self.witness_ids() {
			let progress = state.witnesses.fetch(context, witness);
			let progress = match progress {
				Ok(FetchProgress::RestartRequired | FetchProgress::RejectedPage) => {
					state.witnesses.retry_fetch(context, witness)
				},
				other => other,
			};
			match progress {
				Ok(FetchProgress::Complete) => {
					match state.witnesses.recover_receipt(context, witness, SLOT) {
						Ok(Some(FFORWitnessReceiptProgress::MonitorPersisted { .. }))
						| Ok(None) => {},
						Ok(Some(FFORWitnessReceiptProgress::PendingMonitor { .. })) => {
							complete = false;
						},
						Err(error) => {
							complete = false;
							log_debug!(self.logger, "Receipt import deferred: {error:?}");
						},
					}
				},
				Ok(_) => complete = false,
				Err(error) => {
					complete = false;
					log_debug!(self.logger, "Witness fetch deferred: {error:?}");
				},
			}
		}
		if complete {
			if let Some(live) = state.live.get_mut(key) {
				live.needs_receipts = false;
			}
		}
	}

	/// The preimage from a retained, authenticated witness receipt when one exists. Absence is
	/// never treated as evidence and never blocks the credit.
	fn known_preimage(
		&self, state: &RuntimeState, intent: &OutcomeIntent,
	) -> Option<PaymentPreimage> {
		let context = self
			.manager
			.ffor_receiver_recovery_context(&intent.key.channel, intent.key.epoch)
			.ok()?;
		let binding = WitnessStorageBinding::from_native_context(&context).ok()?;
		self.witness_ids().into_iter().find_map(|witness| {
			let receipt =
				state.witness_store.load_receipt(&binding, witness, intent.key.slot).ok()??;
			let body = receipt.body();
			(body.payment_hash() == intent.payment_hash.0
				&& body.amount_msat() == intent.amount_msat)
				.then(|| PaymentPreimage(body.preimage()))
		})
	}

	/// Run every pending intent through `Intended -> Credited`, returning the events that must
	/// be queued before `Notified`. Intents already queued are marked `Notified` here.
	fn run_credits(&self, state: &mut RuntimeState) -> Vec<(OutcomeIntent, Event)> {
		let pending = std::mem::take(&mut state.credits);
		let mut emits = Vec::new();
		for intent in pending {
			match self.credit(state, &intent) {
				Ok(Some((credited, event))) => emits.push((credited, event)),
				Ok(None) => {},
				Err(error) => {
					self.log_error("credit", &error);
					state.credits.push(intent);
				},
			}
		}
		emits
	}

	fn credit(
		&self, state: &RuntimeState, intent: &OutcomeIntent,
	) -> Result<Option<(OutcomeIntent, Event)>, RuntimeError> {
		let preimage = self.known_preimage(state, intent);
		credit_intent(&state.ledger, &self.payments, &self.event_queue, intent, preimage)
	}
}

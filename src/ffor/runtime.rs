//! Bounded production runtime joining the private FFOR components for one exact-amount,
//! single-channel offline receive per request.
//!
//! The runtime is constructed only when the builder's [`OfflineReceiveConfig`] is set. It owns the protected
//! request and witness stores, the witness owner, the transport and the setup adapter, and a
//! small in-memory request table. One background worker drives every live request through the
//! native progress machine. Readiness is reported only after the exact invoice has been retained,
//! confirmed and released through native publication in this process. A payment is credited only
//! from the native journal outcome getter after the epoch reached `Closed`, through a durable
//! idempotent intent record, never from receipts, preimages, peer reports or monitor snapshots.
//!
//! Known limits: repeated epochs on one channel need native epoch reuse; a fulfilled voucher is
//! observed only after cooperative close; witness route evidence requires P2P gossip retention.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bitcoin::blockdata::constants::ChainHash;
use bitcoin::secp256k1::PublicKey;
use bitcoin::Network;
use lightning::ln::ffor::{
	FFORReceiverError, FFORReceiverId, FFORReceiverParameters, FFORReceiverRecoveryContext,
};
use lightning::ln::types::ChannelId;
use lightning_ffor::amounts::FeePolicy;
use lightning_types::payment::PaymentHash;
use tokio::sync::Notify;

use super::request_store::{RequestIntent, RequestPlan, RequestStore, RequestStoreError};
use super::witness_owner::{WitnessOwner, WitnessOwnerError};
use super::witness_store::{WitnessPolicy, WitnessSecretStore, WitnessStoreError};
use crate::config::OfflineReceiveConfig;
use crate::error::Error;
use crate::event::EventQueue;
use crate::logger::{log_error, log_info, LdkLogger, Logger};
use crate::message_handler::ffor::{FforReceiverTransport, FforSetupAdapter, ReceivedFforMessage};
use crate::payment::{OfflineReceiveOutcome, OfflineReceiveStatus};
use crate::types::{ChainMonitor, ChannelManager, DynStore, Graph, PaymentStore};

mod credit;
mod ledger;
mod route;
mod worker;
use ledger::{LedgerError, OutcomeIntent, OutcomeLedger};

const MAX_LIVE_REQUESTS: usize = 64;
const MAX_PENDING_ACKS: usize = 32;
/// Time a queued witness provision may wait for an acknowledgement before an explicit retry.
const PROVISION_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RuntimeError {
	NotRecovered,
	Ineligible,
	NotFound,
	Conflict,
	InvalidIntent,
	Capacity,
	Request(RequestStoreError),
	Witness(WitnessOwnerError),
	WitnessStore(WitnessStoreError),
	Ledger(LedgerError),
	Native(FFORReceiverError),
}

impl From<RequestStoreError> for RuntimeError {
	fn from(error: RequestStoreError) -> Self {
		match error {
			RequestStoreError::InvalidIntent => Self::InvalidIntent,
			RequestStoreError::Conflict => Self::Conflict,
			RequestStoreError::Capacity => Self::Capacity,
			other => Self::Request(other),
		}
	}
}

impl From<LedgerError> for RuntimeError {
	fn from(error: LedgerError) -> Self {
		Self::Ledger(error)
	}
}

impl From<RuntimeError> for Error {
	fn from(error: RuntimeError) -> Self {
		match error {
			RuntimeError::NotRecovered => Error::OfflineReceiveUnavailable,
			RuntimeError::Ineligible => Error::OfflineReceiveIneligible,
			RuntimeError::NotFound => Error::OfflineReceiveRequestNotFound,
			RuntimeError::Conflict => Error::OfflineReceiveRequestConflict,
			RuntimeError::InvalidIntent => Error::InvalidAmount,
			RuntimeError::Capacity => Error::OfflineReceiveUnavailable,
			RuntimeError::Request(RequestStoreError::Storage)
			| RuntimeError::Request(RequestStoreError::Uncertain)
			| RuntimeError::Ledger(LedgerError::Storage) => Error::PersistenceFailed,
			_ => Error::OfflineReceiveUnavailable,
		}
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CloseReason {
	InvoiceExpired,
	Deadline,
	Cancelled,
	Peer,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Stage {
	/// Durable intent exists; no native selector yet.
	Prepare,
	/// Native selector bound; the epoch is not yet durably Active.
	Setup,
	/// Native Active; witness registration and provisioning in progress.
	Witnesses,
	/// All witness acknowledgements retained; invoice issuance and release in progress.
	Invoice,
	/// The exact invoice was released in this process.
	Ready,
	/// Cooperative close requested; driving Draining to Closed.
	Closing(CloseReason),
	/// Native Closed; joining the journal outcome.
	Closed(CloseReason),
	Settled(OfflineReceiveOutcome),
	Expired,
	Failed(String),
}

impl Stage {
	fn is_terminal(&self) -> bool {
		matches!(self, Self::Settled(_) | Self::Expired | Self::Failed(_))
	}
}

pub(super) struct LiveRequest {
	client_id: String,
	local_request_id: [u8; 32],
	channel: ChannelId,
	settlement: PublicKey,
	amount_msat: u64,
	settlement_deadline: u32,
	voucher_expiry: u32,
	selector: Option<FFORReceiverId>,
	stage: Stage,
	bolt11: Option<String>,
	invoice_expires_at: Option<u64>,
	cancel_requested: bool,
	needs_receipts: bool,
	provision_started: BTreeMap<PublicKey, Instant>,
}

impl fmt::Debug for LiveRequest {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("LiveRequest")
			.field("channel", &self.channel)
			.field("amount_msat", &self.amount_msat)
			.field("stage", &self.stage)
			.finish_non_exhaustive()
	}
}

impl LiveRequest {
	fn status(&self) -> OfflineReceiveStatus {
		match &self.stage {
			Stage::Prepare => OfflineReceiveStatus::Preparing,
			Stage::Setup => OfflineReceiveStatus::AwaitingActivation,
			Stage::Witnesses | Stage::Invoice => OfflineReceiveStatus::AwaitingWitnesses,
			Stage::Ready => match &self.bolt11 {
				Some(bolt11) => OfflineReceiveStatus::Ready { bolt11: bolt11.clone() },
				None => OfflineReceiveStatus::AwaitingWitnesses,
			},
			Stage::Closing(CloseReason::Cancelled) | Stage::Closed(CloseReason::Cancelled) => {
				OfflineReceiveStatus::Failed { reason: "cancelled".to_owned() }
			},
			Stage::Closing(_) | Stage::Closed(_) | Stage::Expired => OfflineReceiveStatus::Expired,
			Stage::Settled(outcome) => OfflineReceiveStatus::Settled { outcome: *outcome },
			Stage::Failed(reason) => OfflineReceiveStatus::Failed { reason: reason.clone() },
		}
	}
}

pub(super) struct RuntimeState {
	requests: RequestStore,
	witness_store: Arc<WitnessSecretStore>,
	witnesses: WitnessOwner,
	ledger: OutcomeLedger,
	live: BTreeMap<String, LiveRequest>,
	pending_acks: Vec<ReceivedFforMessage>,
	credits: Vec<OutcomeIntent>,
	recovered: bool,
}

/// Owns every private FFOR component for one node. Construct only through the builder.
pub(crate) struct FforReceiverRuntime {
	config: OfflineReceiveConfig,
	manager: Arc<ChannelManager>,
	graph: Arc<Graph>,
	payments: Arc<PaymentStore>,
	event_queue: Arc<EventQueue<Arc<Logger>>>,
	transport: Arc<FforReceiverTransport>,
	setup: Arc<FforSetupAdapter>,
	logger: Arc<Logger>,
	state: Mutex<RuntimeState>,
	wake: Notify,
}

impl FforReceiverRuntime {
	/// Open both protected stores with the wallet seed and compose the owner. The seed is used
	/// only here. Nothing is recovered or scheduled until [`Self::recover`] and the worker run.
	#[allow(clippy::too_many_arguments)]
	pub(crate) fn new(
		config: OfflineReceiveConfig, network: Network, seed: &[u8; 64],
		manager: Arc<ChannelManager>, monitor: Arc<ChainMonitor>, graph: Arc<Graph>,
		payments: Arc<PaymentStore>, event_queue: Arc<EventQueue<Arc<Logger>>>,
		storage: Arc<DynStore>, transport: Arc<FforReceiverTransport>,
		setup: Arc<FforSetupAdapter>, logger: Arc<Logger>,
	) -> Result<Self, RuntimeError> {
		let requests = RequestStore::open_bound(
			seed,
			ChainHash::using_genesis_block(network).to_bytes(),
			manager.get_our_node_id(),
			Arc::clone(&manager),
			Arc::clone(&payments),
			Arc::clone(&storage),
		)?;
		let witness_store = Arc::new(
			WitnessSecretStore::open(seed, Arc::clone(&storage))
				.map_err(RuntimeError::WitnessStore)?,
		);
		let witnesses = WitnessOwner::new(
			Arc::clone(&manager),
			monitor,
			Arc::clone(&witness_store),
			Arc::clone(&transport),
		);
		let ledger = OutcomeLedger::new(storage);
		Ok(Self {
			config,
			manager,
			graph,
			payments,
			event_queue,
			transport,
			setup,
			logger,
			state: Mutex::new(RuntimeState {
				requests,
				witness_store,
				witnesses,
				ledger,
				live: BTreeMap::new(),
				pending_acks: Vec::new(),
				credits: Vec::new(),
				recovered: false,
			}),
			wake: Notify::new(),
		})
	}

	pub(crate) fn poll_interval(&self) -> Duration {
		Duration::from_secs(self.config.poll_interval_secs)
	}

	/// Start-time recovery: resolve uncertain writes, rejoin every durable request to native
	/// history and resume unfinished completion intents. Safe to call repeatedly.
	pub(crate) fn recover(&self) -> Result<(), RuntimeError> {
		let mut state = self.state.lock().unwrap();
		self.recover_locked(&mut state)
	}

	fn recover_locked(&self, state: &mut RuntimeState) -> Result<(), RuntimeError> {
		state.requests.recover_write()?;
		state.witnesses.recover_storage().map_err(RuntimeError::Witness)?;
		let records = state.requests.list()?;
		for record in records {
			let client_id = record.intent().client_id().to_owned();
			if state.live.contains_key(&client_id) {
				continue;
			}
			let local_request_id = record.local_request_id();
			let mut live = LiveRequest {
				client_id: client_id.clone(),
				local_request_id,
				channel: record.plan().channel,
				settlement: record.plan().settlement,
				amount_msat: record.intent().amount_msat(),
				settlement_deadline: record.plan().parameters.settlement_deadline,
				voucher_expiry: record.plan().parameters.voucher_expiry,
				selector: None,
				stage: Stage::Prepare,
				bolt11: None,
				invoice_expires_at: None,
				cancel_requested: false,
				needs_receipts: false,
				provision_started: BTreeMap::new(),
			};
			// Storage uncertainty aborts recovery; a record that no longer matches native
			// history is retained as failed rather than blocking every other request.
			match state.requests.recover_native(&client_id) {
				Ok(None) => {
					if state.ledger.is_cancelled(local_request_id)? {
						live.stage = Stage::Failed("cancelled".to_owned());
					}
				},
				Ok(Some(id)) => {
					live.selector = Some(id);
					live.stage = Stage::Setup;
					// A confirmed invoice means witnesses were natively acknowledged; rejoin the
					// assignment so the worker can release it again in this process.
					match state.requests.recover_invoice(&client_id) {
						Ok(Some(_)) => live.stage = Stage::Invoice,
						Ok(None) => {},
						Err(RequestStoreError::Native(FFORReceiverError::ChannelState(_))) => {
							live.stage = Stage::Invoice;
						},
						Err(
							error @ (RequestStoreError::Storage | RequestStoreError::Uncertain),
						) => {
							return Err(error.into());
						},
						Err(error) => live.stage = Stage::Failed(format!("recovery: {error:?}")),
					}
					if record.confirmed_payment()?.is_some() && !live.stage.is_terminal() {
						live.stage = Stage::Invoice;
					}
				},
				Err(error @ (RequestStoreError::Storage | RequestStoreError::Uncertain)) => {
					return Err(error.into());
				},
				Err(error) => live.stage = Stage::Failed(format!("recovery: {error:?}")),
			}
			if state.live.len() >= MAX_LIVE_REQUESTS {
				return Err(RuntimeError::Capacity);
			}
			log_info!(
				self.logger,
				"Recovered offline receive request on channel {} at stage {:?}",
				live.channel,
				live.stage
			);
			state.live.insert(client_id, live);
		}
		for intent in state.ledger.unfinished()? {
			if !state.credits.iter().any(|existing| existing.key == intent.key) {
				state.credits.push(intent);
			}
		}
		state.recovered = true;
		Ok(())
	}

	/// Positive amount within one ready channel's inbound capacity on the settlement peer, with
	/// no other live request or historical native epoch on that channel. Peer connectivity is
	/// transient and is not part of eligibility; native preparation waits for the connection.
	pub(crate) fn can_receive(&self, amount_msat: u64) -> Result<bool, RuntimeError> {
		let state = self.state.lock().unwrap();
		if !state.recovered {
			return Err(RuntimeError::NotRecovered);
		}
		Ok(self.eligible_channel(&state, amount_msat)?.is_some())
	}

	fn eligible_channel(
		&self, state: &RuntimeState, amount_msat: u64,
	) -> Result<Option<ChannelId>, RuntimeError> {
		if amount_msat == 0 {
			return Ok(None);
		}
		let gross = match self.fee_policy().gross_msat(amount_msat) {
			Ok(gross) => gross,
			Err(_) => return Ok(None),
		};
		let contexts =
			self.manager.list_ffor_receiver_recovery_contexts().map_err(RuntimeError::Native)?;
		let busy = |channel: &ChannelId| {
			state.live.values().any(|live| live.channel == *channel && !live.stage.is_terminal())
				|| history_blocks_channel(channel, &contexts, |context| {
					self.epoch_is_terminal(context)
				})
		};
		Ok(self
			.manager
			.list_channels()
			.into_iter()
			.filter(|details| {
				details.counterparty.node_id == self.config.settlement_node_id
					&& details.is_channel_ready
					&& details.inbound_capacity_msat >= gross
					&& !busy(&details.channel_id)
			})
			.map(|details| details.channel_id)
			.next())
	}

	/// The only public terminal signal for a historical epoch is a completed cooperative drain:
	/// the journal outcome getter answers `Some` for slot one. Aborted epochs expose no public
	/// terminal signal and therefore keep their channel ineligible here, even though native may
	/// admit a replacement; pending drains and incomplete terminal writes are not terminal.
	fn epoch_is_terminal(&self, context: &FFORReceiverRecoveryContext) -> bool {
		let Some(voucher) = context.setup().vouchers().first() else { return false };
		matches!(
			self.manager.ffor_receiver_voucher_outcome(
				context,
				1,
				PaymentHash(voucher.payment_hash),
				voucher.amount_msat,
			),
			Ok(Some(_))
		)
	}

	fn fee_policy(&self) -> FeePolicy {
		FeePolicy {
			base_msat: self.config.fee_base_msat,
			proportional_millionths: self.config.fee_proportional_millionths,
		}
	}

	fn witness_ids(&self) -> Vec<PublicKey> {
		let mut ids: Vec<PublicKey> =
			self.config.witnesses.iter().map(|witness| witness.node_id).collect();
		ids.sort_unstable();
		ids
	}

	fn witness_policies(&self, voucher_expiry: u32) -> Vec<WitnessPolicy> {
		self.config
			.witnesses
			.iter()
			.map(|witness| WitnessPolicy {
				witness: witness.node_id,
				retention_until: voucher_expiry.saturating_add(witness.retention_blocks),
				minimum_receipts: witness.minimum_receipts,
			})
			.collect()
	}

	/// Idempotent per request ID. Begins the durable record and schedules the worker. The same
	/// ID with different arguments is a conflict.
	pub(crate) fn prepare(
		&self, client_id: String, amount_msat: u64, description: String,
	) -> Result<OfflineReceiveStatus, RuntimeError> {
		let mut state = self.state.lock().unwrap();
		if !state.recovered {
			return Err(RuntimeError::NotRecovered);
		}
		let intent = RequestIntent::new(client_id.clone(), amount_msat, description)?;
		if state.live.contains_key(&client_id) {
			let record = state.requests.lookup(&client_id)?.ok_or(RuntimeError::NotFound)?;
			if record.intent() != &intent {
				return Err(RuntimeError::Conflict);
			}
			return Ok(state.live[&client_id].status());
		}
		if state.live.len() >= MAX_LIVE_REQUESTS {
			return Err(RuntimeError::Capacity);
		}
		let local_request_id = state.requests.local_request_id(&client_id)?;
		let record = match state.requests.lookup(&client_id)? {
			Some(existing) => existing,
			None => {
				let channel =
					self.eligible_channel(&state, amount_msat)?.ok_or(RuntimeError::Ineligible)?;
				let tip = self.manager.current_best_block().height;
				let plan = RequestPlan {
					channel,
					settlement: self.config.settlement_node_id,
					parameters: FFORReceiverParameters {
						local_request_id,
						amounts_msat: vec![amount_msat],
						minimum_payment_msat: amount_msat,
						settlement_deadline: tip
							.checked_add(self.config.settlement_deadline_blocks)
							.ok_or(RuntimeError::InvalidIntent)?,
						voucher_expiry: tip
							.checked_add(self.config.voucher_expiry_blocks)
							.ok_or(RuntimeError::InvalidIntent)?,
						fee_base_msat: self.config.fee_base_msat,
						fee_proportional_millionths: self.config.fee_proportional_millionths,
						claim_margin_blocks: self.config.claim_margin_blocks,
						witness_peers: Some(self.witness_ids()),
						hash_chain: false,
					},
				};
				state.requests.begin(intent.clone(), plan)?
			},
		};
		if record.intent() != &intent {
			return Err(RuntimeError::Conflict);
		}
		let live = LiveRequest {
			client_id: client_id.clone(),
			local_request_id,
			channel: record.plan().channel,
			settlement: record.plan().settlement,
			amount_msat,
			settlement_deadline: record.plan().parameters.settlement_deadline,
			voucher_expiry: record.plan().parameters.voucher_expiry,
			selector: None,
			stage: Stage::Prepare,
			bolt11: None,
			invoice_expires_at: None,
			cancel_requested: false,
			needs_receipts: false,
			provision_started: BTreeMap::new(),
		};
		let status = live.status();
		state.live.insert(client_id, live);
		drop(state);
		self.wake.notify_one();
		Ok(status)
	}

	pub(crate) fn status(&self, client_id: &str) -> Result<OfflineReceiveStatus, RuntimeError> {
		let state = self.state.lock().unwrap();
		state.live.get(client_id).map(LiveRequest::status).ok_or(RuntimeError::NotFound)
	}

	/// Request cancellation. Before activation the native setup is cancelled; afterwards a
	/// cooperative close is requested. The worker applies it on its next pass.
	pub(crate) fn cancel(&self, client_id: &str) -> Result<(), RuntimeError> {
		let mut state = self.state.lock().unwrap();
		let live = state.live.get_mut(client_id).ok_or(RuntimeError::NotFound)?;
		if !live.stage.is_terminal() {
			live.cancel_requested = true;
		}
		drop(state);
		self.wake.notify_one();
		Ok(())
	}

	/// Spawn the worker. It stops with the node's stop signal.
	pub(crate) fn spawn_worker(
		self: &Arc<Self>, runtime: &crate::runtime::Runtime,
		mut stop: tokio::sync::watch::Receiver<()>,
	) {
		let this = Arc::clone(self);
		runtime.spawn_cancellable_background_task(async move {
			let mut interval = tokio::time::interval(this.poll_interval());
			interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
			loop {
				tokio::select! {
					_ = stop.changed() => {
						log_info!(this.logger, "Stopping offline receive worker.");
						return;
					},
					_ = interval.tick() => {},
					_ = this.wake.notified() => {},
				}
				this.tick().await;
			}
		});
	}

	fn log_error(&self, context: &str, error: &dyn fmt::Debug) {
		log_error!(self.logger, "Offline receive {}: {:?}", context, error);
	}
}

/// A channel with native history is eligible only when every historical epoch is terminal.
/// Native still decides admission; this predicate only avoids allocating an intent that
/// native would refuse with `AlreadyRegistered`.
fn history_blocks_channel(
	channel: &ChannelId, contexts: &[FFORReceiverRecoveryContext],
	terminal: impl Fn(&FFORReceiverRecoveryContext) -> bool,
) -> bool {
	contexts.iter().any(|context| context.channel_id() == *channel && !terminal(context))
}

#[cfg(test)]
mod tests;

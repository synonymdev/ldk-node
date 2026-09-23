// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Regtest driver for the experimental offline-receive runtime.
//!
//! The receiver is an LDK Node whose settlement peer `S` and witness `W` are reference daemons.
//! It is driven by `scripts/ffor-regtest/e2e.mjs`, which reads the line protocol printed here:
//! `NODEID`, `ADDRESS`, `FUNDED`, `OPENED`, `CHANNEL_READY`, `STATUS`, `INVOICE`, `PAYMENT`,
//! `EVENT` and `ERROR`. In `serve` mode the request is prepared only after a `prepare` line
//! arrives on standard input, so the orchestrator can first make S push its `channel_update`
//! for the private channel (native invoice binding needs S's forwarding terms). The process
//! runs until standard input closes (graceful stop) or it is killed.
//!
//! Usage:
//! `ffor_regtest_receiver <storage_dir> <listen_port> <electrum_url> <s_node_id> <s_host:port>
//! <w_node_id> <w_host:port> <info|serve|status> [request_id] [amount_msat]`

use std::env;
use std::io::{self, BufRead, Write};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::bitcoin::Network;
use ldk_node::config::{
	AnchorChannelsConfig, BackgroundSyncConfig, Config, ElectrumSyncConfig, OfflineReceiveConfig,
	OfflineReceiveWitnessConfig,
};
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::logger::LogLevel;
use ldk_node::payment::OfflineReceiveStatus;
use ldk_node::{Builder, Event, Node};

const DEFAULT_REQUEST_ID: &str = "e2e-1";
const DEFAULT_AMOUNT_MSAT: u64 = 50_000_000;
const DESCRIPTION: &str = "e2e";
const CHANNEL_WAIT: Duration = Duration::from_secs(300);
const POLL: Duration = Duration::from_secs(1);
/// The channel R funds towards S. Beignet's v1 open pins the anchor commitment feerate to the
/// 253 sat/kw floor, which LDK refuses on a regtest whose bitcoind carries real fee estimates,
/// so R opens and pushes most of the value to S: the pushed amount is R's inbound capacity.
const CHANNEL_SATS: u64 = 500_000;
const PUSH_TO_SETTLEMENT_MSAT: u64 = 400_000_000;
const FUNDING_WAIT_SATS: u64 = 700_000;

struct Args {
	storage_dir: String,
	listen_port: u16,
	electrum_url: String,
	settlement: PublicKey,
	settlement_addr: SocketAddress,
	witness: PublicKey,
	witness_addr: SocketAddress,
	command: String,
	request_id: String,
	amount_msat: u64,
}

fn parse_args() -> Result<Args, String> {
	let args: Vec<String> = env::args().collect();
	if args.len() < 9 {
		return Err(format!(
			"usage: {} <storage_dir> <listen_port> <electrum_url> <s_node_id> <s_host:port> \
			 <w_node_id> <w_host:port> <info|serve|status> [request_id] [amount_msat]",
			args[0]
		));
	}
	let listen_port = args[2].parse::<u16>().map_err(|e| format!("listen_port: {e}"))?;
	let settlement = PublicKey::from_str(&args[4]).map_err(|e| format!("s_node_id: {e}"))?;
	let settlement_addr =
		SocketAddress::from_str(&args[5]).map_err(|e| format!("s_host:port: {e:?}"))?;
	let witness = PublicKey::from_str(&args[6]).map_err(|e| format!("w_node_id: {e}"))?;
	let witness_addr =
		SocketAddress::from_str(&args[7]).map_err(|e| format!("w_host:port: {e:?}"))?;
	let request_id = args.get(9).cloned().unwrap_or_else(|| DEFAULT_REQUEST_ID.to_owned());
	let amount_msat = match args.get(10) {
		Some(raw) => raw.parse::<u64>().map_err(|e| format!("amount_msat: {e}"))?,
		None => DEFAULT_AMOUNT_MSAT,
	};
	Ok(Args {
		storage_dir: args[1].clone(),
		listen_port,
		electrum_url: args[3].clone(),
		settlement,
		settlement_addr,
		witness,
		witness_addr,
		command: args[8].clone(),
		request_id,
		amount_msat,
	})
}

fn emit(line: String) {
	let mut out = io::stdout().lock();
	let _ = writeln!(out, "{line}");
	let _ = out.flush();
}

fn build_node(args: &Args) -> Result<Node, String> {
	let listen = SocketAddress::from_str(&format!("127.0.0.1:{}", args.listen_port))
		.map_err(|e| format!("listen address: {e:?}"))?;
	let mut config = Config::default();
	config.network = Network::Regtest;
	config.storage_dir_path = args.storage_dir.clone();
	config.listening_addresses = Some(vec![listen]);
	// Trusting the settlement peer keeps the anchor channel with it out of the on-chain reserve
	// so a small funding amount suffices.
	config.anchor_channels_config = Some(AnchorChannelsConfig {
		trusted_peers_no_reserve: vec![args.settlement],
		..AnchorChannelsConfig::default()
	});
	let mut builder = Builder::from_config(config);
	builder.set_entropy_seed_path(format!("{}/seed", args.storage_dir));
	// An alias plus listening address lets the node accept the channel whether or not the
	// settlement peer announces it.
	builder.set_node_alias("ffor-e2e-receiver".to_owned()).map_err(|e| format!("alias: {e}"))?;
	builder.set_chain_source_electrum(
		args.electrum_url.clone(),
		Some(ElectrumSyncConfig {
			background_sync_config: Some(BackgroundSyncConfig {
				onchain_wallet_sync_interval_secs: 10,
				lightning_wallet_sync_interval_secs: 10,
				fee_rate_cache_update_interval_secs: 60,
			}),
			..ElectrumSyncConfig::default()
		}),
	);
	builder.set_filesystem_logger(None, Some(LogLevel::Trace));
	builder.set_offline_receive_config(OfflineReceiveConfig::new(
		args.settlement,
		vec![OfflineReceiveWitnessConfig::new(args.witness)],
	));
	builder.build().map_err(|e| format!("build: {e}"))
}

fn connect_peers(node: &Node, args: &Args) {
	for (name, id, addr) in [
		("settlement", args.settlement, args.settlement_addr.clone()),
		("witness", args.witness, args.witness_addr.clone()),
	] {
		match node.connect(id, addr, true) {
			Ok(()) => emit(format!("CONNECTED {name} {id}")),
			Err(e) => emit(format!("ERROR connect {name}: {e}")),
		}
	}
}

fn spawn_event_printer(node: Arc<Node>) {
	thread::spawn(move || loop {
		let event = node.wait_next_event();
		match &event {
			Event::PaymentReceived { payment_hash, amount_msat, .. } => emit(format!(
				"EVENT PaymentReceived payment_hash={} amount_msat={}",
				payment_hash, amount_msat
			)),
			other => emit(format!("EVENT {other:?}")),
		}
		if let Err(e) = node.event_handled() {
			emit(format!("ERROR event_handled: {e}"));
		}
	});
}

fn print_payments(node: &Node) {
	for payment in node.list_payments() {
		emit(format!(
			"PAYMENT id={} status={:?} direction={:?} amount_msat={:?} kind={:?}",
			payment.id, payment.status, payment.direction, payment.amount_msat, payment.kind
		));
	}
}

/// Fund the on-chain wallet from the orchestrator (`ADDRESS` line) and open the channel to S
/// unless one already exists.
fn open_settlement_channel(node: &Node, args: &Args) -> Result<(), String> {
	if node.list_channels().iter().any(|channel| channel.counterparty_node_id == args.settlement) {
		return Ok(());
	}
	let address = node.onchain_payment().new_address().map_err(|e| format!("address: {e}"))?;
	emit(format!("ADDRESS {address}"));
	let started = Instant::now();
	loop {
		let spendable = node.list_balances().spendable_onchain_balance_sats;
		if spendable >= FUNDING_WAIT_SATS {
			emit(format!("FUNDED spendable_sats={spendable}"));
			break;
		}
		if started.elapsed() > CHANNEL_WAIT {
			return Err(format!("on-chain funding did not arrive (spendable {spendable} sats)"));
		}
		thread::sleep(POLL);
	}
	let user_channel_id = node
		.open_channel(
			args.settlement,
			args.settlement_addr.clone(),
			CHANNEL_SATS,
			Some(PUSH_TO_SETTLEMENT_MSAT),
			None,
		)
		.map_err(|e| format!("open_channel: {e}"))?;
	emit(format!("OPENED user_channel_id={user_channel_id:?}"));
	Ok(())
}

fn wait_for_settlement_channel(node: &Node, settlement: PublicKey) -> Result<(), String> {
	let started = Instant::now();
	loop {
		let ready = node
			.list_channels()
			.into_iter()
			.find(|channel| channel.counterparty_node_id == settlement && channel.is_channel_ready);
		if let Some(channel) = ready {
			emit(format!(
				"CHANNEL_READY channel_id={} inbound_capacity_msat={} announced={}",
				channel.channel_id, channel.inbound_capacity_msat, channel.is_announced
			));
			return Ok(());
		}
		if started.elapsed() > CHANNEL_WAIT {
			return Err("no ready channel with the settlement peer".to_owned());
		}
		thread::sleep(POLL);
	}
}

/// Poll the request until standard input closes, printing every status change.
fn follow_request(node: &Node, request_id: &str, stdin_closed: &Arc<AtomicBool>) {
	let mut last: Option<Result<OfflineReceiveStatus, String>> = None;
	while !stdin_closed.load(Ordering::Acquire) {
		let current =
			node.offline_receive().status(request_id.to_owned()).map_err(|e| e.to_string());
		if last.as_ref() != Some(&current) {
			match &current {
				Ok(status) => {
					emit(format!("STATUS {status:?}"));
					if let OfflineReceiveStatus::Ready { bolt11 } = status {
						emit(format!("INVOICE {bolt11}"));
					}
				},
				Err(e) => emit(format!("ERROR status: {e}")),
			}
			last = Some(current);
		}
		thread::sleep(POLL);
	}
}

struct StdinSignals {
	closed: Arc<AtomicBool>,
	prepare: Arc<AtomicBool>,
}

fn watch_stdin() -> StdinSignals {
	let signals = StdinSignals {
		closed: Arc::new(AtomicBool::new(false)),
		prepare: Arc::new(AtomicBool::new(false)),
	};
	let closed = Arc::clone(&signals.closed);
	let prepare = Arc::clone(&signals.prepare);
	thread::spawn(move || {
		let stdin = io::stdin();
		for line in stdin.lock().lines() {
			match line {
				Ok(line) if line.trim() == "prepare" => prepare.store(true, Ordering::Release),
				Ok(_) => {},
				Err(_) => break,
			}
		}
		closed.store(true, Ordering::Release);
	});
	signals
}

fn wait_for_prepare_signal(signals: &StdinSignals) -> Result<(), String> {
	emit("AWAITING_PREPARE".to_owned());
	let started = Instant::now();
	while !signals.prepare.load(Ordering::Acquire) {
		if signals.closed.load(Ordering::Acquire) {
			return Err("stdin closed before the prepare signal".to_owned());
		}
		if started.elapsed() > CHANNEL_WAIT {
			return Err("no prepare signal".to_owned());
		}
		thread::sleep(POLL);
	}
	Ok(())
}

fn run(args: Args) -> Result<(), String> {
	let node = Arc::new(build_node(&args)?);
	emit(format!("NODEID {}", node.node_id()));
	if args.command == "info" {
		return Ok(());
	}
	if args.command != "serve" && args.command != "status" {
		return Err(format!("unknown command {}", args.command));
	}
	node.start().map_err(|e| format!("start: {e}"))?;
	emit(format!("LISTENING 127.0.0.1:{}", args.listen_port));
	let stdin = watch_stdin();
	spawn_event_printer(Arc::clone(&node));
	connect_peers(&node, &args);
	print_payments(&node);
	if args.command == "serve" {
		open_settlement_channel(&node, &args)?;
		wait_for_settlement_channel(&node, args.settlement)?;
		wait_for_prepare_signal(&stdin)?;
		match node.offline_receive().prepare(
			args.request_id.clone(),
			args.amount_msat,
			DESCRIPTION.to_owned(),
		) {
			Ok(status) => emit(format!("PREPARED {status:?}")),
			Err(e) => return Err(format!("prepare: {e}")),
		}
	}
	follow_request(&node, &args.request_id, &stdin.closed);
	print_payments(&node);
	node.stop().map_err(|e| format!("stop: {e}"))?;
	emit("STOPPED".to_owned());
	Ok(())
}

fn main() {
	let args = match parse_args() {
		Ok(args) => args,
		Err(e) => {
			emit(format!("ERROR {e}"));
			std::process::exit(2);
		},
	};
	if let Err(e) = run(args) {
		emit(format!("ERROR {e}"));
		std::process::exit(1);
	}
}

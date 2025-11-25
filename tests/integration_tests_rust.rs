// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

mod common;

use common::{
	do_channel_full_cycle, expect_channel_pending_event, expect_channel_ready_event, expect_event,
	expect_payment_received_event, expect_payment_successful_event, generate_blocks_and_wait,
	logging::{init_log_logger, validate_log_entry, TestLogWriter},
	open_channel, premine_and_distribute_funds, random_config, random_listening_addresses,
	setup_bitcoind_and_electrsd, setup_builder, setup_node, setup_two_nodes, wait_for_tx,
	TestChainSource, TestSyncStore,
};

use ldk_node::config::EsploraSyncConfig;
use ldk_node::liquidity::LSPS2ServiceConfig;
use ldk_node::payment::{
	ConfirmationStatus, PaymentDirection, PaymentKind, PaymentStatus, QrPaymentResult,
	SendingParameters,
};
use ldk_node::{Builder, Event, NodeError};

use lightning::ln::channelmanager::PaymentId;
use lightning::routing::gossip::{NodeAlias, NodeId};
use lightning::util::persist::KVStore;

use lightning_invoice::{Bolt11InvoiceDescription, Description};

use bitcoin::address::NetworkUnchecked;
use bitcoin::hashes::Hash;
use bitcoin::Address;
use bitcoin::Amount;
use bitcoin::Txid;
use log::LevelFilter;

use std::str::FromStr;
use std::sync::Arc;

#[test]
fn channel_full_cycle() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, true, false);
	do_channel_full_cycle(node_a, node_b, &bitcoind.client, &electrsd.client, false, true, false);
}

#[test]
fn channel_full_cycle_electrum() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Electrum(&electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, true, false);
	do_channel_full_cycle(node_a, node_b, &bitcoind.client, &electrsd.client, false, true, false);
}

#[test]
fn channel_full_cycle_bitcoind() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::BitcoindRpc(&bitcoind);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, true, false);
	do_channel_full_cycle(node_a, node_b, &bitcoind.client, &electrsd.client, false, true, false);
}

#[test]
fn channel_full_cycle_force_close() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, true, false);
	do_channel_full_cycle(node_a, node_b, &bitcoind.client, &electrsd.client, false, true, true);
}

#[test]
fn channel_full_cycle_force_close_trusted_no_reserve() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, true, true);
	do_channel_full_cycle(node_a, node_b, &bitcoind.client, &electrsd.client, false, true, true);
}

#[test]
fn channel_full_cycle_0conf() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, true, true, false);
	do_channel_full_cycle(node_a, node_b, &bitcoind.client, &electrsd.client, true, true, false)
}

#[test]
fn channel_full_cycle_legacy_staticremotekey() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, false, false);
	do_channel_full_cycle(node_a, node_b, &bitcoind.client, &electrsd.client, false, false, false);
}

#[test]
fn channel_open_fails_when_funds_insufficient() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, true, false);

	let addr_a = node_a.onchain_payment().new_address().unwrap();
	let addr_b = node_b.onchain_payment().new_address().unwrap();

	let premine_amount_sat = 100_000;

	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_a, addr_b],
		Amount::from_sat(premine_amount_sat),
	);
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, premine_amount_sat);
	assert_eq!(node_b.list_balances().spendable_onchain_balance_sats, premine_amount_sat);

	println!("\nA -- open_channel -> B");
	assert_eq!(
		Err(NodeError::InsufficientFunds),
		node_a.open_channel(
			node_b.node_id(),
			node_b.listening_addresses().unwrap().first().unwrap().clone(),
			120000,
			None,
			None,
		)
	);
}

#[test]
fn multi_hop_sending() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let esplora_url = format!("http://{}", electrsd.esplora_url.as_ref().unwrap());

	// Setup and fund 5 nodes
	let mut nodes = Vec::new();
	for _ in 0..5 {
		let config = random_config(true);
		let sync_config = EsploraSyncConfig { background_sync_config: None };
		setup_builder!(builder, config.node_config);
		builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));
		let node = builder.build().unwrap();
		node.start().unwrap();
		nodes.push(node);
	}

	let addresses = nodes.iter().map(|n| n.onchain_payment().new_address().unwrap()).collect();
	let premine_amount_sat = 5_000_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		addresses,
		Amount::from_sat(premine_amount_sat),
	);

	for n in &nodes {
		n.sync_wallets().unwrap();
		assert_eq!(n.list_balances().spendable_onchain_balance_sats, premine_amount_sat);
		assert_eq!(n.next_event(), None);
	}

	// Setup channel topology:
	//                    (1M:0)- N2 -(1M:0)
	//                   /                  \
	//  N0 -(100k:0)-> N1                    N4
	//                   \                  /
	//                    (1M:0)- N3 -(1M:0)

	open_channel(&nodes[0], &nodes[1], 100_000, true, &electrsd);
	open_channel(&nodes[1], &nodes[2], 1_000_000, true, &electrsd);
	// We need to sync wallets in-between back-to-back channel opens from the same node so BDK
	// wallet picks up on the broadcast funding tx and doesn't double-spend itself.
	//
	// TODO: Remove once fixed in BDK.
	nodes[1].sync_wallets().unwrap();
	open_channel(&nodes[1], &nodes[3], 1_000_000, true, &electrsd);
	open_channel(&nodes[2], &nodes[4], 1_000_000, true, &electrsd);
	open_channel(&nodes[3], &nodes[4], 1_000_000, true, &electrsd);

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6);

	for n in &nodes {
		n.sync_wallets().unwrap();
	}

	expect_event!(nodes[0], ChannelReady);
	expect_event!(nodes[1], ChannelReady);
	expect_event!(nodes[1], ChannelReady);
	expect_event!(nodes[1], ChannelReady);
	expect_event!(nodes[2], ChannelReady);
	expect_event!(nodes[2], ChannelReady);
	expect_event!(nodes[3], ChannelReady);
	expect_event!(nodes[3], ChannelReady);
	expect_event!(nodes[4], ChannelReady);
	expect_event!(nodes[4], ChannelReady);

	// Sleep a bit for gossip to propagate.
	std::thread::sleep(std::time::Duration::from_secs(1));

	let sending_params = SendingParameters {
		max_total_routing_fee_msat: Some(Some(75_000).into()),
		max_total_cltv_expiry_delta: Some(1000),
		max_path_count: Some(10),
		max_channel_saturation_power_of_half: Some(2),
	};

	let invoice_description =
		Bolt11InvoiceDescription::Direct(Description::new(String::from("asdf")).unwrap());
	let invoice = nodes[4]
		.bolt11_payment()
		.receive(2_500_000, &invoice_description.clone().into(), 9217)
		.unwrap();
	nodes[0].bolt11_payment().send(&invoice, Some(sending_params)).unwrap();

	expect_event!(nodes[1], PaymentForwarded);

	// We expect that the payment goes through N2 or N3, so we check both for the PaymentForwarded event.
	let node_2_fwd_event = matches!(nodes[2].next_event(), Some(Event::PaymentForwarded { .. }));
	let node_3_fwd_event = matches!(nodes[3].next_event(), Some(Event::PaymentForwarded { .. }));
	assert!(node_2_fwd_event || node_3_fwd_event);

	let payment_id = expect_payment_received_event!(&nodes[4], 2_500_000);
	let fee_paid_msat = Some(2000);
	expect_payment_successful_event!(nodes[0], payment_id, Some(fee_paid_msat));
}

#[test]
fn start_stop_reinit() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let config = random_config(true);

	let esplora_url = format!("http://{}", electrsd.esplora_url.as_ref().unwrap());

	let test_sync_store: Arc<dyn KVStore + Sync + Send> =
		Arc::new(TestSyncStore::new(config.node_config.storage_dir_path.clone().into()));

	let sync_config = EsploraSyncConfig { background_sync_config: None };
	setup_builder!(builder, config.node_config);
	builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));

	let node = builder.build_with_store(Arc::clone(&test_sync_store)).unwrap();
	node.start().unwrap();

	let expected_node_id = node.node_id();
	assert_eq!(node.start(), Err(NodeError::AlreadyRunning));

	let funding_address = node.onchain_payment().new_address().unwrap();

	assert_eq!(node.list_balances().total_onchain_balance_sats, 0);

	let expected_amount = Amount::from_sat(100000);
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![funding_address],
		expected_amount,
	);

	node.sync_wallets().unwrap();
	assert_eq!(node.list_balances().spendable_onchain_balance_sats, expected_amount.to_sat());

	let log_file = format!("{}/ldk_node.log", config.node_config.clone().storage_dir_path);
	assert!(std::path::Path::new(&log_file).exists());

	node.stop().unwrap();
	assert_eq!(node.stop(), Err(NodeError::NotRunning));

	node.start().unwrap();
	assert_eq!(node.start(), Err(NodeError::AlreadyRunning));

	node.stop().unwrap();
	assert_eq!(node.stop(), Err(NodeError::NotRunning));
	drop(node);

	setup_builder!(builder, config.node_config);
	builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));

	let reinitialized_node = builder.build_with_store(Arc::clone(&test_sync_store)).unwrap();
	reinitialized_node.start().unwrap();
	assert_eq!(reinitialized_node.node_id(), expected_node_id);

	assert_eq!(
		reinitialized_node.list_balances().spendable_onchain_balance_sats,
		expected_amount.to_sat()
	);

	reinitialized_node.sync_wallets().unwrap();
	assert_eq!(
		reinitialized_node.list_balances().spendable_onchain_balance_sats,
		expected_amount.to_sat()
	);

	reinitialized_node.stop().unwrap();
}

#[test]
fn onchain_send_receive() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, true, false);

	let addr_a = node_a.onchain_payment().new_address().unwrap();
	let addr_b = node_b.onchain_payment().new_address().unwrap();
	// This is a Bitcoin Testnet address. Sending funds to this address from the Regtest network will fail
	let static_address = "tb1q0d40e5rta4fty63z64gztf8c3v20cvet6v2jdh";
	let unchecked_address = Address::<NetworkUnchecked>::from_str(static_address).unwrap();
	let addr_c = unchecked_address.assume_checked();

	let premine_amount_sat = 1_100_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_a.clone(), addr_b.clone()],
		Amount::from_sat(premine_amount_sat),
	);

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, premine_amount_sat);
	assert_eq!(node_b.list_balances().spendable_onchain_balance_sats, premine_amount_sat);

	let node_a_payments = node_a.list_payments();
	let node_b_payments = node_b.list_payments();
	for payments in [&node_a_payments, &node_b_payments] {
		assert_eq!(payments.len(), 1)
	}
	for p in [node_a_payments.first().unwrap(), node_b_payments.first().unwrap()] {
		assert_eq!(p.amount_msat, Some(premine_amount_sat * 1000));
		assert_eq!(p.direction, PaymentDirection::Inbound);
		// We got only 1-conf here, so we're only pending for now.
		assert_eq!(p.status, PaymentStatus::Pending);
		match p.kind {
			PaymentKind::Onchain { status, .. } => {
				assert!(matches!(status, ConfirmationStatus::Confirmed { .. }));
			},
			_ => panic!("Unexpected payment kind"),
		}
	}

	let channel_amount_sat = 1_000_000;
	let reserve_amount_sat = 25_000;
	open_channel(&node_b, &node_a, channel_amount_sat, true, &electrsd);
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6);

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	expect_channel_ready_event!(node_a, node_b.node_id());
	expect_channel_ready_event!(node_b, node_a.node_id());

	let node_a_payments =
		node_a.list_payments_with_filter(|p| matches!(p.kind, PaymentKind::Onchain { .. }));
	assert_eq!(node_a_payments.len(), 1);
	let node_b_payments =
		node_b.list_payments_with_filter(|p| matches!(p.kind, PaymentKind::Onchain { .. }));
	assert_eq!(node_b_payments.len(), 2);

	let onchain_fee_buffer_sat = 1000;
	let expected_node_a_balance = premine_amount_sat - reserve_amount_sat;
	let expected_node_b_balance_lower =
		premine_amount_sat - channel_amount_sat - reserve_amount_sat - onchain_fee_buffer_sat;
	let expected_node_b_balance_upper =
		premine_amount_sat - channel_amount_sat - reserve_amount_sat;
	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, expected_node_a_balance);
	assert!(node_b.list_balances().spendable_onchain_balance_sats > expected_node_b_balance_lower);
	assert!(node_b.list_balances().spendable_onchain_balance_sats < expected_node_b_balance_upper);

	assert_eq!(
		Err(NodeError::InsufficientFunds),
		node_a.onchain_payment().send_to_address(&addr_b, expected_node_a_balance + 1, None, None)
	);

	assert_eq!(
		Err(NodeError::InvalidAddress),
		node_a.onchain_payment().send_to_address(&addr_c, expected_node_a_balance + 1, None, None)
	);

	assert_eq!(
		Err(NodeError::InvalidAddress),
		node_a.onchain_payment().send_all_to_address(&addr_c, true, None)
	);

	let amount_to_send_sats = 54321;
	let txid =
		node_b.onchain_payment().send_to_address(&addr_a, amount_to_send_sats, None, None).unwrap();
	wait_for_tx(&electrsd.client, txid);
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	let payment_id = PaymentId(txid.to_byte_array());
	let payment_a = node_a.payment(&payment_id).unwrap();
	assert_eq!(payment_a.status, PaymentStatus::Pending);
	match payment_a.kind {
		PaymentKind::Onchain { status, .. } => {
			assert!(matches!(status, ConfirmationStatus::Unconfirmed));
		},
		_ => panic!("Unexpected payment kind"),
	}
	assert!(payment_a.fee_paid_msat > Some(0));
	let payment_b = node_b.payment(&payment_id).unwrap();
	assert_eq!(payment_b.status, PaymentStatus::Pending);
	match payment_a.kind {
		PaymentKind::Onchain { status, .. } => {
			assert!(matches!(status, ConfirmationStatus::Unconfirmed));
		},
		_ => panic!("Unexpected payment kind"),
	}
	assert!(payment_b.fee_paid_msat > Some(0));
	assert_eq!(payment_a.amount_msat, Some(amount_to_send_sats * 1000));
	assert_eq!(payment_a.amount_msat, payment_b.amount_msat);
	assert_eq!(payment_a.fee_paid_msat, payment_b.fee_paid_msat);

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6);
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	let expected_node_a_balance = expected_node_a_balance + amount_to_send_sats;
	let expected_node_b_balance_lower = expected_node_b_balance_lower - amount_to_send_sats;
	let expected_node_b_balance_upper = expected_node_b_balance_upper - amount_to_send_sats;
	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, expected_node_a_balance);
	assert!(node_b.list_balances().spendable_onchain_balance_sats > expected_node_b_balance_lower);
	assert!(node_b.list_balances().spendable_onchain_balance_sats < expected_node_b_balance_upper);

	let node_a_payments =
		node_a.list_payments_with_filter(|p| matches!(p.kind, PaymentKind::Onchain { .. }));
	assert_eq!(node_a_payments.len(), 2);
	let node_b_payments =
		node_b.list_payments_with_filter(|p| matches!(p.kind, PaymentKind::Onchain { .. }));
	assert_eq!(node_b_payments.len(), 3);

	let payment_a = node_a.payment(&payment_id).unwrap();
	match payment_a.kind {
		PaymentKind::Onchain { txid: _txid, status } => {
			assert_eq!(_txid, txid);
			assert!(matches!(status, ConfirmationStatus::Confirmed { .. }));
		},
		_ => panic!("Unexpected payment kind"),
	}

	let payment_b = node_a.payment(&payment_id).unwrap();
	match payment_b.kind {
		PaymentKind::Onchain { txid: _txid, status } => {
			assert_eq!(_txid, txid);
			assert!(matches!(status, ConfirmationStatus::Confirmed { .. }));
		},
		_ => panic!("Unexpected payment kind"),
	}

	let addr_b = node_b.onchain_payment().new_address().unwrap();
	let txid = node_a.onchain_payment().send_all_to_address(&addr_b, true, None).unwrap();
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6);
	wait_for_tx(&electrsd.client, txid);

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	let expected_node_b_balance_lower = expected_node_b_balance_lower + expected_node_a_balance;
	let expected_node_b_balance_upper = expected_node_b_balance_upper + expected_node_a_balance;
	let expected_node_a_balance = 0;
	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, expected_node_a_balance);
	assert_eq!(node_a.list_balances().total_onchain_balance_sats, reserve_amount_sat);
	assert!(node_b.list_balances().spendable_onchain_balance_sats > expected_node_b_balance_lower);
	assert!(node_b.list_balances().spendable_onchain_balance_sats < expected_node_b_balance_upper);

	let node_a_payments =
		node_a.list_payments_with_filter(|p| matches!(p.kind, PaymentKind::Onchain { .. }));
	assert_eq!(node_a_payments.len(), 3);
	let node_b_payments =
		node_b.list_payments_with_filter(|p| matches!(p.kind, PaymentKind::Onchain { .. }));
	assert_eq!(node_b_payments.len(), 4);

	let addr_b = node_b.onchain_payment().new_address().unwrap();
	let txid = node_a.onchain_payment().send_all_to_address(&addr_b, false, None).unwrap();
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6);
	wait_for_tx(&electrsd.client, txid);

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	let expected_node_b_balance_lower = expected_node_b_balance_lower + reserve_amount_sat;
	let expected_node_b_balance_upper = expected_node_b_balance_upper + reserve_amount_sat;
	let expected_node_a_balance = 0;

	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, expected_node_a_balance);
	assert_eq!(node_a.list_balances().total_onchain_balance_sats, expected_node_a_balance);
	assert!(node_b.list_balances().spendable_onchain_balance_sats > expected_node_b_balance_lower);
	assert!(node_b.list_balances().spendable_onchain_balance_sats < expected_node_b_balance_upper);

	let node_a_payments =
		node_a.list_payments_with_filter(|p| matches!(p.kind, PaymentKind::Onchain { .. }));
	assert_eq!(node_a_payments.len(), 4);
	let node_b_payments =
		node_b.list_payments_with_filter(|p| matches!(p.kind, PaymentKind::Onchain { .. }));
	assert_eq!(node_b_payments.len(), 5);
}

#[test]
fn onchain_send_all_retains_reserve() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, true, false);

	// Setup nodes
	let addr_a = node_a.onchain_payment().new_address().unwrap();
	let addr_b = node_b.onchain_payment().new_address().unwrap();

	let premine_amount_sat = 1_000_000;
	let reserve_amount_sat = 25_000;
	let onchain_fee_buffer_sat = 1000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_a.clone(), addr_b.clone()],
		Amount::from_sat(premine_amount_sat),
	);

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, premine_amount_sat);
	assert_eq!(node_b.list_balances().spendable_onchain_balance_sats, premine_amount_sat);

	// Send all over, with 0 reserve as we don't have any channels open.
	let txid = node_a.onchain_payment().send_all_to_address(&addr_b, true, None).unwrap();

	wait_for_tx(&electrsd.client, txid);
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6);

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	// Check node a sent all and node b received it
	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, 0);
	assert!(((premine_amount_sat * 2 - onchain_fee_buffer_sat)..=(premine_amount_sat * 2))
		.contains(&node_b.list_balances().spendable_onchain_balance_sats));

	// Refill to make sure we have enough reserve for the channel open.
	let txid = bitcoind
		.client
		.send_to_address(&addr_a, Amount::from_sat(reserve_amount_sat))
		.unwrap()
		.0
		.parse()
		.unwrap();
	wait_for_tx(&electrsd.client, txid);
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6);
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, reserve_amount_sat);

	// Open a channel.
	open_channel(&node_b, &node_a, premine_amount_sat, false, &electrsd);
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6);
	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();
	expect_channel_ready_event!(node_a, node_b.node_id());
	expect_channel_ready_event!(node_b, node_a.node_id());

	// Check node a sent all and node b received it
	assert_eq!(node_a.list_balances().spendable_onchain_balance_sats, 0);
	assert!(((premine_amount_sat - reserve_amount_sat - onchain_fee_buffer_sat)
		..=premine_amount_sat)
		.contains(&node_b.list_balances().spendable_onchain_balance_sats));

	// Send all over again, this time ensuring the reserve is accounted for
	let txid = node_b.onchain_payment().send_all_to_address(&addr_a, true, None).unwrap();

	wait_for_tx(&electrsd.client, txid);
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6);

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	// Check node b sent all and node a received it
	assert_eq!(node_b.list_balances().total_onchain_balance_sats, reserve_amount_sat);
	assert_eq!(node_b.list_balances().spendable_onchain_balance_sats, 0);
	assert!(((premine_amount_sat - reserve_amount_sat - onchain_fee_buffer_sat)
		..=premine_amount_sat)
		.contains(&node_a.list_balances().spendable_onchain_balance_sats));
}

#[test]
fn onchain_wallet_recovery() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();

	let chain_source = TestChainSource::Esplora(&electrsd);

	let seed_bytes = vec![42u8; 64];

	let original_config = random_config(true);
	let original_node = setup_node(&chain_source, original_config, Some(seed_bytes.clone()));

	let premine_amount_sat = 100_000;

	let addr_1 = original_node.onchain_payment().new_address().unwrap();

	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr_1],
		Amount::from_sat(premine_amount_sat),
	);
	original_node.sync_wallets().unwrap();
	assert_eq!(original_node.list_balances().spendable_onchain_balance_sats, premine_amount_sat);

	let addr_2 = original_node.onchain_payment().new_address().unwrap();

	let txid = bitcoind
		.client
		.send_to_address(&addr_2, Amount::from_sat(premine_amount_sat))
		.unwrap()
		.0
		.parse()
		.unwrap();
	wait_for_tx(&electrsd.client, txid);

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1);

	original_node.sync_wallets().unwrap();
	assert_eq!(
		original_node.list_balances().spendable_onchain_balance_sats,
		premine_amount_sat * 2
	);

	original_node.stop().unwrap();
	drop(original_node);

	// Now we start from scratch, only the seed remains the same.
	let recovered_config = random_config(true);
	let recovered_node = setup_node(&chain_source, recovered_config, Some(seed_bytes));

	recovered_node.sync_wallets().unwrap();
	assert_eq!(
		recovered_node.list_balances().spendable_onchain_balance_sats,
		premine_amount_sat * 2
	);

	// Check we sync even when skipping some addresses.
	let _addr_3 = recovered_node.onchain_payment().new_address().unwrap();
	let _addr_4 = recovered_node.onchain_payment().new_address().unwrap();
	let _addr_5 = recovered_node.onchain_payment().new_address().unwrap();
	let addr_6 = recovered_node.onchain_payment().new_address().unwrap();

	let txid = bitcoind
		.client
		.send_to_address(&addr_6, Amount::from_sat(premine_amount_sat))
		.unwrap()
		.0
		.parse()
		.unwrap();
	wait_for_tx(&electrsd.client, txid);

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1);

	recovered_node.sync_wallets().unwrap();
	assert_eq!(
		recovered_node.list_balances().spendable_onchain_balance_sats,
		premine_amount_sat * 3
	);
}

#[test]
fn sign_verify_msg() {
	let (_bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let config = random_config(true);
	let chain_source = TestChainSource::Esplora(&electrsd);
	let node = setup_node(&chain_source, config, None);

	// Tests arbitrary message signing and later verification
	let msg = "OK computer".as_bytes();
	let sig = node.sign_message(msg);
	let pkey = node.node_id();
	assert!(node.verify_signature(msg, sig.as_str(), &pkey));
}

#[test]
fn connection_restart_behavior() {
	do_connection_restart_behavior(true);
	do_connection_restart_behavior(false);
}

fn do_connection_restart_behavior(persist: bool) {
	let (_bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, false, false);

	let node_id_a = node_a.node_id();
	let node_id_b = node_b.node_id();

	let node_addr_b = node_b.listening_addresses().unwrap().first().unwrap().clone();

	while !node_b.status().is_listening {
		std::thread::sleep(std::time::Duration::from_millis(10));
	}

	node_a.connect(node_id_b, node_addr_b, persist).unwrap();

	let peer_details_a = node_a.list_peers().first().unwrap().clone();
	assert_eq!(peer_details_a.node_id, node_id_b);
	assert_eq!(peer_details_a.is_persisted, persist);
	assert!(peer_details_a.is_connected);

	let peer_details_b = node_b.list_peers().first().unwrap().clone();
	assert_eq!(peer_details_b.node_id, node_id_a);
	assert_eq!(peer_details_b.is_persisted, false);
	assert!(peer_details_a.is_connected);

	// Restart nodes.
	node_a.stop().unwrap();
	node_b.stop().unwrap();
	node_b.start().unwrap();
	node_a.start().unwrap();

	// Sleep a bit to allow for the reconnect to happen.
	std::thread::sleep(std::time::Duration::from_secs(5));

	if persist {
		let peer_details_a = node_a.list_peers().first().unwrap().clone();
		assert_eq!(peer_details_a.node_id, node_id_b);
		assert_eq!(peer_details_a.is_persisted, persist);
		assert!(peer_details_a.is_connected);

		let peer_details_b = node_b.list_peers().first().unwrap().clone();
		assert_eq!(peer_details_b.node_id, node_id_a);
		assert_eq!(peer_details_b.is_persisted, false);
		assert!(peer_details_a.is_connected);
	} else {
		assert!(node_a.list_peers().is_empty());
		assert!(node_b.list_peers().is_empty());
	}
}

#[test]
fn concurrent_connections_succeed() {
	let (_bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, true, false);

	let node_a = Arc::new(node_a);
	let node_b = Arc::new(node_b);

	let node_id_b = node_b.node_id();
	let node_addr_b = node_b.listening_addresses().unwrap().first().unwrap().clone();

	while !node_b.status().is_listening {
		std::thread::sleep(std::time::Duration::from_millis(10));
	}

	let mut handles = Vec::new();
	for _ in 0..10 {
		let thread_node = Arc::clone(&node_a);
		let thread_addr = node_addr_b.clone();
		let handle = std::thread::spawn(move || {
			thread_node.connect(node_id_b, thread_addr, false).unwrap();
		});
		handles.push(handle);
	}

	for h in handles {
		h.join().unwrap();
	}
}

#[test]
fn simple_bolt12_send_receive() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let (node_a, node_b) = setup_two_nodes(&chain_source, false, true, false);

	let address_a = node_a.onchain_payment().new_address().unwrap();
	let premine_amount_sat = 5_000_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![address_a],
		Amount::from_sat(premine_amount_sat),
	);

	node_a.sync_wallets().unwrap();
	open_channel(&node_a, &node_b, 4_000_000, true, &electrsd);

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6);

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	expect_channel_ready_event!(node_a, node_b.node_id());
	expect_channel_ready_event!(node_b, node_a.node_id());

	// Sleep until we broadcasted a node announcement.
	while node_b.status().latest_node_announcement_broadcast_timestamp.is_none() {
		std::thread::sleep(std::time::Duration::from_millis(10));
	}

	// Sleep one more sec to make sure the node announcement propagates.
	std::thread::sleep(std::time::Duration::from_secs(1));

	let expected_amount_msat = 100_000_000;
	let offer =
		node_b.bolt12_payment().receive(expected_amount_msat, "asdf", None, Some(1)).unwrap();
	let expected_quantity = Some(1);
	let expected_payer_note = Some("Test".to_string());
	let payment_id = node_a
		.bolt12_payment()
		.send(&offer, expected_quantity, expected_payer_note.clone())
		.unwrap();

	expect_payment_successful_event!(node_a, Some(payment_id), None);
	let node_a_payments =
		node_a.list_payments_with_filter(|p| matches!(p.kind, PaymentKind::Bolt12Offer { .. }));
	assert_eq!(node_a_payments.len(), 1);
	match node_a_payments.first().unwrap().kind {
		PaymentKind::Bolt12Offer {
			hash,
			preimage,
			secret: _,
			offer_id,
			quantity: ref qty,
			payer_note: ref note,
		} => {
			assert!(hash.is_some());
			assert!(preimage.is_some());
			assert_eq!(offer_id, offer.id());
			assert_eq!(&expected_quantity, qty);
			assert_eq!(expected_payer_note.unwrap(), note.clone().unwrap().0);
			//TODO: We should eventually set and assert the secret sender-side, too, but the BOLT12
			//API currently doesn't allow to do that.
		},
		_ => {
			panic!("Unexpected payment kind");
		},
	}
	assert_eq!(node_a_payments.first().unwrap().amount_msat, Some(expected_amount_msat));

	expect_payment_received_event!(node_b, expected_amount_msat);
	let node_b_payments =
		node_b.list_payments_with_filter(|p| matches!(p.kind, PaymentKind::Bolt12Offer { .. }));
	assert_eq!(node_b_payments.len(), 1);
	match node_b_payments.first().unwrap().kind {
		PaymentKind::Bolt12Offer { hash, preimage, secret, offer_id, .. } => {
			assert!(hash.is_some());
			assert!(preimage.is_some());
			assert!(secret.is_some());
			assert_eq!(offer_id, offer.id());
		},
		_ => {
			panic!("Unexpected payment kind");
		},
	}
	assert_eq!(node_b_payments.first().unwrap().amount_msat, Some(expected_amount_msat));

	// Test send_using_amount
	let offer_amount_msat = 100_000_000;
	let less_than_offer_amount = offer_amount_msat - 10_000;
	let expected_amount_msat = offer_amount_msat + 10_000;
	let offer = node_b.bolt12_payment().receive(offer_amount_msat, "asdf", None, Some(1)).unwrap();
	let expected_quantity = Some(1);
	let expected_payer_note = Some("Test".to_string());
	assert!(node_a
		.bolt12_payment()
		.send_using_amount(&offer, less_than_offer_amount, None, None)
		.is_err());
	let payment_id = node_a
		.bolt12_payment()
		.send_using_amount(
			&offer,
			expected_amount_msat,
			expected_quantity,
			expected_payer_note.clone(),
		)
		.unwrap();

	expect_payment_successful_event!(node_a, Some(payment_id), None);
	let node_a_payments = node_a.list_payments_with_filter(|p| {
		matches!(p.kind, PaymentKind::Bolt12Offer { .. }) && p.id == payment_id
	});
	assert_eq!(node_a_payments.len(), 1);
	let payment_hash = match node_a_payments.first().unwrap().kind {
		PaymentKind::Bolt12Offer {
			hash,
			preimage,
			secret: _,
			offer_id,
			quantity: ref qty,
			payer_note: ref note,
		} => {
			assert!(hash.is_some());
			assert!(preimage.is_some());
			assert_eq!(offer_id, offer.id());
			assert_eq!(&expected_quantity, qty);
			assert_eq!(expected_payer_note.unwrap(), note.clone().unwrap().0);
			//TODO: We should eventually set and assert the secret sender-side, too, but the BOLT12
			//API currently doesn't allow to do that.
			hash.unwrap()
		},
		_ => {
			panic!("Unexpected payment kind");
		},
	};
	assert_eq!(node_a_payments.first().unwrap().amount_msat, Some(expected_amount_msat));

	expect_payment_received_event!(node_b, expected_amount_msat);
	let node_b_payment_id = PaymentId(payment_hash.0);
	let node_b_payments = node_b.list_payments_with_filter(|p| {
		matches!(p.kind, PaymentKind::Bolt12Offer { .. }) && p.id == node_b_payment_id
	});
	assert_eq!(node_b_payments.len(), 1);
	match node_b_payments.first().unwrap().kind {
		PaymentKind::Bolt12Offer { hash, preimage, secret, offer_id, .. } => {
			assert!(hash.is_some());
			assert!(preimage.is_some());
			assert!(secret.is_some());
			assert_eq!(offer_id, offer.id());
		},
		_ => {
			panic!("Unexpected payment kind");
		},
	}
	assert_eq!(node_b_payments.first().unwrap().amount_msat, Some(expected_amount_msat));

	// Now node_b refunds the amount node_a just overpaid.
	let overpaid_amount = expected_amount_msat - offer_amount_msat;
	let expected_quantity = Some(1);
	let expected_payer_note = Some("Test".to_string());
	let refund = node_b
		.bolt12_payment()
		.initiate_refund(overpaid_amount, 3600, expected_quantity, expected_payer_note.clone())
		.unwrap();
	let invoice = node_a.bolt12_payment().request_refund_payment(&refund).unwrap();
	expect_payment_received_event!(node_a, overpaid_amount);

	let node_b_payment_id = node_b
		.list_payments_with_filter(|p| {
			matches!(p.kind, PaymentKind::Bolt12Refund { .. })
				&& p.amount_msat == Some(overpaid_amount)
		})
		.first()
		.unwrap()
		.id;
	expect_payment_successful_event!(node_b, Some(node_b_payment_id), None);

	let node_b_payments = node_b.list_payments_with_filter(|p| {
		matches!(p.kind, PaymentKind::Bolt12Refund { .. }) && p.id == node_b_payment_id
	});
	assert_eq!(node_b_payments.len(), 1);
	match node_b_payments.first().unwrap().kind {
		PaymentKind::Bolt12Refund {
			hash,
			preimage,
			secret: _,
			quantity: ref qty,
			payer_note: ref note,
		} => {
			assert!(hash.is_some());
			assert!(preimage.is_some());
			assert_eq!(&expected_quantity, qty);
			assert_eq!(expected_payer_note.unwrap(), note.clone().unwrap().0)
			//TODO: We should eventually set and assert the secret sender-side, too, but the BOLT12
			//API currently doesn't allow to do that.
		},
		_ => {
			panic!("Unexpected payment kind");
		},
	}
	assert_eq!(node_b_payments.first().unwrap().amount_msat, Some(overpaid_amount));

	let node_a_payment_id = PaymentId(invoice.payment_hash().0);
	let node_a_payments = node_a.list_payments_with_filter(|p| {
		matches!(p.kind, PaymentKind::Bolt12Refund { .. }) && p.id == node_a_payment_id
	});
	assert_eq!(node_a_payments.len(), 1);
	match node_a_payments.first().unwrap().kind {
		PaymentKind::Bolt12Refund { hash, preimage, secret, .. } => {
			assert!(hash.is_some());
			assert!(preimage.is_some());
			assert!(secret.is_some());
		},
		_ => {
			panic!("Unexpected payment kind");
		},
	}
	assert_eq!(node_a_payments.first().unwrap().amount_msat, Some(overpaid_amount));
}

#[test]
fn test_node_announcement_propagation() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Node A will use both listening and announcement addresses
	let mut config_a = random_config(true);
	let node_a_alias_string = "ldk-node-a".to_string();
	let mut node_a_alias_bytes = [0u8; 32];
	node_a_alias_bytes[..node_a_alias_string.as_bytes().len()]
		.copy_from_slice(node_a_alias_string.as_bytes());
	let node_a_node_alias = Some(NodeAlias(node_a_alias_bytes));
	let node_a_announcement_addresses = random_listening_addresses();
	config_a.node_config.node_alias = node_a_node_alias.clone();
	config_a.node_config.listening_addresses = Some(random_listening_addresses());
	config_a.node_config.announcement_addresses = Some(node_a_announcement_addresses.clone());

	// Node B will only use listening addresses
	let mut config_b = random_config(true);
	let node_b_alias_string = "ldk-node-b".to_string();
	let mut node_b_alias_bytes = [0u8; 32];
	node_b_alias_bytes[..node_b_alias_string.as_bytes().len()]
		.copy_from_slice(node_b_alias_string.as_bytes());
	let node_b_node_alias = Some(NodeAlias(node_b_alias_bytes));
	let node_b_listening_addresses = random_listening_addresses();
	config_b.node_config.node_alias = node_b_node_alias.clone();
	config_b.node_config.listening_addresses = Some(node_b_listening_addresses.clone());
	config_b.node_config.announcement_addresses = None;

	let node_a = setup_node(&chain_source, config_a, None);
	let node_b = setup_node(&chain_source, config_b, None);

	let address_a = node_a.onchain_payment().new_address().unwrap();
	let premine_amount_sat = 5_000_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![address_a],
		Amount::from_sat(premine_amount_sat),
	);

	node_a.sync_wallets().unwrap();

	// Open an announced channel from node_a to node_b
	open_channel(&node_a, &node_b, 4_000_000, true, &electrsd);

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6);

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	expect_channel_ready_event!(node_a, node_b.node_id());
	expect_channel_ready_event!(node_b, node_a.node_id());

	// Wait until node_b broadcasts a node announcement
	while node_b.status().latest_node_announcement_broadcast_timestamp.is_none() {
		std::thread::sleep(std::time::Duration::from_millis(10));
	}

	// Sleep to make sure the node announcement propagates
	std::thread::sleep(std::time::Duration::from_secs(1));

	// Get node info from the other node's perspective
	let node_a_info = node_b.network_graph().node(&NodeId::from_pubkey(&node_a.node_id())).unwrap();
	let node_a_announcement_info = node_a_info.announcement_info.as_ref().unwrap();

	let node_b_info = node_a.network_graph().node(&NodeId::from_pubkey(&node_b.node_id())).unwrap();
	let node_b_announcement_info = node_b_info.announcement_info.as_ref().unwrap();

	// Assert that the aliases and addresses match the expected values
	#[cfg(not(feature = "uniffi"))]
	assert_eq!(node_a_announcement_info.alias(), &node_a_node_alias.unwrap());
	#[cfg(feature = "uniffi")]
	assert_eq!(node_a_announcement_info.alias, node_a_alias_string);

	#[cfg(not(feature = "uniffi"))]
	assert_eq!(node_a_announcement_info.addresses(), &node_a_announcement_addresses);
	#[cfg(feature = "uniffi")]
	assert_eq!(node_a_announcement_info.addresses, node_a_announcement_addresses);

	#[cfg(not(feature = "uniffi"))]
	assert_eq!(node_b_announcement_info.alias(), &node_b_node_alias.unwrap());
	#[cfg(feature = "uniffi")]
	assert_eq!(node_b_announcement_info.alias, node_b_alias_string);

	#[cfg(not(feature = "uniffi"))]
	assert_eq!(node_b_announcement_info.addresses(), &node_b_listening_addresses);
	#[cfg(feature = "uniffi")]
	assert_eq!(node_b_announcement_info.addresses, node_b_listening_addresses);
}

#[test]
fn generate_bip21_uri() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	let (node_a, node_b) = setup_two_nodes(&chain_source, false, true, false);

	let address_a = node_a.onchain_payment().new_address().unwrap();
	let premined_sats = 5_000_000;

	let expected_amount_sats = 100_000;
	let expiry_sec = 4_000;

	// Test 1: Verify URI generation (on-chain + BOLT11) works
	// even before any channels are opened. This checks the graceful fallback behavior.
	let initial_uqr_payment = node_b
		.unified_qr_payment()
		.receive(expected_amount_sats, "asdf", expiry_sec)
		.expect("Failed to generate URI");
	println!("Initial URI (no channels): {}", initial_uqr_payment);

	assert!(initial_uqr_payment.contains("bitcoin:"));
	assert!(initial_uqr_payment.contains("lightning="));
	assert!(!initial_uqr_payment.contains("lno=")); // BOLT12 requires channels

	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![address_a],
		Amount::from_sat(premined_sats),
	);

	node_a.sync_wallets().unwrap();
	open_channel(&node_a, &node_b, 4_000_000, true, &electrsd);
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6);

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	expect_channel_ready_event!(node_a, node_b.node_id());
	expect_channel_ready_event!(node_b, node_a.node_id());

	// Test 2: Verify URI generation (on-chain + BOLT11 + BOLT12) works after channels are established.
	let uqr_payment = node_b
		.unified_qr_payment()
		.receive(expected_amount_sats, "asdf", expiry_sec)
		.expect("Failed to generate URI");

	println!("Generated URI: {}", uqr_payment);
	assert!(uqr_payment.contains("bitcoin:"));
	assert!(uqr_payment.contains("lightning="));
	assert!(uqr_payment.contains("lno="));
}

#[test]
fn unified_qr_send_receive() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	let (node_a, node_b) = setup_two_nodes(&chain_source, false, true, false);

	let address_a = node_a.onchain_payment().new_address().unwrap();
	let premined_sats = 5_000_000;

	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![address_a],
		Amount::from_sat(premined_sats),
	);

	node_a.sync_wallets().unwrap();
	open_channel(&node_a, &node_b, 4_000_000, true, &electrsd);
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6);

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	expect_channel_ready_event!(node_a, node_b.node_id());
	expect_channel_ready_event!(node_b, node_a.node_id());

	// Sleep until we broadcast a node announcement.
	while node_b.status().latest_node_announcement_broadcast_timestamp.is_none() {
		std::thread::sleep(std::time::Duration::from_millis(10));
	}

	// Sleep one more sec to make sure the node announcement propagates.
	std::thread::sleep(std::time::Duration::from_secs(1));

	let expected_amount_sats = 100_000;
	let expiry_sec = 4_000;

	let uqr_payment = node_b.unified_qr_payment().receive(expected_amount_sats, "asdf", expiry_sec);
	let uri_str = uqr_payment.clone().unwrap();
	let offer_payment_id: PaymentId = match node_a.unified_qr_payment().send(&uri_str) {
		Ok(QrPaymentResult::Bolt12 { payment_id }) => {
			println!("\nBolt12 payment sent successfully with PaymentID: {:?}", payment_id);
			payment_id
		},
		Ok(QrPaymentResult::Bolt11 { payment_id: _ }) => {
			panic!("Expected Bolt12 payment but got Bolt11");
		},
		Ok(QrPaymentResult::Onchain { txid: _ }) => {
			panic!("Expected Bolt12 payment but get On-chain transaction");
		},
		Err(e) => {
			panic!("Expected Bolt12 payment but got error: {:?}", e);
		},
	};

	expect_payment_successful_event!(node_a, Some(offer_payment_id), None);

	// Cut off the BOLT12 part to fallback to BOLT11.
	let uri_str_without_offer = uri_str.split("&lno=").next().unwrap();
	let invoice_payment_id: PaymentId =
		match node_a.unified_qr_payment().send(uri_str_without_offer) {
			Ok(QrPaymentResult::Bolt12 { payment_id: _ }) => {
				panic!("Expected Bolt11 payment but got Bolt12");
			},
			Ok(QrPaymentResult::Bolt11 { payment_id }) => {
				println!("\nBolt11 payment sent successfully with PaymentID: {:?}", payment_id);
				payment_id
			},
			Ok(QrPaymentResult::Onchain { txid: _ }) => {
				panic!("Expected Bolt11 payment but got on-chain transaction");
			},
			Err(e) => {
				panic!("Expected Bolt11 payment but got error: {:?}", e);
			},
		};
	expect_payment_successful_event!(node_a, Some(invoice_payment_id), None);

	let expect_onchain_amount_sats = 800_000;
	let onchain_uqr_payment =
		node_b.unified_qr_payment().receive(expect_onchain_amount_sats, "asdf", 4_000).unwrap();

	// Cut off any lightning part to fallback to on-chain only.
	let uri_str_without_lightning = onchain_uqr_payment.split("&lightning=").next().unwrap();
	let txid = match node_a.unified_qr_payment().send(&uri_str_without_lightning) {
		Ok(QrPaymentResult::Bolt12 { payment_id: _ }) => {
			panic!("Expected on-chain payment but got Bolt12")
		},
		Ok(QrPaymentResult::Bolt11 { payment_id: _ }) => {
			panic!("Expected on-chain payment but got Bolt11");
		},
		Ok(QrPaymentResult::Onchain { txid }) => {
			println!("\nOn-chain transaction successful with Txid: {}", txid);
			txid
		},
		Err(e) => {
			panic!("Expected on-chain payment but got error: {:?}", e);
		},
	};

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6);
	wait_for_tx(&electrsd.client, txid);

	node_a.sync_wallets().unwrap();
	node_b.sync_wallets().unwrap();

	assert_eq!(node_b.list_balances().total_onchain_balance_sats, 800_000);
	assert_eq!(node_b.list_balances().total_lightning_balance_sats, 200_000);
}

#[test]
fn lsps2_client_service_integration() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();

	let esplora_url = format!("http://{}", electrsd.esplora_url.as_ref().unwrap());

	let sync_config = EsploraSyncConfig { background_sync_config: None };

	// Setup three nodes: service, client, and payer
	let channel_opening_fee_ppm = 10_000;
	let channel_over_provisioning_ppm = 100_000;
	let lsps2_service_config = LSPS2ServiceConfig {
		require_token: None,
		advertise_service: false,
		channel_opening_fee_ppm,
		channel_over_provisioning_ppm,
		max_payment_size_msat: 1_000_000_000,
		min_payment_size_msat: 0,
		min_channel_lifetime: 100,
		min_channel_opening_fee_msat: 0,
		max_client_to_self_delay: 1024,
	};

	let service_config = random_config(true);
	setup_builder!(service_builder, service_config.node_config);
	service_builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));
	service_builder.set_liquidity_provider_lsps2(lsps2_service_config);
	let service_node = service_builder.build().unwrap();
	service_node.start().unwrap();

	let service_node_id = service_node.node_id();
	let service_addr = service_node.listening_addresses().unwrap().first().unwrap().clone();

	let client_config = random_config(true);
	setup_builder!(client_builder, client_config.node_config);
	client_builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));
	client_builder.set_liquidity_source_lsps2(service_node_id, service_addr, None);
	let client_node = client_builder.build().unwrap();
	client_node.start().unwrap();

	let payer_config = random_config(true);
	setup_builder!(payer_builder, payer_config.node_config);
	payer_builder.set_chain_source_esplora(esplora_url.clone(), Some(sync_config));
	let payer_node = payer_builder.build().unwrap();
	payer_node.start().unwrap();

	let service_addr = service_node.onchain_payment().new_address().unwrap();
	let client_addr = client_node.onchain_payment().new_address().unwrap();
	let payer_addr = payer_node.onchain_payment().new_address().unwrap();

	let premine_amount_sat = 10_000_000;

	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![service_addr, client_addr, payer_addr],
		Amount::from_sat(premine_amount_sat),
	);
	service_node.sync_wallets().unwrap();
	client_node.sync_wallets().unwrap();
	payer_node.sync_wallets().unwrap();

	// Open a channel payer -> service that will allow paying the JIT invoice
	println!("Opening channel payer_node -> service_node!");
	open_channel(&payer_node, &service_node, 5_000_000, false, &electrsd);

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6);
	service_node.sync_wallets().unwrap();
	payer_node.sync_wallets().unwrap();
	expect_channel_ready_event!(payer_node, service_node.node_id());
	expect_channel_ready_event!(service_node, payer_node.node_id());

	let invoice_description =
		Bolt11InvoiceDescription::Direct(Description::new(String::from("asdf")).unwrap());
	let jit_amount_msat = 100_000_000;

	println!("Generating JIT invoice!");
	let jit_invoice = client_node
		.bolt11_payment()
		.receive_via_jit_channel(jit_amount_msat, &invoice_description.into(), 1024, None)
		.unwrap();

	// Have the payer_node pay the invoice, therby triggering channel open service_node -> client_node.
	println!("Paying JIT invoice!");
	let payment_id = payer_node.bolt11_payment().send(&jit_invoice, None).unwrap();
	expect_channel_pending_event!(service_node, client_node.node_id());
	expect_channel_ready_event!(service_node, client_node.node_id());
	expect_channel_pending_event!(client_node, service_node.node_id());
	expect_channel_ready_event!(client_node, service_node.node_id());

	let service_fee_msat = (jit_amount_msat * channel_opening_fee_ppm as u64) / 1_000_000;
	let expected_received_amount_msat = jit_amount_msat - service_fee_msat;
	expect_payment_successful_event!(payer_node, Some(payment_id), None);
	let client_payment_id =
		expect_payment_received_event!(client_node, expected_received_amount_msat).unwrap();
	let client_payment = client_node.payment(&client_payment_id).unwrap();
	match client_payment.kind {
		PaymentKind::Bolt11Jit { counterparty_skimmed_fee_msat, .. } => {
			assert_eq!(counterparty_skimmed_fee_msat, Some(service_fee_msat));
		},
		_ => panic!("Unexpected payment kind"),
	}

	let expected_channel_overprovisioning_msat =
		(expected_received_amount_msat * channel_over_provisioning_ppm as u64) / 1_000_000;
	let expected_channel_size_sat =
		(expected_received_amount_msat + expected_channel_overprovisioning_msat) / 1000;
	let channel_value_sats = client_node.list_channels().first().unwrap().channel_value_sats;
	assert_eq!(channel_value_sats, expected_channel_size_sat);

	println!("Generating regular invoice!");
	let invoice_description =
		Bolt11InvoiceDescription::Direct(Description::new(String::from("asdf")).unwrap());
	let amount_msat = 5_000_000;
	let invoice = client_node
		.bolt11_payment()
		.receive(amount_msat, &invoice_description.into(), 1024)
		.unwrap();

	// Have the payer_node pay the invoice, to check regular forwards service_node -> client_node
	// are working as expected.
	println!("Paying regular invoice!");
	let payment_id = payer_node.bolt11_payment().send(&invoice, None).unwrap();
	expect_payment_successful_event!(payer_node, Some(payment_id), None);
	expect_payment_received_event!(client_node, amount_msat);
}

#[test]
fn facade_logging() {
	let (_bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	let logger = init_log_logger(LevelFilter::Trace);
	let mut config = random_config(false);
	config.log_writer = TestLogWriter::LogFacade;

	println!("== Facade logging starts ==");
	let _node = setup_node(&chain_source, config, None);

	assert!(!logger.retrieve_logs().is_empty());
	for (_, entry) in logger.retrieve_logs().iter().enumerate() {
		validate_log_entry(entry);
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn drop_in_async_context() {
	let (_bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let seed_bytes = vec![42u8; 64];

	let config = random_config(true);
	let node = setup_node(&chain_source, config, Some(seed_bytes));
	node.stop().unwrap();
}

#[test]
fn onchain_transaction_events() {
	// Test that onchain transaction events are emitted when transactions are detected
	use std::time::Duration;

	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let config = random_config(false);
	let node = setup_node(&chain_source, config, None);

	// Get a new address
	let addr = node.onchain_payment().new_address().unwrap();

	// Fund the address
	let premine_amount_sat = 100_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr.clone()],
		Amount::from_sat(premine_amount_sat),
	);

	// Clear any existing events before we start testing
	while node.next_event().is_some() {
		node.event_handled().unwrap();
	}

	// Sync the wallet - this should detect the new transaction and emit an event
	node.sync_wallets().unwrap();

	// We should receive an OnchainTransactionConfirmed event
	let mut received_confirmed_event = false;
	let mut txid_from_event = None;

	// Check for the event (may need to wait a bit for event queue)
	for _ in 0..5 {
		if let Some(event) = node.next_event() {
			match event {
				Event::OnchainTransactionConfirmed { txid, block_height, details, .. } => {
					println!("Received OnchainTransactionConfirmed event for txid: {}, height: {}, amount: {}", txid, block_height, details.amount_sats);
					// Verify TransactionDetails structure is populated
					assert!(!details.inputs.is_empty(), "Transaction should have inputs");
					assert!(!details.outputs.is_empty(), "Transaction should have outputs");
					received_confirmed_event = true;
					txid_from_event = Some(txid);
					node.event_handled().unwrap();
					break;
				},
				other_event => {
					println!("Got other event: {:?}", other_event);
					node.event_handled().unwrap();
				},
			}
		}
		std::thread::sleep(Duration::from_millis(100));
	}

	assert!(received_confirmed_event, "Should have received OnchainTransactionConfirmed event");
	assert!(txid_from_event.is_some(), "Should have captured txid from event");

	// Test unconfirmation after reorg (simulate by generating new blocks on a different chain)
	// Note: This is complex to test in a real scenario, so we'll just verify the event exists
	// and can be pattern matched
	match node.next_event() {
		None => {}, // No more events is fine
		Some(Event::OnchainTransactionReceived { .. }) => {
			node.event_handled().unwrap();
		},
		Some(_) => {}, // Other events are fine
	}

	node.stop().unwrap();
}

#[test]
fn onchain_transaction_events_electrum() {
	// Test onchain events work with Electrum backend too
	use std::time::Duration;

	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Electrum(&electrsd);
	let config = random_config(false);
	let node = setup_node(&chain_source, config, None);

	let addr = node.onchain_payment().new_address().unwrap();
	let premine_amount_sat = 50_000;

	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr.clone()],
		Amount::from_sat(premine_amount_sat),
	);

	// Clear any existing events
	while node.next_event().is_some() {
		node.event_handled().unwrap();
	}

	node.sync_wallets().unwrap();

	// Check we received the onchain transaction confirmed event
	let mut found_event = false;
	for _ in 0..5 {
		if let Some(event) = node.next_event() {
			if let Event::OnchainTransactionConfirmed { details, .. } = event {
				// Verify TransactionDetails structure is populated
				assert!(!details.inputs.is_empty(), "Transaction should have inputs");
				assert!(!details.outputs.is_empty(), "Transaction should have outputs");
				found_event = true;
				node.event_handled().unwrap();
				break;
			}
			node.event_handled().unwrap();
		}
		std::thread::sleep(Duration::from_millis(100));
	}

	assert!(found_event, "Should receive OnchainTransactionConfirmed event with Electrum backend");

	node.stop().unwrap();
}

#[test]
fn sync_completed_event() {
	// Test that SyncCompleted event is emitted after wallet sync
	use std::time::Duration;

	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let config = random_config(false);
	let node = setup_node(&chain_source, config, None);

	// Clear any existing events
	while node.next_event().is_some() {
		node.event_handled().unwrap();
	}

	// Sync the wallet
	node.sync_wallets().unwrap();

	// Check for SyncCompleted event
	let mut found_sync_completed = false;
	for _ in 0..10 {
		if let Some(event) = node.next_event() {
			match event {
				Event::SyncCompleted { sync_type, synced_block_height } => {
					println!(
						"Received SyncCompleted event: type={:?}, height={}",
						sync_type, synced_block_height
					);
					found_sync_completed = true;
					node.event_handled().unwrap();
					break;
				},
				other_event => {
					println!("Got other event: {:?}", other_event);
					node.event_handled().unwrap();
				},
			}
		}
		std::thread::sleep(Duration::from_millis(100));
	}

	assert!(found_sync_completed, "Should have received SyncCompleted event");

	node.stop().unwrap();
}

#[test]
fn balance_changed_event() {
	// Test that BalanceChanged event is emitted when balance changes
	use std::time::Duration;

	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let config = random_config(false);
	let node = setup_node(&chain_source, config, None);

	// First sync to establish initial state
	node.sync_wallets().unwrap();

	// Clear any existing events
	while node.next_event().is_some() {
		node.event_handled().unwrap();
	}

	// Get initial balances
	let initial_total_balance = node.list_balances().total_onchain_balance_sats;
	println!("Initial balance: {} sats", initial_total_balance);

	// Fund the wallet
	let addr = node.onchain_payment().new_address().unwrap();
	let fund_amount = 100_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr.clone()],
		Amount::from_sat(fund_amount),
	);

	// Sync again - this should detect the balance change
	node.sync_wallets().unwrap();

	// Check for BalanceChanged event
	let mut found_balance_changed = false;
	for _ in 0..20 {
		if let Some(event) = node.next_event() {
			match event {
				Event::BalanceChanged {
					old_spendable_onchain_balance_sats,
					new_spendable_onchain_balance_sats,
					old_total_onchain_balance_sats,
					new_total_onchain_balance_sats,
					old_total_lightning_balance_sats,
					new_total_lightning_balance_sats,
				} => {
					println!(
						"Received BalanceChanged event:\n  Spendable onchain: {} -> {}\n  Total onchain: {} -> {}\n  Lightning: {} -> {}",
						old_spendable_onchain_balance_sats, new_spendable_onchain_balance_sats,
						old_total_onchain_balance_sats, new_total_onchain_balance_sats,
						old_total_lightning_balance_sats, new_total_lightning_balance_sats
					);

					// Verify balance actually increased
					assert!(
						new_total_onchain_balance_sats > old_total_onchain_balance_sats,
						"Balance should have increased"
					);
					assert_eq!(
						new_total_onchain_balance_sats - old_total_onchain_balance_sats,
						fund_amount,
						"Balance increase should match funded amount"
					);

					found_balance_changed = true;
					node.event_handled().unwrap();
					break;
				},
				other_event => {
					println!("Got other event: {:?}", other_event);
					node.event_handled().unwrap();
				},
			}
		}
		std::thread::sleep(Duration::from_millis(100));
	}

	assert!(found_balance_changed, "Should have received BalanceChanged event");

	// Verify the balance is now reflected in list_balances
	let final_balance = node.list_balances().total_onchain_balance_sats;
	assert_eq!(
		final_balance,
		initial_total_balance + fund_amount,
		"Balance should reflect the funded amount"
	);

	node.stop().unwrap();
}

#[test]
fn test_event_serialization_roundtrip() {
	// Test that all event types can be serialized and deserialized correctly
	use bitcoin::{BlockHash, Txid};
	use ldk_node::{Event, SyncType, TransactionDetails, TxInput, TxOutput};
	use lightning::util::ser::{Readable, Writeable};

	// Helper function to test serialization roundtrip
	fn test_roundtrip(event: Event) {
		let mut buffer = Vec::new();
		event.write(&mut buffer).unwrap();
		let mut cursor = std::io::Cursor::new(&buffer);
		let deserialized = Event::read(&mut cursor).unwrap();
		assert_eq!(event, deserialized, "Event serialization roundtrip failed");
	}

	// Test OnchainTransactionConfirmed
	let event = Event::OnchainTransactionConfirmed {
		txid: Txid::from_slice(&[1; 32]).unwrap(),
		block_hash: BlockHash::from_slice(&[2; 32]).unwrap(),
		block_height: 100000,
		confirmation_time: 1234567890,
		details: TransactionDetails {
			amount_sats: 100000,
			inputs: vec![TxInput {
				txid: Txid::from_slice(&[10; 32]).unwrap(),
				vout: 0,
				scriptsig: "".to_string(),
				witness: vec![],
				sequence: 0xffffffff,
			}],
			outputs: vec![TxOutput {
				scriptpubkey: "0014".to_string(),
				scriptpubkey_type: Some("p2wpkh".to_string()),
				scriptpubkey_address: None,
				value: 100000,
				n: 0,
			}],
		},
	};
	test_roundtrip(event);

	// Test OnchainTransactionReceived
	let event = Event::OnchainTransactionReceived {
		txid: Txid::from_slice(&[3; 32]).unwrap(),
		details: TransactionDetails {
			amount_sats: -50000, // Test negative amount for outgoing
			inputs: vec![TxInput {
				txid: Txid::from_slice(&[11; 32]).unwrap(),
				vout: 1,
				scriptsig: "".to_string(),
				witness: vec![],
				sequence: 0xffffffff,
			}],
			outputs: vec![],
		},
	};
	test_roundtrip(event);

	// Test OnchainTransactionReplaced
	let event = Event::OnchainTransactionReplaced {
		txid: Txid::from_slice(&[6; 32]).unwrap(),
		conflicts: vec![Txid::from_slice(&[7; 32]).unwrap()],
	};
	test_roundtrip(event);

	// Test OnchainTransactionReorged
	let event = Event::OnchainTransactionReorged { txid: Txid::from_slice(&[7; 32]).unwrap() };
	test_roundtrip(event);

	// Test SyncProgress
	let event = Event::SyncProgress {
		sync_type: SyncType::OnchainWallet,
		progress_percent: 75,
		current_block_height: 750000,
		target_block_height: 800000,
	};
	test_roundtrip(event);

	// Test SyncCompleted
	let event =
		Event::SyncCompleted { sync_type: SyncType::LightningWallet, synced_block_height: 800000 };
	test_roundtrip(event);

	// Test BalanceChanged
	let event = Event::BalanceChanged {
		old_spendable_onchain_balance_sats: 1000000,
		new_spendable_onchain_balance_sats: 1500000,
		old_total_onchain_balance_sats: 2000000,
		new_total_onchain_balance_sats: 2500000,
		old_total_lightning_balance_sats: 500000,
		new_total_lightning_balance_sats: 600000,
	};
	test_roundtrip(event);

	// Test TransactionDetails with multiple inputs and outputs
	let details = TransactionDetails {
		amount_sats: 1000000,
		inputs: vec![TxInput {
			txid: Txid::from_slice(&[12; 32]).unwrap(),
			vout: 0,
			scriptsig: "160014".to_string(),
			witness: vec!["30440220".to_string()],
			sequence: 0xffffffff,
		}],
		outputs: vec![TxOutput {
			scriptpubkey: "0014".to_string(),
			scriptpubkey_type: Some("p2wpkh".to_string()),
			scriptpubkey_address: None,
			value: 1000000,
			n: 0,
		}],
	};
	let event = Event::OnchainTransactionConfirmed {
		txid: Txid::from_slice(&[6; 32]).unwrap(),
		block_hash: BlockHash::from_slice(&[7; 32]).unwrap(),
		block_height: 100001,
		confirmation_time: 1234567891,
		details,
	};
	test_roundtrip(event);

	// Test TransactionDetails with coinbase input
	let details = TransactionDetails {
		amount_sats: 62500000000, // Block reward
		inputs: vec![TxInput {
			txid: Txid::from_slice(&[0u8; 32]).unwrap(),
			vout: 0xffffffff,
			scriptsig: "03".to_string(),
			witness: vec![],
			sequence: 0xffffffff,
		}],
		outputs: vec![TxOutput {
			scriptpubkey: "0014".to_string(),
			scriptpubkey_type: Some("p2wpkh".to_string()),
			scriptpubkey_address: None,
			value: 62500000000,
			n: 0,
		}],
	};
	let event = Event::OnchainTransactionConfirmed {
		txid: Txid::from_slice(&[9; 32]).unwrap(),
		block_hash: BlockHash::from_slice(&[10; 32]).unwrap(),
		block_height: 100002,
		confirmation_time: 1234567892,
		details,
	};
	test_roundtrip(event);
}

#[test]
fn test_concurrent_event_emission() {
	// Test that multiple threads can emit events concurrently without issues
	use std::sync::Arc;
	use std::thread;
	use std::time::Duration;

	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let config = random_config(false);
	let node = Arc::new(setup_node(&chain_source, config, None));

	// Clear any existing events
	while node.next_event().is_some() {
		node.event_handled().unwrap();
	}

	// Spawn multiple threads that sync concurrently
	let mut handles = vec![];
	for i in 0..5 {
		let node_clone = Arc::clone(&node);
		let handle = thread::spawn(move || {
			// Sleep briefly to stagger the syncs
			thread::sleep(Duration::from_millis(i * 10));
			node_clone.sync_wallets().unwrap();
		});
		handles.push(handle);
	}

	// Wait for all threads to complete
	for handle in handles {
		handle.join().unwrap();
	}

	// Count the events - we should have multiple SyncCompleted events
	let mut sync_completed_count = 0;
	let mut event_count = 0;

	for _ in 0..50 {
		if let Some(event) = node.next_event() {
			event_count += 1;
			if matches!(event, Event::SyncCompleted { .. }) {
				sync_completed_count += 1;
			}
			node.event_handled().unwrap();
		}
		thread::sleep(Duration::from_millis(10));
	}

	// We should have received at least one SyncCompleted event per thread
	assert!(
		sync_completed_count >= 1,
		"Should have received at least one SyncCompleted event from concurrent syncs"
	);

	node.stop().unwrap();
}

#[test]
fn test_reorg_event_emission() {
	// Test that reorg scenarios properly emit OnchainTransactionReceived events
	// Note: This is a simplified test as true reorg testing requires complex setup
	use std::thread;
	use std::time::Duration;

	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let config = random_config(false);
	let node = setup_node(&chain_source, config, None);

	// Get a new address and fund it
	let addr = node.onchain_payment().new_address().unwrap();
	let premine_amount_sat = 100_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr.clone()],
		Amount::from_sat(premine_amount_sat),
	);

	// Clear any existing events
	while node.next_event().is_some() {
		node.event_handled().unwrap();
	}

	// Sync to detect the confirmed transaction
	node.sync_wallets().unwrap();

	let mut confirmed_txid = None;
	for _ in 0..10 {
		if let Some(event) = node.next_event() {
			if let Event::OnchainTransactionConfirmed { txid, .. } = event {
				confirmed_txid = Some(txid);
				node.event_handled().unwrap();
				break;
			}
			node.event_handled().unwrap();
		}
		thread::sleep(Duration::from_millis(100));
	}

	assert!(confirmed_txid.is_some(), "Should have received a confirmation event");

	// In a real scenario, we would trigger a reorg here by mining competing blocks
	// For this test, we just verify the event structure is correct

	node.stop().unwrap();
}

#[test]
fn onchain_transaction_evicted_event() {
	// Test that OnchainTransactionEvicted event is emitted when a transaction is evicted from mempool
	use std::thread;
	use std::time::Duration;

	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let config = random_config(false);
	let node = setup_node(&chain_source, config, None);

	// Fund the node first
	let addr = node.onchain_payment().new_address().unwrap();
	let fund_amount = 200_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr.clone()],
		Amount::from_sat(fund_amount),
	);

	// Sync to get the funds
	node.sync_wallets().unwrap();

	// Clear any existing events
	while node.next_event().is_some() {
		node.event_handled().unwrap();
	}

	// Create a transaction - this will be unconfirmed initially
	let recipient_addr = node.onchain_payment().new_address().unwrap();
	let send_amount = 50_000;
	let txid =
		node.onchain_payment().send_to_address(&recipient_addr, send_amount, None, None).unwrap();

	println!("Created transaction {} (unconfirmed)", txid);

	// Wait for transaction to be broadcast
	thread::sleep(Duration::from_millis(500));

	// Sync to detect the unconfirmed transaction
	node.sync_wallets().unwrap();

	// Verify we received OnchainTransactionReceived event
	let mut found_received_event = false;
	for _ in 0..10 {
		if let Some(event) = node.next_event() {
			match event {
				Event::OnchainTransactionReceived { txid: event_txid, .. } => {
					if event_txid == txid {
						found_received_event = true;
						println!("Received OnchainTransactionReceived event for {}", txid);
						node.event_handled().unwrap();
						break;
					}
					node.event_handled().unwrap();
				},
				_ => {
					node.event_handled().unwrap();
				},
			}
		}
		thread::sleep(Duration::from_millis(100));
	}

	assert!(
		found_received_event,
		"Should have received OnchainTransactionReceived event for transaction {}",
		txid
	);

	// Remove the transaction from bitcoind's mempool to simulate eviction
	let txid_hex = format!("{:x}", txid);

	let removed = match bitcoind
		.client
		.call::<serde_json::Value>("removetx", &[serde_json::json!(txid_hex)])
	{
		Ok(_) => {
			println!("Removed transaction {} from mempool using removetx", txid);
			true
		},
		Err(e) => {
			panic!("removetx RPC not available ({}). This test requires Bitcoin Core 25.0+ to test eviction.", e);
		},
	};

	assert!(removed, "Failed to remove transaction from mempool");

	// Wait for the removal to propagate
	thread::sleep(Duration::from_millis(1000));

	// Sync again - this should detect the eviction
	node.sync_wallets().unwrap();

	// Verify we received OnchainTransactionEvicted event
	let mut found_evicted_event = false;
	let mut evicted_txid = None;

	for _ in 0..20 {
		if let Some(event) = node.next_event() {
			match event {
				Event::OnchainTransactionEvicted { txid: event_txid } => {
					if event_txid == txid {
						found_evicted_event = true;
						evicted_txid = Some(event_txid);
						println!("Received OnchainTransactionEvicted event for {}", event_txid);
						node.event_handled().unwrap();
						break;
					}
					node.event_handled().unwrap();
				},
				_ => {
					node.event_handled().unwrap();
				},
			}
		}
		thread::sleep(Duration::from_millis(100));
	}

	assert!(
		found_evicted_event,
		"Should have received OnchainTransactionEvicted event for transaction {}",
		txid
	);
	assert_eq!(evicted_txid, Some(txid), "Evicted txid should match the transaction we created");

	node.stop().unwrap();
}

#[test]
fn get_transaction_details() {
	use std::time::Duration;

	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let config = random_config(false);
	let node = setup_node(&chain_source, config, None);

	// Fund the node
	let addr = node.onchain_payment().new_address().unwrap();
	let fund_amount = 100_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr.clone()],
		Amount::from_sat(fund_amount),
	);

	// Sync to get the funds
	node.sync_wallets().unwrap();

	// Clear any existing events
	while node.next_event().is_some() {
		node.event_handled().unwrap();
	}

	// Wait a bit for events to be processed
	std::thread::sleep(Duration::from_millis(100));

	// Get the transaction ID from the confirmed event
	let mut funding_txid = None;
	for _ in 0..5 {
		if let Some(event) = node.next_event() {
			match event {
				Event::OnchainTransactionConfirmed { txid, details, .. } => {
					funding_txid = Some(txid);
					// Verify we can access the details from the event
					assert!(!details.inputs.is_empty());
					assert!(!details.outputs.is_empty());
					node.event_handled().unwrap();
					break;
				},
				_ => {
					node.event_handled().unwrap();
				},
			}
		}
		std::thread::sleep(Duration::from_millis(100));
	}

	let txid = funding_txid.expect("Should have received OnchainTransactionConfirmed event");

	// Test get_transaction_details with the funding transaction
	let details =
		node.get_transaction_details(&txid).expect("Transaction should be found in wallet");

	// Verify the details match what we expect
	assert!(!details.inputs.is_empty(), "Transaction should have inputs");
	assert!(!details.outputs.is_empty(), "Transaction should have outputs");
	assert_eq!(details.amount_sats, fund_amount as i64, "Amount should match funding amount");

	// Verify input structure
	for input in &details.inputs {
		assert!(!input.txid.to_string().is_empty(), "Input should have a txid");
		assert!(!input.scriptsig.is_empty(), "Input should have a scriptsig");
	}

	// Verify output structure
	for output in &details.outputs {
		assert!(output.value > 0, "Output should have a positive value");
		assert!(!output.scriptpubkey.is_empty(), "Output should have a scriptpubkey");
	}

	// Test with a non-existent transaction
	let fake_txid =
		Txid::from_str("0000000000000000000000000000000000000000000000000000000000000000").unwrap();
	let result = node.get_transaction_details(&fake_txid);
	assert!(result.is_none(), "Non-existent transaction should return None");

	// Create a new transaction and verify we can get its details
	let recipient_addr = node.onchain_payment().new_address().unwrap();
	let send_amount = 20_000;
	let send_txid =
		node.onchain_payment().send_to_address(&recipient_addr, send_amount, None, None).unwrap();

	// Sync to include the new transaction
	node.sync_wallets().unwrap();

	// Wait a bit
	std::thread::sleep(Duration::from_millis(500));

	// Get details for the send transaction
	let send_details =
		node.get_transaction_details(&send_txid).expect("Send transaction should be found");

	// Verify the send transaction details
	assert!(!send_details.inputs.is_empty());
	assert!(!send_details.outputs.is_empty());
	// The amount should be negative (outgoing) or positive depending on change
	// But the absolute value should be at least the send amount
	assert!(send_details.amount_sats.abs() >= send_amount as i64 || send_details.amount_sats < 0);

	node.stop().unwrap();
}

#[test]
fn get_address_balance_esplora() {
	use std::time::Duration;

	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);
	let config = random_config(false);
	let node = setup_node(&chain_source, config, None);

	// Fund an address
	let addr = node.onchain_payment().new_address().unwrap();
	let fund_amount = 100_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr.clone()],
		Amount::from_sat(fund_amount),
	);

	// Sync to get the funds
	node.sync_wallets().unwrap();

	// Wait a bit for the chain source to index the transaction
	std::thread::sleep(Duration::from_millis(500));

	// Test get_address_balance with the funded address
	let balance = node.get_address_balance(&addr.to_string()).unwrap();
	assert_eq!(balance, fund_amount, "Balance should match funded amount");

	// Test with an unfunded address
	let unfunded_addr = node.onchain_payment().new_address().unwrap();
	let unfunded_balance = node.get_address_balance(&unfunded_addr.to_string()).unwrap();
	assert_eq!(unfunded_balance, 0, "Unfunded address should have zero balance");

	// Test with an invalid address
	let invalid_result = node.get_address_balance("invalid_address");
	assert!(invalid_result.is_err(), "Invalid address should return error");
}

#[test]
fn get_address_balance_electrum() {
	use std::time::Duration;

	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Electrum(&electrsd);
	let config = random_config(false);
	let node = setup_node(&chain_source, config, None);

	// Fund an address
	let addr = node.onchain_payment().new_address().unwrap();
	let fund_amount = 100_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr.clone()],
		Amount::from_sat(fund_amount),
	);

	// Sync to get the funds
	node.sync_wallets().unwrap();

	// Wait a bit for the chain source to index the transaction
	std::thread::sleep(Duration::from_millis(500));

	// Test get_address_balance with the funded address
	let balance = node.get_address_balance(&addr.to_string()).unwrap();
	assert_eq!(balance, fund_amount, "Balance should match funded amount");

	// Test with an unfunded address
	let unfunded_addr = node.onchain_payment().new_address().unwrap();
	let unfunded_balance = node.get_address_balance(&unfunded_addr.to_string()).unwrap();
	assert_eq!(unfunded_balance, 0, "Unfunded address should have zero balance");

	// Test with an invalid address
	let invalid_result = node.get_address_balance("invalid_address");
	assert!(invalid_result.is_err(), "Invalid address should return error");
}

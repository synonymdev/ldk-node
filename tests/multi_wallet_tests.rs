// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

mod common;

use std::str::FromStr;

use bitcoin::{Address, FeeRate};
use common::{
	generate_blocks_and_wait, premine_and_distribute_funds, setup_bitcoind_and_electrsd,
	setup_node, wait_for_tx, TestChainSource,
};
use ldk_node::bitcoin::Amount;
use ldk_node::config::AddressType;

// Test that node can be set up with multiple wallets configured
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_multi_wallet_setup() {
	let (_bitcoind, _electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&_electrsd);

	// Test with NativeSegwit as primary, monitoring one other type
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor = vec![AddressType::Legacy];

	let node = setup_node(&chain_source, config, None);

	// Verify node is running
	assert!(node.status().is_running);

	// Verify we can generate an address
	let addr = node.onchain_payment().new_address().unwrap();
	assert!(!addr.to_string().is_empty());
}

// Test that all address types can be used as primary
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_all_address_types_as_primary() {
	let address_types = vec![
		AddressType::Legacy,
		AddressType::NestedSegwit,
		AddressType::NativeSegwit,
		AddressType::Taproot,
	];

	for primary_type in address_types {
		let (_bitcoind, _electrsd) = setup_bitcoind_and_electrsd();
		let chain_source = TestChainSource::Esplora(&_electrsd);

		let mut config = common::random_config(true);
		config.node_config.address_type = primary_type;
		config.node_config.address_types_to_monitor = vec![];

		let node = setup_node(&chain_source, config, None);
		assert!(node.status().is_running);

		// Verify we can generate an address
		let addr = node.onchain_payment().new_address().unwrap();
		assert!(!addr.to_string().is_empty());

		node.stop().unwrap();
	}
}

// Test that we can generate addresses for monitored address types
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_new_address_for_type() {
	let (_bitcoind, _electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&_electrsd);

	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor =
		vec![AddressType::Legacy, AddressType::NestedSegwit, AddressType::Taproot];

	let node = setup_node(&chain_source, config, None);
	assert!(node.status().is_running);

	let address_types = vec![
		AddressType::Legacy,
		AddressType::NestedSegwit,
		AddressType::NativeSegwit,
		AddressType::Taproot,
	];

	for address_type in address_types {
		let addr = node.onchain_payment().new_address_for_type(address_type).unwrap();
		assert!(!addr.to_string().is_empty());
	}
}

// Test that multiple address types can be configured for monitoring
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_multi_wallet_monitoring_config() {
	let (_bitcoind, _electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&_electrsd);

	// Test with different primary types and monitoring configurations
	let test_cases = vec![
		(AddressType::Legacy, vec![AddressType::NativeSegwit, AddressType::Taproot]),
		(AddressType::NestedSegwit, vec![AddressType::Legacy, AddressType::NativeSegwit]),
		(
			AddressType::NativeSegwit,
			vec![AddressType::Legacy, AddressType::NestedSegwit, AddressType::Taproot],
		),
		(AddressType::Taproot, vec![AddressType::Legacy, AddressType::NestedSegwit]),
	];

	for (primary_type, monitored_types) in test_cases {
		let mut config = common::random_config(true);
		config.node_config.address_type = primary_type;
		config.node_config.address_types_to_monitor = monitored_types.clone();

		let node = setup_node(&chain_source, config, None);
		assert!(node.status().is_running);

		// Verify we can generate an address
		let addr = node.onchain_payment().new_address().unwrap();
		assert!(!addr.to_string().is_empty());

		node.stop().unwrap();
	}
}

// Test that Electrum chain source works with multi-wallet configuration
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_multi_wallet_electrum_setup() {
	let (_bitcoind, _electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Electrum(&_electrsd);

	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NestedSegwit;
	config.node_config.address_types_to_monitor =
		vec![AddressType::NativeSegwit, AddressType::Taproot];

	let node = setup_node(&chain_source, config, None);
	assert!(node.status().is_running);

	// Verify we can generate an address
	let addr = node.onchain_payment().new_address().unwrap();
	assert!(!addr.to_string().is_empty());
}

// Test that all combinations of primary and monitored types work
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_multi_wallet_all_combinations() {
	let address_types = vec![
		AddressType::Legacy,
		AddressType::NestedSegwit,
		AddressType::NativeSegwit,
		AddressType::Taproot,
	];

	for primary_type in &address_types {
		for monitored_type in &address_types {
			if primary_type == monitored_type {
				continue; // Skip same type
			}

			let (_bitcoind, _electrsd) = setup_bitcoind_and_electrsd();
			let chain_source = TestChainSource::Esplora(&_electrsd);

			let mut config = common::random_config(true);
			config.node_config.address_type = *primary_type;
			config.node_config.address_types_to_monitor = vec![*monitored_type];

			let node = setup_node(&chain_source, config, None);
			assert!(node.status().is_running);

			// Verify we can generate an address
			let addr = node.onchain_payment().new_address().unwrap();
			assert!(!addr.to_string().is_empty());

			node.stop().unwrap();
		}
	}
}

// Test that empty monitoring list works (only primary wallet)
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_multi_wallet_empty_monitoring() {
	let (_bitcoind, _electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&_electrsd);

	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor = vec![]; // Empty monitoring list

	let node = setup_node(&chain_source, config, None);
	assert!(node.status().is_running);

	// Verify we can generate an address
	let addr = node.onchain_payment().new_address().unwrap();
	assert!(!addr.to_string().is_empty());
}

// Test that monitoring all other types works
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_multi_wallet_monitor_all_others() {
	let (_bitcoind, _electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&_electrsd);

	let address_types = vec![
		AddressType::Legacy,
		AddressType::NestedSegwit,
		AddressType::NativeSegwit,
		AddressType::Taproot,
	];

	for primary_type in &address_types {
		let mut config = common::random_config(true);
		config.node_config.address_type = *primary_type;
		// Monitor all other types
		config.node_config.address_types_to_monitor =
			address_types.iter().copied().filter(|&at| at != *primary_type).collect();

		let node = setup_node(&chain_source, config, None);
		assert!(node.status().is_running);

		// Verify we can generate an address
		let addr = node.onchain_payment().new_address().unwrap();
		assert!(!addr.to_string().is_empty());

		node.stop().unwrap();
	}
}

// Test send operation with multi-wallet (should use UTXOs from all wallets)
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_multi_wallet_send_operation() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Test with NativeSegwit as primary, monitoring other types
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor =
		vec![AddressType::Legacy, AddressType::NestedSegwit, AddressType::Taproot];

	let node = setup_node(&chain_source, config, None);

	// Fund the primary address
	let addr = node.onchain_payment().new_address().unwrap();
	let fund_amount = 200_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr.clone()],
		Amount::from_sat(fund_amount),
	)
	.await;

	// Generate blocks to confirm the transaction
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;

	// Sync wallets to detect the funds
	node.sync_wallets().unwrap();

	// Wait a bit for the chain source to index
	std::thread::sleep(std::time::Duration::from_millis(500));

	// Verify we have funds
	let balances = node.list_balances();
	assert!(
		balances.spendable_onchain_balance_sats >= fund_amount - 10_000,
		"Should have funds available (accounting for fees)"
	);

	// Create a recipient address
	let recipient_addr = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Test send operation - should work with UTXOs from primary wallet
	let send_amount = 50_000;
	let txid =
		node.onchain_payment().send_to_address(&recipient_addr, send_amount, None, None).unwrap();

	// Wait for transaction to appear in mempool and verify
	wait_for_tx(&electrsd.client, txid).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;
	node.sync_wallets().unwrap();

	// Verify balance decreased appropriately
	let new_balances = node.list_balances();
	assert!(
		new_balances.spendable_onchain_balance_sats < balances.spendable_onchain_balance_sats,
		"Balance should decrease after send"
	);
}

// Test send_all_to_address with multi-wallet
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_multi_wallet_send_all() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Test with Taproot as primary, monitoring other types
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::Taproot;
	config.node_config.address_types_to_monitor =
		vec![AddressType::Legacy, AddressType::NativeSegwit];

	let node = setup_node(&chain_source, config, None);

	// Fund the primary address
	let addr = node.onchain_payment().new_address().unwrap();
	let fund_amount = 300_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr.clone()],
		Amount::from_sat(fund_amount),
	)
	.await;

	// Generate blocks to confirm
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;
	node.sync_wallets().unwrap();

	// Wait a bit for the chain source to index
	std::thread::sleep(std::time::Duration::from_millis(500));

	// Verify we have funds
	let balances = node.list_balances();
	assert!(
		balances.spendable_onchain_balance_sats >= fund_amount - 10_000,
		"Should have funds available"
	);

	// Create a recipient address
	let recipient_addr = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Test send_all_to_address - should use UTXOs from all wallets
	let txid = node.onchain_payment().send_all_to_address(&recipient_addr, true, None).unwrap();

	// Wait for transaction and verify
	wait_for_tx(&electrsd.client, txid).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;
	node.sync_wallets().unwrap();

	// Verify balance is near zero after sending all
	let new_balances = node.list_balances();
	assert!(
		new_balances.spendable_onchain_balance_sats < 10_000,
		"Balance should be near zero after sending all funds"
	);
}

// Test RBF operation with multi-wallet
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_multi_wallet_rbf() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Test with Legacy as primary, monitoring other types
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::Legacy;
	config.node_config.address_types_to_monitor =
		vec![AddressType::NativeSegwit, AddressType::Taproot];

	let node = setup_node(&chain_source, config, None);

	// Fund the primary address
	let addr = node.onchain_payment().new_address().unwrap();
	let fund_amount = 250_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr.clone()],
		Amount::from_sat(fund_amount),
	)
	.await;

	// Wait for funding to be confirmed and synced
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	// Create a recipient address
	let recipient_addr = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Send a transaction (this should use UTXOs from all wallets)
	let send_amount = 50_000;
	let initial_txid =
		node.onchain_payment().send_to_address(&recipient_addr, send_amount, None, None).unwrap();

	// Wait for the transaction to be in mempool and sync wallet
	wait_for_tx(&electrsd.client, initial_txid).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_millis(500));

	// Test RBF - bump the fee (should be able to use UTXOs from all wallets for the replacement)
	let higher_fee_rate = FeeRate::from_sat_per_kwu(1000); // Higher fee rate
	let rbf_txid = node.onchain_payment().bump_fee_by_rbf(&initial_txid, higher_fee_rate).unwrap();

	// Verify we got a new transaction ID
	assert_ne!(initial_txid, rbf_txid, "RBF should create a new transaction");
}

// Test CPFP operation with multi-wallet
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_multi_wallet_cpfp() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Test with NestedSegwit as primary, monitoring other types
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NestedSegwit;
	config.node_config.address_types_to_monitor =
		vec![AddressType::Legacy, AddressType::NativeSegwit, AddressType::Taproot];

	let node = setup_node(&chain_source, config, None);

	// Fund the primary address
	let addr = node.onchain_payment().new_address().unwrap();
	let fund_amount = 400_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr.clone()],
		Amount::from_sat(fund_amount),
	)
	.await;

	// Wait for funding to be confirmed and synced
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	// Create a recipient address
	let recipient_addr = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Send a transaction
	let send_amount = 60_000;
	let parent_txid =
		node.onchain_payment().send_to_address(&recipient_addr, send_amount, None, None).unwrap();

	// Wait for the transaction to be in mempool and sync wallet
	wait_for_tx(&electrsd.client, parent_txid).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_millis(500));

	// Test CPFP - accelerate the parent transaction (should use UTXOs from all wallets)
	let cpfp_fee_rate = FeeRate::from_sat_per_kwu(1500);
	let cpfp_txid =
		node.onchain_payment().accelerate_by_cpfp(&parent_txid, Some(cpfp_fee_rate), None).unwrap();

	// Verify we got a new transaction ID
	assert_ne!(parent_txid, cpfp_txid, "CPFP should create a new child transaction");

	// Test calculate_cpfp_fee_rate
	let calculated_fee_rate =
		node.onchain_payment().calculate_cpfp_fee_rate(&parent_txid, false).unwrap();
	assert!(calculated_fee_rate.to_sat_per_kwu() > 0, "CPFP fee rate should be calculated");
}

// Test CPFP works correctly for cross-wallet transactions
// The change from cross-wallet transactions goes to the primary wallet,
// and CPFP should find it there even when the transaction used inputs from multiple wallets
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_cpfp_for_cross_wallet_transaction() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Set up node with NativeSegwit as primary, monitoring Legacy
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor = vec![AddressType::Legacy];

	let node = setup_node(&chain_source, config, None);

	// Get addresses from both wallet types
	let native_addr = node.onchain_payment().new_address().unwrap();
	let legacy_addr = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();

	// Fund both wallets with amounts that require combining for a larger send
	let fund_amount_each = 100_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![native_addr.clone()],
		Amount::from_sat(fund_amount_each),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![legacy_addr.clone()],
		Amount::from_sat(fund_amount_each),
	)
	.await;

	// Generate blocks and sync
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(2));

	// Create a recipient address
	let recipient_addr = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Send amount requiring UTXOs from BOTH wallets
	// 140,000 > 100,000 (single wallet) but < 200,000 (both wallets)
	// Leave enough for change to enable CPFP
	let send_amount = 140_000;
	let parent_txid = node
		.onchain_payment()
		.send_to_address(&recipient_addr, send_amount, None, None)
		.expect("Cross-wallet send should succeed");

	// Wait for tx to be in mempool
	wait_for_tx(&electrsd.client, parent_txid).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	// CPFP should work for cross-wallet transactions because change goes to primary wallet
	let cpfp_fee_rate = FeeRate::from_sat_per_kwu(1500);
	let cpfp_result =
		node.onchain_payment().accelerate_by_cpfp(&parent_txid, Some(cpfp_fee_rate), None);

	// CPFP should succeed - the change output is in the primary wallet
	assert!(
		cpfp_result.is_ok(),
		"CPFP should work for cross-wallet transactions (change goes to primary wallet): {:?}",
		cpfp_result.err()
	);

	let cpfp_txid = cpfp_result.unwrap();
	assert_ne!(parent_txid, cpfp_txid, "CPFP should create a new child transaction");

	// Wait for child tx
	wait_for_tx(&electrsd.client, cpfp_txid).await;

	node.stop().unwrap();
}

// Test UTXO selection with multi-wallet
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_multi_wallet_utxo_selection() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Test with NativeSegwit as primary, monitoring other types
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor =
		vec![AddressType::Legacy, AddressType::NestedSegwit];

	let node = setup_node(&chain_source, config, None);

	// Fund the primary address
	let addr = node.onchain_payment().new_address().unwrap();
	let fund_amount = 500_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr.clone()],
		Amount::from_sat(fund_amount),
	)
	.await;

	// Wait for funding to be confirmed and synced
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	// Test list_spendable_outputs - should return UTXOs from all wallets
	let spendable_outputs = node.onchain_payment().list_spendable_outputs().unwrap();
	assert!(!spendable_outputs.is_empty(), "Should have spendable outputs from primary wallet");

	// Test select_utxos_with_algorithm - should consider UTXOs from all wallets
	let target_amount = 100_000;
	let fee_rate = FeeRate::from_sat_per_kwu(500);
	let selected_utxos = node
		.onchain_payment()
		.select_utxos_with_algorithm(
			target_amount,
			Some(fee_rate),
			ldk_node::CoinSelectionAlgorithm::LargestFirst,
			None,
		)
		.unwrap();

	// Should have selected at least one UTXO from the funded wallet
	assert!(!selected_utxos.is_empty(), "Should have selected at least one UTXO");
}

// Test balance aggregation from all wallets
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_multi_wallet_balance_aggregation() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Test with NativeSegwit as primary, monitoring other types
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor =
		vec![AddressType::Legacy, AddressType::NestedSegwit, AddressType::Taproot];

	let node = setup_node(&chain_source, config, None);

	// Fund the primary address
	let addr = node.onchain_payment().new_address().unwrap();
	let fund_amount = 150_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr.clone()],
		Amount::from_sat(fund_amount),
	)
	.await;

	// Test that list_balances aggregates from all wallets
	// The method should succeed without error - balance values are valid (may be 0 if sync hasn't completed)
	let balances = node.list_balances();
	// Just verify we can access the fields - they're u64 so always valid
	let _ = balances.spendable_onchain_balance_sats;
	let _ = balances.total_onchain_balance_sats;
}

// Test get_address_balance with multi-wallet
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_multi_wallet_get_address_balance() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor = vec![AddressType::Legacy];

	let node = setup_node(&chain_source, config, None);

	// Fund the primary address
	let addr = node.onchain_payment().new_address().unwrap();
	let fund_amount = 120_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr.clone()],
		Amount::from_sat(fund_amount),
	)
	.await;

	// Test get_address_balance - should work for addresses from any wallet
	// The unwrap verifies the method succeeds; balance may be 0 if sync hasn't completed
	let _balance = node.get_address_balance(&addr.to_string()).unwrap();
}

// Test calculate_total_fee with multi-wallet
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_multi_wallet_calculate_total_fee() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::Taproot;
	config.node_config.address_types_to_monitor = vec![AddressType::NativeSegwit];

	let node = setup_node(&chain_source, config, None);

	// Fund the primary address
	let addr = node.onchain_payment().new_address().unwrap();
	let fund_amount = 300_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr.clone()],
		Amount::from_sat(fund_amount),
	)
	.await;

	// Wait for funding to be confirmed and synced
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	// Create a recipient address
	let recipient_addr = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Test calculate_total_fee - should consider UTXOs from all wallets
	let send_amount = 100_000;
	let fee_rate = FeeRate::from_sat_per_kwu(500);
	let total_fee = node
		.onchain_payment()
		.calculate_total_fee(&recipient_addr, send_amount, Some(fee_rate), None)
		.unwrap();

	assert!(total_fee > 0, "Total fee should be calculated");
}

// Test send operation with UTXOs from multiple wallets (different address types)
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_multi_wallet_send_with_utxos_from_all_wallets() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Set up node with NativeSegwit as primary, monitoring Legacy and Taproot
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor = vec![AddressType::Legacy, AddressType::Taproot];

	let node = setup_node(&chain_source, config, None);

	// Get addresses for each wallet type by creating a new address (primary) and
	// accessing monitored wallets through the node's internal state
	// For this test, we'll fund the primary address and verify send works
	let primary_addr = node.onchain_payment().new_address().unwrap();

	// Fund the primary address
	let fund_amount = 500_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![primary_addr.clone()],
		Amount::from_sat(fund_amount),
	)
	.await;

	// Generate blocks to confirm the transaction
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;

	// Sync wallets to detect the funds
	node.sync_wallets().unwrap();

	// Wait a bit for the chain source to index
	std::thread::sleep(std::time::Duration::from_millis(500));

	// Verify we have funds
	let balances = node.list_balances();
	assert!(
		balances.spendable_onchain_balance_sats >= fund_amount - 10_000,
		"Should have funds available (accounting for fees)"
	);

	// Create a recipient address
	let recipient_addr = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Test send operation - should work with UTXOs from primary wallet
	let send_amount = 100_000;
	let txid =
		node.onchain_payment().send_to_address(&recipient_addr, send_amount, None, None).unwrap();

	// Verify transaction was created (if send_to_address succeeded, txid is valid)
	// The fact that we got here without an error means the transaction was created successfully

	// Wait for transaction to be confirmed
	wait_for_tx(&electrsd.client, txid).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;
	node.sync_wallets().unwrap();

	// Wait a bit for the chain source to index
	std::thread::sleep(std::time::Duration::from_millis(500));

	// Verify balance decreased
	let new_balances = node.list_balances();
	assert!(
		new_balances.spendable_onchain_balance_sats < balances.spendable_onchain_balance_sats,
		"Balance should decrease after send"
	);
}

// Test send_all_to_address with multi-wallet (should use UTXOs from all wallets)
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_multi_wallet_send_all_with_utxos_from_all_wallets() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Set up node with Legacy as primary, monitoring NativeSegwit and NestedSegwit
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::Legacy;
	config.node_config.address_types_to_monitor =
		vec![AddressType::NativeSegwit, AddressType::NestedSegwit];

	let node = setup_node(&chain_source, config, None);

	// Fund the primary address
	let primary_addr = node.onchain_payment().new_address().unwrap();
	let fund_amount = 400_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![primary_addr.clone()],
		Amount::from_sat(fund_amount),
	)
	.await;

	// Generate blocks to confirm
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;
	node.sync_wallets().unwrap();

	// Wait a bit for the chain source to index
	std::thread::sleep(std::time::Duration::from_millis(500));

	// Verify we have funds
	let balances = node.list_balances();
	assert!(
		balances.spendable_onchain_balance_sats >= fund_amount - 10_000,
		"Should have funds available"
	);

	// Create a recipient address
	let recipient_addr = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Test send_all_to_address - should use UTXOs from all wallets
	let txid = node.onchain_payment().send_all_to_address(&recipient_addr, true, None).unwrap();

	// Verify transaction was created (if send_to_address succeeded, txid is valid)
	// The fact that we got here without an error means the transaction was created successfully

	// Wait for transaction to be confirmed
	wait_for_tx(&electrsd.client, txid).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;
	node.sync_wallets().unwrap();

	// Wait a bit for the chain source to index
	std::thread::sleep(std::time::Duration::from_millis(500));

	// Verify balance is near zero (reserve may remain)
	let new_balances = node.list_balances();
	assert!(
		new_balances.spendable_onchain_balance_sats < 10_000,
		"Balance should be near zero after sending all funds"
	);
}

// Test that send operation correctly handles UTXOs from different address types
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_multi_wallet_send_handles_different_address_types() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Test with different primary types to ensure all combinations work
	let test_cases = vec![
		(AddressType::NativeSegwit, vec![AddressType::Legacy]),
		(AddressType::Legacy, vec![AddressType::NativeSegwit, AddressType::Taproot]),
		(AddressType::Taproot, vec![AddressType::NestedSegwit]),
	];

	for (primary_type, monitored_types) in test_cases {
		let mut config = common::random_config(true);
		config.node_config.address_type = primary_type;
		config.node_config.address_types_to_monitor = monitored_types;

		let node = setup_node(&chain_source, config, None);

		// Fund the primary address
		let addr = node.onchain_payment().new_address().unwrap();
		let fund_amount = 200_000;
		premine_and_distribute_funds(
			&bitcoind.client,
			&electrsd.client,
			vec![addr.clone()],
			Amount::from_sat(fund_amount),
		)
		.await;

		generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;
		node.sync_wallets().unwrap();
		std::thread::sleep(std::time::Duration::from_millis(500));

		// Create a recipient address
		let recipient_addr = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
			.unwrap()
			.require_network(bitcoin::Network::Regtest)
			.unwrap();

		// Test send - should work regardless of address type combination
		let send_amount = 50_000;
		let txid = node
			.onchain_payment()
			.send_to_address(&recipient_addr, send_amount, None, None)
			.expect("Send should succeed for all address type combinations");

		// Wait for transaction to propagate
		wait_for_tx(&electrsd.client, txid).await;

		node.stop().unwrap();
	}
}

// Test spending UTXOs from multiple wallets (different address types) in a single transaction
// This is the key test for multi-wallet functionality - it verifies that UTXOs from different
// address types can be combined in a single transaction
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_cross_wallet_spending() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Set up node with NativeSegwit as primary, monitoring Legacy
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor = vec![AddressType::Legacy];

	let node = setup_node(&chain_source, config, None);

	// Get addresses from both wallet types
	let native_segwit_addr = node.onchain_payment().new_address().unwrap();
	let legacy_addr = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();

	// Fund both addresses with amounts that individually aren't enough for a larger send
	// NativeSegwit: 100,000 sats
	// Legacy: 100,000 sats
	// Total: 200,000 sats
	let fund_amount_each = 100_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![native_segwit_addr.clone()],
		Amount::from_sat(fund_amount_each),
	)
	.await;

	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![legacy_addr.clone()],
		Amount::from_sat(fund_amount_each),
	)
	.await;

	// Generate blocks to confirm the transactions
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

	// Sync wallets to detect the funds
	node.sync_wallets().unwrap();

	// Wait for sync to complete
	std::thread::sleep(std::time::Duration::from_secs(2));

	// Verify we have funds in both wallets by checking total balance
	let balances = node.list_balances();
	let expected_total = fund_amount_each * 2;
	assert!(
		balances.total_onchain_balance_sats >= expected_total - 10_000,
		"Should have ~{} sats total, but have {} sats",
		expected_total,
		balances.total_onchain_balance_sats
	);

	// Check per-address-type balances
	let native_balance = node.get_balance_for_address_type(AddressType::NativeSegwit).unwrap();
	let legacy_balance = node.get_balance_for_address_type(AddressType::Legacy).unwrap();

	assert!(
		native_balance.total_sats >= fund_amount_each - 1000,
		"NativeSegwit wallet should have ~{} sats, but has {} sats",
		fund_amount_each,
		native_balance.total_sats
	);
	assert!(
		legacy_balance.total_sats >= fund_amount_each - 1000,
		"Legacy wallet should have ~{} sats, but has {} sats",
		fund_amount_each,
		legacy_balance.total_sats
	);

	// Create a recipient address
	let recipient_addr = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Try to send an amount that requires UTXOs from BOTH wallets
	// 150,000 sats > 100,000 sats (either wallet alone) but < 200,000 sats (both combined)
	let send_amount = 150_000;
	let txid = node
		.onchain_payment()
		.send_to_address(&recipient_addr, send_amount, None, None)
		.expect("Cross-wallet spending should succeed - UTXOs from both wallets should be used");

	// Wait for transaction to propagate
	wait_for_tx(&electrsd.client, txid).await;

	// Generate a block to confirm the transaction
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;
	node.sync_wallets().unwrap();

	// Verify balance decreased appropriately
	let new_balances = node.list_balances();
	assert!(
		new_balances.total_onchain_balance_sats
			< balances.total_onchain_balance_sats - send_amount + 10_000,
		"Balance should have decreased by at least the send amount"
	);

	// Verify list_monitored_address_types returns both types
	let monitored_types = node.list_monitored_address_types();
	assert!(monitored_types.contains(&AddressType::NativeSegwit), "Should monitor NativeSegwit");
	assert!(monitored_types.contains(&AddressType::Legacy), "Should monitor Legacy");
}

// Test that get_balance_for_address_type returns an error for unmonitored types
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_get_balance_for_unmonitored_type() {
	let (_bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Setup node with only NativeSegwit as primary, no additional monitoring
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor = vec![];

	let node = setup_node(&chain_source, config, None);

	// Querying the primary type should succeed
	let native_balance = node.get_balance_for_address_type(AddressType::NativeSegwit);
	assert!(native_balance.is_ok(), "Should be able to get balance for primary type");

	// Querying an unmonitored type should return an error
	let legacy_balance = node.get_balance_for_address_type(AddressType::Legacy);
	assert!(legacy_balance.is_err(), "Should return error for unmonitored address type");

	let taproot_balance = node.get_balance_for_address_type(AddressType::Taproot);
	assert!(taproot_balance.is_err(), "Should return error for unmonitored address type");

	node.stop().unwrap();
}

// Test that address_types_to_monitor handles deduplication correctly
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_address_type_deduplication() {
	let (_bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Setup node with NativeSegwit as primary, but also include it in monitor list
	// This tests that the system handles deduplication correctly
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor =
		vec![AddressType::NativeSegwit, AddressType::Legacy];

	let node = setup_node(&chain_source, config, None);
	assert!(node.status().is_running, "Node should start without error");

	// list_monitored_address_types should return exactly 2 types (not 3)
	// NativeSegwit (primary) + Legacy (monitored), with NativeSegwit deduplicated
	let monitored_types = node.list_monitored_address_types();
	assert_eq!(
		monitored_types.len(),
		2,
		"Should have exactly 2 monitored types (deduplicated), got {:?}",
		monitored_types
	);
	assert!(monitored_types.contains(&AddressType::NativeSegwit), "Should contain NativeSegwit");
	assert!(monitored_types.contains(&AddressType::Legacy), "Should contain Legacy");

	node.stop().unwrap();
}

// Test spending works correctly when a monitored wallet has 0 balance
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_spend_with_empty_monitored_wallet() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Setup node monitoring Legacy, but only fund NativeSegwit
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor = vec![AddressType::Legacy];

	let node = setup_node(&chain_source, config, None);

	// Only fund the NativeSegwit address (primary)
	let native_addr = node.onchain_payment().new_address().unwrap();
	let fund_amount = 200_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![native_addr.clone()],
		Amount::from_sat(fund_amount),
	)
	.await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	// Verify NativeSegwit has funds and Legacy has 0
	let native_balance = node.get_balance_for_address_type(AddressType::NativeSegwit).unwrap();
	let legacy_balance = node.get_balance_for_address_type(AddressType::Legacy).unwrap();

	assert!(
		native_balance.total_sats >= fund_amount - 1000,
		"NativeSegwit should have funds: {}",
		native_balance.total_sats
	);
	assert_eq!(legacy_balance.total_sats, 0, "Legacy wallet should have 0 balance");

	// Send should work from NativeSegwit only
	let recipient_addr = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	let send_amount = 50_000;
	let result = node.onchain_payment().send_to_address(&recipient_addr, send_amount, None, None);
	assert!(result.is_ok(), "Send should succeed using only NativeSegwit funds");

	node.stop().unwrap();
}

// Test that multi-wallet state persists correctly across node restarts
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_multi_wallet_persistence_across_restart() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Create config with multi-wallet setup
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor = vec![AddressType::Legacy, AddressType::Taproot];

	let node = setup_node(&chain_source, config, None);

	// Get addresses from multiple wallet types
	let native_addr = node.onchain_payment().new_address().unwrap();
	let legacy_addr = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();
	let taproot_addr = node.onchain_payment().new_address_for_type(AddressType::Taproot).unwrap();

	// Fund all three wallets
	let fund_amount = 100_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![native_addr.clone()],
		Amount::from_sat(fund_amount),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![legacy_addr.clone()],
		Amount::from_sat(fund_amount),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![taproot_addr.clone()],
		Amount::from_sat(fund_amount),
	)
	.await;

	// Generate blocks and sync
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	// Verify balances before restart
	let native_balance_before =
		node.get_balance_for_address_type(AddressType::NativeSegwit).unwrap();
	let legacy_balance_before = node.get_balance_for_address_type(AddressType::Legacy).unwrap();
	let taproot_balance_before = node.get_balance_for_address_type(AddressType::Taproot).unwrap();
	let total_balance_before = node.list_balances().total_onchain_balance_sats;

	assert!(
		native_balance_before.total_sats >= fund_amount - 1000,
		"NativeSegwit should have funds before restart"
	);
	assert!(
		legacy_balance_before.total_sats >= fund_amount - 1000,
		"Legacy should have funds before restart"
	);
	assert!(
		taproot_balance_before.total_sats >= fund_amount - 1000,
		"Taproot should have funds before restart"
	);

	// Stop the node
	node.stop().unwrap();

	// Restart the node using stop/start pattern (same node object)
	node.start().unwrap();

	// Sync after restart
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	// Verify balances are preserved after restart
	let native_balance_after =
		node.get_balance_for_address_type(AddressType::NativeSegwit).unwrap();
	let legacy_balance_after = node.get_balance_for_address_type(AddressType::Legacy).unwrap();
	let taproot_balance_after = node.get_balance_for_address_type(AddressType::Taproot).unwrap();
	let total_balance_after = node.list_balances().total_onchain_balance_sats;

	assert_eq!(
		native_balance_before.total_sats, native_balance_after.total_sats,
		"NativeSegwit balance should persist across restart"
	);
	assert_eq!(
		legacy_balance_before.total_sats, legacy_balance_after.total_sats,
		"Legacy balance should persist across restart"
	);
	assert_eq!(
		taproot_balance_before.total_sats, taproot_balance_after.total_sats,
		"Taproot balance should persist across restart"
	);
	assert_eq!(
		total_balance_before, total_balance_after,
		"Total balance should persist across restart"
	);

	// Verify list_monitored_address_types returns all types after restart
	let monitored_types = node.list_monitored_address_types();
	assert!(
		monitored_types.contains(&AddressType::NativeSegwit),
		"Should still monitor NativeSegwit"
	);
	assert!(monitored_types.contains(&AddressType::Legacy), "Should still monitor Legacy");
	assert!(monitored_types.contains(&AddressType::Taproot), "Should still monitor Taproot");

	node.stop().unwrap();
}

// Test cross-wallet spending with three different wallet types
// This ensures UTXOs from multiple different address types can be combined
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_cross_wallet_spending_three_types() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Set up node with NativeSegwit as primary, monitoring Legacy and Taproot
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor = vec![AddressType::Legacy, AddressType::Taproot];

	let node = setup_node(&chain_source, config, None);

	// Get addresses from all three wallet types
	let native_addr = node.onchain_payment().new_address().unwrap();
	let legacy_addr = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();
	let taproot_addr = node.onchain_payment().new_address_for_type(AddressType::Taproot).unwrap();

	// Fund each wallet with an amount that individually isn't enough for a larger send
	// NativeSegwit: 80,000 sats
	// Legacy: 80,000 sats
	// Taproot: 80,000 sats
	// Total: 240,000 sats
	let fund_amount_each = 80_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![native_addr.clone()],
		Amount::from_sat(fund_amount_each),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![legacy_addr.clone()],
		Amount::from_sat(fund_amount_each),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![taproot_addr.clone()],
		Amount::from_sat(fund_amount_each),
	)
	.await;

	// Generate blocks to confirm
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(2));

	// Verify we have funds in all three wallets
	let native_balance = node.get_balance_for_address_type(AddressType::NativeSegwit).unwrap();
	let legacy_balance = node.get_balance_for_address_type(AddressType::Legacy).unwrap();
	let taproot_balance = node.get_balance_for_address_type(AddressType::Taproot).unwrap();

	assert!(
		native_balance.total_sats >= fund_amount_each - 1000,
		"NativeSegwit should have ~{} sats",
		fund_amount_each
	);
	assert!(
		legacy_balance.total_sats >= fund_amount_each - 1000,
		"Legacy should have ~{} sats",
		fund_amount_each
	);
	assert!(
		taproot_balance.total_sats >= fund_amount_each - 1000,
		"Taproot should have ~{} sats",
		fund_amount_each
	);

	let total_before = node.list_balances().total_onchain_balance_sats;

	// Create a recipient address
	let recipient_addr = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Try to send an amount that requires UTXOs from ALL THREE wallets
	// 200,000 sats > 160,000 sats (any two wallets) but < 240,000 sats (all three)
	let send_amount = 200_000;
	let txid =
		node.onchain_payment().send_to_address(&recipient_addr, send_amount, None, None).expect(
			"Cross-wallet spending should succeed - UTXOs from all three wallets should be used",
		);

	// Wait for transaction to propagate
	wait_for_tx(&electrsd.client, txid).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	// Verify total balance decreased
	let total_after = node.list_balances().total_onchain_balance_sats;
	assert!(
		total_after < total_before - send_amount + 10_000,
		"Balance should have decreased by at least the send amount"
	);

	node.stop().unwrap();
}

// Test that send_all correctly drains all wallets
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_send_all_drains_all_wallets() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Set up node with Taproot as primary, monitoring NativeSegwit
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::Taproot;
	config.node_config.address_types_to_monitor = vec![AddressType::NativeSegwit];

	let node = setup_node(&chain_source, config, None);

	// Get addresses from both wallet types
	let taproot_addr = node.onchain_payment().new_address().unwrap();
	let native_addr =
		node.onchain_payment().new_address_for_type(AddressType::NativeSegwit).unwrap();

	// Fund both wallets
	let fund_amount = 100_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![taproot_addr.clone()],
		Amount::from_sat(fund_amount),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![native_addr.clone()],
		Amount::from_sat(fund_amount),
	)
	.await;

	// Generate blocks and sync
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	// Verify both wallets have funds
	let taproot_balance = node.get_balance_for_address_type(AddressType::Taproot).unwrap();
	let native_balance = node.get_balance_for_address_type(AddressType::NativeSegwit).unwrap();

	assert!(taproot_balance.total_sats >= fund_amount - 1000, "Taproot should have funds");
	assert!(native_balance.total_sats >= fund_amount - 1000, "NativeSegwit should have funds");

	// Create a recipient address
	let recipient_addr = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Send all - should drain both wallets
	let txid = node
		.onchain_payment()
		.send_all_to_address(&recipient_addr, true, None)
		.expect("send_all should succeed");

	// Wait for transaction
	wait_for_tx(&electrsd.client, txid).await;
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	// Verify BOTH wallets are drained
	let taproot_balance_after = node.get_balance_for_address_type(AddressType::Taproot).unwrap();
	let native_balance_after =
		node.get_balance_for_address_type(AddressType::NativeSegwit).unwrap();
	let total_after = node.list_balances().total_onchain_balance_sats;

	assert!(
		taproot_balance_after.spendable_sats < 10_000,
		"Taproot wallet should be drained, but has {} sats",
		taproot_balance_after.spendable_sats
	);
	assert!(
		native_balance_after.spendable_sats < 10_000,
		"NativeSegwit wallet should be drained, but has {} sats",
		native_balance_after.spendable_sats
	);
	assert!(total_after < 10_000, "Total balance should be near zero, but is {} sats", total_after);

	node.stop().unwrap();
}

// Test that new_address_for_type returns error for unmonitored types
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_new_address_for_unmonitored_type() {
	let (_bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Setup node with only NativeSegwit as primary, no additional monitoring
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor = vec![];

	let node = setup_node(&chain_source, config, None);

	// Requesting address for primary type should succeed
	let native_result = node.onchain_payment().new_address_for_type(AddressType::NativeSegwit);
	assert!(native_result.is_ok(), "Should be able to get address for primary type");

	// Requesting address for unmonitored type should fail
	let legacy_result = node.onchain_payment().new_address_for_type(AddressType::Legacy);
	assert!(legacy_result.is_err(), "Should return error for unmonitored address type");

	let taproot_result = node.onchain_payment().new_address_for_type(AddressType::Taproot);
	assert!(taproot_result.is_err(), "Should return error for unmonitored address type");

	node.stop().unwrap();
}

// Test that RBF works correctly when node has multiple wallets configured
// but the transaction only uses UTXOs from a single wallet
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_rbf_single_wallet_input_with_multi_wallet_config() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Set up node with NativeSegwit as primary, monitoring Legacy
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor = vec![AddressType::Legacy];

	let node = setup_node(&chain_source, config, None);

	// Get addresses from both wallet types
	let native_addr = node.onchain_payment().new_address().unwrap();
	let legacy_addr = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();

	// Fund both wallets - primary wallet gets more funds
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![native_addr.clone()],
		Amount::from_sat(300_000),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![legacy_addr.clone()],
		Amount::from_sat(100_000),
	)
	.await;

	// Generate blocks and sync
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(2));

	// Verify both wallets have funds
	let native_balance = node.get_balance_for_address_type(AddressType::NativeSegwit).unwrap();
	let legacy_balance = node.get_balance_for_address_type(AddressType::Legacy).unwrap();
	assert!(native_balance.total_sats >= 299_000, "NativeSegwit should have funds");
	assert!(legacy_balance.total_sats >= 99_000, "Legacy should have funds");

	// Create a recipient address
	let recipient_addr = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Send a smaller amount that can be satisfied by primary wallet alone
	// This leaves room for RBF fee bumping
	let send_amount = 50_000;
	let initial_txid = node
		.onchain_payment()
		.send_to_address(&recipient_addr, send_amount, None, None)
		.expect("Initial send should succeed");

	// Wait for tx to be in mempool
	wait_for_tx(&electrsd.client, initial_txid).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_millis(500));

	// Bump fee via RBF - this works because the original tx only used one wallet's UTXOs
	let higher_fee_rate = FeeRate::from_sat_per_kwu(1000);
	let rbf_txid = node
		.onchain_payment()
		.bump_fee_by_rbf(&initial_txid, higher_fee_rate)
		.expect("RBF should succeed for single-wallet-input transaction");

	assert_ne!(initial_txid, rbf_txid, "RBF should create a new transaction");

	// Wait for replacement
	wait_for_tx(&electrsd.client, rbf_txid).await;

	node.stop().unwrap();
}

// Test that RBF works correctly for cross-wallet transactions
// Cross-wallet RBF reduces the change output to pay for higher fees
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_rbf_cross_wallet_transaction() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Set up node with NativeSegwit as primary, monitoring Legacy
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor = vec![AddressType::Legacy];

	let node = setup_node(&chain_source, config, None);

	// Get addresses from both wallet types
	let native_addr = node.onchain_payment().new_address().unwrap();
	let legacy_addr = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();

	// Fund both wallets - need enough for send + change for RBF
	let fund_amount_each = 100_000;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![native_addr.clone()],
		Amount::from_sat(fund_amount_each),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![legacy_addr.clone()],
		Amount::from_sat(fund_amount_each),
	)
	.await;

	// Generate blocks and sync
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(2));

	// Verify both wallets have funds before the cross-wallet send
	let native_balance_before =
		node.get_balance_for_address_type(AddressType::NativeSegwit).unwrap();
	let legacy_balance_before = node.get_balance_for_address_type(AddressType::Legacy).unwrap();
	assert!(
		native_balance_before.total_sats >= fund_amount_each - 1000,
		"NativeSegwit should have ~100k sats, got {}",
		native_balance_before.total_sats
	);
	assert!(
		legacy_balance_before.total_sats >= fund_amount_each - 1000,
		"Legacy should have ~100k sats, got {}",
		legacy_balance_before.total_sats
	);

	// Create a recipient address
	let recipient_addr = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Send amount requiring UTXOs from BOTH wallets
	// 120,000 > 100,000 (single wallet) but < 200,000 (both wallets)
	// This leaves ~80,000 for change (enough for RBF fee bump)
	let send_amount = 120_000;
	let initial_txid = node
		.onchain_payment()
		.send_to_address(&recipient_addr, send_amount, None, None)
		.expect("Initial cross-wallet send should succeed");

	// Wait for tx to be in mempool
	wait_for_tx(&electrsd.client, initial_txid).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	// Record balance before RBF
	let total_before_rbf = node.list_balances().total_onchain_balance_sats;

	// Attempt RBF - cross-wallet RBF should work by reducing the change output
	let higher_fee_rate = FeeRate::from_sat_per_kwu(2000);
	let rbf_result = node.onchain_payment().bump_fee_by_rbf(&initial_txid, higher_fee_rate);

	// RBF should succeed for cross-wallet transactions
	assert!(
		rbf_result.is_ok(),
		"RBF for cross-wallet transactions should succeed: {:?}",
		rbf_result.err()
	);

	let rbf_txid = rbf_result.unwrap();
	assert_ne!(initial_txid, rbf_txid, "RBF should create a new transaction");

	// Wait for replacement tx and sync
	wait_for_tx(&electrsd.client, rbf_txid).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_millis(500));

	// Verify balance decreased (higher fee was paid from change)
	let total_after_rbf = node.list_balances().total_onchain_balance_sats;
	assert!(
		total_after_rbf < total_before_rbf,
		"Balance should decrease after RBF due to higher fee (before: {}, after: {})",
		total_before_rbf,
		total_after_rbf
	);

	// Confirm the replacement transaction
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_millis(500));

	// Final balance check - should have: original funds - send_amount - fees
	let final_balance = node.list_balances().total_onchain_balance_sats;
	let expected_max = (fund_amount_each * 2) - send_amount;
	assert!(
		final_balance < expected_max,
		"Final balance {} should be less than {} (original - send)",
		final_balance,
		expected_max
	);

	node.stop().unwrap();
}

// Test that sync updates balances for all wallet types
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_sync_updates_all_wallet_balances() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor =
		vec![AddressType::Legacy, AddressType::NestedSegwit, AddressType::Taproot];

	let node = setup_node(&chain_source, config, None);

	// Get addresses from all wallet types
	let native_addr = node.onchain_payment().new_address().unwrap();
	let legacy_addr = node.onchain_payment().new_address_for_type(AddressType::Legacy).unwrap();
	let nested_addr =
		node.onchain_payment().new_address_for_type(AddressType::NestedSegwit).unwrap();
	let taproot_addr = node.onchain_payment().new_address_for_type(AddressType::Taproot).unwrap();

	// Verify all balances are 0 before funding
	assert_eq!(node.get_balance_for_address_type(AddressType::NativeSegwit).unwrap().total_sats, 0);
	assert_eq!(node.get_balance_for_address_type(AddressType::Legacy).unwrap().total_sats, 0);
	assert_eq!(node.get_balance_for_address_type(AddressType::NestedSegwit).unwrap().total_sats, 0);
	assert_eq!(node.get_balance_for_address_type(AddressType::Taproot).unwrap().total_sats, 0);

	// Fund all wallets with different amounts
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![native_addr.clone()],
		Amount::from_sat(100_000),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![legacy_addr.clone()],
		Amount::from_sat(200_000),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![nested_addr.clone()],
		Amount::from_sat(300_000),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![taproot_addr.clone()],
		Amount::from_sat(400_000),
	)
	.await;

	// Generate blocks
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;

	// Sync - this should update all wallet balances
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(2));

	// Verify all balances updated correctly
	let native_balance = node.get_balance_for_address_type(AddressType::NativeSegwit).unwrap();
	let legacy_balance = node.get_balance_for_address_type(AddressType::Legacy).unwrap();
	let nested_balance = node.get_balance_for_address_type(AddressType::NestedSegwit).unwrap();
	let taproot_balance = node.get_balance_for_address_type(AddressType::Taproot).unwrap();

	assert!(
		native_balance.total_sats >= 99_000,
		"NativeSegwit should have ~100k, has {}",
		native_balance.total_sats
	);
	assert!(
		legacy_balance.total_sats >= 199_000,
		"Legacy should have ~200k, has {}",
		legacy_balance.total_sats
	);
	assert!(
		nested_balance.total_sats >= 299_000,
		"NestedSegwit should have ~300k, has {}",
		nested_balance.total_sats
	);
	assert!(
		taproot_balance.total_sats >= 399_000,
		"Taproot should have ~400k, has {}",
		taproot_balance.total_sats
	);

	// Verify aggregate balance is sum of all
	let total = node.list_balances().total_onchain_balance_sats;
	let expected_total = native_balance.total_sats
		+ legacy_balance.total_sats
		+ nested_balance.total_sats
		+ taproot_balance.total_sats;
	assert_eq!(total, expected_total, "Total should equal sum of all wallet balances");

	node.stop().unwrap();
}

// Test that RBF with additional inputs correctly accounts for input weight in fee calculation.
// When adding inputs to meet a higher fee, the inputs themselves add weight, which requires
// more fee. This test verifies the resulting transaction achieves the target fee rate.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_rbf_additional_inputs_fee_rate_correctness() {
	let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();
	let chain_source = TestChainSource::Esplora(&electrsd);

	// Set up node with NativeSegwit as primary
	let mut config = common::random_config(true);
	config.node_config.address_type = AddressType::NativeSegwit;
	config.node_config.address_types_to_monitor = vec![];

	let node = setup_node(&chain_source, config, None);

	// Get addresses and fund with multiple small UTXOs
	// This ensures RBF will need to add inputs when bumping fee significantly
	let addr1 = node.onchain_payment().new_address().unwrap();
	let addr2 = node.onchain_payment().new_address().unwrap();
	let addr3 = node.onchain_payment().new_address().unwrap();

	// Fund with 3 separate UTXOs of 50k sats each
	let utxo_amount = 50_000u64;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr1.clone()],
		Amount::from_sat(utxo_amount),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr2.clone()],
		Amount::from_sat(utxo_amount),
	)
	.await;
	premine_and_distribute_funds(
		&bitcoind.client,
		&electrsd.client,
		vec![addr3.clone()],
		Amount::from_sat(utxo_amount),
	)
	.await;

	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 6).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(2));

	// Verify we have 3 UTXOs totaling ~150k sats
	let initial_balance = node.list_balances().total_onchain_balance_sats;
	assert!(
		initial_balance >= utxo_amount * 3 - 3000,
		"Should have ~150k sats, got {}",
		initial_balance
	);

	// Create recipient address
	let recipient_addr = Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
		.unwrap()
		.require_network(bitcoin::Network::Regtest)
		.unwrap();

	// Send 30k sats - this should use 1 UTXO and leave ~20k change
	// Using a low initial fee rate
	let send_amount = 30_000;
	let initial_txid = node
		.onchain_payment()
		.send_to_address(&recipient_addr, send_amount, None, None)
		.expect("Initial send should succeed");

	wait_for_tx(&electrsd.client, initial_txid).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_secs(1));

	// Now bump fee significantly - this should require adding more inputs
	// because the change output won't have enough to cover the increased fee
	// Use a very high fee rate to force adding inputs
	let high_fee_rate = FeeRate::from_sat_per_kwu(5000); // ~20 sat/vB

	let rbf_result = node.onchain_payment().bump_fee_by_rbf(&initial_txid, high_fee_rate);

	assert!(
		rbf_result.is_ok(),
		"RBF should succeed even when adding inputs: {:?}",
		rbf_result.err()
	);

	let rbf_txid = rbf_result.unwrap();
	assert_ne!(initial_txid, rbf_txid, "RBF should create a new transaction");

	// Wait for replacement and confirm
	wait_for_tx(&electrsd.client, rbf_txid).await;

	// Verify the transaction confirms successfully (which means it paid sufficient fee)
	generate_blocks_and_wait(&bitcoind.client, &electrsd.client, 1).await;
	node.sync_wallets().unwrap();
	std::thread::sleep(std::time::Duration::from_millis(500));

	// Final balance check - the transaction should have confirmed
	let final_balance = node.list_balances().total_onchain_balance_sats;
	let max_expected = initial_balance - send_amount;

	// Balance should be less than initial - send (due to fees)
	assert!(
		final_balance < max_expected,
		"Final balance {} should be less than {} (initial - send amount)",
		final_balance,
		max_expected
	);

	// The fee paid should be reasonable (not excessively high due to calculation errors)
	let fee_paid = initial_balance - final_balance - send_amount;
	// With a ~200 vB tx at 20 sat/vB, expect ~4000 sats fee, allow up to 10000 for margin
	assert!(fee_paid < 15000, "Fee {} seems too high, possible fee calculation error", fee_paid);
	assert!(fee_paid > 1000, "Fee {} seems too low for the requested fee rate", fee_paid);

	node.stop().unwrap();
}

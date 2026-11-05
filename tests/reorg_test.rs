mod common;
use std::collections::{HashMap, HashSet};

use bitcoin::Amount;
use electrsd::corepc_node::Client as BitcoindClient;
use electrum_client::ElectrumApi;
use ldk_node::payment::{PaymentDirection, PaymentKind};
use ldk_node::{Event, LightningBalance, Node, PendingSweepBalance};
use lightning::chain::channelmonitor::BalanceSource;
use lightning::events::ClosureReason;
use proptest::prelude::prop;
use proptest::proptest;

use crate::common::{
	expect_event, exponential_backoff_poll, generate_blocks_and_wait, invalidate_blocks,
	is_informational_event, open_channel, premine_and_distribute_funds, random_config,
	setup_bitcoind_and_electrsd, setup_node, wait_for_outpoint_spend, wait_for_tx, TestChainSource,
};

proptest! {
	#![proptest_config(proptest::test_runner::Config::with_cases(5))]
	#[test]
	fn reorg_test(reorg_depth in 1..=6usize, force_close in prop::bool::ANY) {
		let rt = tokio::runtime::Builder::new_multi_thread()
			.enable_all()
			.build()
			.unwrap();
		rt.block_on(async {
			let (bitcoind, electrsd) = setup_bitcoind_and_electrsd();

			let chain_source_bitcoind = TestChainSource::BitcoindRpcSync(&bitcoind);
			let chain_source_electrsd = TestChainSource::Electrum(&electrsd);
			let chain_source_esplora = TestChainSource::Esplora(&electrsd);

			macro_rules! config_node {
				($chain_source: expr, $anchor_channels: expr) => {{
					let config_a = random_config($anchor_channels);
					let node = setup_node(&$chain_source, config_a, None);
					node
				}};
			}
			let anchor_channels = true;
			let nodes = vec![
				config_node!(chain_source_electrsd, anchor_channels),
				config_node!(chain_source_bitcoind, anchor_channels),
				config_node!(chain_source_esplora, anchor_channels),
			];

			let (bitcoind, electrs) = (&bitcoind.client, &electrsd.client);
			macro_rules! reorg {
				($reorg_depth: expr) => {{
					invalidate_blocks(bitcoind, $reorg_depth);
					generate_blocks_and_wait(bitcoind, electrs, $reorg_depth).await;
				}};
			}

			let amount_sat = 2_100_000;
			let addr_nodes =
				nodes.iter().map(|node| node.onchain_payment().new_address().unwrap()).collect::<Vec<_>>();
			premine_and_distribute_funds(bitcoind, electrs, addr_nodes, Amount::from_sat(amount_sat)).await;

			macro_rules! sync_wallets {
				() => {
					for node in &nodes {
						node.sync_wallets().unwrap();
					}
				};
			}
			sync_wallets!();
			nodes.iter().for_each(|node| {
				assert_eq!(node.list_balances().spendable_onchain_balance_sats, amount_sat);
				assert_eq!(node.list_balances().total_onchain_balance_sats, amount_sat);
			});


			let mut nodes_funding_tx = HashMap::new();
			let funding_amount_sat = 2_000_000;
			for (node, next_node) in nodes.iter().zip(nodes.iter().cycle().skip(1)) {
				let funding_txo = open_channel(node, next_node, funding_amount_sat, true, &electrsd).await;
				nodes_funding_tx.insert(node.node_id(), funding_txo);
			}

			generate_blocks_and_wait(bitcoind, electrs, 6).await;
			sync_wallets!();

			reorg!(reorg_depth);
			sync_wallets!();

			macro_rules! collect_channel_ready_events {
				($node:expr, $expected:expr) => {{
					let mut user_channels = HashMap::new();
					while user_channels.len() < $expected {
						match $node.next_event_async().await {
							Event::ChannelReady { user_channel_id, counterparty_node_id, .. } => {
								$node.event_handled().unwrap();
								user_channels.insert(counterparty_node_id, user_channel_id);
							},
							ref event if is_informational_event(event) => {
								$node.event_handled().unwrap();
							},
							other => panic!("Unexpected event: {:?}", other),
						}
					}
					user_channels
				}};
			}

			let mut node_channels_id = HashMap::new();
			for (i, node) in nodes.iter().enumerate() {
				assert_eq!(
					node
						.list_payments_with_filter(|p| p.direction == PaymentDirection::Outbound
							&& matches!(p.kind, PaymentKind::Onchain { .. }))
						.len(),
					1
				);

				let user_channels = collect_channel_ready_events!(node, 2);
				let next_node = nodes.get((i + 1) % nodes.len()).unwrap();
				let prev_node = nodes.get((i + nodes.len() - 1) % nodes.len()).unwrap();

				assert!(user_channels.get(&Some(next_node.node_id())) != None);
				assert!(user_channels.get(&Some(prev_node.node_id())) != None);

				let user_channel_id =
					user_channels.get(&Some(next_node.node_id())).expect("Missing user channel for node");
				node_channels_id.insert(node.node_id(), *user_channel_id);
			}


			for (node, next_node) in nodes.iter().zip(nodes.iter().cycle().skip(1)) {
				let user_channel_id = node_channels_id.get(&node.node_id()).expect("user channel id not exist");
				let funding = nodes_funding_tx.get(&node.node_id()).expect("Funding tx not exist");

				if force_close {
					node.force_close_channel(&user_channel_id, next_node.node_id(), None).unwrap();
				} else {
					node.close_channel(&user_channel_id, next_node.node_id()).unwrap();
				}

				expect_channel_closed(node, !force_close && reorg_depth < 6).await;
				expect_channel_closed(next_node, !force_close && reorg_depth < 6).await;

				wait_for_outpoint_spend(electrs, *funding).await;
			}

			reorg!(reorg_depth);
			sync_wallets!();

			generate_blocks_and_wait(bitcoind, electrs, 1).await;
			sync_wallets!();

			// Removing the funding block can force-close a channel even when cooperative
			// closure was requested. Settle the actual balances on every node.
			for node in &nodes {
				settle_closed_channel_balances(node, bitcoind, electrs).await;
			}

			generate_blocks_and_wait(bitcoind, electrs, 6).await;
			sync_wallets!();

			reorg!(reorg_depth);
			sync_wallets!();

			let fee_sat = 7000;
			// Check balance after close channel
			nodes.iter().for_each(|node| {
				assert!(node.list_balances().spendable_onchain_balance_sats > amount_sat - fee_sat);
				assert!(node.list_balances().spendable_onchain_balance_sats < amount_sat);

				assert_eq!(node.list_balances().total_anchor_channels_reserve_sats, 0);
				assert!(node.list_balances().lightning_balances.is_empty());

				while let Some(event) = node.next_event() {
					assert!(is_informational_event(&event), "Unexpected event: {:?}", event);
					node.event_handled().unwrap();
				}
				});
		})
	}
}

// Wait for actual CSV maturities and sweep broadcasts without skipping the property's final checks.
async fn settle_closed_channel_balances(
	node: &Node, bitcoind: &BitcoindClient, electrs: &impl ElectrumApi,
) {
	node.sync_wallets().unwrap();
	let balances = node.list_balances();
	let current_height = node.status().current_best_block.height;
	let mut confirmation_height = current_height;
	let mut channels = HashSet::new();
	for balance in balances.lightning_balances {
		match balance {
			LightningBalance::ClaimableAwaitingConfirmations {
				channel_id,
				amount_satoshis,
				confirmation_height: height,
				source,
				..
			} => {
				confirmation_height = confirmation_height.max(height);
				if amount_satoshis > 0 && source != BalanceSource::CoopClose {
					channels.insert(channel_id);
				}
			},
			other => panic!("Unexpected closed-channel balance: {:?}", other),
		}
	}
	for balance in balances.pending_balances_from_channel_closures {
		let (channel_id, amount_satoshis) = match balance {
			PendingSweepBalance::PendingBroadcast { channel_id, amount_satoshis }
			| PendingSweepBalance::BroadcastAwaitingConfirmation {
				channel_id,
				amount_satoshis,
				..
			}
			| PendingSweepBalance::AwaitingThresholdConfirmations {
				channel_id,
				amount_satoshis,
				..
			} => (channel_id, amount_satoshis),
		};
		if amount_satoshis > 0 {
			channels.insert(channel_id.expect("Sweep must belong to a test channel"));
		}
	}
	if confirmation_height > current_height {
		generate_blocks_and_wait(
			bitcoind,
			electrs,
			(confirmation_height - current_height) as usize,
		)
		.await;
		node.sync_wallets().unwrap();
	}

	if channels.is_empty() {
		return;
	}

	let sweeps = exponential_backoff_poll(|| {
		let balances = node.list_balances();
		channels
			.iter()
			.map(|channel| {
				balances.pending_balances_from_channel_closures.iter().find_map(|balance| {
					match balance {
						PendingSweepBalance::BroadcastAwaitingConfirmation {
							channel_id: Some(id),
							latest_spending_txid,
							..
						} if id == channel => Some(Some(*latest_spending_txid)),
						PendingSweepBalance::AwaitingThresholdConfirmations {
							channel_id: Some(id),
							..
						} if id == channel => Some(None),
						_ => None,
					}
				})
			})
			.collect::<Option<Vec<_>>>()
	})
	.await;
	for txid in sweeps.iter().flatten() {
		wait_for_tx(electrs, *txid).await;
	}
	if sweeps.iter().any(Option::is_some) {
		generate_blocks_and_wait(bitcoind, electrs, 1).await;
		node.sync_wallets().unwrap();
	}
	let balances = node.list_balances();
	for channel in channels {
		assert!(balances.pending_balances_from_channel_closures.iter().any(|balance| matches!(
			balance,
			PendingSweepBalance::AwaitingThresholdConfirmations { channel_id: Some(id), .. } if *id == channel
		)), "Sweep must be confirmed before the final reorganization");
	}
}

// A shallow cooperative case must not pass by silently settling an unexpected force-close.
async fn expect_channel_closed(node: &Node, require_cooperative: bool) {
	if !require_cooperative {
		expect_event!(node, ChannelClosed);
		return;
	}
	loop {
		match node.next_event_async().await {
			Event::ChannelClosed { reason, .. } => {
				assert!(
					matches!(
						reason,
						Some(
							ClosureReason::LocallyInitiatedCooperativeClosure
								| ClosureReason::CounterpartyInitiatedCooperativeClosure
						)
					),
					"Unexpected closure reason: {:?}",
					reason
				);
				node.event_handled().unwrap();
				return;
			},
			ref event if is_informational_event(event) => node.event_handled().unwrap(),
			other => panic!("Unexpected event: {:?}", other),
		}
	}
}

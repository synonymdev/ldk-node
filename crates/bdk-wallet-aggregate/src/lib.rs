// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! A library that aggregates multiple BDK wallets into a single logical wallet
//! with unified balance, UTXO management, transaction building, and signing.
//!
//! # Overview
//!
//! [`AggregateWallet`] wraps a *primary* BDK wallet and zero or more
//! *secondary* wallets, keyed by a user-defined type `K` (e.g. an address-type
//! enum).  The primary wallet is used for generating new addresses and change
//! outputs; secondary wallets are monitored for existing funds and their UTXOs
//! participate in transaction construction and signing.

pub mod rbf;
pub mod signing;
pub mod types;
pub mod utxo;

use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::hash::Hash;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use bdk_chain::local_chain::CheckPoint;
use bdk_chain::spk_client::{FullScanRequest, SyncRequest};
use bdk_chain::{BlockId, ChainPosition};
use bdk_wallet::event::WalletEvent;
use bdk_wallet::{
	AddressInfo, Balance, KeychainKind, LocalOutput, PersistedWallet, Update, WalletPersister,
};
use bitcoin::blockdata::locktime::absolute::LockTime;
use bitcoin::hashes::Hash as _;
use bitcoin::psbt::Psbt;
use bitcoin::{
	Address, Amount, Block, BlockHash, FeeRate, OutPoint, Script, ScriptBuf, Transaction, Txid,
	WPubkeyHash,
};
pub use types::{CoinSelectionAlgorithm, Error, UtxoPsbtInfo};

const BIP32_MAX_NORMAL_INDEX: u32 = (1 << 31) - 1;

/// Maximum number of address metadata entries returned by a single batch derivation call.
pub const MAX_ADDRESS_INFO_BATCH_COUNT: u32 = 10_000;

fn validate_derivation_index(index: u32) -> Result<(), Error> {
	if index > BIP32_MAX_NORMAL_INDEX {
		return Err(Error::InvalidQuantity);
	}
	Ok(())
}

fn validate_derivation_range(start_index: u32, count: u32) -> Result<(), Error> {
	validate_derivation_index(start_index)?;
	if count > MAX_ADDRESS_INFO_BATCH_COUNT {
		return Err(Error::InvalidQuantity);
	}
	if count == 0 {
		return Ok(());
	}

	let last_index = start_index.checked_add(count - 1).ok_or(Error::InvalidQuantity)?;
	validate_derivation_index(last_index)
}

/// A wallet aggregator that presents multiple BDK wallets as a single logical
/// wallet.
///
/// Generic over:
/// * `K` – the key type used to identify individual wallets (e.g. an
///   `AddressType` enum).
/// * `P` – the BDK `WalletPersister` implementation.
pub struct AggregateWallet<K, P>
where
	K: Eq + Hash + Copy + Debug,
	P: WalletPersister,
{
	wallets: HashMap<K, PersistedWallet<P>>,
	persisters: HashMap<K, P>,
	primary: K,
}

/// Delegates to the primary wallet.
impl<K, P> Deref for AggregateWallet<K, P>
where
	K: Eq + Hash + Copy + Debug,
	P: WalletPersister,
{
	type Target = PersistedWallet<P>;

	fn deref(&self) -> &Self::Target {
		self.wallets.get(&self.primary).expect("Primary wallet must always exist")
	}
}

impl<K, P> DerefMut for AggregateWallet<K, P>
where
	K: Eq + Hash + Copy + Debug,
	P: WalletPersister,
{
	fn deref_mut(&mut self) -> &mut Self::Target {
		self.wallets.get_mut(&self.primary).expect("Primary wallet must always exist")
	}
}

impl<K, P> AggregateWallet<K, P>
where
	K: Eq + Hash + Copy + Debug,
	P: WalletPersister,
	P::Error: std::fmt::Display,
{
	/// Create a new aggregate wallet.
	///
	/// * `primary_wallet` / `primary_persister` – the wallet used for new
	///   address generation and change outputs.
	/// * `primary_key` – the key identifying the primary wallet.
	/// * `additional_wallets` – secondary wallets to monitor and use for
	///   transaction construction.
	pub fn new(
		primary_wallet: PersistedWallet<P>, primary_persister: P, primary_key: K,
		additional_wallets: Vec<(K, PersistedWallet<P>, P)>,
	) -> Self {
		let mut wallets = HashMap::new();
		let mut persisters = HashMap::new();

		wallets.insert(primary_key, primary_wallet);
		persisters.insert(primary_key, primary_persister);

		for (key, wallet, persister) in additional_wallets {
			debug_assert!(
				!wallets.contains_key(&key),
				"duplicate wallet key in AggregateWallet::new"
			);
			wallets.insert(key, wallet);
			persisters.insert(key, persister);
		}

		Self { wallets, persisters, primary: primary_key }
	}

	// ─── Accessors ──────────────────────────────────────────────────────

	/// The primary wallet key.
	pub fn primary_key(&self) -> K {
		self.primary
	}

	/// All loaded wallet keys (primary + monitored).
	pub fn loaded_keys(&self) -> Vec<K> {
		self.wallets.keys().copied().collect()
	}

	/// Immutable access to the underlying wallet map.
	pub fn wallets(&self) -> &HashMap<K, PersistedWallet<P>> {
		&self.wallets
	}

	/// Mutable access to the underlying wallet map.
	pub fn wallets_mut(&mut self) -> &mut HashMap<K, PersistedWallet<P>> {
		&mut self.wallets
	}

	/// Immutable access to the persister map.
	pub fn persisters(&self) -> &HashMap<K, P> {
		&self.persisters
	}

	/// Mutable access to the underlying persister map.
	pub fn persisters_mut(&mut self) -> &mut HashMap<K, P> {
		&mut self.persisters
	}

	/// Get a reference to a specific wallet.
	pub fn wallet(&self, key: &K) -> Option<&PersistedWallet<P>> {
		self.wallets.get(key)
	}

	/// Get a mutable reference to a specific wallet.
	pub fn wallet_mut(&mut self, key: &K) -> Option<&mut PersistedWallet<P>> {
		self.wallets.get_mut(key)
	}

	/// Get a reference to the primary wallet.
	pub fn primary_wallet(&self) -> &PersistedWallet<P> {
		self.wallets.get(&self.primary).expect("Primary wallet must always exist")
	}

	/// Get a mutable reference to the primary wallet.
	pub fn primary_wallet_mut(&mut self) -> &mut PersistedWallet<P> {
		self.wallets.get_mut(&self.primary).expect("Primary wallet must always exist")
	}

	/// Get a mutable reference to the primary persister.
	pub fn primary_persister_mut(&mut self) -> &mut P {
		self.persisters.get_mut(&self.primary).expect("Primary persister must always exist")
	}

	/// Add a secondary wallet. Returns `Err(WalletAlreadyExists)` if key exists.
	pub fn add_wallet(
		&mut self, key: K, wallet: PersistedWallet<P>, persister: P,
	) -> Result<(), Error> {
		if self.wallets.contains_key(&key) {
			return Err(Error::WalletAlreadyExists);
		}
		self.wallets.insert(key, wallet);
		self.persisters.insert(key, persister);
		Ok(())
	}

	/// Add a secondary wallet and roll it back if persistence fails.
	pub fn add_wallet_and_persist(
		&mut self, key: K, wallet: PersistedWallet<P>, persister: P,
	) -> Result<(), Error> {
		self.add_wallet(key, wallet, persister)?;
		if let Err(e) = self.persist_all() {
			self.remove_wallet(key).expect("newly added wallet must be removable");
			return Err(e);
		}
		Ok(())
	}

	/// Change the primary wallet. New primary must already be in the aggregate.
	pub fn set_primary(&mut self, new_primary: K) -> Result<(), Error> {
		if !self.wallets.contains_key(&new_primary) {
			return Err(Error::WalletNotFound);
		}
		self.primary = new_primary;
		Ok(())
	}

	/// Remove a secondary wallet. Returns `Err` if key is primary or not found.
	pub fn remove_wallet(&mut self, key: K) -> Result<(), Error> {
		if key == self.primary {
			return Err(Error::CannotRemovePrimary);
		}
		if self.wallets.remove(&key).is_none() {
			return Err(Error::WalletNotFound);
		}
		self.persisters.remove(&key).expect("persister must exist if wallet existed");
		Ok(())
	}

	// ─── Balance ────────────────────────────────────────────────────────

	/// Aggregate balance across all wallets.
	pub fn balance(&self) -> Balance {
		let mut total = Balance::default();
		for wallet in self.wallets.values() {
			let balance = wallet.balance();
			total.confirmed += balance.confirmed;
			total.trusted_pending += balance.trusted_pending;
			total.untrusted_pending += balance.untrusted_pending;
			total.immature += balance.immature;
		}
		total
	}

	/// Balance for a single wallet identified by key.
	pub fn balance_for(&self, key: &K) -> Result<Balance, Error> {
		self.wallets.get(key).map(|w| w.balance()).ok_or(Error::WalletNotFound)
	}

	// ─── UTXO Listing ───────────────────────────────────────────────────

	/// List all unspent outputs across every wallet.
	pub fn list_unspent(&self) -> Vec<LocalOutput> {
		self.wallets.values().flat_map(|w| w.list_unspent()).collect()
	}

	/// List confirmed unspent outputs across all wallets.
	///
	/// Only returns UTXOs whose creating transaction is confirmed in at
	/// least one wallet.
	pub fn list_confirmed_unspent(&self) -> Vec<LocalOutput> {
		let mut confirmed_txids = HashSet::new();
		for wallet in self.wallets.values() {
			for t in wallet.transactions().filter(|t| t.chain_position.is_confirmed()) {
				confirmed_txids.insert(t.tx_node.txid);
			}
		}

		self.list_unspent()
			.into_iter()
			.filter(|u| confirmed_txids.contains(&u.outpoint.txid))
			.collect()
	}

	/// Derive the inner `WPubkeyHash` for a P2SH-wrapped P2WPKH UTXO.
	///
	/// For NestedSegwit wallets (BIP-49) the descriptor is `Sh(Wpkh(...))`.
	/// This method finds the wallet that owns `utxo`, derives the script at
	/// the UTXO's derivation index, and extracts the 20-byte witness
	/// program hash from the inner redeemScript (`OP_0 <20-byte-wpkh>`).
	///
	/// Returns `None` if the UTXO is not owned by any wallet or if the
	/// inner script cannot be parsed as P2WPKH.
	pub fn derive_wpkh_for_p2sh(&self, utxo: &LocalOutput) -> Option<WPubkeyHash> {
		for wallet in self.wallets.values() {
			if wallet.get_utxo(utxo.outpoint).is_some() {
				let descriptor = wallet.public_descriptor(utxo.keychain);
				if let Ok(derived_desc) = descriptor.at_derivation_index(utxo.derivation_index) {
					// For Sh(Wpkh(..)) descriptors, `explicit_script()` gives
					// the inner P2WPKH redeemScript: OP_0 <20-byte-wpkh>.
					if let Ok(explicit) = derived_desc.explicit_script() {
						if explicit.len() == 22
							&& explicit.as_bytes()[0] == 0x00
							&& explicit.as_bytes()[1] == 0x14
						{
							return WPubkeyHash::from_slice(&explicit.as_bytes()[2..22]).ok();
						}
					}
				}
				break;
			}
		}
		None
	}

	// ─── Transaction Lookup ─────────────────────────────────────────────

	/// Find which wallet key contains a transaction.
	pub fn find_wallet_for_tx(&self, txid: Txid) -> Option<K> {
		self.wallets.iter().find_map(|(key, wallet)| wallet.get_tx(txid).map(|_| *key))
	}

	/// Find a transaction across all wallets.
	pub fn find_tx(&self, txid: Txid) -> Option<Transaction> {
		for wallet in self.wallets.values() {
			if let Some(tx_node) = wallet.get_tx(txid) {
				return Some((*tx_node.tx_node.tx).clone());
			}
		}
		None
	}

	/// Check whether a transaction is confirmed in any wallet.
	pub fn is_tx_confirmed(&self, txid: &Txid) -> bool {
		for wallet in self.wallets.values() {
			if let Some(tx_node) = wallet.get_tx(*txid) {
				if tx_node.chain_position.is_confirmed() {
					return true;
				}
			}
		}
		false
	}

	/// Aggregated sent and received amounts for a transaction across all wallets.
	pub fn sent_and_received(&self, txid: Txid) -> Option<(u64, u64)> {
		let tx = self.find_tx(txid)?;
		let mut total_sent = 0u64;
		let mut total_received = 0u64;
		for wallet in self.wallets.values() {
			if wallet.get_tx(txid).is_some() {
				let (sent, received) = wallet.sent_and_received(&tx);
				total_sent += sent.to_sat();
				total_received += received.to_sat();
			}
		}
		Some((total_sent, total_received))
	}

	/// Collect all cached transactions from all wallets, deduplicated by txid.
	pub fn cached_txs(&self) -> Vec<Arc<Transaction>> {
		let mut seen = HashSet::new();
		self.wallets
			.values()
			.flat_map(|w| w.tx_graph().full_txs())
			.filter(|tx_node| seen.insert(tx_node.txid))
			.map(|tx_node| tx_node.tx)
			.collect()
	}

	/// Collect all unconfirmed transaction IDs across wallets (deduplicated).
	pub fn unconfirmed_txids(&self) -> Vec<Txid> {
		let mut seen = HashSet::new();
		self.wallets
			.values()
			.flat_map(|w| {
				w.transactions()
					.filter(|t| t.chain_position.is_unconfirmed())
					.map(|t| t.tx_node.txid)
			})
			.filter(|txid| seen.insert(*txid))
			.collect()
	}

	/// Collect unconfirmed transaction IDs and their latest observed timestamps.
	pub fn unconfirmed_txids_with_last_seen(&self) -> Vec<(Txid, u64)> {
		let mut last_seen_by_txid = HashMap::new();
		for wallet in self.wallets.values() {
			for wallet_tx in wallet.transactions() {
				if let ChainPosition::Unconfirmed { last_seen, .. } = wallet_tx.chain_position {
					let last_seen = last_seen.unwrap_or(0);
					last_seen_by_txid
						.entry(wallet_tx.tx_node.txid)
						.and_modify(|seen: &mut u64| *seen = (*seen).max(last_seen))
						.or_insert(last_seen);
				}
			}
		}
		last_seen_by_txid.into_iter().collect()
	}

	/// All transaction IDs across all wallets.
	pub fn all_txids(&self) -> Vec<Txid> {
		self.wallets.values().flat_map(|w| w.transactions().map(|wtx| wtx.tx_node.txid)).collect()
	}

	// ─── Chain Tip ──────────────────────────────────────────────────────

	/// Tips of every loaded wallet as `(block_hash, height)`.
	///
	/// Bitcoind catch-up should start from each tip that is behind or on a stale fork relative to
	/// the canonical shared tip, rather than assuming a single aggregate cursor is sufficient.
	pub fn chain_tips(&self) -> Vec<(BlockHash, u32)> {
		self.chain_tips_by_key().into_iter().map(|(_, hash, height)| (hash, height)).collect()
	}

	/// Tips of every loaded wallet paired with their key.
	pub fn chain_tips_by_key(&self) -> Vec<(K, BlockHash, u32)> {
		self.wallets
			.iter()
			.map(|(key, wallet)| {
				let checkpoint = wallet.latest_checkpoint();
				(*key, checkpoint.hash(), checkpoint.height())
			})
			.collect()
	}

	/// Reconcile one wallet at `fork_point` and scan every replacement block.
	///
	/// The replacement branch must be contiguous and reach the wallet's previous tip height. This
	/// keeps BDK's chain update unambiguous and ensures transactions in intermediate replacement
	/// blocks are indexed before the new tip is persisted.
	pub fn apply_reorg_blocks_to(
		&mut self, key: &K, fork_point: BlockId, blocks: &[(Block, u32)],
	) -> Result<HashSet<Txid>, Error> {
		let wallet = self.wallets.get_mut(key).ok_or(Error::WalletNotFound)?;
		let original_tip = wallet.latest_checkpoint();
		if original_tip.height() < fork_point.height {
			log::error!(
				"Cannot reconcile wallet {:?} from fork point {} above tip {}",
				key,
				fork_point.height,
				original_tip.height()
			);
			return Err(Error::WalletOperationFailed);
		}

		let replacement_tip_height =
			blocks.last().map(|(_, height)| *height).unwrap_or(fork_point.height);
		if replacement_tip_height < original_tip.height() {
			log::error!(
				"Cannot reconcile wallet {:?}: replacement tip {} is below wallet tip {}",
				key,
				replacement_tip_height,
				original_tip.height()
			);
			return Err(Error::WalletOperationFailed);
		}

		let mut update_ids: Vec<BlockId> = original_tip
			.iter()
			.filter(|checkpoint| checkpoint.height() <= fork_point.height)
			.map(|checkpoint| checkpoint.block_id())
			.collect();
		update_ids.reverse();

		match update_ids.last() {
			Some(checkpoint) if checkpoint.height == fork_point.height => {
				if checkpoint.hash != fork_point.hash {
					log::error!(
						"Wallet {:?} disagrees with fork point {} at height {}",
						key,
						fork_point.hash,
						fork_point.height
					);
					return Err(Error::WalletOperationFailed);
				}
			},
			Some(_) => update_ids.push(fork_point),
			None => return Err(Error::WalletOperationFailed),
		}

		let mut expected_height =
			fork_point.height.checked_add(1).ok_or(Error::WalletOperationFailed)?;
		let mut previous_hash = fork_point.hash;
		for (block, height) in blocks {
			if *height != expected_height || block.header.prev_blockhash != previous_hash {
				log::error!(
					"Non-contiguous replacement block {} at height {} for wallet {:?}",
					block.block_hash(),
					height,
					key
				);
				return Err(Error::WalletOperationFailed);
			}
			previous_hash = block.block_hash();
			update_ids.push(BlockId { height: *height, hash: previous_hash });
			expected_height = expected_height.checked_add(1).ok_or(Error::WalletOperationFailed)?;
		}

		let chain_update = CheckPoint::from_block_ids(update_ids).map_err(|_| {
			log::error!("Failed to construct replacement checkpoint chain for wallet {:?}", key);
			Error::WalletOperationFailed
		})?;
		wallet.apply_update(Update { chain: Some(chain_update), ..Default::default() }).map_err(
			|e| {
				log::error!("Failed to apply replacement chain to wallet {:?}: {}", key, e);
				Error::WalletOperationFailed
			},
		)?;

		for (block, height) in blocks {
			wallet.apply_block(block, *height).map_err(|e| {
				log::error!(
					"Failed to scan replacement block {} at height {} for wallet {:?}: {}",
					block.block_hash(),
					height,
					key,
					e
				);
				Error::WalletOperationFailed
			})?;
		}

		if let Some(persister) = self.persisters.get_mut(key) {
			wallet.persist(persister).map_err(|e| {
				log::error!("Failed to persist wallet {:?} after reorg: {}", key, e);
				Error::PersistenceFailed
			})?;
		}

		Ok(wallet.transactions().map(|wtx| wtx.tx_node.txid).collect())
	}

	/// Apply a connected block to a single wallet, then persist.
	///
	/// Same skip rules as [`Self::apply_block`], scoped to `key`.
	pub fn apply_block_to(
		&mut self, key: &K, block: &Block, height: u32,
	) -> Result<HashSet<Txid>, Error> {
		let block_hash = block.block_hash();
		let wallet = self.wallets.get_mut(key).ok_or(Error::WalletNotFound)?;
		let tip = wallet.latest_checkpoint();
		let checkpoint_at_height = tip.get(height);
		let sparse_ahead = checkpoint_at_height.is_none() && tip.height() > height;
		if !sparse_ahead {
			wallet.apply_block(block, height).map_err(|e| {
				log::error!(
					"Failed to apply connected block {} at height {} to wallet {:?} (tip {}): {}",
					block_hash,
					height,
					key,
					tip.height(),
					e
				);
				Error::WalletOperationFailed
			})?;
			if let Some(persister) = self.persisters.get_mut(key) {
				wallet.persist(persister).map_err(|e| {
					log::error!("Failed to persist wallet {:?}: {}", key, e);
					Error::PersistenceFailed
				})?;
			}
		}

		Ok(wallet.transactions().map(|wtx| wtx.tx_node.txid).collect())
	}

	/// A Listen-oriented aggregate tip: the oldest height across wallets.
	///
	/// When multiple wallets share that minimum height but disagree on the hash (fork), pick the
	/// lexicographically smallest hash. Preferring the primary here would hide stale secondary
	/// tips from Bitcoind catch-up when heights match after an equal-height reorg.
	///
	/// Callers that need to reconcile every loaded wallet (including same-height forks) should use
	/// [`Self::chain_tips`] and sync from each divergent tip.
	pub fn current_best_block(&self) -> (BlockHash, u32) {
		self.wallets
			.values()
			.map(|wallet| wallet.latest_checkpoint())
			.min_by(|a, b| {
				a.height()
					.cmp(&b.height())
					.then_with(|| a.hash().to_byte_array().cmp(&b.hash().to_byte_array()))
			})
			.map(|checkpoint| (checkpoint.hash(), checkpoint.height()))
			.unwrap_or_else(|| {
				let checkpoint = self.primary_wallet().latest_checkpoint();
				(checkpoint.hash(), checkpoint.height())
			})
	}

	// ─── Address Generation ─────────────────────────────────────────────

	/// Generate a new receiving address from the primary wallet.
	pub fn new_address(&mut self) -> Result<Address, Error> {
		self.new_address_info().map(|address_info| address_info.address)
	}

	/// Generate a new receiving address with derivation metadata from the primary wallet.
	pub fn new_address_info(&mut self) -> Result<AddressInfo, Error> {
		let key = self.primary;
		self.new_address_info_for(&key)
	}

	/// Generate a new receiving address for a specific wallet.
	pub fn new_address_for(&mut self, key: &K) -> Result<Address, Error> {
		self.new_address_info_for(key).map(|address_info| address_info.address)
	}

	/// Generate a new receiving address with derivation metadata for a specific wallet.
	pub fn new_address_info_for(&mut self, key: &K) -> Result<AddressInfo, Error> {
		let wallet = self.wallets.get_mut(key).ok_or(Error::WalletNotFound)?;
		let persister = self.persisters.get_mut(key).ok_or(Error::PersisterNotFound)?;

		let address_info = wallet.reveal_next_address(KeychainKind::External);
		wallet.persist(persister).map_err(|e| {
			log::error!("Failed to persist wallet for {:?}: {}", key, e);
			Error::PersistenceFailed
		})?;
		Ok(address_info)
	}

	/// Return address metadata at a derivation index without revealing it.
	pub fn address_info_for(
		&self, key: &K, keychain: KeychainKind, index: u32,
	) -> Result<AddressInfo, Error> {
		validate_derivation_index(index)?;

		let wallet = self.wallets.get(key).ok_or(Error::WalletNotFound)?;
		Ok(wallet.peek_address(keychain, index))
	}

	/// Return address metadata for a range of derivation indexes without revealing them.
	pub fn address_infos_for(
		&self, key: &K, keychain: KeychainKind, start_index: u32, count: u32,
	) -> Result<Vec<AddressInfo>, Error> {
		validate_derivation_range(start_index, count)?;

		let wallet = self.wallets.get(key).ok_or(Error::WalletNotFound)?;
		let end_index = start_index.checked_add(count).ok_or(Error::InvalidQuantity)?;
		Ok((start_index..end_index).map(|index| wallet.peek_address(keychain, index)).collect())
	}

	/// Reveal external receiving addresses through `index` for a specific wallet and persist.
	pub fn reveal_addresses_to(&mut self, key: &K, index: u32) -> Result<(), Error> {
		validate_derivation_index(index)?;

		let wallet = self.wallets.get_mut(key).ok_or(Error::WalletNotFound)?;
		let persister = self.persisters.get_mut(key).ok_or(Error::PersisterNotFound)?;

		wallet.reveal_addresses_to(KeychainKind::External, index).count();
		wallet.persist(persister).map_err(|e| {
			log::error!("Failed to persist wallet for {:?}: {}", key, e);
			Error::PersistenceFailed
		})?;
		Ok(())
	}

	/// Generate a new internal (change) address from the primary wallet.
	pub fn new_internal_address(&mut self) -> Result<Address, Error> {
		let primary = self.primary;
		let wallet = self.wallets.get_mut(&primary).ok_or(Error::WalletNotFound)?;
		let persister = self.persisters.get_mut(&primary).ok_or(Error::PersisterNotFound)?;

		let address_info = wallet.next_unused_address(KeychainKind::Internal);
		wallet.persist(persister).map_err(|e| {
			log::error!("Failed to persist wallet: {}", e);
			Error::PersistenceFailed
		})?;
		Ok(address_info.address)
	}

	// ─── Transaction Cancellation ───────────────────────────────────────

	/// Cancel a transaction in all wallets that know about it.
	pub fn cancel_tx(&mut self, tx: &Transaction) -> Result<(), Error> {
		for (key, wallet) in self.wallets.iter_mut() {
			wallet.cancel_tx(tx);
			if let Some(persister) = self.persisters.get_mut(key) {
				wallet.persist(persister).map_err(|e| {
					log::error!("Failed to persist wallet {:?}: {}", key, e);
					Error::PersistenceFailed
				})?;
			}
		}
		Ok(())
	}

	/// Cancel a dry-run transaction on the primary wallet without persisting.
	///
	/// Unmarks change addresses that were marked "used" by `finish()`, so
	/// the next `finish()` reuses them instead of revealing new ones. The
	/// reveal itself is left in the staged changeset and persists harmlessly.
	///
	/// Only targets the primary wallet. All current build paths use the primary.
	pub fn cancel_dry_run_tx(&mut self, tx: &Transaction) {
		let primary =
			self.wallets.get_mut(&self.primary).expect("Primary wallet must always exist");
		primary.cancel_tx(tx);
	}

	// ─── Fee Calculation ────────────────────────────────────────────────

	/// Calculate the fee of a PSBT by summing input values and subtracting
	/// output values.
	pub fn calculate_fee_from_psbt(&self, psbt: &Psbt) -> Result<u64, Error> {
		let mut total_input_value = 0u64;

		for (i, txin) in psbt.unsigned_tx.input.iter().enumerate() {
			if let Some(psbt_input) = psbt.inputs.get(i) {
				if let Some(witness_utxo) = &psbt_input.witness_utxo {
					total_input_value += witness_utxo.value.to_sat();
				} else if let Some(non_witness_tx) = &psbt_input.non_witness_utxo {
					if let Some(txout) =
						non_witness_tx.output.get(txin.previous_output.vout as usize)
					{
						total_input_value += txout.value.to_sat();
					} else {
						return Err(Error::OnchainTxCreationFailed);
					}
				} else {
					let mut found = false;
					for wallet in self.wallets.values() {
						if let Some(local_utxo) = wallet.get_utxo(txin.previous_output) {
							total_input_value += local_utxo.txout.value.to_sat();
							found = true;
							break;
						}
					}
					if !found {
						return Err(Error::OnchainTxCreationFailed);
					}
				}
			} else {
				let mut found = false;
				for wallet in self.wallets.values() {
					if let Some(local_utxo) = wallet.get_utxo(txin.previous_output) {
						total_input_value += local_utxo.txout.value.to_sat();
						found = true;
						break;
					}
				}
				if !found {
					return Err(Error::OnchainTxCreationFailed);
				}
			}
		}

		let total_output_value: u64 =
			psbt.unsigned_tx.output.iter().map(|txout| txout.value.to_sat()).sum();

		Ok(total_input_value.saturating_sub(total_output_value))
	}

	/// Calculate fee from PSBT with fallback to primary wallet calculation.
	pub fn calculate_fee_with_fallback(&self, psbt: &Psbt) -> Result<u64, Error> {
		self.calculate_fee_from_psbt(psbt).or_else(|_| {
			self.primary_wallet()
				.calculate_fee(&psbt.unsigned_tx)
				.map(|f| f.to_sat())
				.map_err(|_| Error::OnchainTxCreationFailed)
		})
	}

	/// Calculate the drain amount from a PSBT using sent_and_received across
	/// all wallets. Returns the net outgoing amount (sent - received) in sats.
	///
	/// For not-yet-broadcast PSBTs the transaction won't be in any wallet's
	/// tx-graph, so we compute sent/received directly from the unsigned tx
	/// against every wallet's script set.
	pub fn drain_amount_from_psbt(&self, psbt: &Psbt) -> u64 {
		let mut total_sent = Amount::ZERO;
		let mut total_received = Amount::ZERO;
		for wallet in self.wallets.values() {
			let (s, r) = wallet.sent_and_received(&psbt.unsigned_tx);
			total_sent += s;
			total_received += r;
		}
		total_sent.to_sat().saturating_sub(total_received.to_sat())
	}

	// ─── Sync ───────────────────────────────────────────────────────────

	/// Build a full scan request for a specific wallet.
	pub fn wallet_full_scan_request(
		&self, key: &K,
	) -> Result<FullScanRequest<KeychainKind>, Error> {
		let wallet = self.wallets.get(key).ok_or(Error::WalletNotFound)?;
		Ok(wallet.start_full_scan().build())
	}

	/// Build an incremental sync request for a specific wallet.
	pub fn wallet_incremental_sync_request(
		&self, key: &K,
	) -> Result<SyncRequest<(KeychainKind, u32)>, Error> {
		let wallet = self.wallets.get(key).ok_or(Error::WalletNotFound)?;
		Ok(wallet.start_sync_with_revealed_spks().build())
	}

	/// Apply a chain update to the primary wallet.
	///
	/// Returns the wallet events and a list of all transaction IDs in the
	/// primary wallet (so the caller can update its payment store).
	pub fn apply_update(
		&mut self, update: impl Into<Update>,
	) -> Result<(Vec<WalletEvent>, Vec<Txid>), Error> {
		let update = update.into();
		let primary = self.primary;
		let wallet = self.wallets.get_mut(&primary).ok_or(Error::WalletNotFound)?;

		let events = wallet.apply_update_events(update).map_err(|e| {
			log::error!("Failed to apply update to primary wallet: {}", e);
			Error::WalletOperationFailed
		})?;

		let persister = self.persisters.get_mut(&primary).ok_or(Error::PersisterNotFound)?;
		wallet.persist(persister).map_err(|e| {
			log::error!("Failed to persist primary wallet: {}", e);
			Error::PersistenceFailed
		})?;

		let txids: Vec<Txid> = wallet.transactions().map(|wtx| wtx.tx_node.txid).collect();

		Ok((events, txids))
	}

	/// Apply a chain update to a specific wallet.
	///
	/// Returns the wallet events and a list of all transaction IDs in that
	/// wallet.
	pub fn apply_update_to_wallet(
		&mut self, key: K, update: impl Into<Update>,
	) -> Result<(Vec<WalletEvent>, Vec<Txid>), Error> {
		let update = update.into();
		let wallet = self.wallets.get_mut(&key).ok_or(Error::WalletNotFound)?;

		let events = wallet.apply_update_events(update).map_err(|e| {
			log::error!("Failed to apply update to wallet {:?}: {}", key, e);
			Error::WalletOperationFailed
		})?;

		let persister = self.persisters.get_mut(&key).ok_or(Error::PersisterNotFound)?;
		wallet.persist(persister).map_err(|e| {
			log::error!("Failed to persist wallet {:?}: {}", key, e);
			Error::PersistenceFailed
		})?;

		let txids: Vec<Txid> = wallet.transactions().map(|wtx| wtx.tx_node.txid).collect();

		Ok((events, txids))
	}

	/// Apply mempool (unconfirmed) transactions to all wallets.
	pub fn apply_mempool_txs(
		&mut self, unconfirmed_txs: Vec<(Transaction, u64)>, evicted_txids: Vec<(Txid, u64)>,
	) -> Result<(), Error> {
		for (key, wallet) in self.wallets.iter_mut() {
			wallet.apply_unconfirmed_txs(unconfirmed_txs.clone());
			wallet.apply_evicted_txs(evicted_txids.clone());

			if let Some(persister) = self.persisters.get_mut(key) {
				wallet.persist(persister).map_err(|e| {
					log::error!("Failed to persist wallet {:?}: {}", key, e);
					Error::PersistenceFailed
				})?;
			}
		}
		Ok(())
	}

	/// Apply a connected block to wallets that still need it, then persist.
	///
	/// Skip rules per wallet:
	/// - Exact hash already present at `height` → scan again and persist. BDK deduplicates indexed
	///   transactions, and this also retries stranded changesets.
	/// - Different hash at `height` → apply (same-height reorg replacement; `blocks_disconnected`
	///   is a no-op in the node wallet, so BDK must see replacements here)
	/// - No checkpoint at `height` but tip height is already above `height` → skip. Brand-new
	///   wallets may only store a sparse `{genesis, tip}` chain; `get(height)` returning `None`
	///   means "unknown", not "different block". Applying historical blocks into that sparse tip
	///   makes BDK reject the update because a higher unmatched checkpoint already exists.
	/// - Otherwise → apply (linear catch-up / extension)
	///
	/// Returns the set of all transaction IDs across all wallets.
	pub fn apply_block(&mut self, block: &Block, height: u32) -> Result<HashSet<Txid>, Error> {
		let block_hash = block.block_hash();
		let pre_checkpoint = self.primary_wallet().latest_checkpoint();
		if height > 0
			&& (pre_checkpoint.height() != height - 1
				|| pre_checkpoint.hash() != block.header.prev_blockhash)
		{
			log::debug!("Detected reorg while applying connected block at height {}", height);
		}

		for (key, wallet) in self.wallets.iter_mut() {
			let tip = wallet.latest_checkpoint();
			let checkpoint_at_height = tip.get(height);
			let sparse_ahead = checkpoint_at_height.is_none() && tip.height() > height;
			if sparse_ahead {
				continue;
			}

			wallet.apply_block(block, height).map_err(|e| {
				log::error!(
					"Failed to apply connected block {} at height {} to wallet {:?} (tip {}): {}",
					block_hash,
					height,
					key,
					tip.height(),
					e
				);
				Error::WalletOperationFailed
			})?;

			if let Some(persister) = self.persisters.get_mut(key) {
				wallet.persist(persister).map_err(|e| {
					log::error!("Failed to persist wallet {:?}: {}", key, e);
					Error::PersistenceFailed
				})?;
			}
		}

		let mut all_txids = HashSet::new();
		for wallet in self.wallets.values() {
			for wtx in wallet.transactions() {
				all_txids.insert(wtx.tx_node.txid);
			}
		}

		Ok(all_txids)
	}

	// ─── UTXO Preparation Helpers ───────────────────────────────────────

	/// Prepare local outputs for cross-wallet PSBT building.
	pub fn prepare_utxos_for_psbt(
		&self, utxos: &[LocalOutput],
	) -> Result<Vec<UtxoPsbtInfo>, Error> {
		utxo::prepare_utxos_for_psbt(utxos, &self.wallets, &self.primary)
	}

	/// Prepare outpoints for cross-wallet PSBT building.
	pub fn prepare_outpoints_for_psbt(
		&self, outpoints: &[OutPoint],
	) -> Result<Vec<UtxoPsbtInfo>, Error> {
		utxo::prepare_outpoints_for_psbt(outpoints, &self.wallets, &self.primary)
	}

	// ─── Signing ────────────────────────────────────────────────────────

	/// Sign an unsigned transaction using all wallets that own inputs.
	pub fn sign_owned_inputs(&mut self, unsigned_tx: Transaction) -> Result<Transaction, Error> {
		signing::sign_owned_inputs(unsigned_tx, &mut self.wallets)
	}

	/// Sign a PSBT using all wallets that own inputs.
	pub fn sign_psbt_all(&mut self, psbt: Psbt) -> Result<Transaction, Error> {
		signing::sign_psbt_all_wallets(psbt, &mut self.wallets)
	}

	// ─── Cross-wallet RBF ───────────────────────────────────────────────

	/// Build a cross-wallet RBF replacement, adding extra inputs if needed.
	///
	/// 1. Tries adjusting the change output only (no new inputs).
	/// 2. If that is insufficient, selects minimum additional UTXOs from all
	///    wallets and retries.
	fn build_cross_wallet_rbf_with_fallback(
		&mut self, original_tx: &Transaction, new_fee_rate: FeeRate,
	) -> Result<Transaction, Error> {
		// Attempt 1: change-only bump.
		match rbf::build_cross_wallet_rbf(&mut self.wallets, original_tx, new_fee_rate, &[]) {
			Ok(tx) => return Ok(tx),
			Err(Error::InsufficientFunds) => {},
			Err(e) => return Err(e),
		}

		// Attempt 2: select minimum additional UTXOs.
		let original_fee = self.calculate_tx_fee(original_tx)?;
		let new_fee = new_fee_rate.fee_wu(original_tx.weight()).unwrap_or(Amount::ZERO);
		let fee_increase = new_fee.checked_sub(original_fee).unwrap_or(Amount::ZERO);

		// Change value absorbable from the original tx.
		let change_value: Amount = original_tx
			.output
			.iter()
			.filter(|out| {
				self.wallets.values().any(|w| {
					w.list_unspent().any(|u| {
						u.keychain == KeychainKind::Internal
							&& u.txout.script_pubkey == out.script_pubkey
					})
				})
			})
			.map(|out| out.value)
			.sum();

		let deficit = fee_increase.checked_sub(change_value).unwrap_or(Amount::ZERO);

		// Collect UTXOs from all wallets, excluding those already in the
		// original tx and outputs created by the original tx.
		let original_outpoints: HashSet<OutPoint> =
			original_tx.input.iter().map(|i| i.previous_output).collect();
		let original_txid = original_tx.compute_txid();

		let available: Vec<LocalOutput> = self
			.list_unspent()
			.into_iter()
			.filter(|u| !original_outpoints.contains(&u.outpoint))
			.filter(|u| u.outpoint.txid != original_txid)
			.collect();

		if available.is_empty() {
			return Err(Error::InsufficientFunds);
		}

		let drain_script =
			self.primary_wallet().peek_address(KeychainKind::Internal, 0).address.script_pubkey();

		let selected = utxo::select_utxos_with_algorithm(
			deficit.to_sat(),
			available,
			new_fee_rate,
			CoinSelectionAlgorithm::BranchAndBound,
			&drain_script,
			&[],
			&self.wallets,
		)?;

		let extra_infos = self.prepare_outpoints_for_psbt(&selected)?;
		if extra_infos.is_empty() {
			return Err(Error::InsufficientFunds);
		}

		rbf::build_cross_wallet_rbf(&mut self.wallets, original_tx, new_fee_rate, &extra_infos)
	}

	/// Look up an input value across local wallets.
	pub fn get_input_value(&self, outpoint: &OutPoint) -> Result<u64, Error> {
		rbf::get_input_value(outpoint, &self.wallets)
	}

	// ─── Coin Selection ─────────────────────────────────────────────────

	/// Run coin selection across all wallets.
	pub fn select_utxos(
		&self, target_amount: u64, available_utxos: Vec<LocalOutput>, fee_rate: FeeRate,
		algorithm: CoinSelectionAlgorithm, drain_script: &Script, excluded_outpoints: &[OutPoint],
	) -> Result<Vec<OutPoint>, Error> {
		utxo::select_utxos_with_algorithm(
			target_amount,
			available_utxos,
			fee_rate,
			algorithm,
			drain_script,
			excluded_outpoints,
			&self.wallets,
		)
	}

	// ─── Fee Calculation ─────────────────────────────────────────────────

	/// Calculate the fee of a transaction by looking up input values across
	/// all wallets and subtracting total output values.
	///
	/// Tries BDK's native `calculate_fee` on each wallet first (works
	/// reliably for transactions that were built by that wallet), then
	/// falls back to manual input-value lookup across all wallets.
	pub fn calculate_tx_fee(&self, tx: &Transaction) -> Result<Amount, Error> {
		// Fast path: BDK knows the fee for transactions it built.
		for wallet in self.wallets.values() {
			if let Ok(fee) = wallet.calculate_fee(tx) {
				return Ok(fee);
			}
		}

		// Slow path: manually look up each input value.
		let mut total_input = 0u64;
		for txin in &tx.input {
			total_input += self.get_input_value(&txin.previous_output)?;
		}
		let total_output: u64 = tx.output.iter().map(|o| o.value.to_sat()).sum();
		Ok(Amount::from_sat(total_input.saturating_sub(total_output)))
	}

	// ─── High-Level Helpers ─────────────────────────────────────────────

	/// Return prepared PSBT info for all non-primary UTXOs, suitable for
	/// adding as foreign inputs to a transaction built on the primary wallet.
	///
	/// `excluded_txids` allows the caller to filter out UTXOs belonging to
	/// specific transactions (e.g. channel funding transactions).
	pub fn non_primary_foreign_utxos(
		&self, excluded_txids: &HashSet<Txid>,
	) -> Result<Vec<UtxoPsbtInfo>, Error> {
		let primary_outpoints: HashSet<OutPoint> =
			self.primary_wallet().list_unspent().map(|u| u.outpoint).collect();

		let non_primary: Vec<LocalOutput> = self
			.list_unspent()
			.into_iter()
			.filter(|u| !primary_outpoints.contains(&u.outpoint))
			.filter(|u| !excluded_txids.contains(&u.outpoint.txid))
			.collect();

		if non_primary.is_empty() {
			return Ok(Vec::new());
		}

		self.prepare_utxos_for_psbt(&non_primary)
	}

	/// Select the minimum set of non-primary foreign UTXOs needed to cover
	/// `deficit`, using the given coin selection algorithm.
	///
	/// * `segwit_only` – if `true`, excludes bare P2PKH UTXOs (includes
	///   P2SH-P2WPKH / NestedSegwit since those carry witness data).
	/// * `algorithm` – the coin selection strategy to use.
	///
	/// Returns prepared `UtxoPsbtInfo` entries ready to add as foreign
	/// inputs, or an empty vec if no non-primary UTXOs are available.
	pub fn select_non_primary_foreign_utxos(
		&self, deficit: Amount, fee_rate: FeeRate, excluded_txids: &HashSet<Txid>,
		segwit_only: bool, algorithm: CoinSelectionAlgorithm,
	) -> Result<Vec<UtxoPsbtInfo>, Error> {
		let primary_outpoints: HashSet<OutPoint> =
			self.primary_wallet().list_unspent().map(|u| u.outpoint).collect();

		let non_primary: Vec<LocalOutput> = self
			.list_unspent()
			.into_iter()
			.filter(|u| !primary_outpoints.contains(&u.outpoint))
			.filter(|u| !excluded_txids.contains(&u.outpoint.txid))
			.filter(|u| {
				!segwit_only
					|| u.txout.script_pubkey.witness_version().is_some()
					|| u.txout.script_pubkey.is_p2sh()
			})
			.collect();

		if non_primary.is_empty() {
			return Ok(Vec::new());
		}

		let drain_script =
			self.primary_wallet().peek_address(KeychainKind::Internal, 0).address.script_pubkey();

		let selected_outpoints = utxo::select_utxos_with_algorithm(
			deficit.to_sat(),
			non_primary,
			fee_rate,
			algorithm,
			&drain_script,
			&[],
			&self.wallets,
		)?;

		self.prepare_outpoints_for_psbt(&selected_outpoints)
	}

	/// Build a transaction using unified coin selection across all eligible
	/// wallets.
	///
	/// Pools UTXOs from every wallet that passes `utxo_filter`, runs coin
	/// selection once on the combined set, then builds a PSBT on the
	/// `build_key` wallet with primary UTXOs as native inputs and foreign
	/// UTXOs via `add_foreign_utxo`. Signs with all wallets and persists.
	#[allow(deprecated, clippy::too_many_arguments)]
	fn build_tx_unified(
		&mut self, build_key: &K, output_script: ScriptBuf, amount: Amount, fee_rate: FeeRate,
		locktime: Option<LockTime>, excluded_txids: &HashSet<Txid>,
		algorithm: CoinSelectionAlgorithm, utxo_filter: impl Fn(&LocalOutput) -> bool,
	) -> Result<Psbt, Error> {
		let all_utxos: Vec<LocalOutput> = self
			.list_unspent()
			.into_iter()
			.filter(|u| !excluded_txids.contains(&u.outpoint.txid))
			.filter(&utxo_filter)
			.collect();

		if all_utxos.is_empty() {
			return Err(Error::NoSpendableOutputs);
		}

		let drain_script = self
			.wallet(build_key)
			.ok_or(Error::WalletNotFound)?
			.peek_address(KeychainKind::Internal, 0)
			.address
			.script_pubkey();

		let selected = utxo::select_utxos_with_algorithm(
			amount.to_sat(),
			all_utxos,
			fee_rate,
			algorithm,
			&drain_script,
			&[],
			&self.wallets,
		)?;

		let infos = self.prepare_outpoints_for_psbt(&selected)?;
		if infos.is_empty() {
			return Err(Error::InsufficientFunds);
		}

		let w = self.wallets.get_mut(build_key).ok_or(Error::WalletNotFound)?;
		let mut b = w.build_tx();
		b.add_recipient(output_script, amount).fee_rate(fee_rate);
		if let Some(lt) = locktime {
			b.nlocktime(lt);
		}
		utxo::add_utxos_to_tx_builder(&mut b, &infos)?;
		// BDK must not add extra UTXOs — we already selected everything.
		b.manually_selected_only();
		b.finish().map_err(|e| {
			log::error!("Failed to build tx with unified selection: {}", e);
			Error::InsufficientFunds
		})
	}

	/// Build a channel-funding transaction using unified coin selection
	/// across all wallets. Only SegWit-compatible UTXOs are included
	/// (Legacy/P2PKH excluded) as required by BOLT 2.
	#[allow(deprecated)]
	pub fn build_and_sign_funding_tx(
		&mut self, output_script: ScriptBuf, amount: Amount, fee_rate: FeeRate, locktime: LockTime,
	) -> Result<Transaction, Error> {
		let psbt = self.build_tx_unified(
			&self.primary.clone(),
			output_script,
			amount,
			fee_rate,
			Some(locktime),
			&HashSet::new(),
			CoinSelectionAlgorithm::BranchAndBound,
			|u| {
				u.txout.script_pubkey.witness_version().is_some() || u.txout.script_pubkey.is_p2sh()
			},
		)?;

		let tx = self.sign_psbt_all(psbt)?;
		self.persist_all()?;
		Ok(tx)
	}

	/// Build a channel-funding transaction when the primary wallet is not
	/// eligible (e.g. Legacy). Picks the highest-balance wallet from those
	/// passing `filter` as the PSBT builder, then runs unified coin
	/// selection across all eligible wallets.
	#[allow(deprecated)]
	pub fn build_and_sign_tx_with_best_wallet(
		&mut self, output_script: ScriptBuf, amount: Amount, fee_rate: FeeRate, locktime: LockTime,
		filter: impl Fn(&K) -> bool,
	) -> Result<Transaction, Error> {
		let mut matching_keys: Vec<K> =
			self.wallets.keys().filter(|k| filter(k)).cloned().collect();

		if matching_keys.is_empty() {
			return Err(Error::InsufficientFunds);
		}

		matching_keys.sort_by(|a, b| {
			let bal_a = self
				.balance_for(a)
				.map(|bal| bal.confirmed.to_sat() + bal.trusted_pending.to_sat())
				.unwrap_or(0);
			let bal_b = self
				.balance_for(b)
				.map(|bal| bal.confirmed.to_sat() + bal.trusted_pending.to_sat())
				.unwrap_or(0);
			bal_b.cmp(&bal_a)
		});

		let build_key = matching_keys[0];

		let psbt = self.build_tx_unified(
			&build_key,
			output_script,
			amount,
			fee_rate,
			Some(locktime),
			&HashSet::new(),
			CoinSelectionAlgorithm::BranchAndBound,
			|u| {
				u.txout.script_pubkey.witness_version().is_some() || u.txout.script_pubkey.is_p2sh()
			},
		)?;

		let tx = self.sign_psbt_all(psbt)?;
		self.persist_all()?;
		Ok(tx)
	}

	/// Build a PSBT using unified coin selection across all wallets.
	/// Returns the **unsigned** PSBT — the caller is responsible for
	/// signing and persisting.
	#[allow(deprecated)]
	pub fn build_psbt_with_cross_wallet_fallback(
		&mut self, output_script: ScriptBuf, amount: Amount, fee_rate: FeeRate,
		excluded_txids: &HashSet<Txid>, algorithm: CoinSelectionAlgorithm,
	) -> Result<Psbt, Error> {
		self.build_tx_unified(
			&self.primary.clone(),
			output_script,
			amount,
			fee_rate,
			None,
			excluded_txids,
			algorithm,
			|_| true,
		)
	}

	/// Aggregate balance across wallets whose keys satisfy `filter`.
	pub fn balance_filtered(&self, filter: impl Fn(&K) -> bool) -> Balance {
		let mut total = Balance::default();
		for (key, wallet) in &self.wallets {
			if filter(key) {
				let balance = wallet.balance();
				total.confirmed += balance.confirmed;
				total.trusted_pending += balance.trusted_pending;
				total.untrusted_pending += balance.untrusted_pending;
				total.immature += balance.immature;
			}
		}
		total
	}

	/// Build and sign a transaction that drains ALL wallets (primary +
	/// monitored) to a single destination script.
	///
	/// Uses `drain_wallet()` for the primary wallet and adds non-primary
	/// UTXOs as foreign inputs.  BDK's drain calculation then accounts for
	/// the full input value (primary + foreign) minus fees.
	pub fn build_and_sign_drain(
		&mut self, destination: ScriptBuf, fee_rate: FeeRate,
	) -> Result<Transaction, Error> {
		// Collect non-primary UTXOs first (immutable borrow).
		let non_primary_infos = self.non_primary_foreign_utxos(&HashSet::new())?;

		let primary = self.primary;
		let wallet = self.wallets.get_mut(&primary).ok_or(Error::WalletNotFound)?;

		// Check primary wallet has at least something to drain.
		if wallet.balance().total() == Amount::ZERO && non_primary_infos.is_empty() {
			return Err(Error::NoSpendableOutputs);
		}

		let mut builder = wallet.build_tx();

		// drain_wallet() tells BDK to include all primary UTXOs.
		builder.drain_wallet();
		builder.drain_to(destination);
		builder.fee_rate(fee_rate);

		// Add non-primary UTXOs as foreign inputs.
		for info in &non_primary_infos {
			builder
				.add_foreign_utxo_with_sequence(
					info.outpoint,
					info.psbt_input.clone(),
					info.weight,
					bitcoin::Sequence::ENABLE_RBF_NO_LOCKTIME,
				)
				.map_err(|e| {
					log::error!("Failed to add foreign UTXO {:?} for drain: {}", info.outpoint, e);
					Error::OnchainTxCreationFailed
				})?;
		}

		let psbt = builder.finish().map_err(|e| {
			log::error!("Failed to build drain transaction: {}", e);
			Error::OnchainTxCreationFailed
		})?;

		self.sign_psbt_all(psbt)
	}

	// ─── CPFP ──────────────────────────────────────────────────────────

	/// Build a CPFP (Child-Pays-For-Parent) transaction.
	///
	/// Finds spendable outputs of `parent_txid` across all wallets, builds
	/// a child transaction at `fee_rate` that spends those outputs to
	/// `destination_script`.  If `destination_script` is `None` the
	/// primary wallet's next internal address is used.
	///
	/// Returns `(signed_child_tx, parent_fee, parent_fee_rate)`.
	pub fn build_cpfp(
		&mut self, parent_txid: Txid, fee_rate: FeeRate, destination_script: Option<ScriptBuf>,
	) -> Result<(Transaction, Amount, FeeRate), Error> {
		// Find and validate the parent transaction.
		let parent_tx = self.find_tx(parent_txid).ok_or(Error::TransactionNotFound)?;
		if self.is_tx_confirmed(&parent_txid) {
			return Err(Error::TransactionAlreadyConfirmed);
		}

		// Calculate parent fee.
		let parent_fee = self.calculate_tx_fee(&parent_tx)?;
		let parent_fee_rate = parent_fee / parent_tx.weight();

		// Find spendable outputs from this transaction across ALL wallets.
		let utxos: Vec<LocalOutput> =
			self.list_unspent().into_iter().filter(|u| u.outpoint.txid == parent_txid).collect();
		if utxos.is_empty() {
			return Err(Error::NoSpendableOutputs);
		}

		// Classify UTXOs as primary or foreign for tx building.
		let utxo_infos = self.prepare_utxos_for_psbt(&utxos)?;

		// Determine destination.
		let drain_script = match destination_script {
			Some(s) => s,
			None => {
				let wallet = self.primary_wallet_mut();
				wallet.next_unused_address(KeychainKind::Internal).address.script_pubkey()
			},
		};

		// Build the child transaction on the primary wallet.
		let wallet = self.primary_wallet_mut();
		let mut tx_builder = wallet.build_tx();

		utxo::add_utxos_to_tx_builder(&mut tx_builder, &utxo_infos)
			.map_err(|_| Error::OnchainTxCreationFailed)?;

		tx_builder.fee_rate(fee_rate);
		tx_builder.drain_to(drain_script);
		tx_builder.manually_selected_only();

		let psbt = tx_builder.finish().map_err(|e| {
			log::error!("Failed to create CPFP transaction: {}", e);
			Error::OnchainTxCreationFailed
		})?;

		let tx = self.sign_psbt_all(psbt)?;
		Ok((tx, parent_fee, parent_fee_rate))
	}

	/// Calculate an appropriate CPFP child fee rate given a parent txid and
	/// a `target_fee_rate` that the caller wants the package to achieve.
	///
	/// Returns the recommended child fee rate.  If the parent already meets
	/// the target, returns `target + 1 sat/vB`.
	pub fn calculate_cpfp_fee_rate(
		&self, parent_txid: Txid, target_fee_rate: FeeRate,
	) -> Result<FeeRate, Error> {
		let parent_tx = self.find_tx(parent_txid).ok_or(Error::TransactionNotFound)?;
		if self.is_tx_confirmed(&parent_txid) {
			return Err(Error::TransactionAlreadyConfirmed);
		}

		let parent_fee = self.calculate_tx_fee(&parent_tx)?;
		let parent_fee_rate = parent_fee / parent_tx.weight();

		// If parent already meets target, return slightly higher rate.
		if parent_fee_rate >= target_fee_rate {
			return Ok(FeeRate::from_sat_per_kwu(parent_fee_rate.to_sat_per_kwu() + 250));
		}

		// Estimate child size: conservative 1-in/1-out (~120 vB).
		let estimated_child_vbytes: u64 = 120;
		let parent_vbytes = parent_tx.weight().to_vbytes_ceil();

		let parent_fee_deficit = (target_fee_rate.to_sat_per_vb_ceil()
			- parent_fee_rate.to_sat_per_vb_ceil())
			* parent_vbytes;
		let base_child_fee = target_fee_rate.to_sat_per_vb_ceil() * estimated_child_vbytes;
		let total_child_fee = base_child_fee + parent_fee_deficit;
		let child_fee_rate_sat_vb = total_child_fee / estimated_child_vbytes;

		Ok(FeeRate::from_sat_per_vb(child_fee_rate_sat_vb)
			.unwrap_or(FeeRate::from_sat_per_kwu(child_fee_rate_sat_vb * 250)))
	}

	// ─── RBF ────────────────────────────────────────────────────────────

	/// Returns `true` if all inputs of the transaction belong to the primary wallet.
	pub fn is_primary_only_tx(&self, txid: Txid) -> bool {
		let primary = self.primary_wallet();
		if primary.get_tx(txid).is_none() {
			return false;
		}
		let tx = match self.find_tx(txid) {
			Some(tx) => tx,
			None => return false,
		};
		tx.input.iter().all(|txin| {
			// Fast path: primary still has the UTXO (unspent).
			if primary.get_utxo(txin.previous_output).is_some() {
				return true;
			}
			// Slow path: the UTXO was already spent (by this tx).  Check
			// whether the output script belongs to the primary wallet.
			if let Some(tx_node) = primary.get_tx(txin.previous_output.txid) {
				if let Some(txout) =
					tx_node.tx_node.tx.output.get(txin.previous_output.vout as usize)
				{
					return primary.is_mine(txout.script_pubkey.clone());
				}
			}
			false
		})
	}

	/// Build an RBF replacement transaction.
	///
	/// This is a high-level method that:
	/// 1. Finds the original transaction across all wallets.
	/// 2. Validates that it is unconfirmed and the new fee rate is higher.
	/// 3. If the original tx was primary-only, tries BDK's built-in fee
	///    bump and falls back to adding foreign inputs if the primary wallet
	///    has insufficient funds.
	/// 4. If the original tx was cross-wallet, uses manual RBF construction.
	/// 5. Signs the replacement with all wallets.
	///
	/// Returns the signed replacement transaction and the original fee
	/// (for caller logging).
	pub fn build_rbf(
		&mut self, txid: Txid, new_fee_rate: FeeRate,
	) -> Result<(Transaction, Amount), Error> {
		// Find original transaction.
		let original_tx = self.find_tx(txid).ok_or(Error::TransactionNotFound)?;

		// Must not be confirmed.
		if self.is_tx_confirmed(&txid) {
			return Err(Error::TransactionAlreadyConfirmed);
		}

		// Calculate original fee.
		let original_fee = self.calculate_tx_fee(&original_tx)?;
		let original_fee_rate = original_fee / original_tx.weight();

		// New fee rate must be higher.
		if new_fee_rate <= original_fee_rate {
			return Err(Error::InvalidFeeRate);
		}

		// Build the replacement.
		let is_primary = self.is_primary_only_tx(txid);

		let tx = if is_primary {
			self.build_rbf_primary_with_fallback(txid, new_fee_rate)?
		} else {
			self.build_cross_wallet_rbf_with_fallback(&original_tx, new_fee_rate)?
		};

		Ok((tx, original_fee))
	}

	/// Try a primary-only RBF first; if the primary wallet doesn't have
	/// enough funds for the higher fee, fall back to adding non-primary
	/// UTXOs as additional inputs.
	fn build_rbf_primary_with_fallback(
		&mut self, txid: Txid, fee_rate: FeeRate,
	) -> Result<Transaction, Error> {
		// Attempt 1: primary-only fee bump.
		let primary_result: Result<Psbt, Error> = {
			let wallet = self.wallets.get_mut(&self.primary).ok_or(Error::WalletNotFound)?;
			match wallet.build_fee_bump(txid) {
				Ok(mut builder) => {
					builder.fee_rate(fee_rate);
					builder.finish().map_err(|e| {
						log::debug!("Primary-only RBF failed: {}", e);
						Error::InsufficientFunds
					})
				},
				Err(e) => {
					log::debug!("build_fee_bump failed: {}", e);
					Err(Error::OnchainTxCreationFailed)
				},
			}
		}; // mutable wallet borrow released here

		match primary_result {
			Ok(psbt) => self.sign_psbt_all(psbt),
			Err(_) => {
				// Attempt 2: select minimum non-primary UTXOs for the fee deficit.
				let original_tx = self.find_tx(txid).ok_or(Error::TransactionNotFound)?;
				let original_fee = self.calculate_tx_fee(&original_tx)?;
				let new_fee = fee_rate.fee_wu(original_tx.weight()).unwrap_or(Amount::ZERO);
				let fee_increase = new_fee.checked_sub(original_fee).unwrap_or(Amount::ZERO);

				// Wallet-owned outputs (change) in the original tx can absorb
				// part of the fee increase, reducing what foreign UTXOs must cover.
				let absorbable: Amount = original_tx
					.output
					.iter()
					.filter(|out| self.is_mine(out.script_pubkey.clone()))
					.map(|out| out.value)
					.sum();
				// No explicit buffer needed: BDK's coin selection already
				// accounts for each candidate's input weight cost.
				let deficit = fee_increase.checked_sub(absorbable).unwrap_or(Amount::ZERO);

				let non_primary_infos = self.select_non_primary_foreign_utxos(
					deficit,
					fee_rate,
					&HashSet::new(),
					false,
					CoinSelectionAlgorithm::BranchAndBound,
				)?;
				if non_primary_infos.is_empty() {
					return Err(Error::InsufficientFunds);
				}

				let wallet = self.wallets.get_mut(&self.primary).ok_or(Error::WalletNotFound)?;
				let mut builder = wallet.build_fee_bump(txid).map_err(|e| {
					log::error!("Failed to create fee bump builder (with foreign): {}", e);
					Error::OnchainTxCreationFailed
				})?;
				builder.fee_rate(fee_rate);

				for info in &non_primary_infos {
					builder
						.add_foreign_utxo_with_sequence(
							info.outpoint,
							info.psbt_input.clone(),
							info.weight,
							bitcoin::Sequence::ENABLE_RBF_NO_LOCKTIME,
						)
						.map_err(|e| {
							log::error!("Failed to add foreign UTXO {:?}: {}", info.outpoint, e);
							Error::OnchainTxCreationFailed
						})?;
				}

				let psbt = builder.finish().map_err(|e| {
					log::error!("Failed to build RBF with additional inputs: {}", e);
					Error::OnchainTxCreationFailed
				})?;

				self.sign_psbt_all(psbt)
			},
		}
	}

	// ─── Persistence ────────────────────────────────────────────────────

	/// Persist all wallets.
	pub fn persist_all(&mut self) -> Result<(), Error> {
		for (key, wallet) in self.wallets.iter_mut() {
			if let Some(persister) = self.persisters.get_mut(key) {
				wallet.persist(persister).map_err(|e| {
					log::error!("Failed to persist wallet {:?}: {}", key, e);
					Error::PersistenceFailed
				})?;
			}
		}
		Ok(())
	}

	/// Check if an output belongs to any wallet.
	pub fn is_mine(&self, script_pubkey: ScriptBuf) -> bool {
		self.wallets.values().any(|w| w.is_mine(script_pubkey.clone()))
	}
}

#[cfg(test)]
mod tests {
	use std::sync::atomic::{AtomicUsize, Ordering};
	use std::sync::{Arc, Mutex};

	use bdk_chain::Merge;
	use bdk_wallet::template::Bip84;
	use bdk_wallet::{ChangeSet, KeychainKind, PersistedWallet, Wallet, WalletPersister};
	use bitcoin::bip32::Xpriv;
	use bitcoin::{
		Amount, Block, FeeRate, Network, OutPoint, ScriptBuf, Transaction, TxIn, TxOut, Txid,
	};

	use super::*;

	struct NoopPersister;

	impl WalletPersister for NoopPersister {
		type Error = std::convert::Infallible;

		fn initialize(_: &mut Self) -> Result<ChangeSet, Self::Error> {
			Ok(ChangeSet::default())
		}

		fn persist(_: &mut Self, _: &ChangeSet) -> Result<(), Self::Error> {
			Ok(())
		}
	}

	#[derive(Clone, Default)]
	struct MemoryPersister {
		change_set: Arc<Mutex<ChangeSet>>,
	}

	impl WalletPersister for MemoryPersister {
		type Error = std::convert::Infallible;

		fn initialize(persister: &mut Self) -> Result<ChangeSet, Self::Error> {
			Ok(persister.change_set.lock().unwrap().clone())
		}

		fn persist(persister: &mut Self, change_set: &ChangeSet) -> Result<(), Self::Error> {
			persister.change_set.lock().unwrap().merge(change_set.clone());
			Ok(())
		}
	}

	struct FailOncePersister {
		failures: Arc<AtomicUsize>,
		inner: MemoryPersister,
	}

	impl WalletPersister for FailOncePersister {
		type Error = std::io::Error;

		fn initialize(persister: &mut Self) -> Result<ChangeSet, Self::Error> {
			Ok(persister.inner.change_set.lock().unwrap().clone())
		}

		fn persist(persister: &mut Self, change_set: &ChangeSet) -> Result<(), Self::Error> {
			if persister.failures.load(Ordering::SeqCst) > 0 {
				persister.failures.fetch_sub(1, Ordering::SeqCst);
				return Err(std::io::Error::new(
					std::io::ErrorKind::Other,
					"injected persist failure",
				));
			}
			persister.inner.change_set.lock().unwrap().merge(change_set.clone());
			Ok(())
		}
	}

	fn test_xprv() -> Xpriv {
		Xpriv::new_master(Network::Regtest, &[0x42; 32]).unwrap()
	}

	fn create_empty_wallet<P>(persister: &mut P) -> PersistedWallet<P>
	where
		P: WalletPersister,
		P::Error: std::fmt::Debug,
	{
		let xprv = test_xprv();
		PersistedWallet::create(
			persister,
			Wallet::create(
				Bip84(xprv, KeychainKind::External),
				Bip84(xprv, KeychainKind::Internal),
			)
			.network(Network::Regtest),
		)
		.unwrap()
	}

	fn load_empty_wallet<P>(persister: &mut P) -> PersistedWallet<P>
	where
		P: WalletPersister,
		P::Error: std::fmt::Debug,
	{
		let xprv = test_xprv();
		Wallet::load()
			.descriptor(KeychainKind::External, Some(Bip84(xprv, KeychainKind::External)))
			.descriptor(KeychainKind::Internal, Some(Bip84(xprv, KeychainKind::Internal)))
			.extract_keys()
			.check_network(Network::Regtest)
			.load_wallet(persister)
			.unwrap()
			.expect("wallet should have been persisted")
	}

	#[test]
	fn add_wallet_and_persist_rolls_back_on_persist_failure() {
		let failures = Arc::new(AtomicUsize::new(0));
		let mut primary_persister = FailOncePersister {
			failures: Arc::clone(&failures),
			inner: MemoryPersister::default(),
		};
		let primary = create_empty_wallet(&mut primary_persister);
		let mut secondary_persister = FailOncePersister {
			failures: Arc::clone(&failures),
			inner: MemoryPersister::default(),
		};
		let mut secondary = create_empty_wallet(&mut secondary_persister);
		secondary.reveal_next_address(KeychainKind::External);
		let secondary_state = secondary_persister.inner.clone();

		let mut aggregate = AggregateWallet::new(primary, primary_persister, 0u8, vec![]);
		failures.store(1, Ordering::SeqCst);
		assert_eq!(
			aggregate.add_wallet_and_persist(1, secondary, secondary_persister),
			Err(Error::PersistenceFailed)
		);
		assert!(!aggregate.loaded_keys().contains(&1));

		let mut retry_persister = FailOncePersister { failures, inner: secondary_state };
		let retry_wallet = load_empty_wallet(&mut retry_persister);
		aggregate.add_wallet_and_persist(1, retry_wallet, retry_persister).unwrap();
		assert!(aggregate.loaded_keys().contains(&1));
	}

	fn create_funded_wallet(
		persister: &mut NoopPersister, amount: Amount,
	) -> PersistedWallet<NoopPersister> {
		let mut wallet = create_empty_wallet(persister);

		let addr = wallet.reveal_next_address(KeychainKind::External);
		let funding_tx = Transaction {
			version: bitcoin::transaction::Version::TWO,
			lock_time: bitcoin::blockdata::locktime::absolute::LockTime::ZERO,
			input: vec![TxIn {
				previous_output: OutPoint { txid: Txid::from_byte_array([0x01; 32]), vout: 0 },
				..Default::default()
			}],
			output: vec![TxOut { value: amount, script_pubkey: addr.address.script_pubkey() }],
		};
		wallet.apply_unconfirmed_txs([(funding_tx, 0)]);
		wallet
	}

	fn recipient_script() -> ScriptBuf {
		ScriptBuf::new_p2wpkh(&bitcoin::WPubkeyHash::from_byte_array([0xab; 20]))
	}

	#[test]
	fn new_address_info_reveals_external_indexes() {
		let mut persister = NoopPersister;
		let wallet = create_empty_wallet(&mut persister);
		let mut agg = AggregateWallet::<u8, _>::new(wallet, persister, 0, vec![]);

		let first = agg.new_address_info_for(&0).unwrap();
		assert_eq!(first.index, 0);
		assert_eq!(first.keychain, KeychainKind::External);

		let second = agg.new_address_info().unwrap();
		assert_eq!(second.index, 1);
		assert_eq!(second.keychain, KeychainKind::External);

		let third_address = agg.new_address_for(&0).unwrap();
		let third_peek = agg.address_info_for(&0, KeychainKind::External, 2).unwrap();
		assert_eq!(third_peek.index, 2);
		assert_eq!(third_peek.address, third_address);
	}

	#[test]
	fn address_info_for_index_does_not_advance_receive_index() {
		let mut persister = NoopPersister;
		let wallet = create_empty_wallet(&mut persister);
		let mut agg = AggregateWallet::<u8, _>::new(wallet, persister, 0, vec![]);

		let peeked = agg.address_info_for(&0, KeychainKind::External, 7).unwrap();
		assert_eq!(peeked.index, 7);
		assert_eq!(peeked.keychain, KeychainKind::External);

		let next = agg.new_address_info_for(&0).unwrap();
		assert_eq!(next.index, 0);
		assert_ne!(next.address, peeked.address);
	}

	#[test]
	fn address_info_for_index_supports_internal_keychain_without_advancing_receive_index() {
		let mut persister = NoopPersister;
		let wallet = create_empty_wallet(&mut persister);
		let mut agg = AggregateWallet::<u8, _>::new(wallet, persister, 0, vec![]);

		let change = agg.address_info_for(&0, KeychainKind::Internal, 3).unwrap();
		assert_eq!(change.index, 3);
		assert_eq!(change.keychain, KeychainKind::Internal);

		let next_receive = agg.new_address_info_for(&0).unwrap();
		assert_eq!(next_receive.index, 0);
		assert_eq!(next_receive.keychain, KeychainKind::External);
		assert_ne!(next_receive.address, change.address);
	}

	#[test]
	fn address_infos_for_returns_keychain_range_without_advancing() {
		let mut persister = NoopPersister;
		let wallet = create_empty_wallet(&mut persister);
		let mut agg = AggregateWallet::<u8, _>::new(wallet, persister, 0, vec![]);

		let addresses = agg.address_infos_for(&0, KeychainKind::Internal, 2, 3).unwrap();
		let indexes: Vec<u32> = addresses.iter().map(|info| info.index).collect();
		assert_eq!(indexes, vec![2, 3, 4]);
		assert!(addresses.iter().all(|info| info.keychain == KeychainKind::Internal));

		let single = agg.address_info_for(&0, KeychainKind::Internal, 3).unwrap();
		assert_eq!(addresses[1], single);

		let next_receive = agg.new_address_info_for(&0).unwrap();
		assert_eq!(next_receive.index, 0);
	}

	#[test]
	fn address_infos_for_allows_empty_ranges() {
		let mut persister = NoopPersister;
		let wallet = create_empty_wallet(&mut persister);
		let agg = AggregateWallet::<u8, _>::new(wallet, persister, 0, vec![]);

		let addresses = agg.address_infos_for(&0, KeychainKind::External, 0, 0).unwrap();
		assert!(addresses.is_empty());
	}

	#[test]
	fn address_info_for_rejects_hardened_index() {
		let mut persister = NoopPersister;
		let wallet = create_empty_wallet(&mut persister);
		let agg = AggregateWallet::<u8, _>::new(wallet, persister, 0, vec![]);

		let result = agg.address_info_for(&0, KeychainKind::External, BIP32_MAX_NORMAL_INDEX + 1);
		assert_eq!(result, Err(Error::InvalidQuantity));
	}

	#[test]
	fn address_infos_for_rejects_invalid_ranges() {
		let mut persister = NoopPersister;
		let wallet = create_empty_wallet(&mut persister);
		let agg = AggregateWallet::<u8, _>::new(wallet, persister, 0, vec![]);

		let too_many =
			agg.address_infos_for(&0, KeychainKind::External, 0, MAX_ADDRESS_INFO_BATCH_COUNT + 1);
		assert_eq!(too_many, Err(Error::InvalidQuantity));

		let hardened_end =
			agg.address_infos_for(&0, KeychainKind::External, BIP32_MAX_NORMAL_INDEX - 1, 3);
		assert_eq!(hardened_end, Err(Error::InvalidQuantity));

		let overflow = agg.address_infos_for(&0, KeychainKind::External, u32::MAX, 1);
		assert_eq!(overflow, Err(Error::InvalidQuantity));
	}

	#[test]
	fn reveal_addresses_to_rejects_hardened_index() {
		let mut persister = NoopPersister;
		let wallet = create_empty_wallet(&mut persister);
		let mut agg = AggregateWallet::<u8, _>::new(wallet, persister, 0, vec![]);

		let result = agg.reveal_addresses_to(&0, BIP32_MAX_NORMAL_INDEX + 1);
		assert_eq!(result, Err(Error::InvalidQuantity));
	}

	#[test]
	fn reveal_addresses_to_advances_next_receive_index() {
		let mut persister = NoopPersister;
		let wallet = create_empty_wallet(&mut persister);
		let mut agg = AggregateWallet::<u8, _>::new(wallet, persister, 0, vec![]);

		agg.reveal_addresses_to(&0, 4).unwrap();
		agg.reveal_addresses_to(&0, 4).unwrap();

		let next = agg.new_address_info_for(&0).unwrap();
		assert_eq!(next.index, 5);
		assert_eq!(next.keychain, KeychainKind::External);
	}

	#[test]
	fn revealed_receive_index_persists_across_reload() {
		let mut create_persister = MemoryPersister::default();
		let wallet = create_empty_wallet(&mut create_persister);
		let aggregate_persister = create_persister.clone();

		{
			let mut agg = AggregateWallet::<u8, _>::new(wallet, aggregate_persister, 0, vec![]);
			agg.reveal_addresses_to(&0, 3).unwrap();
		}

		let mut reload_persister = create_persister.clone();
		let reloaded_wallet = load_empty_wallet(&mut reload_persister);
		let mut reloaded =
			AggregateWallet::<u8, _>::new(reloaded_wallet, reload_persister, 0, vec![]);

		let next = reloaded.new_address_info_for(&0).unwrap();
		assert_eq!(next.index, 4);
	}

	/// Demonstrates the bug: without cancel_tx, each finish() call
	/// cumulatively advances the internal (change) derivation index.
	#[test]
	fn test_finish_without_cancel_leaks_change_index() {
		let mut persister = NoopPersister;
		let mut wallet = create_funded_wallet(&mut persister, Amount::from_sat(100_000));

		let recipient = recipient_script();
		let fee_rate = FeeRate::from_sat_per_vb(2).unwrap();

		let initial_idx = wallet.derivation_index(KeychainKind::Internal);

		// First dry-run finish(), no cancel
		let mut b1 = wallet.build_tx();
		b1.add_recipient(recipient.clone(), Amount::from_sat(30_000)).fee_rate(fee_rate);
		let _psbt1 = b1.finish().unwrap();
		let after_first = wallet.derivation_index(KeychainKind::Internal);

		// Second dry-run finish(), no cancel
		let mut b2 = wallet.build_tx();
		b2.add_recipient(recipient.clone(), Amount::from_sat(30_000)).fee_rate(fee_rate);
		let _psbt2 = b2.finish().unwrap();
		let after_second = wallet.derivation_index(KeychainKind::Internal);

		// Third dry-run finish(), no cancel
		let mut b3 = wallet.build_tx();
		b3.add_recipient(recipient.clone(), Amount::from_sat(30_000)).fee_rate(fee_rate);
		let _psbt3 = b3.finish().unwrap();
		let after_third = wallet.derivation_index(KeychainKind::Internal);

		assert!(
			after_first > initial_idx || (initial_idx.is_none() && after_first.is_some()),
			"First finish() should advance the internal index"
		);
		assert!(
			after_second > after_first,
			"Second finish() without cancel should advance further: {:?} > {:?}",
			after_second,
			after_first,
		);
		assert!(
			after_third > after_second,
			"Third finish() without cancel should advance further: {:?} > {:?}",
			after_third,
			after_second,
		);
	}

	/// Cancelling repeated dry runs keeps the internal derivation index stable.
	#[test]
	fn test_cancel_dry_run_prevents_cumulative_index_leak() {
		let mut persister = NoopPersister;
		let wallet = create_funded_wallet(&mut persister, Amount::from_sat(100_000));
		let mut agg = AggregateWallet::<u8, _>::new(wallet, persister, 0, vec![]);

		let recipient = recipient_script();
		let fee_rate = FeeRate::from_sat_per_vb(2).unwrap();

		let initial_idx = agg.derivation_index(KeychainKind::Internal);

		// Dry-run 1: finish + cancel
		let psbt1 = {
			let w = agg.primary_wallet_mut();
			let mut b = w.build_tx();
			b.add_recipient(recipient.clone(), Amount::from_sat(30_000)).fee_rate(fee_rate);
			b.finish().unwrap()
		};
		agg.cancel_dry_run_tx(&psbt1.unsigned_tx);
		let after_first = agg.derivation_index(KeychainKind::Internal);

		assert!(
			after_first > initial_idx || (initial_idx.is_none() && after_first.is_some()),
			"First finish() should reveal an internal address"
		);

		// Dry-run 2: finish + cancel
		let psbt2 = {
			let w = agg.primary_wallet_mut();
			let mut b = w.build_tx();
			b.add_recipient(recipient.clone(), Amount::from_sat(30_000)).fee_rate(fee_rate);
			b.finish().unwrap()
		};
		agg.cancel_dry_run_tx(&psbt2.unsigned_tx);
		let after_second = agg.derivation_index(KeychainKind::Internal);

		assert_eq!(
			after_first, after_second,
			"cancel_dry_run_tx should prevent cumulative index advance"
		);

		// Dry-runs 3-5: confirm stability
		for i in 3..=5 {
			let psbt = {
				let w = agg.primary_wallet_mut();
				let mut b = w.build_tx();
				b.add_recipient(recipient.clone(), Amount::from_sat(30_000)).fee_rate(fee_rate);
				b.finish().unwrap()
			};
			agg.cancel_dry_run_tx(&psbt.unsigned_tx);
			let idx = agg.derivation_index(KeychainKind::Internal);
			assert_eq!(after_first, idx, "Dry-run {} should still reuse the same index", i);
		}
	}

	/// Cancelling after an intermediate failure keeps the internal derivation index stable.
	#[test]
	fn test_cancel_after_failed_intermediate_prevents_leak() {
		let mut persister = NoopPersister;
		let wallet = create_funded_wallet(&mut persister, Amount::from_sat(100_000));
		let mut agg = AggregateWallet::<u8, _>::new(wallet, persister, 0, vec![]);

		let recipient = recipient_script();
		let fee_rate = FeeRate::from_sat_per_vb(2).unwrap();

		// Baseline
		let initial_idx = agg.derivation_index(KeychainKind::Internal);

		// Simulate finish() + failed intermediate + unconditional cancel.
		for _ in 0..5 {
			let psbt = {
				let w = agg.primary_wallet_mut();
				let mut b = w.build_tx();
				b.add_recipient(recipient.clone(), Amount::from_sat(30_000)).fee_rate(fee_rate);
				b.finish().unwrap()
			};

			// Simulate an intermediate operation that fails.
			let _simulated_failure: Result<u64, &str> = Err("simulated fee calculation failure");

			// Cancel regardless of failure.
			agg.cancel_dry_run_tx(&psbt.unsigned_tx);
		}

		let final_idx = agg.derivation_index(KeychainKind::Internal);

		assert!(
			final_idx > initial_idx || (initial_idx.is_none() && final_idx.is_some()),
			"Index should be revealed at least once"
		);

		// One more dry-run + cancel should not change it.
		let psbt = {
			let w = agg.primary_wallet_mut();
			let mut b = w.build_tx();
			b.add_recipient(recipient.clone(), Amount::from_sat(30_000)).fee_rate(fee_rate);
			b.finish().unwrap()
		};
		agg.cancel_dry_run_tx(&psbt.unsigned_tx);
		assert_eq!(
			final_idx,
			agg.derivation_index(KeychainKind::Internal),
			"Sixth dry-run should still reuse the same index"
		);
	}

	/// A real transaction reuses the change address from a cancelled dry run.
	#[test]
	fn test_dry_run_cancel_then_real_tx_reuses_change_address() {
		let mut persister = NoopPersister;
		let wallet = create_funded_wallet(&mut persister, Amount::from_sat(200_000));
		let mut agg = AggregateWallet::<u8, _>::new(wallet, persister, 0, vec![]);

		let recipient = recipient_script();
		let fee_rate = FeeRate::from_sat_per_vb(2).unwrap();

		// Dry-run + cancel
		let dry_psbt = {
			let w = agg.primary_wallet_mut();
			let mut b = w.build_tx();
			b.add_recipient(recipient.clone(), Amount::from_sat(50_000)).fee_rate(fee_rate);
			b.finish().unwrap()
		};
		let dry_run_change_script: Option<ScriptBuf> = dry_psbt
			.unsigned_tx
			.output
			.iter()
			.find(|o| agg.is_mine(o.script_pubkey.clone()))
			.map(|o| o.script_pubkey.clone());
		agg.cancel_dry_run_tx(&dry_psbt.unsigned_tx);

		let idx_after_dry = agg.derivation_index(KeychainKind::Internal);

		// Real tx (no cancel — this would be signed and broadcast)
		let real_psbt = {
			let w = agg.primary_wallet_mut();
			let mut b = w.build_tx();
			b.add_recipient(recipient.clone(), Amount::from_sat(50_000)).fee_rate(fee_rate);
			b.finish().unwrap()
		};
		let real_change_script: Option<ScriptBuf> = real_psbt
			.unsigned_tx
			.output
			.iter()
			.find(|o| agg.is_mine(o.script_pubkey.clone()))
			.map(|o| o.script_pubkey.clone());

		let idx_after_real = agg.derivation_index(KeychainKind::Internal);

		assert_eq!(
			idx_after_dry, idx_after_real,
			"Real tx should not advance index beyond what dry-run already revealed"
		);
		if let (Some(dry_change), Some(real_change)) = (&dry_run_change_script, &real_change_script)
		{
			assert_eq!(
				dry_change, real_change,
				"Real tx should reuse the same change address as the cancelled dry-run"
			);
		}
	}

	fn regtest_child_block(prev: &Block, distinct: u8) -> Block {
		use bitcoin::block::Header;
		use bitcoin::blockdata::locktime::absolute::LockTime;
		use bitcoin::hashes::Hash as _;
		use bitcoin::{Sequence, TxMerkleNode, Witness};

		let coinbase = Transaction {
			version: bitcoin::transaction::Version::ONE,
			lock_time: LockTime::ZERO,
			input: vec![TxIn {
				previous_output: OutPoint::null(),
				script_sig: ScriptBuf::builder().push_int(distinct as i64).into_script(),
				sequence: Sequence::MAX,
				witness: Witness::new(),
			}],
			output: vec![TxOut {
				value: Amount::from_sat(50_0000_0000),
				script_pubkey: ScriptBuf::new_op_return([distinct]),
			}],
		};
		let merkle_root = TxMerkleNode::from_byte_array(coinbase.compute_txid().to_byte_array());
		Block {
			header: Header {
				version: bitcoin::block::Version::ONE,
				prev_blockhash: prev.block_hash(),
				merkle_root,
				time: prev.header.time.saturating_add(1),
				bits: prev.header.bits,
				nonce: distinct as u32,
			},
			txdata: vec![coinbase],
		}
	}

	fn regtest_child_block_with_transaction(
		prev: &Block, distinct: u8, transaction: Transaction,
	) -> Block {
		let mut block = regtest_child_block(prev, distinct);
		block.txdata.push(transaction);
		block.header.merkle_root = block.compute_merkle_root().unwrap();
		block
	}

	#[test]
	fn apply_block_skips_wallets_already_ahead_during_catch_up() {
		use bitcoin::blockdata::constants::genesis_block;

		let mut primary_persister = NoopPersister;
		let mut primary = create_empty_wallet(&mut primary_persister);
		let genesis = genesis_block(Network::Regtest);
		primary.apply_block(&genesis, 0).unwrap();
		let block1 = regtest_child_block(&genesis, 1);
		primary.apply_block(&block1, 1).unwrap();
		let block2 = regtest_child_block(&block1, 1);
		primary.apply_block(&block2, 2).unwrap();
		assert_eq!(primary.latest_checkpoint().height(), 2);

		let mut secondary_persister = NoopPersister;
		let mut secondary = create_empty_wallet(&mut secondary_persister);
		secondary.apply_block(&genesis, 0).unwrap();
		assert_eq!(secondary.latest_checkpoint().height(), 0);

		let mut agg = AggregateWallet::new(
			primary,
			primary_persister,
			0u8,
			vec![(1u8, secondary, secondary_persister)],
		);
		assert_eq!(agg.current_best_block().1, 0);

		// Replaying the gap must not fail on the primary wallet that is already at height 2.
		agg.apply_block(&block1, 1).expect("catch-up must skip wallets already ahead");
		assert_eq!(agg.current_best_block().1, 1);
		agg.apply_block(&block2, 2).expect("catch-up must bring the lagging wallet to tip");
		assert_eq!(agg.current_best_block().1, 2);
	}

	#[test]
	fn apply_block_applies_same_height_reorg_replacement_when_tip_ahead() {
		use bitcoin::blockdata::constants::genesis_block;

		let mut primary_persister = NoopPersister;
		let mut primary = create_empty_wallet(&mut primary_persister);
		let mut secondary_persister = NoopPersister;
		let mut secondary = create_empty_wallet(&mut secondary_persister);
		let genesis = genesis_block(Network::Regtest);
		primary.apply_block(&genesis, 0).unwrap();
		secondary.apply_block(&genesis, 0).unwrap();

		let block1_a = regtest_child_block(&genesis, 1);
		let block2_a = regtest_child_block(&block1_a, 1);
		primary.apply_block(&block1_a, 1).unwrap();
		primary.apply_block(&block2_a, 2).unwrap();
		secondary.apply_block(&block1_a, 1).unwrap();
		secondary.apply_block(&block2_a, 2).unwrap();

		let mut agg = AggregateWallet::new(
			primary,
			primary_persister,
			0u8,
			vec![(1u8, secondary, secondary_persister)],
		);
		assert_eq!(agg.current_best_block().1, 2);

		// Height-only skipping would ignore this replacement because tip_height (2) >= 1.
		let block1_b = regtest_child_block(&genesis, 2);
		assert_ne!(block1_a.block_hash(), block1_b.block_hash());
		agg.apply_block(&block1_b, 1).expect("reorg replacement must be applied");

		for key in [0u8, 1u8] {
			let tip = agg.wallet(&key).unwrap().latest_checkpoint();
			assert_eq!(tip.height(), 1, "wallet {:?} must reorg to replacement height", key);
			assert_eq!(
				tip.hash(),
				block1_b.block_hash(),
				"wallet {:?} must adopt replacement hash",
				key
			);
		}
	}

	#[test]
	fn apply_block_skips_sparse_ahead_wallet_during_catch_up() {
		use bdk_chain::BlockId;
		use bdk_wallet::Update;
		use bitcoin::blockdata::constants::genesis_block;

		let mut primary_persister = NoopPersister;
		let mut primary = create_empty_wallet(&mut primary_persister);
		let genesis = genesis_block(Network::Regtest);
		primary.apply_block(&genesis, 0).unwrap();
		let block1 = regtest_child_block(&genesis, 1);
		let block2 = regtest_child_block(&block1, 1);
		// Brand-new wallets insert only `{genesis, tip}` — intermediate heights are absent.
		let sparse_tip = BlockId { height: 2, hash: block2.block_hash() };
		let checkpoint = primary.latest_checkpoint().insert(sparse_tip);
		primary.apply_update(Update { chain: Some(checkpoint), ..Default::default() }).unwrap();
		assert_eq!(primary.latest_checkpoint().height(), 2);
		assert!(primary.latest_checkpoint().get(1).is_none());

		let mut secondary_persister = NoopPersister;
		let mut secondary = create_empty_wallet(&mut secondary_persister);
		secondary.apply_block(&genesis, 0).unwrap();

		let mut agg = AggregateWallet::new(
			primary,
			primary_persister,
			0u8,
			vec![(1u8, secondary, secondary_persister)],
		);
		assert!(agg.wallet(&0u8).unwrap().latest_checkpoint().get(1).is_none());
		agg.apply_block(&block1, 1)
			.expect("sparse ahead wallet must skip unknown intermediate heights");
		assert_eq!(agg.wallet(&0u8).unwrap().latest_checkpoint().height(), 2);
		assert_eq!(agg.wallet(&1u8).unwrap().latest_checkpoint().height(), 1);
		agg.apply_block(&block2, 2).expect("lagging wallet catches up to sparse tip");
		assert_eq!(agg.current_best_block().1, 2);
	}

	#[test]
	fn reorg_reconciliation_scans_intermediate_replacement_blocks() {
		use bdk_wallet::Update;
		use bitcoin::blockdata::constants::genesis_block;

		let mut primary_persister = NoopPersister;
		let mut primary = create_empty_wallet(&mut primary_persister);
		let genesis = genesis_block(Network::Regtest);
		primary.apply_block(&genesis, 0).unwrap();
		let receive_script =
			primary.reveal_next_address(KeychainKind::External).address.script_pubkey();

		let block1_a = regtest_child_block(&genesis, 1);
		let block2_a = regtest_child_block(&block1_a, 1);
		let funding_tx = Transaction {
			version: bitcoin::transaction::Version::TWO,
			lock_time: bitcoin::absolute::LockTime::ZERO,
			input: vec![TxIn {
				previous_output: OutPoint { txid: Txid::from_byte_array([0x44; 32]), vout: 0 },
				..Default::default()
			}],
			output: vec![TxOut { value: Amount::from_sat(70_000), script_pubkey: receive_script }],
		};
		let funding_txid = funding_tx.compute_txid();
		let block1_b = regtest_child_block_with_transaction(&genesis, 2, funding_tx);
		let block2_b = regtest_child_block(&block1_b, 2);
		assert_ne!(block2_a.block_hash(), block2_b.block_hash());

		let sparse_stale = BlockId { height: 2, hash: block2_a.block_hash() };
		let checkpoint = primary.latest_checkpoint().insert(sparse_stale);
		primary.apply_update(Update { chain: Some(checkpoint), ..Default::default() }).unwrap();

		let mut agg = AggregateWallet::new(primary, primary_persister, 0u8, vec![]);
		agg.apply_reorg_blocks_to(
			&0u8,
			BlockId { height: 0, hash: genesis.block_hash() },
			&[(block1_b.clone(), 1), (block2_b.clone(), 2)],
		)
		.expect("replacement branch must reconcile");
		assert_eq!(agg.wallet(&0u8).unwrap().latest_checkpoint().hash(), block2_b.block_hash());
		assert!(agg.wallet(&0u8).unwrap().get_tx(funding_txid).is_some());
		assert_eq!(agg.wallet(&0u8).unwrap().balance().confirmed, Amount::from_sat(70_000));
	}

	#[test]
	fn apply_block_to_updates_only_the_targeted_wallet() {
		use bitcoin::blockdata::constants::genesis_block;

		let mut primary_persister = NoopPersister;
		let mut primary = create_empty_wallet(&mut primary_persister);
		let mut secondary_persister = NoopPersister;
		let mut secondary = create_empty_wallet(&mut secondary_persister);
		let genesis = genesis_block(Network::Regtest);
		primary.apply_block(&genesis, 0).unwrap();
		secondary.apply_block(&genesis, 0).unwrap();

		let block1 = regtest_child_block(&genesis, 1);
		let mut agg = AggregateWallet::new(
			primary,
			primary_persister,
			0u8,
			vec![(1u8, secondary, secondary_persister)],
		);
		agg.apply_block_to(&1u8, &block1, 1).expect("targeted apply");
		assert_eq!(agg.wallet(&0u8).unwrap().latest_checkpoint().height(), 0);
		assert_eq!(agg.wallet(&1u8).unwrap().latest_checkpoint().height(), 1);
	}

	#[test]
	fn reorg_reconciliation_preserves_historical_sparse_checkpoints() {
		use bdk_wallet::Update;
		use bitcoin::blockdata::constants::genesis_block;

		let mut primary_persister = NoopPersister;
		let mut primary = create_empty_wallet(&mut primary_persister);
		let genesis = genesis_block(Network::Regtest);
		primary.apply_block(&genesis, 0).unwrap();

		let block1 = regtest_child_block(&genesis, 1);
		let block2 = regtest_child_block(&block1, 1);
		let block3_a = regtest_child_block(&block2, 1);
		let block4_a = regtest_child_block(&block3_a, 1);
		let block3_b = regtest_child_block(&block2, 2);
		let block4_b = regtest_child_block(&block3_b, 2);

		let sparse_checkpoint =
			primary.latest_checkpoint().insert(BlockId { height: 2, hash: block2.block_hash() });
		primary
			.apply_update(Update { chain: Some(sparse_checkpoint), ..Default::default() })
			.unwrap();
		primary.apply_block(&block3_a, 3).unwrap();
		primary.apply_block(&block4_a, 4).unwrap();

		let mut agg = AggregateWallet::new(primary, primary_persister, 0u8, vec![]);
		agg.apply_reorg_blocks_to(
			&0u8,
			BlockId { height: 2, hash: block2.block_hash() },
			&[(block3_b.clone(), 3), (block4_b.clone(), 4)],
		)
		.expect("replacement branch must retain the common sparse checkpoint");

		let tip = agg.wallet(&0u8).unwrap().latest_checkpoint();
		assert_eq!(tip.hash(), block4_b.block_hash());
		assert_eq!(tip.get(2).unwrap().hash(), block2.block_hash());
	}

	#[test]
	fn reorg_reconciliation_rejects_replacement_below_wallet_tip() {
		use bdk_wallet::Update;
		use bitcoin::blockdata::constants::genesis_block;

		let mut persister = NoopPersister;
		let mut wallet = create_empty_wallet(&mut persister);
		let genesis = genesis_block(Network::Regtest);
		wallet.apply_block(&genesis, 0).unwrap();
		let block1_a = regtest_child_block(&genesis, 1);
		let block2_a = regtest_child_block(&block1_a, 1);
		let block1_b = regtest_child_block(&genesis, 2);
		let block2_b = regtest_child_block(&block1_b, 2);
		let checkpoint =
			wallet.latest_checkpoint().insert(BlockId { height: 2, hash: block2_a.block_hash() });
		wallet.apply_update(Update { chain: Some(checkpoint), ..Default::default() }).unwrap();

		let mut agg = AggregateWallet::new(wallet, persister, 0u8, vec![]);
		let fork_point = BlockId { height: 0, hash: genesis.block_hash() };
		assert_eq!(
			agg.apply_reorg_blocks_to(&0u8, fork_point, &[(block1_b.clone(), 1)]),
			Err(Error::WalletOperationFailed)
		);
		assert_eq!(agg.wallet(&0u8).unwrap().latest_checkpoint().hash(), block2_a.block_hash());

		agg.apply_reorg_blocks_to(&0u8, fork_point, &[(block1_b, 1), (block2_b.clone(), 2)])
			.expect("wallet must reconcile once the replacement reaches its previous height");
		assert_eq!(agg.wallet(&0u8).unwrap().latest_checkpoint().hash(), block2_b.block_hash());
	}

	#[test]
	fn apply_block_retries_persist_when_block_is_already_known() {
		use bitcoin::blockdata::constants::genesis_block;

		let failures = Arc::new(AtomicUsize::new(0));
		let mut primary_persister = FailOncePersister {
			failures: Arc::clone(&failures),
			inner: MemoryPersister::default(),
		};
		let mut primary = create_empty_wallet(&mut primary_persister);
		let genesis = genesis_block(Network::Regtest);
		primary.apply_block(&genesis, 0).unwrap();
		let block1 = regtest_child_block(&genesis, 1);

		let mut agg = AggregateWallet::new(primary, primary_persister, 0u8, vec![]);
		failures.store(1, Ordering::SeqCst);
		let first = agg.apply_block(&block1, 1);
		assert!(first.is_err(), "first persist must fail");
		assert_eq!(agg.primary_wallet().latest_checkpoint().hash(), block1.block_hash());

		agg.apply_block(&block1, 1).expect("known block must retry stranded persist");
		assert_eq!(failures.load(Ordering::SeqCst), 0);

		let replacement = regtest_child_block(&genesis, 2);
		failures.store(1, Ordering::SeqCst);
		assert_eq!(
			agg.apply_reorg_blocks_to(
				&0u8,
				BlockId { height: 0, hash: genesis.block_hash() },
				&[(replacement.clone(), 1)],
			),
			Err(Error::PersistenceFailed)
		);
		assert_eq!(agg.primary_wallet().latest_checkpoint().hash(), replacement.block_hash());
		agg.persist_all().expect("pending reorg changes must remain retryable");
		assert_eq!(failures.load(Ordering::SeqCst), 0);
	}

	#[test]
	fn current_best_block_uses_lexicographic_hash_on_equal_height_fork() {
		use bitcoin::blockdata::constants::genesis_block;

		let mut primary_persister = NoopPersister;
		let mut primary = create_empty_wallet(&mut primary_persister);
		let mut secondary_persister = NoopPersister;
		let mut secondary = create_empty_wallet(&mut secondary_persister);
		let genesis = genesis_block(Network::Regtest);
		primary.apply_block(&genesis, 0).unwrap();
		secondary.apply_block(&genesis, 0).unwrap();

		let block1_a = regtest_child_block(&genesis, 1);
		let block1_b = regtest_child_block(&genesis, 2);
		assert_ne!(block1_a.block_hash(), block1_b.block_hash());
		primary.apply_block(&block1_a, 1).unwrap();
		secondary.apply_block(&block1_b, 1).unwrap();

		let agg = AggregateWallet::new(
			primary,
			primary_persister,
			0u8,
			vec![(1u8, secondary, secondary_persister)],
		);
		let (hash, height) = agg.current_best_block();
		assert_eq!(height, 1);
		// Must not hide a secondary tip behind primary preference: pick lex-smallest hash so
		// Bitcoind catch-up can start from a divergent equal-height checkpoint.
		let expected =
			if block1_a.block_hash().to_byte_array() <= block1_b.block_hash().to_byte_array() {
				block1_a.block_hash()
			} else {
				block1_b.block_hash()
			};
		assert_eq!(hash, expected);
		let tips = agg.chain_tips();
		assert_eq!(tips.len(), 2);
		assert!(tips.contains(&(block1_a.block_hash(), 1)));
		assert!(tips.contains(&(block1_b.block_hash(), 1)));
	}
}

// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::{HashMap, HashSet};
use std::ops::Deref;
use std::sync::{Arc, RwLock};

use bitcoin::secp256k1::PublicKey;
use lightning::impl_writeable_tlv_based;
use lightning::routing::gossip::NodeId;
use lightning::util::persist::KVStoreSync;
use lightning::util::ser::{Readable, ReadableArgs, Writeable, Writer};

use crate::io::{
	PEER_INFO_PERSISTENCE_KEY, PEER_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
	PEER_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
};
use crate::logger::{log_error, log_info, LdkLogger};
use crate::types::{DynStore, Graph};
use crate::{Error, SocketAddress};

pub struct PeerStore<L: Deref>
where
	L::Target: LdkLogger,
{
	peers: RwLock<HashMap<PublicKey, PeerInfo>>,
	kv_store: Arc<DynStore>,
	logger: L,
}

impl<L: Deref> PeerStore<L>
where
	L::Target: LdkLogger,
{
	pub(crate) fn new(kv_store: Arc<DynStore>, logger: L) -> Self {
		let peers = RwLock::new(HashMap::new());
		Self { peers, kv_store, logger }
	}

	pub(crate) fn add_peer(&self, peer_info: PeerInfo) -> Result<(), Error> {
		let mut locked_peers = self.peers.write().unwrap();

		if let Some(existing) = locked_peers.get(&peer_info.node_id) {
			if existing.address == peer_info.address {
				return Ok(());
			}
			log_info!(
				self.logger,
				"Updating socket address for peer {}: {} -> {}",
				peer_info.node_id,
				existing.address,
				peer_info.address
			);
		}

		let mut updated_peers = locked_peers.clone();
		updated_peers.insert(peer_info.node_id, peer_info);
		self.persist_peers(&updated_peers)?;
		*locked_peers = updated_peers;
		Ok(())
	}

	pub(crate) fn remove_peer(&self, node_id: &PublicKey) -> Result<(), Error> {
		let mut locked_peers = self.peers.write().unwrap();

		locked_peers.remove(node_id);
		self.persist_peers(&*locked_peers)
	}

	pub(crate) fn list_peers(&self) -> Vec<PeerInfo> {
		self.peers.read().unwrap().values().cloned().collect()
	}

	pub(crate) fn get_peer(&self, node_id: &PublicKey) -> Option<PeerInfo> {
		self.peers.read().unwrap().get(node_id).cloned()
	}

	fn persist_peers(&self, locked_peers: &HashMap<PublicKey, PeerInfo>) -> Result<(), Error> {
		let data = PeerStoreSerWrapper(&*locked_peers).encode();
		KVStoreSync::write(
			&*self.kv_store,
			PEER_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
			PEER_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
			PEER_INFO_PERSISTENCE_KEY,
			data,
		)
		.map_err(|e| {
			log_error!(
				self.logger,
				"Write for key {}/{}/{} failed due to: {}",
				PEER_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
				PEER_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
				PEER_INFO_PERSISTENCE_KEY,
				e
			);
			Error::PersistenceFailed
		})?;
		Ok(())
	}
}

impl<L: Deref> ReadableArgs<(Arc<DynStore>, L)> for PeerStore<L>
where
	L::Target: LdkLogger,
{
	#[inline]
	fn read<R: lightning::io::Read>(
		reader: &mut R, args: (Arc<DynStore>, L),
	) -> Result<Self, lightning::ln::msgs::DecodeError> {
		let (kv_store, logger) = args;
		let read_peers: PeerStoreDeserWrapper = Readable::read(reader)?;
		let peers: RwLock<HashMap<PublicKey, PeerInfo>> = RwLock::new(read_peers.0);
		Ok(Self { peers, kv_store, logger })
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PeerStoreDeserWrapper(HashMap<PublicKey, PeerInfo>);

impl Readable for PeerStoreDeserWrapper {
	fn read<R: lightning::io::Read>(
		reader: &mut R,
	) -> Result<Self, lightning::ln::msgs::DecodeError> {
		let len: u16 = Readable::read(reader)?;
		let mut peers = HashMap::with_capacity(len as usize);
		for _ in 0..len {
			let k: PublicKey = Readable::read(reader)?;
			let v: PeerInfo = Readable::read(reader)?;
			peers.insert(k, v);
		}
		Ok(Self(peers))
	}
}

pub(crate) struct PeerStoreSerWrapper<'a>(&'a HashMap<PublicKey, PeerInfo>);

impl Writeable for PeerStoreSerWrapper<'_> {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), lightning::io::Error> {
		(self.0.len() as u16).write(writer)?;
		for (k, v) in self.0.iter() {
			k.write(writer)?;
			v.write(writer)?;
		}
		Ok(())
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PeerInfo {
	pub node_id: PublicKey,
	pub address: SocketAddress,
}

impl_writeable_tlv_based!(PeerInfo, {
	(0, node_id, required),
	(2, address, required),
});

pub(crate) fn persist_missing_channel_peers<L, I>(
	counterparty_node_ids: I, network_graph: &Graph, peer_store: &PeerStore<L>, logger: L,
) where
	L: Deref,
	L::Target: LdkLogger,
	I: IntoIterator<Item = PublicKey>,
{
	persist_missing_channel_peers_excluding(
		counterparty_node_ids,
		network_graph,
		peer_store,
		&HashSet::new(),
		logger,
	)
}

pub(crate) fn persist_missing_channel_peers_excluding<L, I>(
	counterparty_node_ids: I, network_graph: &Graph, peer_store: &PeerStore<L>,
	excluded_node_ids: &HashSet<PublicKey>, logger: L,
) where
	L: Deref,
	L::Target: LdkLogger,
	I: IntoIterator<Item = PublicKey>,
{
	let graph = network_graph.read_only();
	let mut seen = HashSet::new();
	let missing_peers = counterparty_node_ids
		.into_iter()
		.filter_map(|counterparty_node_id| {
			if !seen.insert(counterparty_node_id)
				|| excluded_node_ids.contains(&counterparty_node_id)
				|| peer_store.get_peer(&counterparty_node_id).is_some()
			{
				return None;
			}

			graph
				.nodes()
				.get(&NodeId::from_pubkey(&counterparty_node_id))
				.and_then(|node_info| node_info.announcement_info.as_ref())
				.and_then(|announcement_info| announcement_info.addresses().first())
				.cloned()
				.map(|address| PeerInfo { node_id: counterparty_node_id, address })
		})
		.collect::<Vec<_>>();
	drop(graph);

	for peer_info in missing_peers {
		let node_id = peer_info.node_id;
		if peer_store.get_peer(&node_id).is_some() {
			continue;
		}

		match peer_store.add_peer(peer_info) {
			Ok(()) => {
				log_info!(logger, "Persisted peer {} from channel counterparty", node_id)
			},
			Err(e) => {
				log_error!(logger, "Failed to persist peer {}: {}", node_id, e)
			},
		}
	}
}

#[cfg(test)]
mod tests {
	use std::collections::HashMap;
	use std::future::Future;
	use std::pin::Pin;
	use std::str::FromStr;
	use std::sync::atomic::{AtomicBool, Ordering};
	use std::sync::{Arc, Mutex};

	use bitcoin::Network;
	use lightning::io;
	use lightning::ln::msgs::UnsignedNodeAnnouncement;
	use lightning::routing::gossip::{NodeAlias, NodeId};
	use lightning::types::features::{ChannelFeatures, NodeFeatures};
	use lightning::util::persist::KVStore;
	use lightning::util::test_utils::TestLogger;

	use super::*;
	use crate::io::test_utils::InMemoryStore;
	use crate::logger::Logger;

	#[test]
	fn peer_info_persistence() {
		let store: Arc<DynStore> = Arc::new(InMemoryStore::new());
		let logger = Arc::new(TestLogger::new());
		let peer_store = PeerStore::new(Arc::clone(&store), Arc::clone(&logger));

		let node_id = PublicKey::from_str(
			"0276607124ebe6a6c9338517b6f485825b27c2dcc0b9fc2aa6a4c0df91194e5993",
		)
		.unwrap();
		let address = SocketAddress::from_str("127.0.0.1:9738").unwrap();
		let expected_peer_info = PeerInfo { node_id, address };
		assert!(KVStoreSync::read(
			&*store,
			PEER_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
			PEER_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
			PEER_INFO_PERSISTENCE_KEY,
		)
		.is_err());
		peer_store.add_peer(expected_peer_info.clone()).unwrap();

		// Check we can read back what we persisted.
		let persisted_bytes = KVStoreSync::read(
			&*store,
			PEER_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
			PEER_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
			PEER_INFO_PERSISTENCE_KEY,
		)
		.unwrap();
		let deser_peer_store =
			PeerStore::read(&mut &persisted_bytes[..], (Arc::clone(&store), logger)).unwrap();

		let peers = deser_peer_store.list_peers();
		assert_eq!(peers.len(), 1);
		assert_eq!(peers[0], expected_peer_info);
		assert_eq!(deser_peer_store.get_peer(&node_id), Some(expected_peer_info));
	}

	#[test]
	fn peer_address_updated_on_readd() {
		let store: Arc<DynStore> = Arc::new(InMemoryStore::new());
		let logger = Arc::new(TestLogger::new());
		let peer_store = PeerStore::new(Arc::clone(&store), Arc::clone(&logger));

		let node_id = PublicKey::from_str(
			"0276607124ebe6a6c9338517b6f485825b27c2dcc0b9fc2aa6a4c0df91194e5993",
		)
		.unwrap();
		let old_address = SocketAddress::from_str("34.65.186.40:9735").unwrap();
		let new_address = SocketAddress::from_str("34.65.153.174:9735").unwrap();

		peer_store.add_peer(PeerInfo { node_id, address: old_address.clone() }).unwrap();
		assert_eq!(peer_store.get_peer(&node_id).unwrap().address, old_address);

		peer_store.add_peer(PeerInfo { node_id, address: new_address.clone() }).unwrap();
		assert_eq!(peer_store.get_peer(&node_id).unwrap().address, new_address);

		assert_eq!(peer_store.list_peers().len(), 1);

		let persisted_bytes = KVStoreSync::read(
			&*store,
			PEER_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
			PEER_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
			PEER_INFO_PERSISTENCE_KEY,
		)
		.unwrap();
		let deser_peer_store =
			PeerStore::read(&mut &persisted_bytes[..], (Arc::clone(&store), logger)).unwrap();
		assert_eq!(deser_peer_store.get_peer(&node_id).unwrap().address, new_address);
	}

	#[test]
	fn peer_same_address_skips_persist() {
		let store: Arc<DynStore> = Arc::new(InMemoryStore::new());
		let logger = Arc::new(TestLogger::new());
		let peer_store = PeerStore::new(Arc::clone(&store), Arc::clone(&logger));

		let node_id = PublicKey::from_str(
			"0276607124ebe6a6c9338517b6f485825b27c2dcc0b9fc2aa6a4c0df91194e5993",
		)
		.unwrap();
		let address = SocketAddress::from_str("127.0.0.1:9738").unwrap();

		peer_store.add_peer(PeerInfo { node_id, address: address.clone() }).unwrap();

		peer_store.add_peer(PeerInfo { node_id, address }).unwrap();
		assert_eq!(peer_store.list_peers().len(), 1);
	}

	#[test]
	fn peer_add_persistence_failure_leaves_peer_retryable() {
		let store: Arc<DynStore> = Arc::new(FailFirstWriteStore::new());
		let logger = Arc::new(TestLogger::new());
		let peer_store = PeerStore::new(Arc::clone(&store), Arc::clone(&logger));

		let node_id = PublicKey::from_str(
			"0276607124ebe6a6c9338517b6f485825b27c2dcc0b9fc2aa6a4c0df91194e5993",
		)
		.unwrap();
		let address = SocketAddress::from_str("127.0.0.1:9738").unwrap();
		let peer_info = PeerInfo { node_id, address };

		assert!(matches!(peer_store.add_peer(peer_info.clone()), Err(Error::PersistenceFailed)));
		assert!(peer_store.get_peer(&node_id).is_none());

		peer_store.add_peer(peer_info.clone()).unwrap();
		assert_eq!(peer_store.get_peer(&node_id), Some(peer_info));
	}

	#[test]
	fn missing_channel_peer_is_persisted_from_graph() {
		let store: Arc<DynStore> = Arc::new(InMemoryStore::new());
		let logger = Arc::new(Logger::new_log_facade());
		let peer_store = PeerStore::new(Arc::clone(&store), Arc::clone(&logger));
		let network_graph = Graph::new(Network::Regtest.into(), Arc::clone(&logger));

		let node_id = PublicKey::from_str(
			"0276607124ebe6a6c9338517b6f485825b27c2dcc0b9fc2aa6a4c0df91194e5993",
		)
		.unwrap();
		let other_node_id = PublicKey::from_str(
			"02eec7245d6b7d2ccb30380bfbe2a3648cd7a942653f5aa340edcea1f283686619",
		)
		.unwrap();
		let address = SocketAddress::from_str("127.0.0.1:9738").unwrap();

		network_graph
			.add_channel_from_partial_announcement(
				42,
				None,
				0,
				ChannelFeatures::empty(),
				NodeId::from_pubkey(&node_id),
				NodeId::from_pubkey(&other_node_id),
			)
			.unwrap();
		network_graph
			.update_node_from_unsigned_announcement(&UnsignedNodeAnnouncement {
				features: NodeFeatures::empty(),
				timestamp: 1,
				node_id: NodeId::from_pubkey(&node_id),
				rgb: [0; 3],
				alias: NodeAlias([0; 32]),
				addresses: vec![address.clone()],
				excess_address_data: Vec::new(),
				excess_data: Vec::new(),
			})
			.unwrap();

		persist_missing_channel_peers(vec![node_id], &network_graph, &peer_store, logger);

		let peer = peer_store.get_peer(&node_id).unwrap();
		assert_eq!(peer.node_id, node_id);
		assert_eq!(peer.address, address);
	}

	#[test]
	fn missing_channel_peer_without_announced_address_is_skipped() {
		let store: Arc<DynStore> = Arc::new(InMemoryStore::new());
		let logger = Arc::new(Logger::new_log_facade());
		let peer_store = PeerStore::new(Arc::clone(&store), Arc::clone(&logger));
		let network_graph = Graph::new(Network::Regtest.into(), Arc::clone(&logger));

		let node_id = PublicKey::from_str(
			"0276607124ebe6a6c9338517b6f485825b27c2dcc0b9fc2aa6a4c0df91194e5993",
		)
		.unwrap();

		persist_missing_channel_peers(vec![node_id], &network_graph, &peer_store, logger);

		assert!(peer_store.get_peer(&node_id).is_none());
	}

	#[test]
	fn missing_channel_peer_is_persisted_after_graph_retry() {
		let store: Arc<DynStore> = Arc::new(InMemoryStore::new());
		let logger = Arc::new(Logger::new_log_facade());
		let peer_store = PeerStore::new(Arc::clone(&store), Arc::clone(&logger));
		let network_graph = Graph::new(Network::Regtest.into(), Arc::clone(&logger));

		let node_id = PublicKey::from_str(
			"0276607124ebe6a6c9338517b6f485825b27c2dcc0b9fc2aa6a4c0df91194e5993",
		)
		.unwrap();
		let other_node_id = PublicKey::from_str(
			"02eec7245d6b7d2ccb30380bfbe2a3648cd7a942653f5aa340edcea1f283686619",
		)
		.unwrap();
		let address = SocketAddress::from_str("127.0.0.1:9738").unwrap();

		persist_missing_channel_peers(
			vec![node_id],
			&network_graph,
			&peer_store,
			Arc::clone(&logger),
		);
		assert!(peer_store.get_peer(&node_id).is_none());

		network_graph
			.add_channel_from_partial_announcement(
				42,
				None,
				0,
				ChannelFeatures::empty(),
				NodeId::from_pubkey(&node_id),
				NodeId::from_pubkey(&other_node_id),
			)
			.unwrap();
		network_graph
			.update_node_from_unsigned_announcement(&UnsignedNodeAnnouncement {
				features: NodeFeatures::empty(),
				timestamp: 1,
				node_id: NodeId::from_pubkey(&node_id),
				rgb: [0; 3],
				alias: NodeAlias([0; 32]),
				addresses: vec![address.clone()],
				excess_address_data: Vec::new(),
				excess_data: Vec::new(),
			})
			.unwrap();

		persist_missing_channel_peers(vec![node_id], &network_graph, &peer_store, logger);

		let peer = peer_store.get_peer(&node_id).unwrap();
		assert_eq!(peer.node_id, node_id);
		assert_eq!(peer.address, address);
	}

	#[test]
	fn excluded_missing_channel_peer_is_not_persisted_from_graph() {
		let store: Arc<DynStore> = Arc::new(InMemoryStore::new());
		let logger = Arc::new(Logger::new_log_facade());
		let peer_store = PeerStore::new(Arc::clone(&store), Arc::clone(&logger));
		let network_graph = Graph::new(Network::Regtest.into(), Arc::clone(&logger));

		let node_id = PublicKey::from_str(
			"0276607124ebe6a6c9338517b6f485825b27c2dcc0b9fc2aa6a4c0df91194e5993",
		)
		.unwrap();
		let other_node_id = PublicKey::from_str(
			"02eec7245d6b7d2ccb30380bfbe2a3648cd7a942653f5aa340edcea1f283686619",
		)
		.unwrap();
		let address = SocketAddress::from_str("127.0.0.1:9738").unwrap();
		let excluded_node_ids = HashSet::from([node_id]);

		network_graph
			.add_channel_from_partial_announcement(
				42,
				None,
				0,
				ChannelFeatures::empty(),
				NodeId::from_pubkey(&node_id),
				NodeId::from_pubkey(&other_node_id),
			)
			.unwrap();
		network_graph
			.update_node_from_unsigned_announcement(&UnsignedNodeAnnouncement {
				features: NodeFeatures::empty(),
				timestamp: 1,
				node_id: NodeId::from_pubkey(&node_id),
				rgb: [0; 3],
				alias: NodeAlias([0; 32]),
				addresses: vec![address],
				excess_address_data: Vec::new(),
				excess_data: Vec::new(),
			})
			.unwrap();

		persist_missing_channel_peers_excluding(
			vec![node_id],
			&network_graph,
			&peer_store,
			&excluded_node_ids,
			logger,
		);

		assert!(peer_store.get_peer(&node_id).is_none());
	}

	struct FailFirstWriteStore {
		persisted_bytes: Mutex<HashMap<String, HashMap<String, Vec<u8>>>>,
		fail_next_write: AtomicBool,
	}

	impl FailFirstWriteStore {
		fn new() -> Self {
			Self {
				persisted_bytes: Mutex::new(HashMap::new()),
				fail_next_write: AtomicBool::new(true),
			}
		}

		fn read_internal(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
		) -> io::Result<Vec<u8>> {
			let persisted_lock = self.persisted_bytes.lock().unwrap();
			let prefixed = format!("{}/{}", primary_namespace, secondary_namespace);
			persisted_lock
				.get(&prefixed)
				.and_then(|inner| inner.get(key))
				.cloned()
				.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Key not found"))
		}

		fn write_internal(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
		) -> io::Result<()> {
			if self.fail_next_write.swap(false, Ordering::Relaxed) {
				return Err(io::Error::new(io::ErrorKind::Other, "Injected write failure"));
			}

			let mut persisted_lock = self.persisted_bytes.lock().unwrap();
			let prefixed = format!("{}/{}", primary_namespace, secondary_namespace);
			persisted_lock
				.entry(prefixed)
				.or_insert_with(HashMap::new)
				.insert(key.to_string(), buf);
			Ok(())
		}

		fn remove_internal(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
		) -> io::Result<()> {
			let mut persisted_lock = self.persisted_bytes.lock().unwrap();
			let prefixed = format!("{}/{}", primary_namespace, secondary_namespace);
			if let Some(inner) = persisted_lock.get_mut(&prefixed) {
				inner.remove(key);
			}
			Ok(())
		}

		fn list_internal(
			&self, primary_namespace: &str, secondary_namespace: &str,
		) -> io::Result<Vec<String>> {
			let persisted_lock = self.persisted_bytes.lock().unwrap();
			let prefixed = format!("{}/{}", primary_namespace, secondary_namespace);
			Ok(persisted_lock
				.get(&prefixed)
				.map(|inner| inner.keys().cloned().collect())
				.unwrap_or_default())
		}
	}

	impl KVStore for FailFirstWriteStore {
		fn read(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
		) -> Pin<Box<dyn Future<Output = io::Result<Vec<u8>>> + Send + 'static>> {
			let res = self.read_internal(primary_namespace, secondary_namespace, key);
			Box::pin(async move { res })
		}

		fn write(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
		) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + 'static>> {
			let res = self.write_internal(primary_namespace, secondary_namespace, key, buf);
			Box::pin(async move { res })
		}

		fn remove(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, _lazy: bool,
		) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + 'static>> {
			let res = self.remove_internal(primary_namespace, secondary_namespace, key);
			Box::pin(async move { res })
		}

		fn list(
			&self, primary_namespace: &str, secondary_namespace: &str,
		) -> Pin<Box<dyn Future<Output = io::Result<Vec<String>>> + Send + 'static>> {
			let res = self.list_internal(primary_namespace, secondary_namespace);
			Box::pin(async move { res })
		}
	}

	impl KVStoreSync for FailFirstWriteStore {
		fn read(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
		) -> io::Result<Vec<u8>> {
			self.read_internal(primary_namespace, secondary_namespace, key)
		}

		fn write(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
		) -> io::Result<()> {
			self.write_internal(primary_namespace, secondary_namespace, key, buf)
		}

		fn remove(
			&self, primary_namespace: &str, secondary_namespace: &str, key: &str, _lazy: bool,
		) -> io::Result<()> {
			self.remove_internal(primary_namespace, secondary_namespace, key)
		}

		fn list(
			&self, primary_namespace: &str, secondary_namespace: &str,
		) -> io::Result<Vec<String>> {
			self.list_internal(primary_namespace, secondary_namespace)
		}
	}
}

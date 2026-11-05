// Test-only loopback VSS transport; public fixed seed, ephemeral local storage, no funds.
use super::*;
use crate::bitcoin::Network;
use crate::builder::NodeBuilder;
use crate::config::Config;
use prost::Message;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::AtomicBool;
use std::thread;
use vss_client::types::*;

struct Fixture {
	url: String,
	stop: Arc<AtomicBool>,
	server: Option<thread::JoinHandle<()>>,
	writes: Arc<std::sync::atomic::AtomicUsize>,
}

#[path = "shutdown_handoff_tests.rs"]
mod handoff_tests;

fn request(socket: &mut TcpStream) -> (String, Vec<u8>) {
	// Accepted sockets inherit nonblocking mode on some platforms.
	socket.set_nonblocking(false).unwrap();
	socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
	let mut header = Vec::new();
	while !header.ends_with(b"\r\n\r\n") {
		let mut byte = [0];
		socket.read_exact(&mut byte).unwrap();
		header.push(byte[0]);
		assert!(header.len() < 16384);
	}
	let header = String::from_utf8(header).unwrap();
	let path = header.lines().next().unwrap().split_whitespace().nth(1).unwrap().to_owned();
	let length = header
		.lines()
		.find_map(|line| {
			let (name, value) = line.split_once(':')?;
			name.eq_ignore_ascii_case("content-length")
				.then(|| value.trim().parse::<usize>().unwrap())
		})
		.unwrap_or(0);
	let mut body = vec![0; length];
	socket.read_exact(&mut body).unwrap();
	(path, body)
}

impl Fixture {
	fn new() -> Self {
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		listener.set_nonblocking(true).unwrap();
		let url = format!("http://{}", listener.local_addr().unwrap());
		let stop = Arc::new(AtomicBool::new(false));
		let stopped = stop.clone();
		let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
		let written = writes.clone();
		let server = thread::spawn(move || {
			let mut values = HashMap::<String, KeyValue>::new();
			while !stopped.load(Ordering::Acquire) {
				let mut socket = match listener.accept() {
					Ok((socket, _)) => socket,
					Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
						thread::sleep(Duration::from_millis(1));
						continue;
					},
					Err(e) => panic!("loopback accept failed: {e}"),
				};
				let (path, body) = request(&mut socket);
				let mut status = "200 OK";
				let response = match path.as_str() {
					"/getObject" => {
						let request = GetObjectRequest::decode(&*body).unwrap();
						match values.get(&request.key) {
							Some(value) => {
								GetObjectResponse { value: Some(value.clone()) }.encode_to_vec()
							},
							None => {
								status = "404 Not Found";
								ErrorResponse {
									error_code: ErrorCode::NoSuchKeyException as i32,
									message: "absent".to_owned(),
								}
								.encode_to_vec()
							},
						}
					},
					"/putObjects" => {
						let request = PutObjectRequest::decode(&*body).unwrap();
						for value in request.transaction_items {
							values.insert(value.key.clone(), value);
						}
						for value in request.delete_items {
							values.remove(&value.key);
						}
						written.fetch_add(1, Ordering::AcqRel);
						PutObjectResponse {}.encode_to_vec()
					},
					"/deleteObject" => {
						let request = DeleteObjectRequest::decode(&*body).unwrap();
						values.remove(&request.key_value.unwrap().key);
						DeleteObjectResponse {}.encode_to_vec()
					},
					"/listKeyVersions" => {
						let request = ListKeyVersionsRequest::decode(&*body).unwrap();
						let prefix = request.key_prefix.unwrap_or_default();
						let key_versions = values
							.values()
							.filter(|value| value.key.starts_with(&prefix))
							.map(|value| KeyValue {
								key: value.key.clone(),
								version: value.version,
								value: Vec::new(),
							})
							.collect();
						ListKeyVersionsResponse {
							key_versions,
							next_page_token: Some(String::new()),
							global_version: None,
						}
						.encode_to_vec()
					},
					"/fee-estimates" => br#"{"1":1,"2":1,"6":1,"12":1,"144":1}"#.to_vec(),
					_ => {
						status = "404 Not Found";
						Vec::new()
					},
				};
				write!(
					socket,
					"HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
					response.len()
				)
				.unwrap();
				let _ = socket.write_all(&response);
			}
		});
		Self { url, stop, server: Some(server), writes }
	}

	fn node(&self) -> crate::Node {
		self.node_with_listener(42, None)
	}

	fn node_with_listener(
		&self, seed: u8, address: Option<lightning::ln::msgs::SocketAddress>,
	) -> crate::Node {
		let path = std::env::temp_dir().join(format!("ldk-quiescence-{}", rand::random::<u64>()));
		let config = Config {
			network: Network::Regtest,
			storage_dir_path: path.to_str().unwrap().to_owned(),
			..Config::default()
		};
		let mut builder = NodeBuilder::from_config(config);
		builder.set_entropy_seed_bytes([seed; 64]);
		if let Some(address) = address {
			builder.set_listening_addresses(vec![address]).unwrap();
		}
		builder.set_log_facade_logger();
		builder.set_chain_source_esplora(self.url.clone(), None);
		builder
			.build_with_vss_store_and_header_provider(
				self.url.clone(),
				"fixture".to_owned(),
				Arc::new(vss_client::headers::FixedHeaders::new(HashMap::new())),
			)
			.unwrap()
	}
}

impl Drop for Fixture {
	fn drop(&mut self) {
		self.stop.store(true, Ordering::Release);
		let result = self.server.take().unwrap().join();
		if !thread::panicking() {
			result.unwrap();
		}
	}
}

#[test]
fn consumed_node_refuses_retained_children_then_finishes_original_store() {
	let fixture = Fixture::new();
	let node = fixture.node();
	let child = node.onchain_payment();
	let node = match node.try_into_shutdown() {
		Err(node) => *node,
		Ok(_) => panic!("retained child admitted"),
	};
	// Refusal leaves the actual original API and encrypted store usable.
	child.new_address().unwrap();
	drop(child);
	let lsps = node.lsps1_liquidity();
	let clone = lsps.clone();
	drop(lsps);
	let node = match node.try_into_shutdown() {
		Err(node) => *node,
		Ok(_) => panic!("retained LSPS clone admitted"),
	};
	drop(clone);
	let original_store = node.shutdown_store.as_ref().unwrap().clone();
	let mut owner = match node.try_into_shutdown() {
		Ok(owner) => owner,
		Err(_) => panic!("exclusive original refused"),
	};
	assert_eq!(owner.wait(Duration::from_secs(5)), Ok(()));
	let writes = fixture.writes.load(Ordering::Acquire);
	assert_eq!(owner.wait(Duration::ZERO), Ok(()));
	assert_eq!(fixture.writes.load(Ordering::Acquire), writes);
	assert!(lightning::util::persist::KVStoreSync::write(
		&*original_store,
		"",
		"",
		"late",
		vec![1]
	)
	.is_err());
}

#[test]
fn running_node_finishes_final_persistence_before_closing_vss() {
	let fixture = Fixture::new();
	let node = fixture.node();
	node.start().unwrap();
	let writes_before = fixture.writes.load(Ordering::Acquire);
	let mut owner = match node.try_into_shutdown() {
		Ok(owner) => owner,
		Err(_) => panic!("exclusive original refused"),
	};
	assert_eq!(owner.wait(Duration::from_secs(10)), Ok(()));
	assert!(fixture.writes.load(Ordering::Acquire) > writes_before);
	assert_eq!(owner.wait(Duration::ZERO), Ok(()));
}

fn shutdown(node: crate::Node) -> NodeShutdown {
	match node.try_into_shutdown() {
		Ok(owner) => owner,
		Err(_) => panic!("exclusive fixture node refused"),
	}
}

#[test]
fn retained_owner_waits_for_background_then_processor_without_replay() {
	let fixture = Fixture::new();
	let node = fixture.node();
	let (release_background, background) = tokio::sync::oneshot::channel();
	let (release_processor, processor) = tokio::sync::oneshot::channel();
	node.runtime.spawn_background_task(async move {
		background.await.unwrap();
	});
	node.runtime.spawn_background_processor_task(
		async move {
			processor.await.unwrap();
			Ok(())
		},
		|_| {},
	);
	let mut owner = shutdown(node);
	assert_eq!(owner.wait(Duration::from_millis(20)), Err(ShutdownError::TimedOut));
	release_background.send(()).unwrap();
	assert_eq!(owner.wait(Duration::from_millis(20)), Err(ShutdownError::TimedOut));
	release_processor.send(()).unwrap();
	assert_eq!(owner.wait(Duration::from_secs(2)), Ok(()));
	let writes = fixture.writes.load(Ordering::Acquire);
	assert_eq!(owner.wait(Duration::ZERO), Ok(()));
	assert_eq!(fixture.writes.load(Ordering::Acquire), writes);
}

#[test]
fn persistence_panic_and_cancelled_background_failures_remain_on_original_owner() {
	for failure in [ShutdownError::PersistenceFailed, ShutdownError::TaskFailed] {
		let fixture = Fixture::new();
		let node = fixture.node();
		node.runtime.spawn_background_processor_task(
			async move {
				if failure == ShutdownError::TaskFailed {
					panic!("controlled processor panic");
				}
				Err(lightning::io::Error::new(
					lightning::io::ErrorKind::Other,
					"controlled persistence failure",
				))
			},
			|_| {},
		);
		let mut owner = shutdown(node);
		assert_eq!(owner.wait(Duration::from_secs(2)), Err(failure));
		assert_eq!(owner.wait(Duration::ZERO), Err(failure));
	}
	let fixture = Fixture::new();
	let node = fixture.node();
	node.runtime.spawn_background_task(std::future::pending());
	node.runtime.cancel_tracked_tasks();
	let mut owner = shutdown(node);
	assert_eq!(owner.wait(Duration::from_secs(2)), Err(ShutdownError::TaskFailed));
	assert_eq!(owner.wait(Duration::ZERO), Err(ShutdownError::TaskFailed));
}

#[test]
fn peer_event_worker_clone_is_retained_and_panic_is_sticky() {
	for panic in [false, true] {
		let fixture = Fixture::new();
		let node = fixture.node();
		let reader = node.connection_manager.tracked_peer_manager().unwrap();
		let event_worker = reader.clone();
		drop(reader);
		let mut owner = shutdown(node);
		assert_eq!(owner.wait(Duration::from_millis(20)), Err(ShutdownError::TimedOut));
		if panic {
			assert!(thread::spawn(move || {
				let _worker = event_worker;
				panic!("controlled peer event worker panic");
			})
			.join()
			.is_err());
		} else {
			drop(event_worker);
		}
		let expected = if panic { Err(ShutdownError::TaskFailed) } else { Ok(()) };
		assert_eq!(owner.wait(Duration::from_secs(2)), expected);
		assert_eq!(owner.wait(Duration::ZERO), expected);
	}
}

#[test]
fn closed_connection_admission_refuses_both_callers_without_waiting() {
	let fixture = Fixture::new();
	let node = fixture.node();
	node.connection_manager.close_admission();
	let addr = lightning::ln::msgs::SocketAddress::TcpIpV4 { addr: [127, 0, 0, 1], port: 1 };
	let node_id = node.node_id();
	for _ in 0..2 {
		let result = node.runtime.block_on(async {
			tokio::time::timeout(
				Duration::from_millis(100),
				node.connection_manager.do_connect_peer(node_id, addr.clone()),
			)
			.await
		});
		assert_eq!(result.unwrap(), Err(crate::Error::NotRunning));
	}
	let mut owner = shutdown(node);
	assert_eq!(owner.wait(Duration::from_secs(2)), Ok(()));
}

#[test]
fn successful_final_graph_write_does_not_erase_earlier_failure() {
	let fixture = Fixture::new();
	let node = fixture.node();
	let path = std::env::temp_dir().join(format!("ldk-graph-failure-{}", rand::random::<u64>()));
	std::fs::create_dir(&path).unwrap();
	let cache = path.join(crate::io::local_graph_store::NETWORK_GRAPH_LOCAL_CACHE_FILENAME);
	// A directory at the target makes the actual local file write fail deterministically.
	std::fs::create_dir(&cache).unwrap();
	let store = crate::io::local_graph_store::LocalGraphStore::new(
		node.shutdown_store.as_ref().unwrap().clone(),
		path.to_str().unwrap().to_owned(),
		Arc::new(std::sync::atomic::AtomicU32::new(0)),
		node.runtime.persistence_recorder(),
	);
	let write = || {
		lightning::util::persist::KVStoreSync::write(
			&store,
			lightning::util::persist::NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
			lightning::util::persist::NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
			lightning::util::persist::NETWORK_GRAPH_PERSISTENCE_KEY,
			vec![1],
		)
	};
	assert!(write().is_err());
	std::fs::remove_dir(&cache).unwrap();
	assert!(write().is_ok());
	drop(store);
	let mut owner = shutdown(node);
	assert_eq!(owner.wait(Duration::from_secs(2)), Err(ShutdownError::PersistenceFailed));
	assert_eq!(owner.wait(Duration::ZERO), Err(ShutdownError::PersistenceFailed));
}

#[test]
fn ordinary_stop_can_still_restart_and_borrowed_runtime_is_refused_unchanged() {
	let fixture = Fixture::new();
	let node = fixture.node();
	node.start().unwrap();
	node.stop().unwrap();
	node.start().unwrap();
	assert!(node.status().is_running);
	let mut owner = shutdown(node);
	assert_eq!(owner.wait(Duration::from_secs(5)), Ok(()));
	assert_eq!(owner.wait(Duration::ZERO), Ok(()));
	drop(owner);
	let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
	let node = runtime.block_on(async { fixture.node() });
	let node = match node.try_into_shutdown() {
		Err(node) => node,
		Ok(_) => panic!("borrowed runtime admitted"),
	};
	assert!(!node.strict_shutdown.load(Ordering::Acquire));
	node.onchain_payment().new_address().unwrap();
	drop(node);
}

#[test]
fn ordinary_stop_with_inbound_peer_can_restart_and_finish_shutdown() {
	let fixture = Fixture::new();
	let listener = TcpListener::bind("127.0.0.1:0").unwrap();
	let port = listener.local_addr().unwrap().port();
	drop(listener);
	let address = lightning::ln::msgs::SocketAddress::TcpIpV4 { addr: [127, 0, 0, 1], port };
	let node = fixture.node_with_listener(42, Some(address.clone()));
	node.start().unwrap();
	let peer_fixture = Fixture::new();
	let peer = peer_fixture.node_with_listener(43, None);
	peer.start().unwrap();
	peer.connect(node.node_id(), address, false).unwrap();
	let deadline = std::time::Instant::now() + Duration::from_secs(5);
	while node.peer_manager.peer_by_node_id(&peer.node_id()).is_none() {
		assert!(std::time::Instant::now() < deadline, "inbound handshake did not complete");
		thread::sleep(Duration::from_millis(1));
	}
	node.stop().unwrap();
	node.start().unwrap();
	assert!(node.status().is_running);
	let mut owner = shutdown(node);
	assert_eq!(owner.wait(Duration::from_secs(5)), Ok(()));
	assert_eq!(owner.wait(Duration::ZERO), Ok(()));
	peer.stop().unwrap();
}

#[test]
fn late_handshake_is_disconnected_after_background_completion() {
	let remote_fixture = Fixture::new();
	let listener = TcpListener::bind("127.0.0.1:0").unwrap();
	let socket_addr = listener.local_addr().unwrap();
	drop(listener);
	let remote = remote_fixture.node_with_listener(
		43,
		Some(lightning::ln::msgs::SocketAddress::TcpIpV4 {
			addr: [127, 0, 0, 1],
			port: socket_addr.port(),
		}),
	);
	remote.start().unwrap();
	let fixture = Fixture::new();
	let node = fixture.node();
	node.start().unwrap();
	let peer = node.connection_manager.tracked_peer_manager().unwrap();
	let peer_manager = Arc::clone(&node.peer_manager);
	let remote_id = remote.node_id();
	let (release, held) = tokio::sync::oneshot::channel();
	let (connected, observed) = std::sync::mpsc::sync_channel(1);
	node.runtime.spawn_background_task(async move {
		held.await.unwrap();
		let closed =
			lightning_net_tokio::connect_outbound(peer, remote_id, socket_addr).await.unwrap();
		tokio::pin!(closed);
		let handshake = async {
			while peer_manager.peer_by_node_id(&remote_id).is_none() {
				tokio::select! {
					_ = &mut closed => panic!("peer closed before handshake"),
					_ = tokio::time::sleep(Duration::from_millis(1)) => {},
				}
			}
		};
		tokio::time::timeout(Duration::from_secs(5), handshake).await.unwrap();
		connected.send(()).unwrap();
	});
	let mut owner = shutdown(node);
	assert_eq!(owner.wait(Duration::from_millis(20)), Err(ShutdownError::TimedOut));
	// Force completion after this iteration's initial disconnect and before its final drain.
	owner.after_background_disconnect = Some(Box::new(move || {
		release.send(()).unwrap();
		observed.recv_timeout(Duration::from_secs(6)).unwrap();
	}));
	assert_eq!(owner.wait(Duration::from_secs(5)), Ok(()));
	assert_eq!(owner.wait(Duration::ZERO), Ok(()));
	remote.stop().unwrap();
}

#[test]
fn unsupported_store_returns_original_node_without_closing_admission() {
	let path = std::env::temp_dir().join(format!("ldk-custom-shutdown-{}", rand::random::<u64>()));
	let config = Config {
		network: Network::Regtest,
		storage_dir_path: path.to_str().unwrap().to_owned(),
		..Config::default()
	};
	let mut builder = NodeBuilder::from_config(config);
	builder.set_entropy_seed_bytes([44; 64]);
	builder.set_log_facade_logger();
	let node = builder.build().unwrap();
	let node = match node.try_into_shutdown() {
		Err(node) => node,
		Ok(_) => panic!("unsupported store admitted"),
	};
	assert!(!node.strict_shutdown.load(Ordering::Acquire));
	let peer = node.connection_manager.tracked_peer_manager().unwrap();
	drop(peer);
	node.onchain_payment().new_address().unwrap();
}

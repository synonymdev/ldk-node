// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::boxed::Box;
use std::collections::HashMap;
use std::future::Future;
#[cfg(test)]
use std::panic::RefUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::time::Duration;
use std::{error, fmt};

use bitcoin::hashes::{sha256, Hash, HashEngine, Hmac, HmacEngine};
use lightning::impl_writeable_tlv_based_enum;
use lightning::io::{self, Error, ErrorKind};
use lightning::util::persist::{KVStore, KVStoreSync};
use lightning::util::ser::{Readable, Writeable};
use prost::Message;
use rand::RngCore;
use vss_client::client::VssClient;
use vss_client::error::VssError;
use vss_client::headers::VssHeaderProvider;
use vss_client::types::{
	DeleteObjectRequest, GetObjectRequest, KeyValue, ListKeyVersionsRequest, PutObjectRequest,
	Storable,
};
use vss_client::util::key_obfuscator::KeyObfuscator;
use vss_client::util::retry::{
	retry, ExponentialBackoffRetryPolicy, FilteredRetryPolicy, JitteredRetryPolicy,
	MaxAttemptsRetryPolicy, MaxTotalDelayRetryPolicy, RetryPolicy,
};
use vss_client::util::storable_builder::{EntropySource, StorableBuilder};

use crate::io::utils::check_namespace_key_validity;
use crate::runtime::StoreRuntime;

type CustomRetryPolicy = FilteredRetryPolicy<
	JitteredRetryPolicy<
		MaxTotalDelayRetryPolicy<MaxAttemptsRetryPolicy<ExponentialBackoffRetryPolicy<VssError>>>,
	>,
	Box<dyn Fn(&VssError) -> bool + 'static + Send + Sync>,
>;

type WriteRetryPolicy = FilteredRetryPolicy<
	JitteredRetryPolicy<
		MaxTotalDelayRetryPolicy<
			MaxAttemptsRetryPolicy<ExponentialBackoffRetryPolicy<FencedWriteError>>,
		>,
	>,
	Box<dyn Fn(&FencedWriteError) -> bool + 'static + Send + Sync>,
>;

#[derive(Debug, PartialEq)]
enum VssSchemaVersion {
	// The initial schema version.
	// This used an empty `aad` and unobfuscated `primary_namespace`/`secondary_namespace`s in the
	// stored key.
	V0,
	// The second deployed schema version.
	// Here we started to obfuscate the primary and secondary namespaces and the obfuscated `store_key` (`obfuscate(primary_namespace#secondary_namespace)#obfuscate(key)`) is now used as `aad` for encryption, ensuring that the encrypted blobs commit to the key they're stored under.
	V1,
}

impl_writeable_tlv_based_enum!(VssSchemaVersion,
	(0, V0) => {},
	(1, V1) => {},
);

const VSS_SCHEMA_VERSION_KEY: &str = "vss_schema_version";

// How long a new store waits for writes of the store it replaces to end. A write retries for
// minutes while VSS is unreachable (see `retry_policy`), so building fails after this rather than
// letting such a write land after the new store's writes.
const REPLACED_WRITES_TIMEOUT: Duration = Duration::from_secs(60);

static STORE_FENCES: LazyLock<StoreFences> = LazyLock::new(StoreFences::default);

/// Fences the writes of a [`VssStore`] once another one is built for the same VSS store.
///
/// VSS writes are unconditional puts, so a write of a stopped node that is still retrying, or one
/// its detached work starts later, would overwrite what a node rebuilt on the same store wrote
/// since, e.g. a newer channel monitor. Building a store therefore starts a new generation for its
/// VSS store and waits for the write requests of earlier generations still on the wire. From then
/// on, an earlier generation's writes are dropped before their next attempt (see `fenced_retry`).
///
/// A request whose task is cancelled, e.g. by `VssStore`'s runtime shutting down, stops counting
/// at once, although the server may still apply it shortly after.
#[derive(Default)]
struct StoreFences {
	stores: Mutex<HashMap<(String, String), FenceState>>,
	writes_ended: Condvar,
}

#[derive(Default)]
struct FenceState {
	generation: u64,
	writes_in_flight: usize,
}

impl StoreFences {
	fn start_generation(&self, store: &(String, String), timeout: Duration) -> io::Result<u64> {
		let mut stores = self.stores.lock().unwrap();
		let state = stores.entry(store.clone()).or_default();
		state.generation += 1;
		let generation = state.generation;
		// Wakes an earlier store still waiting here, which has now been replaced itself.
		self.writes_ended.notify_all();
		let (stores, result) = self
			.writes_ended
			.wait_timeout_while(stores, timeout, |stores| {
				stores.get(store).is_some_and(|state| {
					state.generation == generation && state.writes_in_flight > 0
				})
			})
			.unwrap();
		if stores.get(store).is_none_or(|state| state.generation != generation) {
			let msg = "VSS store was replaced while being built";
			return Err(Error::new(ErrorKind::Other, msg));
		}
		if result.timed_out() {
			let msg = "Writes of a replaced store to this VSS store are still in flight";
			return Err(Error::new(ErrorKind::TimedOut, msg));
		}
		Ok(generation)
	}

	fn is_current(&self, store: &(String, String), generation: u64) -> bool {
		self.stores.lock().unwrap().get(store).is_some_and(|state| state.generation == generation)
	}

	/// Returns `None` if the store of `generation` has been replaced.
	fn begin_write<'a>(
		&'a self, store: &'a (String, String), generation: u64,
	) -> Option<WriteGuard<'a>> {
		let mut stores = self.stores.lock().unwrap();
		let state = stores.get_mut(store).filter(|state| state.generation == generation)?;
		state.writes_in_flight += 1;
		Some(WriteGuard { fences: self, store })
	}
}

/// Counts one write request as in flight until it ends or its future is dropped.
struct WriteGuard<'a> {
	fences: &'a StoreFences,
	store: &'a (String, String),
}

impl Drop for WriteGuard<'_> {
	fn drop(&mut self) {
		let mut stores = self.fences.stores.lock().unwrap();
		if let Some(state) = stores.get_mut(self.store) {
			state.writes_in_flight -= 1;
			if state.writes_in_flight == 0 {
				self.fences.writes_ended.notify_all();
			}
		}
	}
}

/// A [`VssStore`]'s place in [`STORE_FENCES`].
#[derive(Clone)]
struct StoreFence {
	store: (String, String),
	generation: u64,
}

#[derive(Debug)]
enum FencedWriteError {
	Replaced,
	Vss(VssError),
}

impl fmt::Display for FencedWriteError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Replaced => write!(f, "VSS store was replaced"),
			Self::Vss(e) => e.fmt(f),
		}
	}
}

impl error::Error for FencedWriteError {}

/// Runs `attempt` with `policy`'s retries while the store of `fence` is current, counting each
/// attempt as in flight. Returns `Ok` without writing once the store has been replaced: its node
/// is gone, and a failed write would make LDK treat a monitor update as unrecoverable and panic.
async fn fenced_retry<F, Fut>(
	fence: &StoreFence, policy: &WriteRetryPolicy, mut attempt: F,
) -> Result<(), VssError>
where
	F: FnMut() -> Fut,
	Fut: Future<Output = Result<(), VssError>>,
{
	let res = retry(
		|| {
			let write = fence.begin_write();
			let request = write.as_ref().map(|_| attempt());
			async move {
				let (Some(_write), Some(request)) = (write, request) else {
					return Err(FencedWriteError::Replaced);
				};
				// Once replaced, a failed attempt is not retried after a backoff either.
				request.await.map_err(|e| {
					if fence.is_current() {
						FencedWriteError::Vss(e)
					} else {
						FencedWriteError::Replaced
					}
				})
			}
		},
		policy,
	)
	.await;
	match res {
		Ok(()) | Err(FencedWriteError::Replaced) => Ok(()),
		Err(FencedWriteError::Vss(_)) if !fence.is_current() => Ok(()),
		Err(FencedWriteError::Vss(e)) => Err(e),
	}
}

impl StoreFence {
	fn is_current(&self) -> bool {
		STORE_FENCES.is_current(&self.store, self.generation)
	}

	fn begin_write(&self) -> Option<WriteGuard<'_>> {
		STORE_FENCES.begin_write(&self.store, self.generation)
	}
}

// We set this to a small number of threads that would still allow to make some progress if one
// would hit a blocking case
const INTERNAL_RUNTIME_WORKERS: usize = 2;

/// A [`KVStoreSync`] implementation that writes to and reads from a [VSS](https://github.com/lightningdevkit/vss-server/blob/main/README.md) backend.
pub struct VssStore {
	inner: Arc<VssStoreInner>,
	// Version counter to ensure that writes are applied in the correct order. It is assumed that read and list
	// operations aren't sensitive to the order of execution.
	next_version: AtomicU64,
	// A VSS-internal runtime that drives VSS I/O independently from the node runtime.
	internal_runtime: Option<Arc<StoreRuntime>>,
}

impl VssStore {
	pub(crate) fn new(
		base_url: String, store_id: String, vss_seed: [u8; 32],
		header_provider: Arc<dyn VssHeaderProvider>,
	) -> io::Result<Self> {
		let store = (base_url.trim_end_matches('/').to_string(), store_id.clone());
		let generation = tokio::task::block_in_place(|| {
			STORE_FENCES.start_generation(&store, REPLACED_WRITES_TIMEOUT)
		})?;
		let fence = StoreFence { store, generation };

		let next_version = AtomicU64::new(1);
		let internal_runtime =
			Arc::new(StoreRuntime::new("ldk-node-vss-runtime", INTERNAL_RUNTIME_WORKERS, "VSS")?);

		let (data_encryption_key, obfuscation_master_key) =
			derive_data_encryption_and_obfuscation_keys(&vss_seed);
		let key_obfuscator = KeyObfuscator::new(obfuscation_master_key);

		let sync_retry_policy = retry_policy();
		let blocking_client = VssClient::new_with_headers(
			base_url.clone(),
			sync_retry_policy,
			header_provider.clone(),
		);

		let runtime_handle = internal_runtime.handle();
		let schema_version = tokio::task::block_in_place(|| {
			// Not fenced: the schema version it may write is the same for every store.
			runtime_handle.block_on(async {
				determine_and_write_schema_version(
					&blocking_client,
					&store_id,
					data_encryption_key,
					&key_obfuscator,
				)
				.await
			})
		})?;
		if !fence.is_current() {
			return Err(Error::new(ErrorKind::Other, "VSS store was replaced while being built"));
		}

		let async_retry_policy = retry_policy();
		let async_client = VssClient::new_with_headers(
			base_url.clone(),
			async_retry_policy,
			header_provider.clone(),
		);
		// Writes retry in `fenced_retry`, so that each attempt is fenced.
		let blocking_write_client = VssClient::new_with_headers(
			base_url.clone(),
			no_retry_policy(),
			header_provider.clone(),
		);
		let async_write_client =
			VssClient::new_with_headers(base_url, no_retry_policy(), header_provider);

		let inner = Arc::new(VssStoreInner::new(
			schema_version,
			blocking_client,
			async_client,
			blocking_write_client,
			async_write_client,
			store_id,
			fence,
			data_encryption_key,
			key_obfuscator,
		));

		Ok(Self { inner, next_version, internal_runtime: Some(internal_runtime) })
	}

	fn internal_runtime(&self) -> Arc<StoreRuntime> {
		Arc::clone(self.internal_runtime.as_ref().expect("VSS runtime must be available"))
	}

	// Same logic as for the obfuscated keys below, but just for locking, using the plaintext keys
	fn build_locking_key(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> String {
		if primary_namespace.is_empty() {
			key.to_owned()
		} else {
			format!("{}#{}#{}", primary_namespace, secondary_namespace, key)
		}
	}

	fn get_new_version_and_lock_ref(
		&self, locking_key: String,
	) -> (Arc<tokio::sync::Mutex<u64>>, u64) {
		let version = self.next_version.fetch_add(1, Ordering::Relaxed);
		if version == u64::MAX {
			panic!("VssStore version counter overflowed");
		}

		// Get a reference to the inner lock. We do this early so that the arc can double as an in-flight counter for
		// cleaning up unused locks.
		let inner_lock_ref = self.inner.get_inner_lock_ref(locking_key);

		(inner_lock_ref, version)
	}
}

impl KVStoreSync for VssStore {
	fn read(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> io::Result<Vec<u8>> {
		let internal_runtime = self.internal_runtime.as_ref().ok_or_else(|| {
			debug_assert!(false, "Failed to access internal runtime");
			let msg = format!("Failed to access internal runtime");
			Error::new(ErrorKind::Other, msg)
		})?;
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();
		let inner = Arc::clone(&self.inner);
		let fut = async move {
			inner
				.read_internal(&inner.blocking_client, primary_namespace, secondary_namespace, key)
				.await
		};
		tokio::task::block_in_place(move || internal_runtime.handle().block_on(fut))
	}

	fn write(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> io::Result<()> {
		let internal_runtime = self.internal_runtime.as_ref().ok_or_else(|| {
			debug_assert!(false, "Failed to access internal runtime");
			let msg = format!("Failed to access internal runtime");
			Error::new(ErrorKind::Other, msg)
		})?;
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();
		let inner = Arc::clone(&self.inner);
		let locking_key = self.build_locking_key(&primary_namespace, &secondary_namespace, &key);
		let (inner_lock_ref, version) = self.get_new_version_and_lock_ref(locking_key.clone());
		let fut = async move {
			inner
				.write_internal(
					&inner.blocking_write_client,
					inner_lock_ref,
					locking_key,
					version,
					primary_namespace,
					secondary_namespace,
					key,
					buf,
				)
				.await
		};
		tokio::task::block_in_place(move || internal_runtime.handle().block_on(fut))
	}

	fn remove(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
	) -> io::Result<()> {
		let internal_runtime = self.internal_runtime.as_ref().ok_or_else(|| {
			debug_assert!(false, "Failed to access internal runtime");
			let msg = format!("Failed to access internal runtime");
			Error::new(ErrorKind::Other, msg)
		})?;
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();
		let inner = Arc::clone(&self.inner);
		let locking_key = self.build_locking_key(&primary_namespace, &secondary_namespace, &key);
		let (inner_lock_ref, version) = self.get_new_version_and_lock_ref(locking_key.clone());
		let fut = async move {
			inner
				.remove_internal(
					&inner.blocking_write_client,
					inner_lock_ref,
					locking_key,
					version,
					primary_namespace,
					secondary_namespace,
					key,
				)
				.await
		};
		if lazy {
			internal_runtime.spawn(async { fut.await });
			Ok(())
		} else {
			tokio::task::block_in_place(move || internal_runtime.handle().block_on(fut))
		}
	}

	fn list(&self, primary_namespace: &str, secondary_namespace: &str) -> io::Result<Vec<String>> {
		let internal_runtime = self.internal_runtime.as_ref().ok_or_else(|| {
			debug_assert!(false, "Failed to access internal runtime");
			let msg = format!("Failed to access internal runtime");
			Error::new(ErrorKind::Other, msg)
		})?;
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let inner = Arc::clone(&self.inner);
		let fut = async move {
			inner
				.list_internal(&inner.blocking_client, primary_namespace, secondary_namespace)
				.await
		};
		tokio::task::block_in_place(move || internal_runtime.handle().block_on(fut))
	}
}

impl KVStore for VssStore {
	fn read(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, io::Error>> + Send>> {
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();
		let inner = Arc::clone(&self.inner);
		let runtime = self.internal_runtime();
		Box::pin(async move {
			let task = runtime.spawn(async move {
				inner
					.read_internal(&inner.async_client, primary_namespace, secondary_namespace, key)
					.await
			});
			task.await.map_err(|e| {
				io::Error::new(io::ErrorKind::Other, format!("VSS runtime task failed: {}", e))
			})?
		})
	}
	fn write(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> Pin<Box<dyn Future<Output = Result<(), io::Error>> + Send>> {
		let locking_key = self.build_locking_key(primary_namespace, secondary_namespace, key);
		let (inner_lock_ref, version) = self.get_new_version_and_lock_ref(locking_key.clone());
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();
		let inner = Arc::clone(&self.inner);
		let runtime = self.internal_runtime();
		Box::pin(async move {
			let task = runtime.spawn(async move {
				inner
					.write_internal(
						&inner.async_write_client,
						inner_lock_ref,
						locking_key,
						version,
						primary_namespace,
						secondary_namespace,
						key,
						buf,
					)
					.await
			});
			task.await.map_err(|e| {
				io::Error::new(io::ErrorKind::Other, format!("VSS runtime task failed: {}", e))
			})?
		})
	}
	fn remove(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
	) -> Pin<Box<dyn Future<Output = Result<(), io::Error>> + Send>> {
		let locking_key = self.build_locking_key(primary_namespace, secondary_namespace, key);
		let (inner_lock_ref, version) = self.get_new_version_and_lock_ref(locking_key.clone());
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let key = key.to_string();
		let inner = Arc::clone(&self.inner);
		let runtime = self.internal_runtime();
		let fut = async move {
			inner
				.remove_internal(
					&inner.async_write_client,
					inner_lock_ref,
					locking_key,
					version,
					primary_namespace,
					secondary_namespace,
					key,
				)
				.await
		};
		Box::pin(async move {
			if lazy {
				runtime.spawn(async move {
					let _ = fut.await;
				});
				Ok(())
			} else {
				let task = runtime.spawn(fut);
				task.await.map_err(|e| {
					io::Error::new(io::ErrorKind::Other, format!("VSS runtime task failed: {}", e))
				})?
			}
		})
	}
	fn list(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> Pin<Box<dyn Future<Output = Result<Vec<String>, io::Error>> + Send>> {
		let primary_namespace = primary_namespace.to_string();
		let secondary_namespace = secondary_namespace.to_string();
		let inner = Arc::clone(&self.inner);
		let runtime = self.internal_runtime();
		Box::pin(async move {
			let task = runtime.spawn(async move {
				inner
					.list_internal(&inner.async_client, primary_namespace, secondary_namespace)
					.await
			});
			task.await.map_err(|e| {
				io::Error::new(io::ErrorKind::Other, format!("VSS runtime task failed: {}", e))
			})?
		})
	}
}

impl Drop for VssStore {
	fn drop(&mut self) {
		if let Some(runtime) = self.internal_runtime.take() {
			if let Ok(runtime) = Arc::try_unwrap(runtime) {
				runtime.shutdown_background();
			}
		}
	}
}

struct VssStoreInner {
	schema_version: VssSchemaVersion,
	blocking_client: VssClient<CustomRetryPolicy>,
	// A secondary client that will only be used for async persistence via `KVStore`, to ensure TCP
	// connections aren't shared between our outer and the internal runtime.
	async_client: VssClient<CustomRetryPolicy>,
	// Clients for writes and removes, which `fenced_retry` retries.
	blocking_write_client: VssClient<CustomRetryPolicy>,
	async_write_client: VssClient<CustomRetryPolicy>,
	write_retry_policy: WriteRetryPolicy,
	store_id: String,
	fence: StoreFence,
	data_encryption_key: [u8; 32],
	key_obfuscator: KeyObfuscator,
	// Per-key locks that ensures that we don't have concurrent writes to the same namespace/key.
	// The lock also encapsulates the latest written version per key.
	locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<u64>>>>,
}

impl VssStoreInner {
	#[allow(clippy::too_many_arguments)]
	pub(crate) fn new(
		schema_version: VssSchemaVersion, blocking_client: VssClient<CustomRetryPolicy>,
		async_client: VssClient<CustomRetryPolicy>,
		blocking_write_client: VssClient<CustomRetryPolicy>,
		async_write_client: VssClient<CustomRetryPolicy>, store_id: String, fence: StoreFence,
		data_encryption_key: [u8; 32], key_obfuscator: KeyObfuscator,
	) -> Self {
		let locks = Mutex::new(HashMap::new());
		Self {
			schema_version,
			blocking_client,
			async_client,
			blocking_write_client,
			async_write_client,
			write_retry_policy: write_retry_policy(),
			store_id,
			fence,
			data_encryption_key,
			key_obfuscator,
			locks,
		}
	}

	fn get_inner_lock_ref(&self, locking_key: String) -> Arc<tokio::sync::Mutex<u64>> {
		let mut outer_lock = self.locks.lock().unwrap();
		Arc::clone(&outer_lock.entry(locking_key).or_default())
	}

	fn build_obfuscated_key(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> String {
		if self.schema_version == VssSchemaVersion::V1 {
			let obfuscated_prefix =
				self.build_obfuscated_prefix(primary_namespace, secondary_namespace);
			let obfuscated_key = self.key_obfuscator.obfuscate(key);
			format!("{}#{}", obfuscated_prefix, obfuscated_key)
		} else {
			// Default to V0 schema
			let obfuscated_key = self.key_obfuscator.obfuscate(key);
			if primary_namespace.is_empty() {
				obfuscated_key
			} else {
				format!("{}#{}#{}", primary_namespace, secondary_namespace, obfuscated_key)
			}
		}
	}

	fn build_obfuscated_prefix(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> String {
		if self.schema_version == VssSchemaVersion::V1 {
			let prefix = format!("{}#{}", primary_namespace, secondary_namespace);
			self.key_obfuscator.obfuscate(&prefix)
		} else {
			// Default to V0 schema
			format!("{}#{}", primary_namespace, secondary_namespace)
		}
	}

	fn extract_key(&self, unified_key: &str) -> io::Result<String> {
		let mut parts = if self.schema_version == VssSchemaVersion::V1 {
			let mut parts = unified_key.splitn(2, '#');
			let _obfuscated_namespace = parts.next();
			parts
		} else {
			// Default to V0 schema
			let mut parts = unified_key.splitn(3, '#');
			let (_primary_namespace, _secondary_namespace) = (parts.next(), parts.next());
			parts
		};
		match parts.next() {
			Some(obfuscated_key) => {
				let actual_key = self.key_obfuscator.deobfuscate(obfuscated_key)?;
				Ok(actual_key)
			},
			None => Err(Error::new(ErrorKind::InvalidData, "Invalid key format")),
		}
	}

	async fn list_all_keys(
		&self, client: &VssClient<CustomRetryPolicy>, primary_namespace: &str,
		secondary_namespace: &str,
	) -> io::Result<Vec<String>> {
		let mut page_token = None;
		let mut keys = vec![];
		let key_prefix = self.build_obfuscated_prefix(primary_namespace, secondary_namespace);
		while page_token != Some("".to_string()) {
			let request = ListKeyVersionsRequest {
				store_id: self.store_id.clone(),
				key_prefix: Some(key_prefix.clone()),
				page_token,
				page_size: None,
			};

			let response = client.list_key_versions(&request).await.map_err(|e| {
				let msg = format!(
					"Failed to list keys in {}/{}: {}",
					primary_namespace, secondary_namespace, e
				);
				Error::new(ErrorKind::Other, msg)
			})?;

			for kv in response.key_versions {
				keys.push(self.extract_key(&kv.key)?);
			}
			page_token = response.next_page_token;
		}
		Ok(keys)
	}

	async fn read_internal(
		&self, client: &VssClient<CustomRetryPolicy>, primary_namespace: String,
		secondary_namespace: String, key: String,
	) -> io::Result<Vec<u8>> {
		check_namespace_key_validity(&primary_namespace, &secondary_namespace, Some(&key), "read")?;

		let store_key = self.build_obfuscated_key(&primary_namespace, &secondary_namespace, &key);
		let request = GetObjectRequest { store_id: self.store_id.clone(), key: store_key.clone() };
		let resp = client.get_object(&request).await.map_err(|e| {
			let msg = format!(
				"Failed to read from key {}/{}/{}: {}",
				primary_namespace, secondary_namespace, key, e
			);
			match e {
				VssError::NoSuchKeyError(..) => Error::new(ErrorKind::NotFound, msg),
				_ => Error::new(ErrorKind::Other, msg),
			}
		})?;

		// unwrap safety: resp.value must be always present for a non-erroneous VSS response, otherwise
		// it is an API-violation which is converted to [`VssError::InternalServerError`] in [`VssClient`]
		let storable = Storable::decode(&resp.value.unwrap().value[..]).map_err(|e| {
			let msg = format!(
				"Failed to decode data read from key {}/{}/{}: {}",
				primary_namespace, secondary_namespace, key, e
			);
			Error::new(ErrorKind::Other, msg)
		})?;

		let storable_builder = StorableBuilder::new(RandEntropySource);
		let aad =
			if self.schema_version == VssSchemaVersion::V1 { store_key.as_bytes() } else { &[] };
		let decrypted = storable_builder.deconstruct(storable, &self.data_encryption_key, aad)?.0;
		Ok(decrypted)
	}

	async fn write_internal(
		&self, client: &VssClient<CustomRetryPolicy>, inner_lock_ref: Arc<tokio::sync::Mutex<u64>>,
		locking_key: String, version: u64, primary_namespace: String, secondary_namespace: String,
		key: String, buf: Vec<u8>,
	) -> io::Result<()> {
		check_namespace_key_validity(
			&primary_namespace,
			&secondary_namespace,
			Some(&key),
			"write",
		)?;

		let store_key = self.build_obfuscated_key(&primary_namespace, &secondary_namespace, &key);
		let vss_version = -1;
		let storable_builder = StorableBuilder::new(RandEntropySource);
		let aad =
			if self.schema_version == VssSchemaVersion::V1 { store_key.as_bytes() } else { &[] };
		let storable =
			storable_builder.build(buf.to_vec(), vss_version, &self.data_encryption_key, aad);
		let request = PutObjectRequest {
			store_id: self.store_id.clone(),
			global_version: None,
			transaction_items: vec![KeyValue {
				key: store_key,
				version: vss_version,
				value: storable.encode_to_vec(),
			}],
			delete_items: vec![],
		};

		self.execute_locked_write(inner_lock_ref, locking_key, version, async move || {
			let res = fenced_retry(&self.fence, &self.write_retry_policy, || async {
				client.put_object(&request).await.map(|_| ())
			})
			.await;
			match res {
				Ok(()) => Ok(()),
				Err(e) => {
					let msg = format!(
						"Failed to write to key {}/{}/{}: {}",
						primary_namespace, secondary_namespace, key, e
					);
					Err(Error::new(ErrorKind::Other, msg))
				},
			}
		})
		.await
	}

	async fn remove_internal(
		&self, client: &VssClient<CustomRetryPolicy>, inner_lock_ref: Arc<tokio::sync::Mutex<u64>>,
		locking_key: String, version: u64, primary_namespace: String, secondary_namespace: String,
		key: String,
	) -> io::Result<()> {
		check_namespace_key_validity(
			&primary_namespace,
			&secondary_namespace,
			Some(&key),
			"remove",
		)?;

		let obfuscated_key =
			self.build_obfuscated_key(&primary_namespace, &secondary_namespace, &key);

		let key_value = KeyValue { key: obfuscated_key, version: -1, value: vec![] };
		self.execute_locked_write(inner_lock_ref, locking_key, version, async move || {
			let request =
				DeleteObjectRequest { store_id: self.store_id.clone(), key_value: Some(key_value) };

			let res = fenced_retry(&self.fence, &self.write_retry_policy, || async {
				client.delete_object(&request).await.map(|_| ())
			})
			.await;
			match res {
				Ok(()) => Ok(()),
				Err(e) => {
					let msg = format!(
						"Failed to delete key {}/{}/{}: {}",
						primary_namespace, secondary_namespace, key, e
					);
					Err(Error::new(ErrorKind::Other, msg))
				},
			}
		})
		.await
	}

	async fn list_internal(
		&self, client: &VssClient<CustomRetryPolicy>, primary_namespace: String,
		secondary_namespace: String,
	) -> io::Result<Vec<String>> {
		check_namespace_key_validity(&primary_namespace, &secondary_namespace, None, "list")?;

		let keys = self
			.list_all_keys(client, &primary_namespace, &secondary_namespace)
			.await
			.map_err(|e| {
				let msg = format!(
					"Failed to retrieve keys in namespace: {}/{} : {}",
					primary_namespace, secondary_namespace, e
				);
				Error::new(ErrorKind::Other, msg)
			})?;

		Ok(keys)
	}

	async fn execute_locked_write<
		F: Future<Output = Result<(), lightning::io::Error>>,
		FN: FnOnce() -> F,
	>(
		&self, inner_lock_ref: Arc<tokio::sync::Mutex<u64>>, locking_key: String, version: u64,
		callback: FN,
	) -> Result<(), lightning::io::Error> {
		let res = {
			let mut last_written_version = inner_lock_ref.lock().await;

			// Check if we already have a newer version written/removed. This is used in async contexts to realize eventual
			// consistency.
			let is_stale_version = version <= *last_written_version;

			// If the version is not stale, we execute the callback. Otherwise we can and must skip writing.
			if is_stale_version {
				Ok(())
			} else {
				callback().await.map(|_| {
					*last_written_version = version;
				})
			}
		};

		self.clean_locks(&inner_lock_ref, locking_key);

		res
	}

	fn clean_locks(&self, inner_lock_ref: &Arc<tokio::sync::Mutex<u64>>, locking_key: String) {
		// If there no arcs in use elsewhere, this means that there are no in-flight writes. We can remove the map entry
		// to prevent leaking memory. The two arcs that are expected are the one in the map and the one held here in
		// inner_lock_ref. The outer lock is obtained first, to avoid a new arc being cloned after we've already
		// counted.
		let mut outer_lock = self.locks.lock().unwrap();

		let strong_count = Arc::strong_count(&inner_lock_ref);
		debug_assert!(strong_count >= 2, "Unexpected VssStore strong count");

		if strong_count == 2 {
			outer_lock.remove(&locking_key);
		}
	}
}

fn derive_data_encryption_and_obfuscation_keys(vss_seed: &[u8; 32]) -> ([u8; 32], [u8; 32]) {
	let hkdf = |initial_key_material: &[u8], salt: &[u8]| -> [u8; 32] {
		let mut engine = HmacEngine::<sha256::Hash>::new(salt);
		engine.input(initial_key_material);
		Hmac::from_engine(engine).to_byte_array()
	};

	let prk = hkdf(vss_seed, b"pseudo_random_key");
	let k1 = hkdf(&prk, b"data_encryption_key");
	let k2 = hkdf(&prk, &[&k1[..], b"obfuscation_key"].concat());
	(k1, k2)
}

fn retry_policy() -> CustomRetryPolicy {
	ExponentialBackoffRetryPolicy::new(Duration::from_millis(10))
		.with_max_attempts(100)
		.with_max_total_delay(Duration::from_secs(180))
		.with_max_jitter(Duration::from_millis(100))
		.skip_retry_on_error(Box::new(|e: &VssError| is_permanent(e)) as _)
}

fn is_permanent(e: &VssError) -> bool {
	matches!(
		e,
		VssError::NoSuchKeyError(..)
			| VssError::InvalidRequestError(..)
			| VssError::ConflictError(..)
	)
}

fn no_retry_policy() -> CustomRetryPolicy {
	ExponentialBackoffRetryPolicy::new(Duration::ZERO)
		.with_max_attempts(1)
		.with_max_total_delay(Duration::ZERO)
		.with_max_jitter(Duration::ZERO)
		.skip_retry_on_error(Box::new(|_: &VssError| true) as _)
}

// The same retries as `retry_policy`, for `fenced_retry`.
fn write_retry_policy() -> WriteRetryPolicy {
	ExponentialBackoffRetryPolicy::new(Duration::from_millis(10))
		.with_max_attempts(100)
		.with_max_total_delay(Duration::from_secs(180))
		.with_max_jitter(Duration::from_millis(100))
		.skip_retry_on_error(Box::new(|e: &FencedWriteError| match e {
			FencedWriteError::Replaced => true,
			FencedWriteError::Vss(e) => is_permanent(e),
		}) as _)
}

async fn determine_and_write_schema_version(
	client: &VssClient<CustomRetryPolicy>, store_id: &String, data_encryption_key: [u8; 32],
	key_obfuscator: &KeyObfuscator,
) -> io::Result<VssSchemaVersion> {
	// Build the obfuscated `vss_schema_version` key.
	let obfuscated_prefix = key_obfuscator.obfuscate(&format! {"{}#{}", "", ""});
	let obfuscated_key = key_obfuscator.obfuscate(VSS_SCHEMA_VERSION_KEY);
	let store_key = format!("{}#{}", obfuscated_prefix, obfuscated_key);

	// Try to read the stored schema version.
	let request = GetObjectRequest { store_id: store_id.clone(), key: store_key.clone() };
	let resp = match client.get_object(&request).await {
		Ok(resp) => Some(resp),
		Err(VssError::NoSuchKeyError(..)) => {
			// The value is not set.
			None
		},
		Err(e) => {
			let msg = format!("Failed to read schema version: {}", e);
			return Err(Error::new(ErrorKind::Other, msg));
		},
	};

	if let Some(resp) = resp {
		// The schema version was present, so just decrypt the stored data.

		// unwrap safety: resp.value must be always present for a non-erroneous VSS response, otherwise
		// it is an API-violation which is converted to [`VssError::InternalServerError`] in [`VssClient`]
		let storable = Storable::decode(&resp.value.unwrap().value[..]).map_err(|e| {
			let msg = format!("Failed to decode schema version: {}", e);
			Error::new(ErrorKind::Other, msg)
		})?;

		let storable_builder = StorableBuilder::new(RandEntropySource);
		// Schema version was added starting with V1, so if set at all, we use the key as `aad`
		let aad = store_key.as_bytes();
		let decrypted = storable_builder
			.deconstruct(storable, &data_encryption_key, aad)
			.map_err(|e| {
				let msg = format!("Failed to decode schema version: {}", e);
				Error::new(ErrorKind::Other, msg)
			})?
			.0;

		let schema_version: VssSchemaVersion = Readable::read(&mut io::Cursor::new(decrypted))
			.map_err(|e| {
				let msg = format!("Failed to decode schema version: {}", e);
				Error::new(ErrorKind::Other, msg)
			})?;
		Ok(schema_version)
	} else {
		// The schema version wasn't present, this either means we're running for the first time *or* it's V0 pre-migration (predating writing of the schema version).

		// Check if any `bdk_wallet` data was written by listing keys under the respective
		// (unobfuscated) prefix.
		const V0_BDK_WALLET_PREFIX: &str = "bdk_wallet#";
		let request = ListKeyVersionsRequest {
			store_id: store_id.clone(),
			key_prefix: Some(V0_BDK_WALLET_PREFIX.to_string()),
			page_token: None,
			page_size: None,
		};

		let response = client.list_key_versions(&request).await.map_err(|e| {
			let msg = format!("Failed to determine schema version: {}", e);
			Error::new(ErrorKind::Other, msg)
		})?;

		let wallet_data_present = !response.key_versions.is_empty();
		if wallet_data_present {
			// If the wallet data is present, it means we're not running for the first time.
			Ok(VssSchemaVersion::V0)
		} else {
			// We're running for the first time, write the schema version to save unnecessary IOps
			// on future startup.
			let schema_version = VssSchemaVersion::V1;
			let encoded_version = schema_version.encode();

			let storable_builder = StorableBuilder::new(RandEntropySource);
			let vss_version = -1;
			let aad = store_key.as_bytes();
			let storable =
				storable_builder.build(encoded_version, vss_version, &data_encryption_key, aad);

			let request = PutObjectRequest {
				store_id: store_id.clone(),
				global_version: None,
				transaction_items: vec![KeyValue {
					key: store_key,
					version: vss_version,
					value: storable.encode_to_vec(),
				}],
				delete_items: vec![],
			};

			client.put_object(&request).await.map_err(|e| {
				let msg = format!("Failed to write schema version: {}", e);
				Error::new(ErrorKind::Other, msg)
			})?;

			Ok(schema_version)
		}
	}
}

/// A source for generating entropy/randomness using [`rand`].
pub(crate) struct RandEntropySource;

impl EntropySource for RandEntropySource {
	fn fill_bytes(&self, buffer: &mut [u8]) {
		rand::rng().fill_bytes(buffer);
	}
}

#[cfg(test)]
impl RefUnwindSafe for VssStore {}

#[cfg(test)]
mod store_fences_tests {
	use super::*;

	fn store() -> (String, String) {
		("https://vss.example".to_string(), "store".to_string())
	}

	#[test]
	fn each_store_starts_a_new_generation() {
		let fences = StoreFences::default();
		let first = fences.start_generation(&store(), Duration::ZERO).unwrap();
		let second = fences.start_generation(&store(), Duration::ZERO).unwrap();
		assert!(second > first);
	}

	#[test]
	fn a_store_replaced_while_waiting_stops_waiting() {
		let fences = Arc::new(StoreFences::default());
		let replaced = fences.start_generation(&store(), Duration::ZERO).unwrap();
		let key = store();
		let write = fences.begin_write(&key, replaced).unwrap();
		let waiting = {
			let fences = Arc::clone(&fences);
			std::thread::spawn(move || fences.start_generation(&store(), Duration::from_secs(10)))
		};
		while fences.is_current(&key, replaced) {
			std::thread::yield_now();
		}
		let err = fences.start_generation(&key, Duration::from_millis(20)).unwrap_err();
		assert_eq!(err.kind(), ErrorKind::TimedOut);
		assert_eq!(waiting.join().unwrap().unwrap_err().kind(), ErrorKind::Other);
		drop(write);
	}

	#[test]
	fn a_replaced_store_cannot_begin_writes() {
		let fences = StoreFences::default();
		let replaced = fences.start_generation(&store(), Duration::ZERO).unwrap();
		assert!(fences.begin_write(&store(), replaced).is_some());
		let current = fences.start_generation(&store(), Duration::ZERO).unwrap();
		assert!(fences.begin_write(&store(), replaced).is_none());
		assert!(!fences.is_current(&store(), replaced));
		assert!(fences.begin_write(&store(), current).is_some());
	}

	#[test]
	fn a_new_store_waits_for_writes_of_the_replaced_one() {
		let fences = Arc::new(StoreFences::default());
		let replaced = fences.start_generation(&store(), Duration::ZERO).unwrap();
		let (started_tx, started_rx) = std::sync::mpsc::channel();
		let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
		let writer = {
			let fences = Arc::clone(&fences);
			std::thread::spawn(move || {
				let store = store();
				let _write = fences.begin_write(&store, replaced).unwrap();
				started_tx.send(()).unwrap();
				release_rx.recv().unwrap();
			})
		};
		started_rx.recv().unwrap();
		let builder = {
			let fences = Arc::clone(&fences);
			std::thread::spawn(move || fences.start_generation(&store(), Duration::from_secs(10)))
		};
		std::thread::sleep(Duration::from_millis(50));
		assert!(!builder.is_finished(), "new store did not wait for the in-flight write");
		release_tx.send(()).unwrap();
		writer.join().unwrap();
		builder.join().unwrap().unwrap();
	}

	#[test]
	fn a_new_store_fails_but_still_fences_while_a_write_stays_in_flight() {
		let fences = StoreFences::default();
		let store = store();
		let replaced = fences.start_generation(&store, Duration::ZERO).unwrap();
		let _write = fences.begin_write(&store, replaced).unwrap();
		let err = fences.start_generation(&store, Duration::from_millis(20)).unwrap_err();
		assert_eq!(err.kind(), ErrorKind::TimedOut);
		assert!(fences.begin_write(&store, replaced).is_none());
	}

	#[test]
	fn dropping_an_unfinished_write_future_ends_the_write() {
		let fences = StoreFences::default();
		let replaced = fences.start_generation(&store(), Duration::ZERO).unwrap();
		let store = store();
		let write = fences.begin_write(&store, replaced).unwrap();
		let fut = async move {
			let _write = write;
			std::future::pending::<()>().await;
		};
		drop(fut);
		fences.start_generation(&store, Duration::ZERO).unwrap();
	}

	fn current_fence(store_id: &str) -> StoreFence {
		let store = ("https://vss.example".to_string(), store_id.to_string());
		let generation = STORE_FENCES.start_generation(&store, Duration::ZERO).unwrap();
		StoreFence { store, generation }
	}

	fn server_error() -> VssError {
		VssError::InternalServerError("unavailable".to_string())
	}

	#[tokio::test]
	async fn fenced_retry_retries_until_a_write_succeeds() {
		let fence = current_fence("retries");
		let attempts = std::sync::atomic::AtomicUsize::new(0);
		let res = fenced_retry(&fence, &write_retry_policy(), || async {
			if attempts.fetch_add(1, Ordering::SeqCst) < 2 {
				Err(server_error())
			} else {
				Ok(())
			}
		})
		.await;
		assert!(res.is_ok());
		assert_eq!(attempts.load(Ordering::SeqCst), 3);
	}

	#[tokio::test]
	async fn fenced_retry_returns_permanent_errors_without_retrying() {
		let fence = current_fence("permanent");
		let attempts = std::sync::atomic::AtomicUsize::new(0);
		let res = fenced_retry(&fence, &write_retry_policy(), || async {
			attempts.fetch_add(1, Ordering::SeqCst);
			Err(VssError::ConflictError("conflict".to_string()))
		})
		.await;
		assert!(matches!(res, Err(VssError::ConflictError(..))));
		assert_eq!(attempts.load(Ordering::SeqCst), 1);
	}

	#[tokio::test]
	async fn fenced_retry_drops_writes_of_a_replaced_store() {
		let fence = current_fence("replaced");
		current_fence("replaced");
		let attempts = std::sync::atomic::AtomicUsize::new(0);
		let res = fenced_retry(&fence, &write_retry_policy(), || async {
			attempts.fetch_add(1, Ordering::SeqCst);
			Ok(())
		})
		.await;
		assert!(res.is_ok());
		assert_eq!(attempts.load(Ordering::SeqCst), 0);
	}

	#[tokio::test]
	async fn fenced_retry_stops_retrying_once_the_store_is_replaced() {
		let fence = current_fence("replaced-between-attempts");
		let attempts = std::sync::atomic::AtomicUsize::new(0);
		let res = fenced_retry(&fence, &write_retry_policy(), || async {
			attempts.fetch_add(1, Ordering::SeqCst);
			// A new store starts its generation now; it times out waiting for this attempt.
			let _ = STORE_FENCES.start_generation(&fence.store, Duration::ZERO);
			Err(server_error())
		})
		.await;
		assert!(res.is_ok());
		assert_eq!(attempts.load(Ordering::SeqCst), 1);
	}

	#[test]
	fn stores_fence_only_their_own_writes() {
		let fences = StoreFences::default();
		let other = ("https://vss.example".to_string(), "other".to_string());
		let other_generation = fences.start_generation(&other, Duration::ZERO).unwrap();
		let _write = fences.begin_write(&other, other_generation).unwrap();
		fences.start_generation(&store(), Duration::ZERO).unwrap();
		assert!(fences.begin_write(&other, other_generation).is_some());
	}
}

#[cfg(test)]
#[cfg(vss_test)]
mod tests {
	use std::collections::HashMap;

	use rand::distr::Alphanumeric;
	use rand::{rng, Rng, RngCore};
	use vss_client::headers::FixedHeaders;

	use super::*;
	use crate::io::test_utils::do_read_write_remove_list_persist;

	#[test]
	fn vss_read_write_remove_list_persist() {
		let vss_base_url = std::env::var("TEST_VSS_BASE_URL").unwrap();
		let mut rng = rng();
		let rand_store_id: String = (0..7).map(|_| rng.sample(Alphanumeric) as char).collect();
		let mut vss_seed = [0u8; 32];
		rng.fill_bytes(&mut vss_seed);
		let header_provider = Arc::new(FixedHeaders::new(HashMap::new()));
		let vss_store =
			VssStore::new(vss_base_url, rand_store_id, vss_seed, header_provider).unwrap();
		do_read_write_remove_list_persist(&vss_store);
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
	async fn vss_read_write_remove_list_persist_in_runtime_context() {
		let vss_base_url = std::env::var("TEST_VSS_BASE_URL").unwrap();
		let mut rng = rng();
		let rand_store_id: String = (0..7).map(|_| rng.sample(Alphanumeric) as char).collect();
		let mut vss_seed = [0u8; 32];
		rng.fill_bytes(&mut vss_seed);
		let header_provider = Arc::new(FixedHeaders::new(HashMap::new()));
		let vss_store =
			VssStore::new(vss_base_url, rand_store_id, vss_seed, header_provider).unwrap();

		do_read_write_remove_list_persist(&vss_store);
		drop(vss_store)
	}

	#[test]
	fn vss_writes_of_a_replaced_store_are_dropped() {
		let vss_base_url = std::env::var("TEST_VSS_BASE_URL").unwrap();
		let mut rng = rng();
		let rand_store_id: String = (0..7).map(|_| rng.sample(Alphanumeric) as char).collect();
		let mut vss_seed = [0u8; 32];
		rng.fill_bytes(&mut vss_seed);
		let header_provider = Arc::new(FixedHeaders::new(HashMap::new()));
		let replaced = VssStore::new(
			vss_base_url.clone(),
			rand_store_id.clone(),
			vss_seed,
			header_provider.clone(),
		)
		.unwrap();
		let current =
			VssStore::new(vss_base_url, rand_store_id, vss_seed, header_provider).unwrap();

		KVStoreSync::write(&current, "ns", "", "key", vec![2]).unwrap();
		KVStoreSync::write(&replaced, "ns", "", "key", vec![1]).unwrap();
		KVStoreSync::remove(&replaced, "ns", "", "key", false).unwrap();
		assert_eq!(KVStoreSync::read(&current, "ns", "", "key").unwrap(), vec![2]);
	}
}

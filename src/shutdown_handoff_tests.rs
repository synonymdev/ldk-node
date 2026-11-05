// This file is licensed under the Apache License, Version 2.0 or the MIT license.

use super::*;
use crate::ShutdownHandleError;
use proptest::prelude::*;

proptest! {
	#![proptest_config(ProptestConfig::with_cases(8))]
	#[test]
	fn handoff_preparation_deduplicates_and_waits_for_every_alias(aliases in 1usize..8) {
		let fixture = Fixture::new();
		let node = Arc::new(fixture.node());
		let owner = node.clone().prepare_shutdown().unwrap();
		let repeated = node.clone().prepare_shutdown().unwrap();
		prop_assert!(Arc::ptr_eq(&owner, &repeated));
		let mut references = vec![node.clone(); aliases];
		drop(node);
		while let Some(reference) = references.pop() {
			prop_assert_eq!(owner.wait(0), Err(ShutdownHandleError::PendingHandles));
			prop_assert!(!reference.strict_shutdown.load(Ordering::Acquire));
			drop(reference);
		}
		prop_assert_eq!(owner.wait(5000), Ok(()));
		let writes = fixture.writes.load(Ordering::Acquire);
		prop_assert_eq!(repeated.wait(0), Ok(()));
		prop_assert_eq!(fixture.writes.load(Ordering::Acquire), writes);
	}
}

#[test]
fn handoff_retains_extracted_original_while_payment_child_finishes() {
	let fixture = Fixture::new();
	let node = Arc::new(fixture.node());
	let child = node.onchain_payment();
	let original_store = node.shutdown_store.as_ref().unwrap().clone();
	let owner = node.prepare_shutdown().unwrap();
	assert_eq!(owner.wait(0), Err(ShutdownHandleError::PendingHandles));
	// The original child can finish its genuine work before release; shutdown cannot replay it.
	child.new_address().unwrap();
	drop(child);
	assert_eq!(owner.wait(5000), Ok(()));
	let writes = fixture.writes.load(Ordering::Acquire);
	assert_eq!(owner.wait(0), Ok(()));
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
fn handoff_reuses_original_after_timeout_and_rejects_concurrent_waiter() {
	let fixture = Fixture::new();
	let node = Arc::new(fixture.node());
	let (release, held) = tokio::sync::oneshot::channel();
	node.runtime.spawn_background_task(async move {
		held.await.unwrap();
	});
	let owner = node.prepare_shutdown().unwrap();
	assert_eq!(owner.wait(10), Err(ShutdownHandleError::TimedOut));
	let waiting = owner.clone();
	let waiter = thread::spawn(move || loop {
		match waiting.wait(5000) {
			Err(ShutdownHandleError::Busy) => thread::yield_now(),
			result => break result,
		}
	});
	let deadline = Instant::now() + Duration::from_secs(2);
	loop {
		match owner.wait(0) {
			Err(ShutdownHandleError::Busy) => break,
			Err(ShutdownHandleError::TimedOut) if Instant::now() < deadline => {
				thread::sleep(Duration::from_millis(1))
			},
			other => panic!("concurrent waiter did not retain admission: {other:?}"),
		}
	}
	release.send(()).unwrap();
	assert_eq!(waiter.join().unwrap(), Ok(()));
	assert_eq!(owner.wait(0), Ok(()));
}

#[test]
fn handoff_keeps_original_native_failures_sticky() {
	for failure in [ShutdownHandleError::PersistenceFailed, ShutdownHandleError::TaskFailed] {
		let fixture = Fixture::new();
		let node = Arc::new(fixture.node());
		node.runtime.spawn_background_processor_task(
			async move {
				if failure == ShutdownHandleError::TaskFailed {
					panic!("controlled native processor panic");
				}
				Err(lightning::io::Error::new(
					lightning::io::ErrorKind::Other,
					"controlled write failure",
				))
			},
			|_| {},
		);
		let owner = node.prepare_shutdown().unwrap();
		assert_eq!(owner.wait(5000), Err(failure));
		assert_eq!(owner.wait(0), Err(failure));
	}
}

#[test]
fn handoff_retains_real_original_work_when_entry_or_wait_panics() {
	for entry_panic in [true, false] {
		let fixture = Fixture::new();
		let node = Arc::new(fixture.node());
		let running = node.is_running.clone();
		let (release, held) = tokio::sync::oneshot::channel();
		let (started, observed_start) = std::sync::mpsc::channel();
		let (finished, observed_finish) = std::sync::mpsc::channel();
		node.runtime.spawn_background_task(async move {
			started.send(()).unwrap();
			held.await.unwrap();
			finished.send(()).unwrap();
		});
		observed_start.recv_timeout(Duration::from_secs(2)).unwrap();
		let owner = node.prepare_shutdown().unwrap();
		if entry_panic {
			let poisoned_running = running.clone();
			assert!(thread::spawn(move || {
				let _guard = poisoned_running.write().unwrap();
				panic!("controlled node lock poison before shutdown entry");
			})
			.join()
			.is_err());
			assert!(
				std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| owner.wait(10))).is_err()
			);
			// Clear only this synthetic poison so fixture destruction itself can finish.
			running.clear_poison();
		} else {
			let runtime =
				tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
			// Deliberately violate the documented blocking-runtime precondition after entry.
			assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
				runtime.block_on(async { owner.wait(10) })
			}))
			.is_err());
		}
		assert_eq!(owner.wait(0), Err(ShutdownHandleError::Poisoned));
		// If unwinding destroyed the node, Runtime::drop would cancel this original task.
		release.send(()).expect("original operation was destroyed during handoff unwind");
		observed_finish.recv_timeout(Duration::from_secs(2)).unwrap();
		assert_eq!(owner.wait(0), Err(ShutdownHandleError::Poisoned));
	}
}

#[test]
fn handoff_unsupported_preparation_leaves_original_usable() {
	let fixture = Fixture::new();
	let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
	let borrowed = Arc::new(runtime.block_on(async { fixture.node() }));
	let path = std::env::temp_dir().join(format!("ldk-handoff-{}", rand::random::<u64>()));
	let mut builder = NodeBuilder::from_config(Config {
		network: Network::Regtest,
		storage_dir_path: path.to_str().unwrap().to_owned(),
		..Config::default()
	});
	builder.set_entropy_seed_bytes([43; 64]);
	builder.set_log_facade_logger();
	let sqlite = Arc::new(builder.build().unwrap());
	for node in [borrowed, sqlite] {
		assert!(matches!(node.clone().prepare_shutdown(), Err(ShutdownHandleError::Unsupported)));
		assert!(!node.strict_shutdown.load(Ordering::Acquire));
		node.onchain_payment().new_address().unwrap();
	}
}

#[test]
fn handoff_dropped_preparation_does_not_close_original_admission() {
	let fixture = Fixture::new();
	let node = Arc::new(fixture.node());
	let owner = node.clone().prepare_shutdown().unwrap();
	let weak = Arc::downgrade(&owner);
	drop(owner);
	assert!(weak.upgrade().is_none());
	assert!(!node.strict_shutdown.load(Ordering::Acquire));
	node.onchain_payment().new_address().unwrap();
	let replacement = node.prepare_shutdown().unwrap();
	assert_eq!(replacement.wait(5000), Ok(()));
}

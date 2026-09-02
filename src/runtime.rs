// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::future::Future;
use std::io;
use std::ops::Deref;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use lightning::io::{Error as LdkIoError, ErrorKind as LdkIoErrorKind};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::config::{
	BACKGROUND_TASK_SHUTDOWN_TIMEOUT_SECS, LDK_EVENT_HANDLER_SHUTDOWN_TIMEOUT_SECS,
};
use crate::error::Error;
use crate::logger::{log_debug, log_error, log_trace, LdkLogger, Logger};

fn mutex_lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
	mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn rwlock_read<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
	lock.read().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn rwlock_write<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
	lock.write().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Owns the Tokio runtime when ldk-node creates one.
///
/// This type is intentionally non-cloneable and is held only by `Node`. All spawned work and
/// exported child handles receive [`RuntimeControl`], which cannot keep an owned runtime alive.
pub(crate) struct Runtime {
	owned_runtime: Option<tokio::runtime::Runtime>,
	control: Arc<RuntimeControl>,
}

impl Runtime {
	pub fn new(logger: Arc<Logger>) -> io::Result<Self> {
		match tokio::runtime::Handle::try_current() {
			Ok(handle) => Ok(Self::with_handle(handle, logger)),
			Err(_) => {
				let mut runtime_builder = tokio::runtime::Builder::new_multi_thread();
				runtime_builder.enable_all();
				runtime_builder.thread_name_fn(|| {
					static ATOMIC_ID: AtomicUsize = AtomicUsize::new(0);
					let id = ATOMIC_ID.fetch_add(1, Ordering::SeqCst);
					format!("ldk-node-runtime-{}", id)
				});
				let runtime = runtime_builder.build()?;
				Ok(Self::with_owned_runtime(runtime, logger))
			},
		}
	}

	pub fn with_handle(handle: tokio::runtime::Handle, logger: Arc<Logger>) -> Self {
		Self { owned_runtime: None, control: Arc::new(RuntimeControl::new(handle, logger)) }
	}

	fn with_owned_runtime(runtime: tokio::runtime::Runtime, logger: Arc<Logger>) -> Self {
		let control = Arc::new(RuntimeControl::new(runtime.handle().clone(), logger));
		Self { owned_runtime: Some(runtime), control }
	}

	pub(crate) fn control(&self) -> Arc<RuntimeControl> {
		Arc::clone(&self.control)
	}
}

impl Deref for Runtime {
	type Target = RuntimeControl;

	fn deref(&self) -> &Self::Target {
		&self.control
	}
}

impl Drop for Runtime {
	fn drop(&mut self) {
		self.control.cancel_all_tasks();
		let _ = self
			.control
			.drain_detached_work(Duration::from_secs(BACKGROUND_TASK_SHUTDOWN_TIMEOUT_SECS));
		self.control.close_runtime_use();
		if let Some(runtime) = self.owned_runtime.take() {
			runtime.shutdown_background();
		}
	}
}

/// Cloneable runtime access that never owns the Tokio runtime.
pub(crate) struct RuntimeControl {
	handle: tokio::runtime::Handle,
	runtime_use: RwLock<bool>,
	background_tasks: Mutex<TrackedTasks>,
	cancellable_background_tasks: Mutex<TrackedTasks>,
	background_processor_task: Mutex<Option<JoinHandle<Result<(), LdkIoError>>>>,
	leftover_task_trackers: Mutex<Vec<TaskTracker>>,
	leftover_processor_task: Mutex<Option<JoinHandle<Result<(), LdkIoError>>>>,
	logger: Arc<Logger>,
}

struct TrackedTasks {
	tasks: TaskTracker,
	cancellation_token: CancellationToken,
}

impl TrackedTasks {
	fn new() -> Self {
		Self { tasks: TaskTracker::new(), cancellation_token: CancellationToken::new() }
	}

	fn close(&self) {
		self.tasks.close();
	}

	fn cancel(&self) {
		self.cancellation_token.cancel();
	}
}

impl Drop for TrackedTasks {
	fn drop(&mut self) {
		self.close();
		self.cancel();
	}
}

impl RuntimeControl {
	fn new(handle: tokio::runtime::Handle, logger: Arc<Logger>) -> Self {
		Self {
			handle,
			runtime_use: RwLock::new(true),
			background_tasks: Mutex::new(TrackedTasks::new()),
			cancellable_background_tasks: Mutex::new(TrackedTasks::new()),
			background_processor_task: Mutex::new(None),
			leftover_task_trackers: Mutex::new(Vec::new()),
			leftover_processor_task: Mutex::new(None),
			logger,
		}
	}

	pub fn allow_task_spawns(&self) -> bool {
		self.allow_task_spawns_with_timeout(Duration::from_secs(
			BACKGROUND_TASK_SHUTDOWN_TIMEOUT_SECS,
		))
	}

	pub(crate) fn allow_task_spawns_with_timeout(&self, timeout: Duration) -> bool {
		if !self.drain_detached_work(timeout) {
			log_error!(
				self.logger,
				"Refusing a new task generation while previous work is still running."
			);
			return false;
		}
		Self::reset_closed_task_generation(&self.background_tasks);
		Self::reset_closed_task_generation(&self.cancellable_background_tasks);
		true
	}

	fn reset_closed_task_generation(tasks: &Mutex<TrackedTasks>) {
		let mut tasks = mutex_lock(tasks);
		if tasks.tasks.is_closed() {
			*tasks = TrackedTasks::new();
		}
	}

	pub fn close_task_admission(&self) {
		mutex_lock(&self.background_tasks).close();
		mutex_lock(&self.cancellable_background_tasks).close();
	}

	fn cancel_all_tasks(&self) {
		self.cancel_tracked_tasks();
		if let Some(task) = mutex_lock(&self.background_processor_task).as_ref() {
			task.abort();
		}
		if let Some(task) = mutex_lock(&self.leftover_processor_task).as_ref() {
			task.abort();
		}
	}

	pub(crate) fn cancel_tracked_tasks(&self) {
		for tasks in [&self.background_tasks, &self.cancellable_background_tasks] {
			let tasks = mutex_lock(tasks);
			tasks.close();
			tasks.cancel();
		}
	}

	fn close_runtime_use(&self) {
		*rwlock_write(&self.runtime_use) = false;
	}

	fn detach_task_tracker(&self, tasks: TaskTracker) {
		if !tasks.is_empty() {
			mutex_lock(&self.leftover_task_trackers).push(tasks);
		}
	}

	fn drain_detached_work(&self, timeout: Duration) -> bool {
		let trackers = std::mem::take(&mut *mutex_lock(&self.leftover_task_trackers));
		let mut processor = mutex_lock(&self.leftover_processor_task).take();
		let drained = match self.try_block_on(async {
			tokio::time::timeout(timeout, async {
				for tracker in &trackers {
					tracker.wait().await;
				}
				if let Some(task) = &mut processor {
					let _ = task.await;
				}
			})
			.await
			.is_ok()
		}) {
			Ok(drained) => drained,
			Err(_) => false,
		};
		if drained {
			true
		} else {
			mutex_lock(&self.leftover_task_trackers).extend(trackers);
			if let Some(task) = processor {
				if !task.is_finished() {
					*mutex_lock(&self.leftover_processor_task) = Some(task);
				}
			}
			false
		}
	}

	pub fn spawn_background_task<F>(&self, future: F)
	where
		F: Future<Output = ()> + Send + 'static,
	{
		self.spawn_tracked_task(&self.background_tasks, "background", future);
	}

	pub fn spawn_cancellable_background_task<F>(&self, future: F)
	where
		F: Future<Output = ()> + Send + 'static,
	{
		self.spawn_tracked_task(
			&self.cancellable_background_tasks,
			"cancellable background",
			future,
		);
	}

	fn spawn_tracked_task<F>(&self, tasks: &Mutex<TrackedTasks>, task_name: &str, future: F)
	where
		F: Future<Output = ()> + Send + 'static,
	{
		let tasks = mutex_lock(tasks);
		if tasks.tasks.is_closed() {
			log_trace!(self.logger, "Ignoring {} task spawned during shutdown.", task_name);
			return;
		}

		let cancellation_token = tasks.cancellation_token.clone();
		let _ = tasks.tasks.spawn_on(
			async move {
				tokio::select! {
					biased;
					_ = cancellation_token.cancelled() => {},
					_ = future => {},
				}
			},
			&self.handle,
		);
	}

	pub fn spawn_background_processor_task<F, C>(&self, future: F, on_failure: C)
	where
		F: Future<Output = Result<(), LdkIoError>> + Send + 'static,
		C: FnOnce(&LdkIoError) + Send + 'static,
	{
		let mut background_processor_task = mutex_lock(&self.background_processor_task);
		debug_assert!(background_processor_task.is_none(), "Expected no background processor task");
		*background_processor_task = Some(self.handle.spawn(async move {
			let result = future.await;
			if let Err(e) = &result {
				on_failure(e);
			}
			result
		}));
	}

	pub fn try_block_on<F: Future>(&self, future: F) -> Result<F::Output, Error> {
		let runtime_available = rwlock_read(&self.runtime_use);
		if !*runtime_available {
			return Err(Error::NotRunning);
		}
		let handle = tokio::runtime::Handle::try_current().unwrap_or_else(|_| self.handle.clone());
		Ok(tokio::task::block_in_place(move || handle.block_on(async { future.await })))
	}

	pub fn block_on<F: Future>(&self, future: F) -> F::Output {
		self.try_block_on(future).expect("owned runtime must outlive owner-driven work")
	}

	pub fn abort_cancellable_background_tasks(&self) {
		self.abort_cancellable_background_tasks_with_timeout(Duration::from_secs(
			BACKGROUND_TASK_SHUTDOWN_TIMEOUT_SECS,
		));
	}

	fn abort_cancellable_background_tasks_with_timeout(&self, shutdown_timeout: Duration) {
		let tasks = {
			let tasks = mutex_lock(&self.cancellable_background_tasks);
			tasks.close();
			tasks.cancel();
			tasks.tasks.clone()
		};
		let timed_out = self.block_on(async {
			tokio::time::timeout(shutdown_timeout, tasks.wait()).await.is_err()
		});
		if timed_out {
			log_error!(
				self.logger,
				"Detaching cancellable background tasks after cancellation timed out."
			);
			self.detach_task_tracker(tasks);
		} else {
			log_debug!(self.logger, "Stopped all cancellable background tasks.");
		}
	}

	pub fn wait_on_background_tasks(&self) {
		self.wait_on_background_tasks_with_timeout(Duration::from_secs(
			BACKGROUND_TASK_SHUTDOWN_TIMEOUT_SECS,
		));
	}

	fn wait_on_background_tasks_with_timeout(&self, shutdown_timeout: Duration) {
		let (tasks, cancellation_token) = {
			let tasks = mutex_lock(&self.background_tasks);
			tasks.close();
			(tasks.tasks.clone(), tasks.cancellation_token.clone())
		};

		let timed_out = self.block_on(async {
			tokio::time::timeout(shutdown_timeout, tasks.wait()).await.is_err()
		});
		if timed_out {
			log_error!(self.logger, "Stopping background tasks timed out.");
			cancellation_token.cancel();
			let cancellation_timed_out = self.block_on(async {
				tokio::time::timeout(shutdown_timeout, tasks.wait()).await.is_err()
			});
			if cancellation_timed_out {
				log_error!(self.logger, "Detaching background tasks after cancellation timed out.");
				self.detach_task_tracker(tasks);
				return;
			}
		}
		log_debug!(self.logger, "Stopped all background tasks.");
	}

	pub fn wait_on_background_processor_task(&self) -> Result<(), LdkIoError> {
		self.wait_on_background_processor_task_with_timeout(Duration::from_secs(
			LDK_EVENT_HANDLER_SHUTDOWN_TIMEOUT_SECS,
		))
	}

	fn wait_on_background_processor_task_with_timeout(
		&self, shutdown_timeout: Duration,
	) -> Result<(), LdkIoError> {
		let Some(mut task) = mutex_lock(&self.background_processor_task).take() else {
			log_error!(self.logger, "Skipped waiting for missing background processor task.");
			return Ok(());
		};

		enum ProcessorWait {
			Finished(Result<(), LdkIoError>),
			Detached,
		}

		let outcome = self.block_on(async {
			match tokio::time::timeout(shutdown_timeout, &mut task).await {
				Ok(Ok(result)) => ProcessorWait::Finished(result),
				Ok(Err(e)) => ProcessorWait::Finished(Err(LdkIoError::new(
					LdkIoErrorKind::Other,
					format!("Event processor task failed: {}", e),
				))),
				Err(e) => {
					log_error!(self.logger, "Stopping event handling timed out: {}", e);
					task.abort();
					match tokio::time::timeout(shutdown_timeout, &mut task).await {
						Err(e) => {
							log_error!(
								self.logger,
								"Detaching event processor after cancellation timed out: {}",
								e
							);
							ProcessorWait::Detached
						},
						Ok(Ok(result)) => ProcessorWait::Finished(result),
						Ok(Err(e)) if e.is_cancelled() => ProcessorWait::Finished(Ok(())),
						Ok(Err(e)) => ProcessorWait::Finished(Err(LdkIoError::new(
							LdkIoErrorKind::Other,
							format!("Event processor task failed: {}", e),
						))),
					}
				},
			}
		});

		let result = match outcome {
			ProcessorWait::Finished(result) => result,
			ProcessorWait::Detached => {
				*mutex_lock(&self.leftover_processor_task) = Some(task);
				Ok(())
			},
		};

		if result.is_ok() {
			log_debug!(self.logger, "Stopped background processing of events.");
		}
		result
	}

	#[cfg(tokio_unstable)]
	pub fn log_metrics(&self) {
		log_trace!(
			self.logger,
			"Active runtime tasks left prior to shutdown: {}",
			self.handle.metrics().active_tasks_count()
		);
	}

	pub(crate) fn handle(&self) -> &tokio::runtime::Handle {
		&self.handle
	}
}

impl Drop for RuntimeControl {
	fn drop(&mut self) {
		if let Ok(task) = self.background_processor_task.get_mut() {
			if let Some(task) = task.take() {
				task.abort();
			}
		}
	}
}

/// Runtime used by async store backends while ldk-node still exposes synchronous APIs.
///
/// Store I/O uses an independent runtime so synchronous node APIs cannot block the worker that
/// must drive the persistence future they are waiting for.
pub(crate) struct StoreRuntime {
	runtime: Option<tokio::runtime::Runtime>,
}

impl StoreRuntime {
	pub(crate) fn new(
		thread_name_prefix: &'static str, worker_threads: usize, runtime_name: &'static str,
	) -> io::Result<Self> {
		let runtime = tokio::runtime::Builder::new_multi_thread()
			.enable_all()
			.thread_name_fn(move || {
				static ATOMIC_ID: AtomicUsize = AtomicUsize::new(0);
				let id = ATOMIC_ID.fetch_add(1, Ordering::SeqCst);
				format!("{}-{}", thread_name_prefix, id)
			})
			.worker_threads(worker_threads)
			.max_blocking_threads(worker_threads)
			.build()
			.map_err(|e| {
				io::Error::new(
					io::ErrorKind::Other,
					format!("Failed to build {runtime_name} runtime: {e}"),
				)
			})?;
		Ok(Self { runtime: Some(runtime) })
	}

	pub(crate) fn handle(&self) -> &tokio::runtime::Handle {
		self.runtime.as_ref().expect("store runtime must be available").handle()
	}

	pub(crate) fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
	where
		F: Future + Send + 'static,
		F::Output: Send + 'static,
	{
		self.handle().spawn(future)
	}

	pub(crate) fn shutdown_background(mut self) {
		if let Some(runtime) = self.runtime.take() {
			runtime.shutdown_background();
		}
	}
}

impl Drop for StoreRuntime {
	fn drop(&mut self) {
		if let Some(runtime) = self.runtime.take() {
			runtime.shutdown_background();
		}
	}
}

#[cfg(test)]
mod tests {
	use std::process::Command;
	use std::thread;

	use tokio::sync::{mpsc, oneshot};

	use super::*;

	const RUNTIME_SELF_DROP_CHILD_ENV: &str = "LDK_NODE_RUNTIME_SELF_DROP_CHILD";
	const STORE_RUNTIME_SELF_DROP_CHILD_ENV: &str = "LDK_NODE_STORE_RUNTIME_SELF_DROP_CHILD";
	const EXPORTED_HANDLE_DROP_CHILD_ENV: &str = "LDK_NODE_EXPORTED_HANDLE_DROP_CHILD";

	struct DropNotifier(Option<oneshot::Sender<()>>);

	impl Drop for DropNotifier {
		fn drop(&mut self) {
			if let Some(sender) = self.0.take() {
				let _ = sender.send(());
			}
		}
	}

	fn test_runtime() -> Runtime {
		Runtime::new(Arc::new(Logger::new_log_facade())).unwrap()
	}

	fn test_runtime_with_workers(worker_threads: usize) -> Runtime {
		let runtime = tokio::runtime::Builder::new_multi_thread()
			.worker_threads(worker_threads)
			.enable_all()
			.build()
			.unwrap();
		Runtime::with_owned_runtime(runtime, Arc::new(Logger::new_log_facade()))
	}

	#[test]
	fn runtime_control_does_not_retain_runtime_owner() {
		let runtime = test_runtime();
		let control = runtime.control();
		let weak_control = Arc::downgrade(&control);

		drop(runtime);

		assert!(weak_control.upgrade().is_some());
		let (late_sender, mut late_receiver) = oneshot::channel();
		control.spawn_background_task(async move {
			let _ = late_sender.send(());
		});
		assert!(matches!(late_receiver.try_recv(), Err(oneshot::error::TryRecvError::Closed)));
		drop(control);
		assert!(weak_control.upgrade().is_none());
	}

	#[test]
	fn completed_cancellable_tasks_are_released_before_shutdown() {
		const TASK_COUNT: usize = 64;

		let runtime = test_runtime();
		let (completion_sender, mut completion_receiver) = mpsc::channel(TASK_COUNT);
		for _ in 0..TASK_COUNT {
			let completion_sender = completion_sender.clone();
			runtime.spawn_cancellable_background_task(async move {
				completion_sender.send(()).await.unwrap();
			});
		}
		drop(completion_sender);

		runtime.block_on(async {
			for _ in 0..TASK_COUNT {
				completion_receiver.recv().await.unwrap();
			}
			tokio::time::timeout(Duration::from_secs(1), async {
				loop {
					if runtime.cancellable_background_tasks.lock().unwrap().tasks.is_empty() {
						break;
					}
					tokio::task::yield_now().await;
				}
			})
			.await
			.unwrap();
		});
	}

	#[test]
	fn late_task_spawns_are_not_polled_after_shutdown_starts() {
		let runtime = test_runtime();
		runtime.close_task_admission();

		let (background_sender, background_receiver) = oneshot::channel();
		runtime.spawn_background_task(async move {
			let _ = background_sender.send(());
		});
		let (cancellable_sender, cancellable_receiver) = oneshot::channel();
		runtime.spawn_cancellable_background_task(async move {
			let _ = cancellable_sender.send(());
		});

		assert!(runtime.block_on(background_receiver).is_err());
		assert!(runtime.block_on(cancellable_receiver).is_err());
	}

	#[test]
	fn timed_out_background_task_is_drained_before_shutdown_returns() {
		let runtime = test_runtime();
		let (started_sender, started_receiver) = oneshot::channel();
		let (dropped_sender, dropped_receiver) = oneshot::channel();
		runtime.spawn_background_task(async move {
			let _drop_notifier = DropNotifier(Some(dropped_sender));
			let _ = started_sender.send(());
			std::future::pending::<()>().await;
		});
		runtime.block_on(started_receiver).unwrap();

		runtime.wait_on_background_tasks_with_timeout(Duration::from_millis(10));

		runtime.block_on(dropped_receiver).unwrap();
	}

	#[test]
	fn non_cooperative_background_task_does_not_block_shutdown() {
		let runtime = test_runtime_with_workers(2);
		let (started_sender, started_receiver) = std::sync::mpsc::sync_channel(1);
		let (release_sender, release_receiver) = std::sync::mpsc::sync_channel(1);
		let (finished_sender, finished_receiver) = std::sync::mpsc::sync_channel(1);
		runtime.spawn_background_task(async move {
			started_sender.send(()).unwrap();
			release_receiver.recv().unwrap();
			finished_sender.send(()).unwrap();
		});
		started_receiver.recv_timeout(Duration::from_secs(1)).unwrap();

		let start = std::time::Instant::now();
		runtime.wait_on_background_tasks_with_timeout(Duration::from_millis(10));
		assert!(start.elapsed() < Duration::from_secs(1));

		release_sender.send(()).unwrap();
		finished_receiver.recv_timeout(Duration::from_secs(1)).unwrap();
	}

	#[test]
	fn non_cooperative_cancellable_task_does_not_block_shutdown() {
		let runtime = test_runtime_with_workers(2);
		let (started_sender, started_receiver) = std::sync::mpsc::sync_channel(1);
		let (release_sender, release_receiver) = std::sync::mpsc::sync_channel(1);
		let (finished_sender, finished_receiver) = std::sync::mpsc::sync_channel(1);
		runtime.spawn_cancellable_background_task(async move {
			started_sender.send(()).unwrap();
			release_receiver.recv().unwrap();
			finished_sender.send(()).unwrap();
		});
		started_receiver.recv_timeout(Duration::from_secs(1)).unwrap();

		let start = std::time::Instant::now();
		runtime.abort_cancellable_background_tasks_with_timeout(Duration::from_millis(10));
		assert!(start.elapsed() < Duration::from_secs(1));

		release_sender.send(()).unwrap();
		finished_receiver.recv_timeout(Duration::from_secs(1)).unwrap();
	}

	#[test]
	fn timed_out_event_processor_is_drained_before_shutdown_returns() {
		let runtime = test_runtime();
		let (started_sender, started_receiver) = oneshot::channel();
		let (dropped_sender, dropped_receiver) = oneshot::channel();
		runtime.spawn_background_processor_task(
			async move {
				let _drop_notifier = DropNotifier(Some(dropped_sender));
				let _ = started_sender.send(());
				std::future::pending::<Result<(), LdkIoError>>().await
			},
			|_| {},
		);
		runtime.block_on(started_receiver).unwrap();

		runtime.wait_on_background_processor_task_with_timeout(Duration::from_millis(10)).unwrap();

		runtime.block_on(dropped_receiver).unwrap();
	}

	#[test]
	fn non_cooperative_event_processor_does_not_block_shutdown() {
		let runtime = test_runtime_with_workers(2);
		let (started_sender, started_receiver) = std::sync::mpsc::sync_channel(1);
		let (release_sender, release_receiver) = std::sync::mpsc::sync_channel(1);
		let (finished_sender, finished_receiver) = std::sync::mpsc::sync_channel(1);
		runtime.spawn_background_processor_task(
			async move {
				started_sender.send(()).unwrap();
				release_receiver.recv().unwrap();
				finished_sender.send(()).unwrap();
				Ok(())
			},
			|_| {},
		);
		started_receiver.recv_timeout(Duration::from_secs(1)).unwrap();

		let start = std::time::Instant::now();
		runtime.wait_on_background_processor_task_with_timeout(Duration::from_millis(10)).unwrap();
		assert!(start.elapsed() < Duration::from_secs(1));

		release_sender.send(()).unwrap();
		finished_receiver.recv_timeout(Duration::from_secs(1)).unwrap();
	}

	#[test]
	fn event_processor_persistence_failure_is_returned() {
		let runtime = test_runtime();
		let (failure_sender, failure_receiver) = oneshot::channel();
		runtime.spawn_background_processor_task(
			async { Err(LdkIoError::new(LdkIoErrorKind::Other, "persistence failed")) },
			move |_| {
				let _ = failure_sender.send(());
			},
		);

		runtime.block_on(failure_receiver).unwrap();

		let error = runtime.wait_on_background_processor_task().unwrap_err();

		assert_eq!(error.kind(), LdkIoErrorKind::Other);
		assert!(error.to_string().contains("persistence failed"));
	}

	#[test]
	fn repeated_one_worker_runtime_self_drop_is_abort_free() {
		if std::env::var_os(RUNTIME_SELF_DROP_CHILD_ENV).is_some() {
			run_repeated_one_worker_runtime_self_drop();
			return;
		}

		let status = Command::new(std::env::current_exe().unwrap())
			.args([
				"--exact",
				"runtime::tests::repeated_one_worker_runtime_self_drop_is_abort_free",
				"--nocapture",
			])
			.env(RUNTIME_SELF_DROP_CHILD_ENV, "1")
			.status()
			.unwrap();

		assert!(status.success(), "owned runtime self-drop aborted the subprocess");
	}

	fn run_repeated_one_worker_runtime_self_drop() {
		for _ in 0..100 {
			let tokio_runtime = tokio::runtime::Builder::new_multi_thread()
				.worker_threads(1)
				.enable_all()
				.build()
				.unwrap();
			let runtime =
				Runtime::with_owned_runtime(tokio_runtime, Arc::new(Logger::new_log_facade()));
			let exported_control = runtime.control();
			let handle = exported_control.handle().clone();
			let (started_sender, started_receiver) = std::sync::mpsc::sync_channel(1);
			exported_control.spawn_background_task(async move {
				started_sender.send(()).unwrap();
				std::future::pending::<()>().await;
			});
			started_receiver.recv_timeout(Duration::from_secs(1)).unwrap();
			exported_control.close_task_admission();
			exported_control.wait_on_background_tasks_with_timeout(Duration::from_millis(1));
			let (dropped_sender, dropped_receiver) = std::sync::mpsc::sync_channel(1);

			handle.spawn(async move {
				drop(runtime);
				dropped_sender.send(()).unwrap();
			});

			dropped_receiver.recv_timeout(Duration::from_secs(1)).unwrap();
			drop(exported_control);
		}
	}

	#[test]
	fn store_runtime_self_drop_is_abort_free() {
		if std::env::var_os(STORE_RUNTIME_SELF_DROP_CHILD_ENV).is_some() {
			let runtime = StoreRuntime::new("vss-test-runtime", 1, "VSS test").unwrap();
			let handle = runtime.handle().clone();
			let (dropped_sender, dropped_receiver) = std::sync::mpsc::sync_channel(1);
			handle.spawn(async move {
				drop(runtime);
				dropped_sender.send(()).unwrap();
			});
			dropped_receiver.recv_timeout(Duration::from_secs(1)).unwrap();
			return;
		}

		let status = Command::new(std::env::current_exe().unwrap())
			.args([
				"--exact",
				"runtime::tests::store_runtime_self_drop_is_abort_free",
				"--nocapture",
			])
			.env(STORE_RUNTIME_SELF_DROP_CHILD_ENV, "1")
			.status()
			.unwrap();

		assert!(status.success(), "VSS store runtime self-drop aborted the subprocess");
	}

	#[test]
	fn exported_control_block_on_after_runtime_drop_returns_not_running() {
		let runtime = test_runtime();
		let control = runtime.control();
		drop(runtime);
		assert_eq!(control.try_block_on(async { 1u8 }), Err(Error::NotRunning));
	}

	#[test]
	fn in_flight_exported_block_on_completes_before_runtime_drop() {
		if std::env::var_os(EXPORTED_HANDLE_DROP_CHILD_ENV).is_some() {
			let runtime = test_runtime();
			let control = runtime.control();
			let (started_sender, started_receiver) = std::sync::mpsc::sync_channel(1);
			let (done_sender, done_receiver) = std::sync::mpsc::sync_channel(1);
			thread::spawn(move || {
				let result = control.try_block_on(async {
					started_sender.send(()).unwrap();
					tokio::time::sleep(Duration::from_millis(200)).await;
					1u8
				});
				done_sender.send(result).unwrap();
			});
			started_receiver.recv_timeout(Duration::from_secs(1)).unwrap();
			drop(runtime);
			assert_eq!(done_receiver.recv_timeout(Duration::from_secs(2)).unwrap(), Ok(1));
			return;
		}

		let status = Command::new(std::env::current_exe().unwrap())
			.args([
				"--exact",
				"runtime::tests::in_flight_exported_block_on_completes_before_runtime_drop",
				"--nocapture",
			])
			.env(EXPORTED_HANDLE_DROP_CHILD_ENV, "1")
			.status()
			.unwrap();

		assert!(status.success(), "exported handle block_on aborted during runtime teardown");
	}

	#[test]
	fn detached_background_work_blocks_a_new_task_generation() {
		let runtime = test_runtime_with_workers(2);
		let (started_sender, started_receiver) = std::sync::mpsc::sync_channel(1);
		let (release_sender, release_receiver) = std::sync::mpsc::sync_channel(1);
		runtime.spawn_background_task(async move {
			started_sender.send(()).unwrap();
			release_receiver.recv().unwrap();
		});
		started_receiver.recv_timeout(Duration::from_secs(1)).unwrap();

		runtime.wait_on_background_tasks_with_timeout(Duration::from_millis(10));
		assert!(!runtime.allow_task_spawns_with_timeout(Duration::from_millis(20)));

		release_sender.send(()).unwrap();
		assert!(runtime.allow_task_spawns_with_timeout(Duration::from_secs(1)));
	}

	#[test]
	fn detached_event_processor_blocks_a_new_task_generation() {
		let runtime = test_runtime_with_workers(2);
		let (started_sender, started_receiver) = std::sync::mpsc::sync_channel(1);
		let (release_sender, release_receiver) = std::sync::mpsc::sync_channel(1);
		runtime.spawn_background_processor_task(
			async move {
				started_sender.send(()).unwrap();
				release_receiver.recv().unwrap();
				Ok(())
			},
			|_| {},
		);
		started_receiver.recv_timeout(Duration::from_secs(1)).unwrap();

		runtime.wait_on_background_processor_task_with_timeout(Duration::from_millis(10)).unwrap();
		assert!(!runtime.allow_task_spawns_with_timeout(Duration::from_millis(20)));

		release_sender.send(()).unwrap();
		assert!(runtime.allow_task_spawns_with_timeout(Duration::from_secs(1)));
	}
}

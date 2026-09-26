//! Single owner for watcher-driven index mutations and recovery requests.
//!
//! Enumeration still runs in cancellable helper threads, but completed loads,
//! scans, refreshes, and persistence all return through this command stream.
//! Watcher callbacks only enqueue commands here, keeping OS notification
//! handling bounded and giving every index mutation one ordering point.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::filecache::{BuildOutput, FileCache, LoadOutput, RefreshOutput, SubtreeRefreshOutput};
use crate::index_watcher::PathChange;

const COMMAND_QUEUE_CAPACITY: usize = 256;

enum Command {
    Load {
        epoch: u64,
        config: Config,
        active_paths: Vec<String>,
    },
    Demand {
        epoch: u64,
        config: Config,
        active_paths: Vec<String>,
    },
    Changes {
        epoch: u64,
        changes: Vec<PathChange>,
    },
    Reconfigure {
        epoch: u64,
    },
    CancelBuild,
    BuildFinished {
        output: BuildOutput,
    },
    RefreshFinished {
        output: RefreshOutput,
        completed: SyncSender<()>,
    },
    SubtreeRefreshFinished {
        output: SubtreeRefreshOutput,
        completed: SyncSender<()>,
    },
    LoadFinished {
        output: LoadOutput,
    },
    Shutdown,
    Recovery {
        epoch: u64,
    },
}

pub struct IndexService {
    tx: SyncSender<Command>,
    epoch: Arc<AtomicU64>,
    recovery_pending: Arc<std::sync::atomic::AtomicBool>,
    cache: Arc<FileCache>,
    load_pending: Arc<AtomicBool>,
    demand_pending: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
}

impl IndexService {
    pub fn start(cache: Arc<FileCache>) -> Arc<Self> {
        let (tx, rx) = sync_channel(COMMAND_QUEUE_CAPACITY);
        let epoch = Arc::new(AtomicU64::new(0));
        let recovery_pending = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let load_pending = Arc::new(AtomicBool::new(false));
        let demand_pending = Arc::new(AtomicBool::new(false));
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_epoch = epoch.clone();
        let worker_recovery_pending = recovery_pending.clone();
        let worker_load_pending = load_pending.clone();
        let worker_demand_pending = demand_pending.clone();
        let worker_shutdown = shutdown.clone();
        let completion_tx = tx.clone();
        cache.set_build_completion_callback(Arc::new(move |output| {
            match completion_tx.send(Command::BuildFinished { output }) {
                Ok(()) => None,
                Err(error) => match error.0 {
                    Command::BuildFinished { output } => Some(output),
                    _ => None,
                },
            }
        }));
        let refresh_completion_tx = tx.clone();
        cache.set_refresh_completion_callback(Arc::new(move |output| {
            let (completed_tx, completed_rx) = sync_channel(0);
            match refresh_completion_tx.send(Command::RefreshFinished {
                output,
                completed: completed_tx,
            }) {
                Ok(()) => {
                    let _ = completed_rx.recv();
                    None
                }
                Err(error) => match error.0 {
                    Command::RefreshFinished { output, .. } => Some(output),
                    _ => None,
                },
            }
        }));
        let subtree_completion_tx = tx.clone();
        cache.set_subtree_refresh_completion_callback(Arc::new(move |output| {
            let (completed_tx, completed_rx) = sync_channel(0);
            match subtree_completion_tx.send(Command::SubtreeRefreshFinished {
                output,
                completed: completed_tx,
            }) {
                Ok(()) => {
                    let _ = completed_rx.recv();
                    None
                }
                Err(error) => match error.0 {
                    Command::SubtreeRefreshFinished { output, .. } => Some(output),
                    _ => None,
                },
            }
        }));
        let load_completion_tx = tx.clone();
        cache.set_load_completion_callback(Arc::new(move |output| {
            match load_completion_tx.send(Command::LoadFinished { output }) {
                Ok(()) => None,
                Err(error) => match error.0 {
                    Command::LoadFinished { output } => Some(output),
                    _ => None,
                },
            }
        }));
        let worker_cache = cache.clone();
        std::thread::spawn(move || {
            loop {
                if worker_shutdown.load(Ordering::Acquire) {
                    worker_cache.shutdown();
                    break;
                }
                let command = match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(command) => command,
                    Err(RecvTimeoutError::Timeout) => {
                        if worker_shutdown.load(Ordering::Acquire) {
                            worker_cache.shutdown();
                            break;
                        }
                        if worker_cache.take_ready_recovery() {
                            FileCache::refresh_now(worker_cache.clone());
                        }
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                };
                match command {
                    Command::Load {
                        epoch,
                        config,
                        active_paths,
                    } => {
                        worker_load_pending.store(false, Ordering::Release);
                        if epoch == worker_epoch.load(Ordering::Acquire) {
                            FileCache::load_persisted(worker_cache.clone(), config, active_paths);
                        }
                    }
                    Command::Demand {
                        epoch,
                        config,
                        active_paths,
                    } => {
                        worker_demand_pending.store(false, Ordering::Release);
                        if epoch == worker_epoch.load(Ordering::Acquire) {
                            FileCache::ensure_built(worker_cache.clone(), config, active_paths);
                        }
                    }
                    Command::Changes { epoch, changes }
                        if epoch == worker_epoch.load(Ordering::Acquire) =>
                    {
                        let directories = changes
                            .iter()
                            .filter_map(|change| match change {
                                PathChange::Upsert(path) if path.is_dir() => Some(path.clone()),
                                _ => None,
                            })
                            .collect::<Vec<_>>();
                        worker_cache.apply_changes(changes);
                        if !directories.is_empty() {
                            // A directory move can invalidate an entire
                            // subtree. Reconcile only those directories;
                            // this avoids a full-drive scan for a project
                            // folder move while retaining the authoritative
                            // fallback if the targeted walk fails.
                            FileCache::refresh_subtrees(worker_cache.clone(), directories);
                        }
                    }
                    Command::Reconfigure { epoch } => {
                        // The epoch is advanced before this command is
                        // queued. Processing the command in-order is what
                        // makes invalidation happen before the load/build
                        // demand that follows it, while stale watcher/change
                        // commands are rejected by the epoch guards above.
                        if epoch == worker_epoch.load(Ordering::Acquire) {
                            worker_cache.invalidate();
                        }
                    }
                    Command::CancelBuild => {
                        worker_cache.kill_build();
                    }
                    Command::BuildFinished { output } => {
                        crate::filecache::finish_build(&worker_cache, output);
                    }
                    Command::RefreshFinished { output, completed } => {
                        FileCache::finish_refresh(&worker_cache, output);
                        let _ = completed.send(());
                    }
                    Command::SubtreeRefreshFinished { output, completed } => {
                        FileCache::finish_subtree_refresh(&worker_cache, output);
                        let _ = completed.send(());
                    }
                    Command::LoadFinished { output } => {
                        crate::filecache::finish_load(&worker_cache, output);
                    }
                    Command::Shutdown => {
                        worker_cache.shutdown();
                        break;
                    }
                    Command::Recovery { epoch }
                        if epoch == worker_epoch.load(Ordering::Acquire) =>
                    {
                        FileCache::request_recovery(worker_cache.clone())
                    }
                    _ => {}
                }
                if worker_shutdown.load(Ordering::Acquire) {
                    worker_cache.shutdown();
                    break;
                }
                // A recovery marker survives a full command queue. Consume
                // it after each command so a dropped Recovery message still
                // results in one reconciliation once the queue drains.
                if worker_recovery_pending.swap(false, Ordering::AcqRel) {
                    FileCache::request_recovery(worker_cache.clone());
                }
                if worker_cache.take_ready_recovery() {
                    FileCache::refresh_now(worker_cache.clone());
                }
            }
        });
        Arc::new(Self {
            tx,
            epoch,
            recovery_pending,
            cache,
            load_pending,
            demand_pending,
            shutdown,
        })
    }

    pub fn current_epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    pub fn shutdown(&self) {
        if self.shutdown.swap(true, Ordering::AcqRel) {
            return;
        }
        self.cache.shutdown();
        // The atomic is the priority path: the worker exits after its current
        // command even when the bounded event queue is full. This command is
        // only a wake-up for the normal idle case.
        let _ = self.tx.try_send(Command::Shutdown);
    }

    pub fn reconfigure(&self) -> u64 {
        self.recovery_pending.store(false, Ordering::Release);
        self.load_pending.store(false, Ordering::Release);
        self.demand_pending.store(false, Ordering::Release);
        let epoch = self.epoch.fetch_add(1, Ordering::AcqRel) + 1;
        // Queue invalidation behind any already accepted change batch. The
        // following load/build command is queued after this one, so callers
        // never observe a new configuration racing an old cache generation.
        let _ = self.tx.send(Command::Reconfigure { epoch });
        epoch
    }

    /// Stops an in-flight filename build in the same command stream as
    /// searches and watcher changes. Cancellation is deliberately
    /// non-blocking; if a bounded queue is saturated, kill the registered
    /// child immediately and let the next worker command observe the empty
    /// generation.
    pub fn cancel_build(&self) {
        match self.tx.try_send(Command::CancelBuild) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.cache.kill_build();
            }
        }
    }

    pub fn submit_changes(&self, epoch: u64, changes: Vec<PathChange>) {
        if changes.is_empty() {
            return;
        }
        if let Err(TrySendError::Full(_)) = self.tx.try_send(Command::Changes { epoch, changes }) {
            self.request_recovery_for(epoch);
        }
    }

    pub fn request_recovery(&self) {
        self.request_recovery_for(self.current_epoch());
    }

    pub fn request_recovery_for(&self, epoch: u64) {
        if epoch != self.current_epoch() {
            return;
        }
        self.recovery_pending.store(true, Ordering::Release);
        let _ = self.tx.try_send(Command::Recovery { epoch });
    }

    /// Routes startup catalog loading through the same owner as watcher
    /// changes. Sending is allowed to wait briefly behind a burst of events;
    /// unlike an event, a demand request must not be silently dropped.
    pub fn load_persisted(&self, config: Config, active_paths: Vec<String>) {
        let epoch = self.current_epoch();
        if self.load_pending.swap(true, Ordering::AcqRel) {
            return;
        }
        if self
            .tx
            .send(Command::Load {
                epoch,
                config,
                active_paths,
            })
            .is_err()
        {
            self.load_pending.store(false, Ordering::Release);
        }
    }

    /// Routes a filename-search demand through the index owner. The
    /// cancellable scanner remains a helper owned by `FileCache`, while its
    /// completed result is committed by the worker.
    pub fn ensure_built(&self, config: Config, active_paths: Vec<String>) {
        let epoch = self.current_epoch();
        if self.demand_pending.swap(true, Ordering::AcqRel) {
            return;
        }
        if self
            .tx
            .send(Command::Demand {
                epoch,
                config,
                active_paths,
            })
            .is_err()
        {
            self.demand_pending.store(false, Ordering::Release);
        }
    }

    pub fn spawn_periodic_refresh(
        self: &Arc<Self>,
        interval: Duration,
        watcher_live: Arc<std::sync::atomic::AtomicBool>,
    ) {
        let service = self.clone();
        std::thread::spawn(move || loop {
            let deadline = Instant::now() + interval;
            while !service.shutdown.load(Ordering::Acquire) {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                std::thread::sleep(remaining.min(Duration::from_millis(100)));
            }
            if service.shutdown.load(Ordering::Acquire) {
                return;
            }
            // A healthy recursive watcher already applies ordinary file
            // changes incrementally. Avoid rereading every active root on
            // every timer tick; the timer remains the fallback when no
            // watcher could be registered, while overflow/directory signals
            // still enqueue immediate recovery requests.
            if !watcher_live.load(Ordering::Acquire) {
                service.request_recovery();
            }
        });
    }
}

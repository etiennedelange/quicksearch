//! Filename index for "Files" mode, porting quicksearch's
//! `_ensure_file_cache`/`_build_file_cache`: built once via `rg --files`,
//! published for searching after retained filesystem changes are replayed.

use crate::platform::subprocess::no_window_command;
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::Config;
use crate::index_store::{
    ripgrep_version, CatalogEntry, CatalogIdentity, CatalogLoad, CatalogStore,
};

#[derive(PartialEq, Clone, Copy)]
pub enum CacheState {
    Empty,
    /// The persisted catalog is being read off disk. Distinct from `Building`
    /// because it must *suppress* a build rather than be superseded by one:
    /// this is the state a search lands in for the first second or two after
    /// launch, and treating it as `Empty` is what used to send every relaunch
    /// off to walk both drives with `rg --files` while a perfectly good
    /// catalog was already on its way in.
    Loading,
    Building,
    Ready,
}

/// A spawned `rg --files` child, held here so it can be killed immediately
/// from outside the builder thread (a source toggle, or hiding the window
/// mid-build) — same shape as `search::RunningSearch`, generation tag
/// included.
///
/// That tag is load-bearing even though `ensure_built` only ever starts a
/// build while `state == Empty`. "At most one build in flight" holds for
/// *registered* builds, but a build is spawned before it is registered, and
/// an `invalidate` in that gap sets the state back to `Empty` — which re-arms
/// `ensure_built` to start a second one. Both threads then reach for this one
/// slot, and only the generation says which of them owns what is in it.
struct RunningBuild {
    generation: u64,
    child: Child,
}

/// Completed enumeration handed back to the IndexService owner. Scanning is
/// allowed to run on its own thread so cancellation remains responsive, but
/// publication and persistence happen only when the worker consumes this
/// outcome.
pub(crate) struct BuildOutput {
    pub(crate) generation: u64,
    pub(crate) config: Config,
    pub(crate) active_paths: Vec<String>,
    pub(crate) entries: Vec<(Arc<str>, Arc<str>)>,
    pub(crate) completed_roots: usize,
    pub(crate) failed_roots: Vec<String>,
}

/// Completed maintenance walk handed back to the IndexService owner. The
/// refresh thread only enumerates; the worker validates, publishes, and saves
/// the replacement view in order with watcher changes.
pub(crate) struct RefreshOutput {
    pub(crate) epoch: u64,
    pub(crate) config: Config,
    pub(crate) active_paths: Vec<String>,
    pub(crate) entries: Vec<(Arc<str>, Arc<str>)>,
}

pub(crate) struct SubtreeRefreshOutput {
    pub(crate) epoch: u64,
    pub(crate) directories: Vec<std::path::PathBuf>,
    pub(crate) entries: Vec<(Arc<str>, Arc<str>)>,
}

pub(crate) struct LoadOutput {
    pub(crate) generation: u64,
    pub(crate) config: Config,
    pub(crate) active_paths: Vec<String>,
    pub(crate) loaded: CatalogLoad,
}

/// Everything a reader must observe consistently: which build produced the
/// entries, whether that build is still running, and the entries themselves.
///
/// These were four independent `Mutex`es. Keeping them apart meant a scanning
/// reader could hold a byte offset into `entries` while a concurrent
/// [`FileCache::invalidate`] emptied that same `Vec` — the offset then
/// pointed past the end, and the next slice panicked, taking the search
/// worker thread (and with it the `qs-done` the frontend waits on) down with
/// it. One lock makes that state unrepresentable, and incidentally cuts the
/// scan loop from three lock acquisitions per pass to one.
struct Inner {
    /// Bumped by every event that makes an existing `index` offset
    /// meaningless: a new build starting, an invalidation, a killed build.
    generation: u64,
    state: CacheState,
    /// (full path, lowercased basename). The scanner appends rows; replay
    /// reconciles buffered changes before searches capture the final view.
    ///
    /// `Arc<str>` rather than `String` so handing a window of the index to a
    /// scanning thread costs a refcount bump per entry instead of two heap
    /// allocations. On a home-directory index of a few hundred thousand
    /// files that is the difference between transiently copying the entire
    /// index on every search and copying none of it.
    entries: Vec<(Arc<str>, Arc<str>)>,
    /// Last completed searchable view. Readers clone this `Arc` and can keep
    /// traversing it while the worker applies a newer watcher revision.
    published: Arc<Vec<(Arc<str>, Arc<str>)>>,
    error: Option<String>,
    /// Set when a search asked for a build while the state was `Loading`. If
    /// the catalog turns out to be missing or stale, that request is what
    /// decides whether the load hands off to a real `rg --files` build or
    /// simply leaves the index cold — a load nobody is waiting on must not
    /// start a whole-drive walk on its own.
    build_requested: bool,
    pending_changes: Vec<crate::index_watcher::PathChange>,
    pending_bytes: usize,
    pending_overflow: bool,
    refresh_tracking: bool,
    persistence_error: Option<String>,
}

impl Inner {
    fn clear_pending(&mut self) {
        self.pending_changes.clear();
        self.pending_bytes = 0;
        self.pending_overflow = false;
    }

    fn retain_changes(&mut self, changes: Vec<crate::index_watcher::PathChange>) {
        use crate::index_watcher::PathChange;
        const MAX_EVENTS: usize = 16_384;
        const MAX_BYTES: usize = 8 * 1024 * 1024;
        for change in changes {
            let bytes = match &change {
                PathChange::Remove(path) | PathChange::Upsert(path) => path.as_os_str().len() * 2,
            };
            if self.pending_overflow
                || self.pending_changes.len() >= MAX_EVENTS
                || self.pending_bytes.saturating_add(bytes) > MAX_BYTES
            {
                self.pending_changes.clear();
                self.pending_bytes = 0;
                self.pending_overflow = true;
                return;
            }
            self.pending_bytes += bytes;
            self.pending_changes.push(change);
        }
    }
}

pub struct FileCache {
    inner: Mutex<Inner>,
    /// Deliberately outside `inner`: killing and reaping a child blocks, and
    /// must never happen while holding the lock the scan loop needs.
    running: Mutex<Option<RunningBuild>>,
    /// A background refresh's `rg` child. Separate from `running` because a
    /// refresh deliberately does *not* claim the cache while it walks — it
    /// has no cache generation to be keyed by until the moment it swaps its
    /// result in.
    refreshing: Mutex<Option<Child>>,
    /// Bumped by anything that makes an in-flight background refresh's result
    /// unusable (another refresh, an invalidate, a source toggle). The
    /// refresh re-checks it before swapping, so a superseded walk is
    /// discarded rather than overwriting newer state.
    refresh_epoch: std::sync::atomic::AtomicU64,
    /// Whether a background refresh is walking or saving right now. A refresh
    /// holds a complete second copy of the index for its whole life, so these
    /// must never stack: a refresh interval shorter than a refresh takes, or
    /// a burst of watcher overflows, would otherwise pile up threads that
    /// each cost the entire index in memory.
    refresh_active: std::sync::atomic::AtomicBool,
    /// Set before application shutdown so late scan completions cannot publish
    /// into a cache whose worker and persistence owner are going away.
    shutting_down: std::sync::atomic::AtomicBool,
    /// Set when a recovery arrives during a refresh. The completed refresh
    /// cannot include an event that arrived after its walk boundary, so one
    /// follow-up pass is required; repeated requests remain coalesced.
    recovery_pending: std::sync::atomic::AtomicBool,
    /// Unix seconds at which the last watcher-triggered recovery was
    /// accepted. Repeated notify errors must not relaunch a whole-drive walk
    /// every time the backend reports the same overflow condition.
    last_recovery_request: std::sync::atomic::AtomicU64,
    store: Arc<dyn CatalogStore>,
    /// Serializes catalog writes and lets full snapshots validate their
    /// generation before publication. Lock order is persistence then inner;
    /// searches/cancellation never wait for persistence while holding inner.
    persistence: Mutex<()>,
    /// Notified with `"patch"` when the watcher patches a `Ready` index, or
    /// `"rebuild"` when a full rebuild starts (periodic timer, or the
    /// watcher's own overflow fallback) — purely a UI signal for the
    /// colophon's activity dot, `None` in tests that don't need it.
    on_activity: Option<Arc<dyn Fn(&str) + Send + Sync>>,
    on_build_complete: Mutex<Option<Arc<dyn Fn(BuildOutput) -> Option<BuildOutput> + Send + Sync>>>,
    on_refresh_complete:
        Mutex<Option<Arc<dyn Fn(RefreshOutput) -> Option<RefreshOutput> + Send + Sync>>>,
    on_subtree_refresh_complete: Mutex<
        Option<Arc<dyn Fn(SubtreeRefreshOutput) -> Option<SubtreeRefreshOutput> + Send + Sync>>,
    >,
    on_load_complete: Mutex<Option<Arc<dyn Fn(LoadOutput) -> Option<LoadOutput> + Send + Sync>>>,
}

/// What a scanning reader gets back from [`FileCache::window`].
pub enum Window {
    /// Entries at `index..`, capped at the requested length. May be empty —
    /// that just means the builder has not produced more of them yet.
    Chunk(Vec<(Arc<str>, Arc<str>)>),
    /// Nothing further is coming for this generation and the reader has
    /// consumed everything: the build finished, or it failed, or it was
    /// killed and no replacement was started.
    Exhausted,
    /// The index this reader was walking was dropped or rebuilt underneath
    /// it. Its `index` is meaningless now and it must stop.
    Superseded,
}

impl FileCache {
    pub fn new(
        store: Arc<dyn CatalogStore>,
        on_activity: Option<Arc<dyn Fn(&str) + Send + Sync>>,
    ) -> Self {
        FileCache {
            inner: Mutex::new(Inner {
                generation: 0,
                state: CacheState::Empty,
                entries: Vec::new(),
                published: Arc::new(Vec::new()),
                error: None,
                build_requested: false,
                pending_changes: Vec::new(),
                pending_bytes: 0,
                pending_overflow: false,
                refresh_tracking: false,
                persistence_error: None,
            }),
            running: Mutex::new(None),
            refreshing: Mutex::new(None),
            refresh_epoch: std::sync::atomic::AtomicU64::new(0),
            refresh_active: std::sync::atomic::AtomicBool::new(false),
            shutting_down: std::sync::atomic::AtomicBool::new(false),
            recovery_pending: std::sync::atomic::AtomicBool::new(false),
            last_recovery_request: std::sync::atomic::AtomicU64::new(0),
            store,
            persistence: Mutex::new(()),
            on_activity,
            on_build_complete: Mutex::new(None),
            on_refresh_complete: Mutex::new(None),
            on_subtree_refresh_complete: Mutex::new(None),
            on_load_complete: Mutex::new(None),
        }
    }

    pub(crate) fn set_build_completion_callback(
        &self,
        callback: Arc<dyn Fn(BuildOutput) -> Option<BuildOutput> + Send + Sync>,
    ) {
        *self.on_build_complete.lock().unwrap() = Some(callback);
    }

    pub(crate) fn set_refresh_completion_callback(
        &self,
        callback: Arc<dyn Fn(RefreshOutput) -> Option<RefreshOutput> + Send + Sync>,
    ) {
        *self.on_refresh_complete.lock().unwrap() = Some(callback);
    }

    pub(crate) fn set_subtree_refresh_completion_callback(
        &self,
        callback: Arc<dyn Fn(SubtreeRefreshOutput) -> Option<SubtreeRefreshOutput> + Send + Sync>,
    ) {
        *self.on_subtree_refresh_complete.lock().unwrap() = Some(callback);
    }

    pub(crate) fn set_load_completion_callback(
        &self,
        callback: Arc<dyn Fn(LoadOutput) -> Option<LoadOutput> + Send + Sync>,
    ) {
        *self.on_load_complete.lock().unwrap() = Some(callback);
    }

    pub(crate) fn shutdown(&self) {
        self.shutting_down
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.supersede_refresh();
        self.kill_running();
        {
            let mut inner = self.inner.lock().unwrap();
            inner.build_requested = false;
            inner.refresh_tracking = false;
            inner.clear_pending();
        }
        *self.on_build_complete.lock().unwrap() = None;
        *self.on_refresh_complete.lock().unwrap() = None;
        *self.on_subtree_refresh_complete.lock().unwrap() = None;
        *self.on_load_complete.lock().unwrap() = None;
    }

    fn notify_activity(&self, kind: &str) {
        if let Some(cb) = &self.on_activity {
            cb(kind);
        }
    }

    pub fn persistence_error(&self) -> Option<String> {
        self.inner.lock().unwrap().persistence_error.clone()
    }

    fn record_persistence_result(&self, result: Result<(), crate::index_store::IndexError>) {
        let mut inner = self.inner.lock().unwrap();
        inner.persistence_error = result.err().map(|error| error.to_string());
    }

    fn record_refresh_error(&self, message: String) {
        let mut inner = self.inner.lock().unwrap();
        if inner.state == CacheState::Ready {
            inner.error = Some(message);
        }
    }

    /// The startup fast path: loads the persisted catalog straight into the
    /// in-memory snapshot — no `rg --files` spawn — if its identity still
    /// matches `active_paths`/`config.exclude_dirs`/the installed `rg`
    /// version. Runs on its own thread since SQLite I/O over a few hundred
    /// thousand rows shouldn't block startup.
    ///
    /// A persisted catalog is only ever as fresh as the last time this app
    /// (or a full rebuild) actually ran — a file created while the app was
    /// closed had no watcher around to see it, and stays invisible until
    /// `filename_index_refresh_minutes` next elapses or a source gets
    /// manually toggled (`invalidate_file_index`). Deliberately not
    /// self-reconciling on every launch: a couple of million paths across
    /// `C:\` + `D:\` makes that walk expensive enough (and, under real
    /// endpoint AV interception, disk-heavy enough) that it should be an
    /// explicit action, not something every relaunch pays for silently.
    pub fn load_persisted(cache: Arc<FileCache>, config: Config, active_paths: Vec<String>) {
        if cache
            .shutting_down
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }
        // Claimed *synchronously*, before the reading thread starts. The
        // whole point: a search arriving in the next millisecond must find
        // `Loading` and wait, not find `Empty` and start walking two drives.
        // Reading the catalog first and claiming afterwards is the race that
        // made every relaunch pay for a full `rg --files` it did not need.
        let generation = {
            let mut inner = cache.inner.lock().unwrap();
            if inner.state != CacheState::Empty {
                return;
            }
            inner.generation += 1;
            inner.state = CacheState::Loading;
            inner.clear_pending();
            inner.entries = Vec::new();
            inner.published = Arc::new(Vec::new());
            inner.error = None;
            inner.build_requested = false;
            inner.generation
        };

        std::thread::spawn(move || {
            let identity = CatalogIdentity::compute_with_args(
                &active_paths,
                &config.exclude_dirs,
                ripgrep_version(),
                &config.rg_extra_args,
            );
            // Read after earlier published revisions have finished saving.
            // Do not hold the cache mutex while waiting on disk work.
            let loaded = {
                let _persist = cache.persistence.lock().unwrap();
                if cache.generation() != generation {
                    return;
                }
                cache.store.load(&identity)
            };
            let output = LoadOutput {
                generation,
                config: config.clone(),
                active_paths: active_paths.clone(),
                loaded: match loaded {
                    Ok(value) => value,
                    Err(_) => CatalogLoad::MissingOrStale,
                },
            };
            let callback = cache.on_load_complete.lock().unwrap().clone();
            if let Some(callback) = callback {
                if let Some(output) = callback(output) {
                    finish_load(&cache, output);
                }
            } else {
                finish_load(&cache, output);
            }
        });
    }

    /// Rebuilds the index on a timer regardless of its current freshness —
    /// the bounded-staleness backstop underneath [`IndexWatcher`]
    /// (`index_watcher.rs`): a watcher can fail to register for a root (a
    /// network path, a permissions error) or report a dropped/overflowed
    /// notification, and this is what still catches up eventually either way.
    /// Re-walks every currently-active root using the config on disk right
    /// now (not whatever was current when the caller started) — shared by the
    /// periodic refresh and the watcher's own overflow fallback.
    pub fn refresh_now(cache: Arc<FileCache>) {
        if cache
            .shutting_down
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }
        if cache
            .refresh_active
            .load(std::sync::atomic::Ordering::Acquire)
        {
            cache
                .recovery_pending
                .store(true, std::sync::atomic::Ordering::Release);
            return;
        }
        let cache_state = cache.status().0;
        if cache_state != CacheState::Ready {
            if matches!(cache_state, CacheState::Loading | CacheState::Building) {
                // Keep recovery requested until the initial snapshot is
                // available. The IndexService worker consumes this marker
                // and launches one authoritative follow-up walk.
                cache
                    .recovery_pending
                    .store(true, std::sync::atomic::Ordering::Release);
            }
            return;
        }
        let config = crate::config::load();
        let active_paths = crate::search::active_paths(&config);
        if active_paths.is_empty() {
            return;
        }
        FileCache::refresh_in_background(cache, config, active_paths);
    }

    /// Requests watcher recovery with a cooldown. The watcher callback can
    /// receive a stream of errors for one underlying overflow; each one must
    /// not start another full-drive reconciliation after the first scan is
    /// already scheduled.
    pub fn request_recovery(cache: Arc<FileCache>) {
        if cache
            .shutting_down
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }
        const COOLDOWN_SECS: u64 = 300;
        if cache
            .refresh_active
            .load(std::sync::atomic::Ordering::Acquire)
        {
            cache
                .recovery_pending
                .store(true, std::sync::atomic::Ordering::Release);
            return;
        }
        let now = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            Ok(value) => value.as_secs(),
            Err(_) => return,
        };
        let previous = cache
            .last_recovery_request
            .load(std::sync::atomic::Ordering::Relaxed);
        if now.saturating_sub(previous) < COOLDOWN_SECS {
            return;
        }
        if cache
            .last_recovery_request
            .compare_exchange(
                previous,
                now,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::Relaxed,
            )
            .is_ok()
        {
            FileCache::refresh_now(cache);
        }
    }

    /// Whether a background refresh is currently walking or saving a catalog.
    /// This is exposed for status reporting so the UI can distinguish a quiet
    /// ready index from one that is actively doing disk work.
    pub fn refresh_is_active(&self) -> bool {
        self.refresh_active
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Takes a deferred recovery only when the current view is ready. Scan
    /// and refresh threads set this marker when they cross a recovery request;
    /// the IndexService worker owns starting the follow-up operation.
    pub fn take_ready_recovery(&self) -> bool {
        if self.status().0 != CacheState::Ready {
            return false;
        }
        self.recovery_pending
            .swap(false, std::sync::atomic::Ordering::AcqRel)
    }

    /// Bumps the refresh epoch and kills any refresh child still walking, so
    /// a superseded refresh stops burning a drive walk nobody will use.
    fn supersede_refresh(&self) {
        self.refresh_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Some(mut child) = self.refreshing.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Walks the roots into a side buffer and swaps the finished result in,
    /// rather than emptying the index first and refilling it.
    ///
    /// The old shape (`invalidate` then `ensure_built`) meant every hourly
    /// refresh — and every watcher overflow, which on whole-drive recursive
    /// watches is routine — made filename search dead for the entire
    /// multi-minute walk of both drives, for an index that was almost
    /// entirely correct already. Nothing here touches the live index until
    /// the replacement is complete, so a refresh is invisible to the user
    /// except for the colophon's activity dot.
    ///
    /// The cost is holding both the old and the new entry lists at once
    /// around the swap. That is the deliberate trade: a transient memory
    /// spike during a background refresh, instead of the tool not working.
    pub fn refresh_in_background(cache: Arc<FileCache>, config: Config, active_paths: Vec<String>) {
        if cache
            .shutting_down
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }
        // A maintenance reconciliation is only meaningful when a completed
        // view already exists. Initial loading/building owns the first walk;
        // allowing a timer or watcher recovery to run beside it doubles the
        // whole-drive I/O and can make the index appear to restart forever.
        {
            let mut inner = cache.inner.lock().unwrap();
            if inner.state != CacheState::Ready {
                return;
            }
            if cache
                .refresh_active
                .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                return;
            }
            inner.clear_pending();
            inner.refresh_tracking = true;
        }
        let epoch = cache
            .refresh_epoch
            .load(std::sync::atomic::Ordering::SeqCst);
        cache.notify_activity("rebuild");

        std::thread::spawn(move || {
            // Cleared however this thread leaves — an early return on a
            // missing `rg`, a superseded epoch, or normal completion — so a
            // refusal above can never become permanent.
            let _active = RefreshActive(cache.clone());
            let Ok(mut child) = spawn_files_walk(&config, &active_paths) else {
                // `rg` is missing. The live index — which predates this
                // refresh — is still perfectly usable, and `build` already
                // surfaces this failure on the path where it actually
                // leaves the user with nothing.
                return;
            };
            let stdout = child.stdout.take().expect("piped stdout");
            let stderr = child.stderr.take().expect("piped stderr");
            let stderr_reader = capture_stderr(stderr);

            {
                let mut slot = cache.refreshing.lock().unwrap();
                if cache
                    .refresh_epoch
                    .load(std::sync::atomic::Ordering::SeqCst)
                    != epoch
                {
                    // Superseded between the spawn and here; kill it now,
                    // because `Child::drop` does not.
                    let _ = child.kill();
                    let _ = child.wait();
                    return;
                }
                *slot = Some(child);
            }

            let mut reader = BufReader::new(stdout);
            let mut entries: Vec<(Arc<str>, Arc<str>)> = Vec::new();
            let mut line = String::new();
            let mut read_error = None;
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Err(error) => {
                        read_error = Some(error.to_string());
                        break;
                    }
                    Ok(_) => {}
                }
                let path = line.trim_end_matches(['\r', '\n']);
                if path.is_empty() {
                    continue;
                }
                // Cheap enough to check per line, and it stops a superseded
                // refresh from reading a whole drive into a buffer that is
                // already guaranteed to be discarded.
                if cache
                    .refresh_epoch
                    .load(std::sync::atomic::Ordering::SeqCst)
                    != epoch
                {
                    return;
                }
                let cut = path.rfind(['\\', '/']).map(|i| i + 1).unwrap_or(0);
                let name: Arc<str> = path[cut..].to_lowercase().into();
                entries.push((Arc::from(path), name));
            }

            let status = {
                let mut slot = cache.refreshing.lock().unwrap();
                if let Some(mut child) = slot.take() {
                    child.wait().ok()
                } else {
                    None
                }
            };
            let stderr_text = stderr_reader.join().unwrap_or_default();

            // An empty successful walk is authoritative: all files under the
            // selected roots are gone. A failed walk with no output is not;
            // preserve the working snapshot and report the failure instead.
            if let Some(error) = read_error {
                cache.record_refresh_error(format!(
                    "could not read filename enumeration output: {error}"
                ));
                return;
            }
            if status.map_or(true, |status| !matches!(status.code(), Some(0) | Some(1))) {
                let detail = if stderr_text.is_empty() {
                    "no diagnostic output"
                } else {
                    stderr_text.as_str()
                };
                cache.record_refresh_error(format!(
                    "ripgrep could not finish refreshing the filename index: {detail}"
                ));
                return;
            }

            let output = RefreshOutput {
                epoch,
                config: config.clone(),
                active_paths: active_paths.clone(),
                entries,
            };
            let callback = cache.on_refresh_complete.lock().unwrap().clone();
            if let Some(callback) = callback {
                if let Some(output) = callback(output) {
                    Self::finish_refresh(&cache, output);
                }
            } else {
                // Isolated FileCache tests do not install an IndexService
                // owner. Keep that construction useful without adding a
                // second production publication path.
                Self::finish_refresh(&cache, output);
            }
        });
    }

    /// Reconciles moved-in directory subtrees without rereading every active
    /// root. The watcher has already applied the directory event itself; this
    /// targeted walk replaces only that subtree and persists a small patch.
    pub fn refresh_subtrees(cache: Arc<FileCache>, directories: Vec<std::path::PathBuf>) {
        if cache
            .shutting_down
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }
        let mut directories = directories;
        directories.sort_by(|left, right| left.to_string_lossy().cmp(&right.to_string_lossy()));
        directories.dedup_by(|left, right| {
            left.to_string_lossy()
                .eq_ignore_ascii_case(&right.to_string_lossy())
        });
        if directories.is_empty() {
            return;
        }
        {
            let mut inner = cache.inner.lock().unwrap();
            if inner.state != CacheState::Ready {
                return;
            }
            if cache
                .refresh_active
                .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                cache
                    .recovery_pending
                    .store(true, std::sync::atomic::Ordering::Release);
                return;
            }
            inner.clear_pending();
            inner.refresh_tracking = true;
        }
        let epoch = cache
            .refresh_epoch
            .load(std::sync::atomic::Ordering::SeqCst);
        cache.notify_activity("rebuild");
        std::thread::spawn(move || {
            let _active = RefreshActive(cache.clone());
            let config = crate::config::load();
            let mut entries = Vec::new();
            for directory in &directories {
                match enumerate_subtree(&cache, &config, directory, epoch) {
                    Ok(mut found) => entries.append(&mut found),
                    Err(error) if error == "superseded" => return,
                    Err(error) => {
                        cache.record_refresh_error(format!(
                            "could not refresh directory {}: {error}",
                            directory.display()
                        ));
                        cache
                            .recovery_pending
                            .store(true, std::sync::atomic::Ordering::Release);
                        return;
                    }
                }
            }
            let output = SubtreeRefreshOutput {
                epoch,
                directories,
                entries,
            };
            let callback = cache.on_subtree_refresh_complete.lock().unwrap().clone();
            if let Some(callback) = callback {
                if let Some(output) = callback(output) {
                    Self::finish_subtree_refresh(&cache, output);
                }
            } else {
                Self::finish_subtree_refresh(&cache, output);
            }
        });
    }

    fn accept_subtree_refresh(
        &self,
        epoch: u64,
        directories: &[std::path::PathBuf],
        discovered: Vec<(Arc<str>, Arc<str>)>,
    ) -> Option<(Vec<String>, Vec<CatalogEntry>)> {
        let mut inner = self.inner.lock().unwrap();
        if self.refresh_epoch.load(std::sync::atomic::Ordering::SeqCst) != epoch
            || !inner.refresh_tracking
            || inner.pending_overflow
            || inner.state != CacheState::Ready
        {
            return None;
        }
        let directory_keys = directories
            .iter()
            .map(|directory| directory.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let mut removed = Vec::new();
        inner.entries.retain(|(path, _)| {
            let keep = !directory_keys
                .iter()
                .any(|directory| is_removed_path(directory, path));
            if !keep {
                removed.push(path.to_ascii_lowercase());
            }
            keep
        });
        let mut seen = std::collections::HashSet::new();
        let mut upserts = Vec::new();
        for (path, basename_folded) in discovered {
            let key = path.to_ascii_lowercase();
            if !seen.insert(key) {
                continue;
            }
            inner.entries.push((path.clone(), basename_folded.clone()));
            upserts.push(CatalogEntry {
                path,
                basename_folded,
            });
        }
        if let Ok((late_removed, late_upserts)) = replay_pending(&mut inner) {
            removed.extend(late_removed);
            upserts.extend(late_upserts);
        } else {
            return None;
        }
        inner.generation += 1;
        inner.error = None;
        inner.refresh_tracking = false;
        inner.published = Arc::new(inner.entries.clone());
        removed.sort_unstable();
        removed.dedup();
        Some((removed, upserts))
    }

    pub(crate) fn finish_subtree_refresh(cache: &FileCache, output: SubtreeRefreshOutput) {
        if cache
            .shutting_down
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }
        let _persist = cache.persistence.lock().unwrap();
        let Some((removed, upserts)) =
            cache.accept_subtree_refresh(output.epoch, &output.directories, output.entries)
        else {
            return;
        };
        if !removed.is_empty() || !upserts.is_empty() {
            if let Err(error) = cache.store.apply(&removed, &upserts) {
                eprintln!("[quicksearch] failed to persist directory refresh: {error}");
                cache.record_persistence_result(Err(error));
            } else {
                cache.record_persistence_result(Ok(()));
            }
        }
    }

    /// Caller holds persistence so publication and its save stay ordered.
    fn accept_refresh(
        &self,
        epoch: u64,
        entries: Vec<(Arc<str>, Arc<str>)>,
    ) -> Option<Vec<CatalogEntry>> {
        let mut inner = self.inner.lock().unwrap();
        if self.refresh_epoch.load(std::sync::atomic::Ordering::SeqCst) != epoch
            || !inner.refresh_tracking
            || inner.pending_overflow
            || inner.state != CacheState::Ready
        {
            return None;
        }
        inner.entries = entries;
        replay_pending(&mut inner).ok()?;
        inner.generation += 1;
        inner.error = None;
        inner.refresh_tracking = false;
        inner.published = Arc::new(inner.entries.clone());
        Some(
            inner
                .entries
                .iter()
                .map(|(path, basename_folded)| CatalogEntry {
                    path: path.clone(),
                    basename_folded: basename_folded.clone(),
                })
                .collect(),
        )
    }

    /// Publishes and persists a completed full refresh. This is called by the
    /// IndexService worker so a refresh cannot publish a stale snapshot in the
    /// middle of an ordered watcher/configuration command sequence.
    pub(crate) fn finish_refresh(cache: &FileCache, output: RefreshOutput) {
        if cache
            .shutting_down
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }
        let identity = CatalogIdentity::compute_with_args(
            &output.active_paths,
            &output.config.exclude_dirs,
            ripgrep_version(),
            &output.config.rg_extra_args,
        );
        let _persist = cache.persistence.lock().unwrap();
        let Some(snapshot) = cache.accept_refresh(output.epoch, output.entries) else {
            return;
        };

        if let Err(e) = cache.store.save(&identity, &snapshot) {
            eprintln!("[quicksearch] failed to persist refreshed filename index: {e}");
            cache.record_persistence_result(Err(e));
        } else {
            cache.record_persistence_result(Ok(()));
        }
    }

    /// Patches a `Ready` index in place with coalesced filesystem changes
    /// from [`IndexWatcher`], instead of a full `rg --files` rebuild.
    ///
    /// Loading/building retain notifications for replay before publication.
    /// Ready views are patched immediately and journal changes during a
    /// background refresh. Empty caches have no view to patch.
    pub fn apply_changes(&self, changes: Vec<crate::index_watcher::PathChange>) {
        if changes.is_empty() {
            return;
        }
        {
            let mut inner = self.inner.lock().unwrap();
            if matches!(inner.state, CacheState::Loading | CacheState::Building) {
                inner.retain_changes(changes);
                return;
            }
        }
        let _persist = self.persistence.lock().unwrap();
        // Cheap pre-check before the `is_file` stat per upserted path below.
        // Only an advisory read — the authoritative check is under the lock
        // further down — but it keeps a cold or still-building index from
        // paying a syscall per event for a patch it is going to discard.
        let prepared_generation = {
            let inner = self.inner.lock().unwrap();
            if inner.state != CacheState::Ready {
                return;
            }
            inner.generation
        };

        let refresh_changes = changes.clone();
        let (removed, upserts) = coalesce_batch(changes);

        let new_entries: Vec<(Arc<str>, Arc<str>)> = upserts
            .into_iter()
            // A create/rename-to event for a directory (or something already
            // gone again by the time this batch is processed) has nothing
            // that belongs in a *filename* index.
            .filter(|p| p.is_file())
            .map(|p| {
                let path_str = p.to_string_lossy().into_owned();
                let cut = path_str.rfind(['\\', '/']).map(|i| i + 1).unwrap_or(0);
                let name: Arc<str> = path_str[cut..].to_lowercase().into();
                (Arc::from(path_str), name)
            })
            .collect();

        let upsert_entries: Vec<CatalogEntry> = new_entries
            .iter()
            .map(|(path, basename_folded)| CatalogEntry {
                path: path.clone(),
                basename_folded: basename_folded.clone(),
            })
            .collect();

        // Capture the published Arc while holding the lock briefly, then do
        // the potentially linear filtering work without blocking readers.
        // The generation check below makes this optimistic preparation safe:
        // an invalidate or competing patch simply discards the prepared view.
        let (published, refresh_tracking) = {
            let inner = self.inner.lock().unwrap();
            if inner.state != CacheState::Ready || inner.generation != prepared_generation {
                return;
            }
            let published = if inner.published.is_empty() && !inner.entries.is_empty() {
                // Keep the defensive path useful for test fixtures and any
                // legacy Ready state created before publication was split
                // from the mutable build vector.
                Arc::new(inner.entries.clone())
            } else {
                inner.published.clone()
            };
            (published, inner.refresh_tracking)
        };
        // A watcher can repeat an event, report a directory that is not part
        // of the filename index, or report a removal after the path was
        // already gone. Do not publish a new generation for those no-op
        // batches: every generation change forces active searches to restart
        // and needlessly copies the snapshot.
        let removal_changes_live = removed.iter().any(|removed_path| {
            published
                .iter()
                .any(|(path, _)| is_removed_path(removed_path, path))
        });
        let upsert_changes_live = new_entries.iter().any(|(new_path, new_name)| {
            published
                .iter()
                .find(|(path, _)| path.as_ref() == new_path.as_ref())
                .map_or(true, |(_, old_name)| old_name.as_ref() != new_name.as_ref())
        });
        if !removal_changes_live && !upsert_changes_live {
            if refresh_tracking {
                let mut inner = self.inner.lock().unwrap();
                if inner.refresh_tracking && inner.generation == prepared_generation {
                    inner.retain_changes(refresh_changes);
                }
            }
            return;
        }
        // Only a batch that will publish needs a writable copy. No-op watcher
        // bursts therefore avoid copying the entire catalog altogether.
        let mut prepared_entries = (*published).clone();
        // Still guarded so a batch with no deletions does not walk the index
        // at all, and `is_removed` keeps the walk allocation-free when it
        // does. Both matter: the guard alone left every batch that deleted a
        // single temp file re-allocating the entire catalog.
        if !removed.is_empty() {
            prepared_entries.retain(|(path, _)| !is_removed(&removed, path));
        }
        if !new_entries.is_empty() {
            // Watchers may report create/rename-to repeatedly. Replace the
            // canonical path in the in-memory snapshot before appending so
            // live search results stay idempotent like SQLite's upsert.
            prepared_entries
                .retain(|(path, _)| !new_entries.iter().any(|(new_path, _)| new_path == path));
        }
        prepared_entries.extend(new_entries);

        let mut inner = self.inner.lock().unwrap();
        if inner.state != CacheState::Ready || inner.generation != prepared_generation {
            if inner.refresh_tracking {
                inner.retain_changes(refresh_changes);
            }
            return;
        }
        if inner.refresh_tracking {
            inner.retain_changes(refresh_changes);
        }
        // Bumped even though the entries aren't dropped this time: a scan
        // holding an offset into the pre-patch `Vec` must still restart rather
        // than read past entries that just moved when removed rows were
        // filtered out.
        inner.generation += 1;
        inner.entries = prepared_entries;
        inner.published = Arc::new(inner.entries.clone());
        drop(inner);

        // Best-effort, same as the full build's save: a write failure here
        // must not undo the in-memory patch that's already live for this
        // session. Without this, a watcher-caught create/delete would only
        // ever exist in memory — invisible again the moment the app restarts
        // and reloads the older catalog straight off disk.
        let removed_keys: Vec<String> = removed.into_iter().collect();
        // No transaction at all for a batch that turned out to be entirely
        // directory events or already-deleted paths.
        if !removed_keys.is_empty() || !upsert_entries.is_empty() {
            let result = self.store.apply(&removed_keys, &upsert_entries);
            if let Err(e) = &result {
                eprintln!("[quicksearch] failed to persist filename index patch: {e}");
            }
            self.record_persistence_result(result);
        }

        self.notify_activity("patch");
    }

    /// Kills whatever build is registered, whichever generation it belongs
    /// to. For the *external* cancels only (`invalidate`, `kill_build`), which
    /// bump the generation first and so are entitled to stop anything still
    /// running. A builder thread cleaning up after itself wants
    /// [`FileCache::reap_own`] instead.
    fn kill_running(&self) {
        if let Some(mut running) = self.running.lock().unwrap().take() {
            let _ = running.child.kill();
            let _ = running.child.wait();
        }
    }

    /// Installs `child` as the running build for `generation`, unless that
    /// generation has already been superseded — in which case `child` is
    /// killed here before returning `false`, since `Child::drop` does not kill
    /// the process and the caller drops its last reference on the way out.
    ///
    /// Lock order is `running` then `inner`, and only here. That cannot invert
    /// against `invalidate`/`kill_build`, because both of those scope their
    /// `inner` guard and drop it *before* reaching for `running` — neither
    /// ever holds `inner` while acquiring `running`.
    fn register(&self, generation: u64, mut child: Child) -> bool {
        let mut running = self.running.lock().unwrap();
        if self.inner.lock().unwrap().generation != generation {
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
        *running = Some(RunningBuild { generation, child });
        true
    }

    /// Reaps a builder thread's own child, but only if it is still the one
    /// installed. The generation check is the point: an unconditional take
    /// here would let a superseded build kill the child of the *newer* build
    /// that replaced it.
    fn reap_own(&self, generation: u64) -> Option<std::process::ExitStatus> {
        let mut running = self.running.lock().unwrap();
        if running.as_ref().is_some_and(|r| r.generation == generation) {
            if let Some(mut build) = running.take() {
                return build.child.wait().ok();
            }
        }
        None
    }

    /// The build a reader should tag itself with before its first
    /// [`FileCache::window`] call.
    pub fn generation(&self) -> u64 {
        self.inner.lock().unwrap().generation
    }

    /// `(state, entries so far)` — read under one lock so the count always
    /// belongs to the state it is reported with.
    pub fn status(&self) -> (CacheState, usize) {
        let inner = self.inner.lock().unwrap();
        (inner.state, inner.entries.len())
    }

    /// Returns the last completed searchable view. The returned `Arc` keeps
    /// that view alive while a watcher patch or refresh publishes a newer one.
    pub fn snapshot(&self) -> Option<Arc<Vec<(Arc<str>, Arc<str>)>>> {
        let inner = self.inner.lock().unwrap();
        (inner.state == CacheState::Ready).then(|| inner.published.clone())
    }

    /// The build error, if there is one *and* no usable index to report
    /// instead. Matches the original's "only surface `rg` not being on PATH
    /// when the scan actually came up empty because of it".
    pub fn error(&self) -> Option<String> {
        let inner = self.inner.lock().unwrap();
        if inner.state == CacheState::Empty {
            inner.error.clone()
        } else {
            None
        }
    }

    /// Up to `max` entries starting at `index`, for the build tagged
    /// `generation`.
    ///
    /// Note there is no slicing by an unchecked offset anywhere here: a
    /// stale `index` either belongs to a superseded generation (rejected
    /// outright) or is clamped against the current length, which is the
    /// normal case while the builder is between chunks.
    pub fn window(&self, generation: u64, index: usize, max: usize) -> Window {
        let inner = self.inner.lock().unwrap();
        if inner.generation != generation {
            return Window::Superseded;
        }
        if index >= inner.entries.len() {
            // `Loading` and `Building` are the only states with more entries
            // still coming. Deriving this from the state, rather than from a
            // separate `done` flag, is what stops a reader waiting forever on
            // a build that was killed or abandoned without ever setting it.
            return if matches!(inner.state, CacheState::Building | CacheState::Loading) {
                Window::Chunk(Vec::new())
            } else {
                Window::Exhausted
            };
        }
        let end = inner.entries.len().min(index.saturating_add(max));
        Window::Chunk(inner.entries[index..end].to_vec())
    }

    /// Demotes a build still in flight back to `Empty` and kills its `rg`
    /// child; a `Ready` index is left untouched — it's still good and
    /// expensive to rebuild. Matches `_kill_cache_build`, used when the
    /// window is hidden or the app quits.
    pub fn kill_build(&self) {
        {
            let mut inner = self.inner.lock().unwrap();
            if !matches!(inner.state, CacheState::Loading | CacheState::Building) {
                return;
            }
            inner.build_requested = false;
            inner.clear_pending();
            inner.generation += 1;
            inner.state = CacheState::Empty;
            inner.entries = Vec::new();
            inner.published = Arc::new(Vec::new());
        }
        self.kill_running();
    }

    /// Demotes a `Building` OR `Ready` index back to `Empty`, drops its
    /// entries, and kills any `rg` child still running — matches
    /// `_invalidate_file_cache`. Source changes discard this active view;
    /// the caller then loads the selected roots from the retained catalog.
    pub fn invalidate(&self) {
        // Unconditional, and before the state check below: a background
        // refresh holds no cache state at all while it walks, so an `Empty`
        // cache is no evidence that one isn't in flight — and its result
        // describes the configuration that was just invalidated.
        self.supersede_refresh();
        {
            let mut inner = self.inner.lock().unwrap();
            inner.build_requested = false;
            inner.refresh_tracking = false;
            inner.clear_pending();
            if inner.state == CacheState::Empty {
                return;
            }
            // Bumped *before* the entries are dropped, and under the same
            // lock, so no reader can ever see the emptied `Vec` still tagged
            // with the generation whose offsets it holds.
            inner.generation += 1;
            inner.state = CacheState::Empty;
            inner.entries = Vec::new();
            inner.published = Arc::new(Vec::new());
            inner.error = None;
        }
        self.recovery_pending
            .store(false, std::sync::atomic::Ordering::Release);
        self.kill_running();
    }

    pub fn ensure_built(cache: Arc<FileCache>, config: Config, active_paths: Vec<String>) {
        if cache
            .shutting_down
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }
        let generation = {
            let mut inner = cache.inner.lock().unwrap();
            // The catalog is already on its way in. Record that a search
            // wants an index — so a load that comes back empty hands off to a
            // build instead of stranding that search — and let it finish.
            if inner.state == CacheState::Loading {
                inner.build_requested = true;
                return;
            }
            if inner.state != CacheState::Empty {
                return;
            }
            inner.generation += 1;
            inner.state = CacheState::Building;
            inner.clear_pending();
            inner.entries = Vec::new();
            inner.published = Arc::new(Vec::new());
            inner.error = None;
            inner.generation
        };

        spawn_build(cache, config, active_paths, generation);
    }

    /// Blocks the calling (search worker) thread briefly, matching the
    /// original's `cache_done.wait(0.05)` poll while the index is still
    /// filling in.
    pub fn poll_wait() {
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn spawn_files_walk(config: &Config, active_paths: &[String]) -> std::io::Result<Child> {
    let mut cmd = no_window_command("rg");
    cmd.arg("--files");
    // Exclusion here is entirely `config.exclude_dirs`'s `--iglob`s below —
    // this app has no use for VCS/gitignore semantics, so `--no-ignore`
    // skips the `.gitignore`/`.ignore`/`.rgignore` file lookups `rg` would
    // otherwise do in every single directory across two whole drives. It
    // also closes a real gap: without it, a file sitting inside *any* git
    // repo's `.gitignore` rules anywhere on `C:\`/`D:\` was silently
    // invisible to this index, regardless of `exclude_dirs`.
    cmd.arg("--no-ignore");
    // Keeps the walk to what's actually on the two physical drives instead
    // of following a reparse point/junction into a mounted volume, a WSL
    // distro folder, or a network share — any of which could be far slower
    // or effectively unbounded compared to a plain local directory tree.
    cmd.arg("--one-file-system");
    cmd.args(exclude_glob_args(&config.exclude_dirs));
    cmd.args(&config.rg_extra_args);
    cmd.args(active_paths);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    crate::platform::subprocess::spawn_bound_to_job(&mut cmd)
}

fn enumerate_subtree(
    cache: &FileCache,
    config: &Config,
    directory: &std::path::Path,
    epoch: u64,
) -> Result<Vec<(Arc<str>, Arc<str>)>, String> {
    let root = directory.to_string_lossy().into_owned();
    let mut child = spawn_files_walk(config, std::slice::from_ref(&root))
        .map_err(|error| format!("ripgrep could not start: {error}"))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let stderr_reader = capture_stderr(stderr);
    {
        let mut slot = cache.refreshing.lock().unwrap();
        if cache
            .refresh_epoch
            .load(std::sync::atomic::Ordering::SeqCst)
            != epoch
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err("superseded".into());
        }
        *slot = Some(child);
    }
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    let mut entries = Vec::new();
    let mut read_error = None;
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Err(error) => {
                read_error = Some(error.to_string());
                break;
            }
            Ok(_) => {}
        }
        if cache
            .refresh_epoch
            .load(std::sync::atomic::Ordering::SeqCst)
            != epoch
        {
            let mut slot = cache.refreshing.lock().unwrap();
            if let Some(mut child) = slot.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            return Err("superseded".into());
        }
        let path = line.trim_end_matches(['\r', '\n']);
        if path.is_empty() {
            continue;
        }
        let cut = path.rfind(['\\', '/']).map(|index| index + 1).unwrap_or(0);
        entries.push((Arc::from(path), path[cut..].to_lowercase().into()));
    }
    let status = {
        let mut slot = cache.refreshing.lock().unwrap();
        slot.take().and_then(|mut child| child.wait().ok())
    };
    let stderr_text = stderr_reader.join().unwrap_or_default();
    if let Some(error) = read_error {
        return Err(format!("could not read output: {error}"));
    }
    if status.map_or(true, |status| !matches!(status.code(), Some(0) | Some(1))) {
        return Err(if stderr_text.is_empty() {
            "ripgrep returned an unsuccessful status".into()
        } else {
            stderr_text
        });
    }
    Ok(entries)
}

/// Drain ripgrep diagnostics concurrently with its filename stream. A
/// bounded capture prevents a broken scanner from filling stderr and hanging
/// the worker while retaining enough context to explain a failed scan.
fn capture_stderr(stderr: std::process::ChildStderr) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = std::io::Read::take(stderr, 8 * 1024).read_to_end(&mut bytes);
        String::from_utf8_lossy(&bytes).trim().to_string()
    })
}

/// Reconcile retained notifications against the final filesystem state before
/// publishing a loaded/built view. Caller holds persistence, then inner.
fn replay_pending(inner: &mut Inner) -> Result<(Vec<String>, Vec<CatalogEntry>), ()> {
    if inner.pending_overflow {
        inner.state = CacheState::Empty;
        inner.entries.clear();
        inner.published = Arc::new(Vec::new());
        inner.error = Some("Too many filesystem changes during indexing; the incomplete index was not saved. Search again to retry.".into());
        inner.clear_pending();
        return Err(());
    }
    let changes = std::mem::take(&mut inner.pending_changes);
    inner.pending_bytes = 0;
    if changes.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let (removed, upserts) = coalesce_batch(changes);
    let replaced: std::collections::HashSet<_> = upserts
        .iter()
        .map(|path| path.to_string_lossy().to_ascii_lowercase())
        .collect();
    let mut removed_rows = Vec::new();
    inner.entries.retain(|(path, _)| {
        let keep = !is_removed(&removed, path)
            && (replaced.is_empty() || !replaced.contains(&path.to_ascii_lowercase()));
        if !keep {
            removed_rows.push(path.to_ascii_lowercase());
        }
        keep
    });
    let mut added = Vec::new();
    for path in upserts.into_iter().filter(|path| path.is_file()) {
        let path = path.to_string_lossy().into_owned();
        let cut = path.rfind(['\\', '/']).map(|i| i + 1).unwrap_or(0);
        let name: Arc<str> = path[cut..].to_lowercase().into();
        let path: Arc<str> = path.into();
        inner.entries.push((path.clone(), name.clone()));
        added.push(CatalogEntry {
            path,
            basename_folded: name,
        });
    }
    Ok((removed_rows, added))
}

fn build(cache: Arc<FileCache>, config: &Config, active_paths: &[String], generation: u64) {
    let mut failed_roots = Vec::new();
    let mut completed_roots = 0usize;
    let mut entries = Vec::new();
    for root in active_paths {
        match build_root(&cache, config, root, generation) {
            Ok(mut found) => {
                completed_roots += 1;
                entries.append(&mut found);
            }
            Err(BuildRootError::Superseded) => return,
            Err(BuildRootError::Failed(error)) => failed_roots.push(format!("{root}: {error}")),
        }
    }

    let output = BuildOutput {
        generation,
        config: config.clone(),
        active_paths: active_paths.to_vec(),
        entries,
        completed_roots,
        failed_roots,
    };
    let callback = cache.on_build_complete.lock().unwrap().clone();
    if let Some(callback) = callback {
        if let Some(output) = callback(output) {
            finish_build(&cache, output);
        }
    } else {
        // Isolated FileCache tests do not install an IndexService owner.
        // Keep that construction useful without reintroducing a second
        // production publication path.
        finish_build(&cache, output);
    }
}

pub(crate) fn finish_build(cache: &FileCache, output: BuildOutput) {
    if cache
        .shutting_down
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return;
    }
    if output.completed_roots == 0 {
        let mut inner = cache.inner.lock().unwrap();
        if inner.generation == output.generation {
            inner.state = CacheState::Empty;
            inner.error = Some(format!(
                "ripgrep could not finish enumerating the configured paths: {}",
                if output.failed_roots.is_empty() {
                    "no configured paths could be indexed".to_string()
                } else {
                    output.failed_roots.join("; ")
                }
            ));
        }
        return;
    }

    let identity = CatalogIdentity::compute_with_args(
        &output.active_paths,
        &output.config.exclude_dirs,
        ripgrep_version(),
        &output.config.rg_extra_args,
    );
    let _persist = cache.persistence.lock().unwrap();
    let snapshot = {
        let mut inner = cache.inner.lock().unwrap();
        if inner.generation != output.generation {
            return;
        }
        inner.entries = output.entries;
        if replay_pending(&mut inner).is_err() {
            return;
        }
        inner.state = CacheState::Ready;
        inner.published = Arc::new(inner.entries.clone());
        inner
            .entries
            .iter()
            .map(|(path, basename_folded)| CatalogEntry {
                path: path.clone(),
                basename_folded: basename_folded.clone(),
            })
            .collect::<Vec<_>>()
    };

    // Never persist a partial walk as if it were a complete catalog. The
    // successful roots remain searchable in memory; the stale identity causes
    // the next launch to retry the failed roots.
    if output.failed_roots.is_empty() {
        if let Err(e) = cache.store.save(&identity, &snapshot) {
            eprintln!(
                "[quicksearch] failed to persist filename index, will rebuild next restart: {e}"
            );
            cache.record_persistence_result(Err(e));
        } else {
            cache.record_persistence_result(Ok(()));
        }
    } else {
        eprintln!(
            "[quicksearch] indexed {}/{} configured roots; preserving successful roots: {}",
            output.completed_roots,
            output.active_paths.len(),
            output.failed_roots.join("; ")
        );
    }
}

pub(crate) fn finish_load(cache: &Arc<FileCache>, output: LoadOutput) {
    if cache
        .shutting_down
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return;
    }
    let _persist = cache.persistence.lock().unwrap();
    let mut promote = false;
    let patch = {
        let mut inner = cache.inner.lock().unwrap();
        if inner.generation != output.generation || inner.state != CacheState::Loading {
            return;
        }
        match output.loaded {
            CatalogLoad::Ready(entries) => {
                // Keep the generation stable: readers that started while the
                // catalog was loading are already positioned at offset zero.
                inner.entries = entries
                    .into_iter()
                    .map(|e| (e.path, e.basename_folded))
                    .collect();
                let patch = replay_pending(&mut inner).ok();
                if patch.is_none() {
                    return;
                }
                inner.published = Arc::new(inner.entries.clone());
                inner.state = CacheState::Ready;
                patch
            }
            CatalogLoad::MissingOrStale => {
                if inner.build_requested {
                    inner.state = CacheState::Building;
                    promote = true;
                } else {
                    inner.state = CacheState::Empty;
                }
                None
            }
        }
    };

    if let Some((removed, upserts)) = patch {
        if !removed.is_empty() || !upserts.is_empty() {
            let result = cache.store.apply(&removed, &upserts);
            if let Err(error) = &result {
                eprintln!(
                    "[quicksearch] could not persist changes received during catalog load: {error}"
                );
            }
            cache.record_persistence_result(result);
        }
    }
    drop(_persist);

    if promote {
        spawn_build(
            cache.clone(),
            output.config,
            output.active_paths,
            output.generation,
        );
    }
}

fn spawn_build(cache: Arc<FileCache>, config: Config, active_paths: Vec<String>, generation: u64) {
    std::thread::spawn(move || build(cache, &config, &active_paths, generation));
}

enum BuildRootError {
    Superseded,
    Failed(String),
}

fn build_root(
    cache: &FileCache,
    config: &Config,
    root: &str,
    generation: u64,
) -> Result<Vec<(Arc<str>, Arc<str>)>, BuildRootError> {
    let mut child = spawn_files_walk(config, &[root.to_string()])
        .map_err(|error| BuildRootError::Failed(format!("ripgrep could not start: {error}")))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let stderr_reader = capture_stderr(stderr);
    if !cache.register(generation, child) {
        let _ = stderr_reader.join();
        return Err(BuildRootError::Superseded);
    }

    let mut chunk = Vec::new();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    let mut read_error = None;
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Err(error) => {
                read_error = Some(error.to_string());
                break;
            }
            Ok(_) => {}
        }
        if cache.generation() != generation {
            cache.reap_own(generation);
            let _ = stderr_reader.join();
            return Err(BuildRootError::Superseded);
        }
        let path = line.trim_end_matches(['\r', '\n']);
        if path.is_empty() {
            continue;
        }
        let cut = path.rfind(['\\', '/']).map(|i| i + 1).unwrap_or(0);
        chunk.push((Arc::from(path), path[cut..].to_lowercase().into()));
    }

    let status = cache.reap_own(generation);
    let stderr_text = stderr_reader.join().unwrap_or_default();
    let failure = if let Some(error) = read_error {
        Some(format!(
            "could not read filename enumeration output: {error}"
        ))
    } else {
        match status {
            None if cache.generation() != generation => return Err(BuildRootError::Superseded),
            None => Some("ripgrep process was stopped before enumeration completed".into()),
            Some(status) if !matches!(status.code(), Some(0) | Some(1)) => Some(format!(
                "exit status {}: {}",
                status
                    .code()
                    .map_or_else(|| "unknown".to_string(), |code| code.to_string()),
                if stderr_text.is_empty() {
                    "no diagnostic output"
                } else {
                    stderr_text.as_str()
                }
            )),
            Some(_) => None,
        }
    };
    if let Some(error) = failure {
        return Err(BuildRootError::Failed(error));
    }
    Ok(chunk)
}

/// Clears [`FileCache::refresh_active`] when the refresh thread unwinds,
/// whichever of its several early returns it takes.
struct RefreshActive(Arc<FileCache>);

impl Drop for RefreshActive {
    fn drop(&mut self) {
        {
            let mut inner = self.0.inner.lock().unwrap();
            if inner.refresh_tracking {
                inner.refresh_tracking = false;
                inner.clear_pending();
            }
        }
        self.0
            .refresh_active
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Whether `path` is one of the paths this batch removed, compared
/// case-insensitively.
///
/// Allocation-free on purpose. The obvious spelling — `removed.contains(&path
/// .to_lowercase())` — costs one heap allocation per entry *per batch*, and
/// this runs against every path in the index. On a whole-drive catalog that
/// is millions of allocations to find the two or three files Windows just
/// deleted from a temp directory.
///
/// The length check is what makes the linear form cheap: `removed` holds a
/// handful of paths, and almost every index entry is eliminated by an integer
/// comparison before any character is looked at.
fn is_removed(removed: &std::collections::HashSet<String>, path: &str) -> bool {
    removed
        .iter()
        .any(|candidate| is_removed_path(candidate, path))
}

fn is_removed_path(candidate: &str, path: &str) -> bool {
    if candidate.len() == path.len() {
        return candidate.eq_ignore_ascii_case(path);
    }
    if candidate.len() >= path.len() {
        return false;
    }
    path.get(..candidate.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(candidate))
        && matches!(path.as_bytes().get(candidate.len()), Some(b'\\' | b'/'))
}

/// Reduces one watcher window's raw changes to "paths to drop" and "paths to
/// (re)add", last write winning per path, compared case-insensitively.
///
/// Order is preserved for the upserts, and this is linear in the batch.
/// The previous shape re-scanned the whole pending upsert list for every
/// change — lowercasing each candidate path again on every comparison — which
/// is quadratic in the batch size. A 500ms window on a recursively-watched
/// drive root is routinely thousands of events, and that is where the work
/// stopped fitting in the window it arrived in.
fn coalesce_batch(
    changes: Vec<crate::index_watcher::PathChange>,
) -> (std::collections::HashSet<String>, Vec<std::path::PathBuf>) {
    use crate::index_watcher::PathChange;
    use std::collections::HashMap;

    // key -> position in `slots`, so a repeat of a path overwrites its
    // earlier decision in place instead of being searched for linearly.
    let mut seen: HashMap<String, usize> = HashMap::with_capacity(changes.len());
    let mut slots: Vec<Option<PathChange>> = Vec::with_capacity(changes.len());

    for change in changes {
        // ASCII-only, matching SQLite's `lower()` and `index_store`'s
        // `path_key` — the removal predicate combines all three, so they have
        // to agree with each other on what "the same path" means. Full
        // Unicode folding here would silently fail to match the rows the
        // database is asked to delete.
        let key = match &change {
            PathChange::Remove(path) | PathChange::Upsert(path) => {
                path.to_string_lossy().to_ascii_lowercase()
            }
        };
        match seen.get(&key) {
            Some(&at) => slots[at] = Some(change),
            None => {
                seen.insert(key, slots.len());
                slots.push(Some(change));
            }
        }
    }

    let mut removed = std::collections::HashSet::new();
    let mut upserts = Vec::new();
    for change in slots.into_iter().flatten() {
        match change {
            PathChange::Remove(path) => {
                removed.insert(path.to_string_lossy().to_ascii_lowercase());
            }
            PathChange::Upsert(path) => upserts.push(path),
        }
    }
    (removed, upserts)
}

pub fn exclude_glob_args(exclude_dirs: &[String]) -> Vec<String> {
    let mut args = Vec::new();
    for name in exclude_dirs {
        let trimmed = name.trim().trim_matches(['/', '\\']);
        if !trimmed.is_empty() {
            args.push("--iglob".to_string());
            // `/**` prunes the directory itself and every descendant. The
            // old trailing-slash form did not reliably prune protected
            // directories at a drive root on Windows, so ripgrep could abort
            // before reaching ordinary user files.
            args.push(format!("!{trimmed}/**"));
        }
    }
    // These Windows root entries are commonly locked or ACL-protected. Keep
    // them out of both the filename and content walks; otherwise ripgrep's
    // ignore walker can terminate the entire drive search before it reaches
    // C:\Users (including hidden MCP/config files).
    for name in [
        "Config.Msi",
        "Documents and Settings",
        "inetpub",
        "OneDriveTemp",
        "DumpStack.log.tmp",
        "hiberfil.sys",
        "pagefile.sys",
        "swapfile.sys",
    ] {
        args.push("--iglob".to_string());
        args.push(format!("!{name}"));
        args.push("--iglob".to_string());
        args.push(format!("!{name}/**"));
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index_store::{NullCatalogStore, SqliteCatalogStore};

    fn test_cache() -> FileCache {
        FileCache::new(Arc::new(NullCatalogStore), None)
    }

    fn temp_sqlite_store() -> Arc<SqliteCatalogStore> {
        let mut path = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        path.push(format!(
            "qs-filecache-test-{}-{nanos}.sqlite3",
            std::process::id()
        ));
        Arc::new(SqliteCatalogStore::open(&path).unwrap())
    }

    fn entry(path: &str) -> (Arc<str>, Arc<str>) {
        let cut = path.rfind(['\\', '/']).map(|i| i + 1).unwrap_or(0);
        (Arc::from(path), path[cut..].to_lowercase().into())
    }

    fn spawn_dummy() -> Child {
        std::process::Command::new("cmd")
            .args(["/c", "ver"])
            .spawn()
            .expect("spawn dummy child for test")
    }

    /// What `ensure_built` does to `inner` before handing a generation to a
    /// builder thread.
    fn begin_build(cache: &FileCache) -> u64 {
        let mut inner = cache.inner.lock().unwrap();
        inner.generation += 1;
        inner.state = CacheState::Building;
        inner.generation
    }

    /// Reproduces the spawn-before-register race. Build A is descheduled
    /// between spawning `rg` and registering it; an `invalidate` in that gap
    /// finds an empty slot (nothing to kill) and drops the state back to
    /// `Empty`, which re-arms `ensure_built` — so build B can start and claim
    /// the slot first. When A finally registers, an unconditional store would
    /// overwrite B's `RunningBuild`, and since `Child::drop` does not kill,
    /// B's `rg` would keep walking the whole drive with nothing able to
    /// cancel it.
    #[test]
    fn register_rejects_a_build_superseded_before_it_registered() {
        let cache = test_cache();

        let gen_a = begin_build(&cache);
        cache.invalidate();
        assert!(
            cache.running.lock().unwrap().is_none(),
            "the invalidate landed while A was still unregistered"
        );

        let gen_b = begin_build(&cache);
        assert!(cache.register(gen_b, spawn_dummy()));

        assert!(
            !cache.register(gen_a, spawn_dummy()),
            "a superseded build must not register"
        );
        let guard = cache.running.lock().unwrap();
        assert_eq!(
            guard
                .as_ref()
                .expect("B's child must still be tracked")
                .generation,
            gen_b
        );
    }

    /// The other half of the same bug: a superseded build cleaning up after
    /// itself (its `append` was rejected, or it hit EOF) must reap only its
    /// own child. The old code took whatever was in the slot, which by then
    /// could be the newer build's — killing a live index build outright.
    #[test]
    fn a_superseded_build_reaping_itself_never_touches_a_newer_builds_child() {
        let cache = test_cache();

        let gen_a = begin_build(&cache);
        assert!(cache.register(gen_a, spawn_dummy()));

        cache.invalidate(); // kills A's child and empties the slot
        let gen_b = begin_build(&cache);
        assert!(cache.register(gen_b, spawn_dummy()));

        cache.reap_own(gen_a);

        let guard = cache.running.lock().unwrap();
        assert_eq!(
            guard
                .as_ref()
                .expect("reap_own(gen_a) must not have taken B's child")
                .generation,
            gen_b
        );
    }

    /// Reproduces the reported crash: a scan holds an offset into a `Ready`
    /// index, a source toggle invalidates it, and the scan asks for its next
    /// window. The old code sliced `entries[index..]` on the now-empty `Vec`
    /// and panicked, killing the search worker (so `qs-done` never fired and
    /// the UI hung on INDEXING forever).
    #[test]
    fn window_rejects_a_stale_index_after_invalidate() {
        let cache = test_cache();
        {
            let mut inner = cache.inner.lock().unwrap();
            inner.state = CacheState::Ready;
            inner.entries = (0..1000)
                .map(|i| entry(&format!("C:\\a\\f{i}.txt")))
                .collect();
        }
        let generation = cache.generation();
        assert!(matches!(cache.window(generation, 500, 64), Window::Chunk(c) if c.len() == 64));

        cache.invalidate();

        // The scan's offset (500) is now far past the emptied index.
        assert!(matches!(
            cache.window(generation, 500, 64),
            Window::Superseded
        ));
    }

    /// An index offset that has merely outrun a still-building index is
    /// normal, not an error — the reader gets an empty chunk and polls.
    #[test]
    fn window_clamps_an_offset_past_the_end_while_building() {
        let cache = test_cache();
        {
            let mut inner = cache.inner.lock().unwrap();
            inner.state = CacheState::Building;
            inner.entries = vec![entry("C:\\a\\one.txt")];
        }
        let generation = cache.generation();
        assert!(matches!(cache.window(generation, 1, 64), Window::Chunk(c) if c.is_empty()));
        assert!(matches!(cache.window(generation, 99, 64), Window::Chunk(c) if c.is_empty()));
    }

    /// The other half of the same bug: a build that was killed or abandoned
    /// leaves `Empty`, and a reader waiting on it must be told there is
    /// nothing more coming rather than polling every 50ms forever. The old
    /// code keyed this off a separate `done` flag that no kill path ever set.
    #[test]
    fn window_reports_exhausted_once_no_build_is_running() {
        let cache = test_cache();
        {
            let mut inner = cache.inner.lock().unwrap();
            inner.state = CacheState::Building;
        }
        let generation = cache.generation();
        assert!(matches!(cache.window(generation, 0, 64), Window::Chunk(_)));

        // The build fails or is abandoned without a replacement starting.
        cache.inner.lock().unwrap().state = CacheState::Empty;
        assert!(matches!(cache.window(generation, 0, 64), Window::Exhausted));
    }

    #[test]
    fn exclude_glob_args_builds_negated_directory_globs() {
        let args = exclude_glob_args(&["node_modules".into(), " /target/ ".into(), "  ".into()]);
        assert_eq!(
            args,
            vec![
                "--iglob",
                "!node_modules/**",
                "--iglob",
                "!target/**",
                "--iglob",
                "!Config.Msi",
                "--iglob",
                "!Config.Msi/**",
                "--iglob",
                "!Documents and Settings",
                "--iglob",
                "!Documents and Settings/**",
                "--iglob",
                "!inetpub",
                "--iglob",
                "!inetpub/**",
                "--iglob",
                "!OneDriveTemp",
                "--iglob",
                "!OneDriveTemp/**",
                "--iglob",
                "!DumpStack.log.tmp",
                "--iglob",
                "!DumpStack.log.tmp/**",
                "--iglob",
                "!hiberfil.sys",
                "--iglob",
                "!hiberfil.sys/**",
                "--iglob",
                "!pagefile.sys",
                "--iglob",
                "!pagefile.sys/**",
                "--iglob",
                "!swapfile.sys",
                "--iglob",
                "!swapfile.sys/**",
            ]
        );
    }

    /// The startup fast path: a catalog saved under the identity this run
    /// computes must become queryable without a new `rg --files` process
    /// ever spawning.
    #[test]
    fn toggling_saved_sources_reloads_only_enabled_entries_without_building() {
        let store = temp_sqlite_store();
        let config = Config::default();
        let roots = vec![r"C:\".to_string(), r"D:\".to_string()];
        let identity = CatalogIdentity::compute_with_args(
            &roots,
            &config.exclude_dirs,
            ripgrep_version(),
            &config.rg_extra_args,
        );
        store
            .save(
                &identity,
                &[
                    CatalogEntry {
                        path: Arc::from(r"C:\saved.rs"),
                        basename_folded: Arc::from("saved.rs"),
                    },
                    CatalogEntry {
                        path: Arc::from(r"D:\retained.rs"),
                        basename_folded: Arc::from("retained.rs"),
                    },
                ],
            )
            .unwrap();
        let cache = Arc::new(FileCache::new(store, None));
        for selected in [roots.clone(), vec![roots[0].clone()], roots] {
            cache.invalidate();
            FileCache::load_persisted(cache.clone(), config.clone(), selected.clone());
            wait_until(
                || cache.status().0 != CacheState::Loading,
                "source catalog load",
            );
            assert!(cache.status().0 == CacheState::Ready);
            assert_eq!(cache.status().1, selected.len());
            assert!(cache.running.lock().unwrap().is_none());
            if selected.len() == 1 {
                assert_eq!(
                    cache.inner.lock().unwrap().entries[0].0.as_ref(),
                    r"C:\saved.rs"
                );
            }
        }
    }

    #[test]
    fn a_persisted_catalog_matching_identity_is_ready_without_spawning_a_build() {
        use crate::index_store::{CatalogEntry, CatalogIdentity};

        let store = temp_sqlite_store();
        let config = Config {
            exclude_dirs: vec!["node_modules".into()],
            ..Config::default()
        };
        let active_paths = vec!["C:\\".to_string()];
        // `load_persisted` computes identity with the real installed `rg`'s
        // version, so the fixture's saved identity must match that, not a
        // placeholder string.
        let identity = CatalogIdentity::compute_with_args(
            &active_paths,
            &config.exclude_dirs,
            ripgrep_version(),
            &config.rg_extra_args,
        );
        store
            .save(
                &identity,
                &[CatalogEntry {
                    path: Arc::from(r"C:\src\README.md"),
                    basename_folded: Arc::from("readme.md"),
                }],
            )
            .unwrap();

        let cache = Arc::new(FileCache::new(store, None));
        FileCache::load_persisted(cache.clone(), config, active_paths);

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let (state, count) = cache.status();
            if state == CacheState::Ready {
                assert_eq!(count, 1);
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "load_persisted never reached Ready"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        let generation = cache.generation();
        assert!(matches!(
            cache.window(generation, 0, 10),
            Window::Chunk(rows) if rows[0].0.as_ref() == r"C:\src\README.md"
        ));
    }

    /// A catalog saved under a different identity (a changed root, in this
    /// case) must be treated as absent — `load_persisted` must leave the
    /// cache `Empty` rather than surface entries for a configuration that no
    /// longer applies.
    #[test]
    fn a_stale_persisted_catalog_leaves_the_cache_empty() {
        use crate::index_store::{CatalogEntry, CatalogIdentity};

        let store = temp_sqlite_store();
        let old_identity = CatalogIdentity::compute(&["C:\\".to_string()], &[], "test-rg-1".into());
        store
            .save(
                &old_identity,
                &[CatalogEntry {
                    path: Arc::from(r"C:\old.txt"),
                    basename_folded: Arc::from("old.txt"),
                }],
            )
            .unwrap();

        let cache = Arc::new(FileCache::new(store, None));
        FileCache::load_persisted(cache.clone(), Config::default(), vec!["D:\\".to_string()]);

        // The load now claims the cache up front, so wait for it to resolve
        // rather than for a fixed interval — the assertion is still that a
        // stale catalog leaves nothing behind.
        wait_until(
            || cache.status().0 != CacheState::Loading,
            "the load to resolve",
        );
        assert!(matches!(cache.status(), (CacheState::Empty, 0)));
    }

    /// `load_persisted` must not clobber a build that started (and is
    /// already ahead) before the SQLite read finished — the race a fast
    /// keystroke-triggered `ensure_built` could win against a slow disk.
    #[test]
    fn load_persisted_does_not_overwrite_a_build_already_in_progress() {
        let cache = Arc::new(test_cache());
        let gen_before = {
            let mut inner = cache.inner.lock().unwrap();
            inner.generation += 1;
            inner.state = CacheState::Building;
            inner.generation
        };

        // A `MissingOrStale` "load" (NullCatalogStore) landing after the
        // build already claimed the slot must leave that build's state and
        // generation untouched.
        FileCache::load_persisted(cache.clone(), Config::default(), vec!["C:\\".to_string()]);
        std::thread::sleep(Duration::from_millis(50));

        let inner = cache.inner.lock().unwrap();
        assert_eq!(inner.generation, gen_before);
        assert!(inner.state == CacheState::Building);
    }

    /// A store whose `load` blocks until the test releases it, so the
    /// startup-race tests below can decide exactly where a search lands
    /// relative to the catalog read instead of hoping for a timing.
    struct BlockingStore {
        gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
    }

    impl BlockingStore {
        fn new() -> (Arc<Self>, Arc<(Mutex<bool>, std::sync::Condvar)>) {
            let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
            (Arc::new(BlockingStore { gate: gate.clone() }), gate)
        }
    }

    fn release(gate: &Arc<(Mutex<bool>, std::sync::Condvar)>) {
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
    }

    impl CatalogStore for BlockingStore {
        fn load(
            &self,
            _identity: &CatalogIdentity,
        ) -> Result<CatalogLoad, crate::index_store::IndexError> {
            let mut released = self.gate.0.lock().unwrap();
            while !*released {
                released = self.gate.1.wait(released).unwrap();
            }
            Ok(CatalogLoad::MissingOrStale)
        }
        fn save(
            &self,
            _identity: &CatalogIdentity,
            _entries: &[CatalogEntry],
        ) -> Result<(), crate::index_store::IndexError> {
            Ok(())
        }
        fn apply(
            &self,
            _removed: &[String],
            _upserts: &[CatalogEntry],
        ) -> Result<(), crate::index_store::IndexError> {
            Ok(())
        }
    }

    fn wait_until(mut done: impl FnMut() -> bool, what: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The restart cost the user actually felt. `load_persisted` claimed the
    /// cache only once its background SQLite read had *finished*, so the very
    /// first keystroke after launch — the entire point of a hotkey launcher —
    /// found the cache `Empty` and kicked off a whole-drive `rg --files`
    /// walk. The catalog then finished loading and was thrown away because
    /// the state was no longer `Empty`. Every relaunch paid for both.
    #[test]
    fn a_search_during_the_catalog_load_waits_instead_of_walking_the_drive() {
        let (store, gate) = BlockingStore::new();
        let cache = Arc::new(FileCache::new(store, None));

        FileCache::load_persisted(cache.clone(), Config::default(), vec!["C:\\".to_string()]);
        let generation = cache.generation();

        // What a search does the moment the window opens.
        FileCache::ensure_built(cache.clone(), Config::default(), vec!["C:\\".to_string()]);

        assert!(
            cache.status().0 == CacheState::Loading,
            "a search must not supersede a catalog load that is already in flight"
        );
        assert_eq!(
            cache.generation(),
            generation,
            "and must not invalidate the reader waiting on it"
        );
        // The scan has to keep polling, not conclude there is nothing to find.
        assert!(matches!(
            cache.window(generation, 0, 10),
            Window::Chunk(rows) if rows.is_empty()
        ));

        release(&gate);
    }

    /// The other half: once the catalog lands it must become visible to the
    /// reader already waiting on it, rather than arriving under a new
    /// generation that reader is forced to abandon.
    #[test]
    fn a_loaded_catalog_reaches_the_reader_already_waiting_on_it() {
        let store = temp_sqlite_store();
        let config = Config {
            exclude_dirs: vec!["node_modules".into()],
            ..Config::default()
        };
        let active_paths = vec!["C:\\".to_string()];
        let identity = CatalogIdentity::compute_with_args(
            &active_paths,
            &config.exclude_dirs,
            ripgrep_version(),
            &config.rg_extra_args,
        );
        store
            .save(
                &identity,
                &[CatalogEntry {
                    path: Arc::from(r"C:\src\README.md"),
                    basename_folded: Arc::from("readme.md"),
                }],
            )
            .unwrap();

        let cache = Arc::new(FileCache::new(store, None));
        FileCache::load_persisted(cache.clone(), config, active_paths);
        // Claimed synchronously, so this is the generation a search starting
        // right now would tag itself with.
        let generation = cache.generation();

        wait_until(
            || cache.status().0 == CacheState::Ready,
            "the catalog to load",
        );

        assert_eq!(
            cache.generation(),
            generation,
            "a completed load must not supersede the reader it was loaded for"
        );
        assert!(matches!(
            cache.window(generation, 0, 10),
            Window::Chunk(rows) if rows[0].0.as_ref() == r"C:\src\README.md"
        ));
    }

    /// And when there is no usable catalog, the waiting search must be handed
    /// on to a real build under the *same* generation — otherwise the search
    /// that triggered the build is superseded by it and returns nothing.
    #[test]
    fn a_completed_initial_build_publishes_rows_for_the_next_search() {
        let root = std::env::temp_dir().join(format!("qs-built-view-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("fixture.txt");
        std::fs::write(&file, "fixture").unwrap();
        let cache = Arc::new(test_cache());
        FileCache::ensure_built(
            cache.clone(),
            Config::default(),
            vec![root.to_string_lossy().into_owned()],
        );
        wait_until(|| cache.status().0 != CacheState::Building, "fixture build");
        let view = cache
            .snapshot()
            .expect("completed build must publish a view");
        assert_eq!(view.len(), 1);
        assert!(view[0].0.ends_with("fixture.txt"));
        std::fs::remove_file(file).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn changes_during_a_gated_load_are_replayed_and_persisted_before_completion() {
        use crate::index_store::IndexError;
        use crate::index_watcher::PathChange;
        use std::sync::mpsc;
        struct GatedLoad {
            store: Arc<SqliteCatalogStore>,
            entered: mpsc::Sender<()>,
            resume: Mutex<mpsc::Receiver<()>>,
        }
        impl CatalogStore for GatedLoad {
            fn load(&self, id: &CatalogIdentity) -> Result<CatalogLoad, IndexError> {
                let snapshot = self.store.load(id)?;
                self.entered.send(()).unwrap();
                self.resume
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap();
                Ok(snapshot)
            }
            fn save(&self, id: &CatalogIdentity, rows: &[CatalogEntry]) -> Result<(), IndexError> {
                self.store.save(id, rows)
            }
            fn apply(
                &self,
                removed: &[String],
                upserts: &[CatalogEntry],
            ) -> Result<(), IndexError> {
                self.store.apply(removed, upserts)
            }
        }
        let root = std::env::temp_dir().join(format!("qs-replay-load-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let created = root.join("new.txt");
        let removed = root.join("old.txt");
        let config = Config::default();
        let roots = vec![root.to_string_lossy().into_owned()];
        let id = CatalogIdentity::compute_with_args(
            &roots,
            &config.exclude_dirs,
            ripgrep_version(),
            &config.rg_extra_args,
        );
        let store = temp_sqlite_store();
        store
            .save(
                &id,
                &[CatalogEntry {
                    path: removed.to_string_lossy().as_ref().into(),
                    basename_folded: "old.txt".into(),
                }],
            )
            .unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let cache = Arc::new(FileCache::new(
            Arc::new(GatedLoad {
                store: store.clone(),
                entered: entered_tx,
                resume: Mutex::new(resume_rx),
            }),
            None,
        ));
        FileCache::load_persisted(cache.clone(), config, roots);
        entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        std::fs::write(&created, "new").unwrap();
        // These calls must not block behind the paused database read.
        cache.apply_changes(vec![
            PathChange::Remove(removed),
            PathChange::Upsert(created.clone()),
        ]);
        assert_eq!(cache.inner.lock().unwrap().pending_changes.len(), 2);
        resume_tx.send(()).unwrap();
        wait_until(|| cache.status().0 == CacheState::Ready, "replayed load");
        let _persist = cache.persistence.lock().unwrap();
        let view = cache.snapshot().unwrap();
        assert_eq!(view.len(), 1);
        assert!(view[0].0.ends_with("new.txt"));
        let CatalogLoad::Ready(rows) = store.load(&id).unwrap() else {
            panic!("persisted replay")
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, view[0].0);
        std::fs::remove_file(created).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn refresh_replays_changes_even_when_a_deletion_was_absent_from_the_old_view() {
        use crate::index_watcher::PathChange;
        let root = std::env::temp_dir().join(format!("qs-refresh-replay-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let created = root.join("new.txt");
        std::fs::write(&created, "new").unwrap();
        let cache = ready_cache_with(vec![entry(r"C:\keep.txt")]);
        cache.inner.lock().unwrap().refresh_tracking = true;
        cache.apply_changes(vec![PathChange::Remove(r"C:\gone.txt".into())]);
        cache.apply_changes(vec![PathChange::Upsert(created.clone())]);
        let _persist = cache.persistence.lock().unwrap();
        let rows = cache
            .accept_refresh(0, vec![entry(r"C:\gone.txt"), entry(r"C:\keep.txt")])
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert!(!rows.iter().any(|row| row.path.ends_with("gone.txt")));
        assert!(rows.iter().any(|row| row.path.ends_with("new.txt")));
        let view = cache.snapshot().unwrap();
        assert_eq!(view.len(), 2);
        std::fs::remove_file(created).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn an_overflowed_refresh_keeps_the_working_snapshot() {
        let cache = ready_cache_with(vec![entry(r"C:\keep.txt")]);
        {
            let mut inner = cache.inner.lock().unwrap();
            inner.refresh_tracking = true;
            inner.pending_overflow = true;
        }
        let _persist = cache.persistence.lock().unwrap();
        assert!(cache
            .accept_refresh(0, vec![entry(r"C:\obsolete.txt")])
            .is_none());
        assert_eq!(cache.snapshot().unwrap()[0].0.as_ref(), r"C:\keep.txt");
    }

    #[test]
    fn build_replay_removes_late_scan_rows_and_cancellation_discards_pending_changes() {
        use crate::index_watcher::PathChange;
        let cache = test_cache();
        {
            cache.inner.lock().unwrap().state = CacheState::Building;
        }
        cache.apply_changes(vec![PathChange::Remove(r"C:\gone".into())]);
        {
            let mut inner = cache.inner.lock().unwrap();
            // The scanner buffered this row before the delete arrived.
            inner.entries = vec![
                entry(r"C:\gone\old.txt"),
                entry(r"C:\gone-sibling\keep.txt"),
            ];
            let (removed, _) = replay_pending(&mut inner).unwrap();
            assert_eq!(removed, vec![r"c:\gone\old.txt"]);
            assert_eq!(inner.entries.len(), 1);
        }
        cache.apply_changes(vec![PathChange::Remove(r"C:\gone-sibling".into())]);
        cache.kill_build();
        assert!(cache.inner.lock().unwrap().pending_changes.is_empty());
    }

    #[test]
    fn a_failed_root_does_not_discard_successful_roots() {
        let root = std::env::temp_dir().join(format!("qs-partial-build-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("kept.txt");
        std::fs::write(&file, "kept").unwrap();
        let missing = root.join("does-not-exist");
        let cache = Arc::new(FileCache::new(temp_sqlite_store(), None));
        FileCache::ensure_built(
            cache.clone(),
            Config::default(),
            vec![
                missing.to_string_lossy().into_owned(),
                root.to_string_lossy().into_owned(),
            ],
        );
        wait_until(|| cache.status().0 == CacheState::Ready, "partial build");
        let snapshot = cache.snapshot().unwrap();
        let expected = file.to_string_lossy().into_owned();
        assert!(snapshot.iter().any(|(path, _)| path.as_ref() == expected));
        assert_eq!(snapshot.len(), 1);
        std::fs::remove_file(file).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn replay_overflow_is_bounded_and_cannot_publish_an_incomplete_catalog() {
        use crate::index_watcher::PathChange;
        let cache = test_cache();
        cache.inner.lock().unwrap().state = CacheState::Building;
        cache.apply_changes(
            (0..16_385)
                .map(|n| PathChange::Remove(format!(r"C:\file-{n}").into()))
                .collect(),
        );
        let mut inner = cache.inner.lock().unwrap();
        assert!(inner.pending_overflow);
        assert!(inner.pending_changes.is_empty());
        assert!(replay_pending(&mut inner).is_err());
        assert!(inner.state == CacheState::Empty);
        assert!(inner.error.is_some());
    }

    #[test]
    fn watcher_publication_waits_for_a_full_save_and_survives_reload() {
        use crate::index_store::IndexError;
        use crate::index_watcher::PathChange;
        use std::sync::mpsc;

        struct GatedSave {
            store: Arc<SqliteCatalogStore>,
            entered: mpsc::Sender<()>,
            resume: Mutex<mpsc::Receiver<()>>,
        }
        impl CatalogStore for GatedSave {
            fn load(&self, id: &CatalogIdentity) -> Result<CatalogLoad, IndexError> {
                self.store.load(id)
            }
            fn save(&self, id: &CatalogIdentity, rows: &[CatalogEntry]) -> Result<(), IndexError> {
                self.entered.send(()).unwrap();
                self.resume
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap();
                self.store.save(id, rows)
            }
            fn apply(
                &self,
                removed: &[String],
                upserts: &[CatalogEntry],
            ) -> Result<(), IndexError> {
                self.store.apply(removed, upserts)
            }
        }
        let root = std::env::temp_dir().join(format!("qs-save-order-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("fixture.txt");
        std::fs::write(&file, "fixture").unwrap();
        let store = temp_sqlite_store();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let cache = Arc::new(FileCache::new(
            Arc::new(GatedSave {
                store: store.clone(),
                entered: entered_tx,
                resume: Mutex::new(resume_rx),
            }),
            None,
        ));
        let config = Config::default();
        let roots = vec![root.to_string_lossy().into_owned()];
        FileCache::ensure_built(cache.clone(), config.clone(), roots.clone());
        entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let before = cache.generation();
        let (started_tx, started_rx) = mpsc::channel();
        let patch_cache = cache.clone();
        let removed = file.clone();
        let patch = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            patch_cache.apply_changes(vec![PathChange::Remove(removed)]);
        });
        started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        // The gate guarantees the full save cannot finish during this window.
        std::thread::sleep(Duration::from_millis(100));
        let during = cache.generation();
        resume_tx.send(()).unwrap();
        patch.join().unwrap();
        assert_eq!(
            during, before,
            "patch must not publish ahead of the blocked full save"
        );
        assert!(cache.snapshot().unwrap().is_empty());
        let id = CatalogIdentity::compute_with_args(
            &roots,
            &config.exclude_dirs,
            ripgrep_version(),
            &config.rg_extra_args,
        );
        let CatalogLoad::Ready(rows) = store.load(&id).unwrap() else {
            panic!("saved catalog")
        };
        assert!(
            rows.is_empty(),
            "the older full save must not restore the removed file"
        );
        std::fs::remove_file(file).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn a_missing_catalog_promotes_the_waiting_search_to_a_build() {
        let (store, gate) = BlockingStore::new();
        let cache = Arc::new(FileCache::new(store, None));
        let root = std::env::temp_dir().to_string_lossy().into_owned();

        FileCache::load_persisted(cache.clone(), Config::default(), vec![root.clone()]);
        let generation = cache.generation();
        FileCache::ensure_built(cache.clone(), Config::default(), vec![root]);

        release(&gate);
        wait_until(
            || cache.status().0 != CacheState::Loading,
            "the load to resolve",
        );

        assert_eq!(
            cache.generation(),
            generation,
            "the build must continue the waiting search's generation, not replace it"
        );
        assert!(
            !matches!(cache.window(generation, 0, 10), Window::Superseded),
            "the search that asked for this build must not be superseded by it"
        );
    }

    /// A load nobody is waiting on must not walk two whole drives on its own
    /// — a cold index is the documented state, and `Tab` builds it on first
    /// use.
    #[test]
    fn a_missing_catalog_with_no_search_waiting_stays_cold() {
        let (store, gate) = BlockingStore::new();
        let cache = Arc::new(FileCache::new(store, None));

        FileCache::load_persisted(cache.clone(), Config::default(), vec!["C:\\".to_string()]);
        release(&gate);
        wait_until(
            || cache.status().0 != CacheState::Loading,
            "the load to resolve",
        );

        assert!(cache.status().0 == CacheState::Empty);
    }

    /// The hourly refresh used to `invalidate()` first and rebuild second, so
    /// filename search was dead for the entire multi-minute walk of both
    /// drives. The existing index has to stay searchable until its
    /// replacement is actually ready.
    #[test]
    fn a_background_refresh_never_leaves_the_index_cold() {
        let dir = std::env::temp_dir().join(format!("qs-refresh-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..3 {
            std::fs::write(dir.join(format!("refreshed{i}.txt")), b"x").unwrap();
        }

        let cache = Arc::new(ready_cache_with(vec![entry(r"C:\src\stale.txt")]));
        let generation = cache.generation();
        FileCache::refresh_in_background(
            cache.clone(),
            Config {
                exclude_dirs: Vec::new(),
                rg_extra_args: Vec::new(),
                ..Config::default()
            },
            vec![dir.to_string_lossy().into_owned()],
        );

        // Poll to completion, asserting the invariant on every single pass:
        // the index is continuously searchable throughout.
        wait_until(
            || {
                let (state, count) = cache.status();
                assert!(
                    state != CacheState::Empty,
                    "a refresh must never leave the index cold"
                );
                state == CacheState::Ready && count == 3
            },
            "the refreshed index to swap in",
        );

        assert!(
            cache.generation() != generation,
            "the swapped-in index must supersede readers holding offsets into the old one"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_successful_empty_refresh_clears_stale_entries() {
        let dir = std::env::temp_dir().join(format!("qs-empty-refresh-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cache = Arc::new(ready_cache_with(vec![entry(r"C:\stale.txt")]));
        FileCache::refresh_in_background(
            cache.clone(),
            Config {
                exclude_dirs: Vec::new(),
                rg_extra_args: Vec::new(),
                ..Config::default()
            },
            vec![dir.to_string_lossy().into_owned()],
        );
        wait_until(|| cache.refresh_is_active(), "empty refresh to start");
        wait_until(|| !cache.refresh_is_active(), "empty refresh");
        assert!(cache.status().0 == CacheState::Ready);
        assert_eq!(cache.status().1, 0);
        assert!(cache.snapshot().unwrap().is_empty());
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn maintenance_refresh_does_not_compete_with_an_initial_build() {
        let cache = Arc::new(FileCache::new(
            Arc::new(crate::index_store::NullCatalogStore),
            None,
        ));
        FileCache::refresh_in_background(cache.clone(), Config::default(), vec![r"C:\".into()]);
        assert!(!cache.refresh_is_active());
        assert!(matches!(cache.status().0, CacheState::Empty));
    }

    #[test]
    fn directory_refresh_replaces_only_the_changed_subtree() {
        let root = std::env::temp_dir().join(format!("qs-subtree-refresh-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let live = root.join("live.txt");
        std::fs::write(&live, b"x").unwrap();
        let stale = root.join("stale.txt");
        let sibling = root.with_file_name(format!(
            "{}-sibling",
            root.file_name().unwrap().to_string_lossy()
        ));
        let cache = Arc::new(ready_cache_with(vec![
            entry(&stale.to_string_lossy()),
            entry(&sibling.join("keep.txt").to_string_lossy()),
        ]));

        FileCache::refresh_subtrees(cache.clone(), vec![root.clone()]);
        wait_until(|| cache.refresh_is_active(), "subtree refresh to start");
        wait_until(|| !cache.refresh_is_active(), "subtree refresh");
        let snapshot = cache.snapshot().unwrap();
        assert!(snapshot
            .iter()
            .any(|(path, _)| path.as_ref() == live.to_string_lossy()));
        assert!(!snapshot
            .iter()
            .any(|(path, _)| path.as_ref() == stale.to_string_lossy()));
        assert!(snapshot
            .iter()
            .any(|(path, _)| path.as_ref() == sibling.join("keep.txt").to_string_lossy()));
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&sibling).ok();
    }

    /// Refreshes must not stack. Each one holds a complete copy of the index
    /// while it walks, so a refresh interval shorter than a refresh takes —
    /// or a burst of watcher overflows, which is the same thing — piles up
    /// threads that each cost the whole index in memory. Observed at 2.2GB
    /// resident before this guard existed.
    #[test]
    fn a_second_refresh_cannot_start_while_one_is_still_running() {
        let dir = std::env::temp_dir().join(format!("qs-refresh-guard-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..3 {
            std::fs::write(dir.join(format!("guarded{i}.txt")), b"x").unwrap();
        }
        let config = Config {
            exclude_dirs: Vec::new(),
            rg_extra_args: Vec::new(),
            ..Config::default()
        };
        let roots = vec![dir.to_string_lossy().into_owned()];

        let cache = Arc::new(ready_cache_with(vec![entry(r"C:\src\stale.txt")]));
        FileCache::refresh_in_background(cache.clone(), config.clone(), roots.clone());
        assert!(
            cache.refresh_is_active(),
            "the first refresh claimed the slot"
        );

        // Every one of these must be refused, not queued.
        for _ in 0..50 {
            FileCache::refresh_in_background(cache.clone(), config.clone(), roots.clone());
        }

        wait_until(|| !cache.refresh_is_active(), "the refresh to finish");
        assert_eq!(
            cache.status().1,
            3,
            "exactly one refresh swapped its result in"
        );

        // And once it is done, a later refresh is allowed again.
        FileCache::refresh_in_background(cache.clone(), config, roots);
        assert!(
            cache.refresh_is_active(),
            "the guard must not latch permanently"
        );
        wait_until(
            || !cache.refresh_is_active(),
            "the second refresh to finish",
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn deferred_recovery_is_consumed_once_only_after_ready() {
        let cache = Arc::new(ready_cache_with(vec![entry(r"C:\keep.txt")]));
        cache
            .recovery_pending
            .store(true, std::sync::atomic::Ordering::Release);
        assert!(cache.take_ready_recovery());
        assert!(!cache.take_ready_recovery());

        cache
            .recovery_pending
            .store(true, std::sync::atomic::Ordering::Release);
        cache.inner.lock().unwrap().state = CacheState::Building;
        assert!(!cache.take_ready_recovery());
        assert!(cache
            .recovery_pending
            .load(std::sync::atomic::Ordering::Acquire));
    }

    /// The watcher's coalescing was quadratic: for every change it scanned
    /// the whole pending upsert list, lowercasing each path again to compare.
    /// A burst on a watched drive root is thousands of events per 500ms
    /// window, which is enough for that to stop finishing.
    #[test]
    fn coalescing_a_large_watcher_batch_is_not_quadratic() {
        use crate::index_watcher::PathChange;

        let changes: Vec<PathChange> = (0..20_000)
            .map(|i| PathChange::Upsert(std::path::PathBuf::from(format!(r"C:\dir\f{i}.txt"))))
            .collect();

        let start = std::time::Instant::now();
        let (removed, upserts) = coalesce_batch(changes);
        assert!(removed.is_empty());
        assert_eq!(upserts.len(), 20_000);
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "coalescing 20k changes took {:?} — that is the quadratic scan",
            start.elapsed()
        );
    }

    /// The half of the watcher hot path that survived the first fix. Guarding
    /// the `retain` on "are there any removals at all" helps only for batches
    /// that have none — and on a watched `C:\` a steady trickle of deleted
    /// temp files means most batches have one. Each of those then lowercased
    /// every path in a 2.5M-entry index to build a comparison key, which is
    /// millions of string allocations for a couple of deletions, and measured
    /// as a steady 9-20% of a core at idle with no disk activity at all.
    ///
    /// This asserts the *behaviour* only. An earlier version also asserted
    /// that the allocation-free form beat the allocating one by 2x, reasoning
    /// that a ratio is build-independent even though an absolute threshold is
    /// not. That reasoning was wrong: in an unoptimised build the comparison
    /// loop is itself slow enough to mask the allocation being saved, and the
    /// assertion failed at 1.49x on a machine merely busy with another build.
    /// A test that measures the machine cannot tell a regression from a noisy
    /// box, so the timing claim moved to `bench_removal_cost` below, run
    /// deliberately instead of on every suite.
    #[test]
    fn removing_two_paths_from_a_large_index_drops_exactly_those_two() {
        let entries: Vec<(Arc<str>, Arc<str>)> = (0..200_000)
            .map(|i| entry(&format!(r"C:\some\deep\directory\path\file{i}.txt")))
            .collect();
        let cache = ready_cache_with(entries);

        // Deliberately cased differently to the indexed paths — the watcher
        // reports whatever spelling the OS hands it.
        let mut removed = std::collections::HashSet::new();
        removed.insert(r"c:\some\deep\directory\path\file7.txt".to_string());
        removed.insert(r"c:\some\deep\directory\path\file9999.txt".to_string());

        cache
            .inner
            .lock()
            .unwrap()
            .entries
            .retain(|(path, _)| !is_removed(&removed, path));

        assert_eq!(cache.status().1, 199_998, "exactly the two named paths go");
        let survivors = cache.inner.lock().unwrap();
        assert!(
            !survivors.entries.iter().any(
                |(path, _)| path.eq_ignore_ascii_case(r"C:\some\deep\directory\path\file7.txt")
            ),
            "the named path must be gone regardless of case"
        );
        assert!(
            survivors
                .entries
                .iter()
                .any(|(path, _)| path.as_ref() == r"C:\some\deep\directory\path\file70.txt"),
            "a path sharing a prefix with a removed one must survive"
        );
    }

    /// Opt-in measurement of the removal path, kept out of the default suite
    /// because it times the machine rather than the code. Run with
    /// `cargo test -p quicksearch --release bench_removal_cost -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn bench_removal_cost() {
        let snapshot: Vec<(Arc<str>, Arc<str>)> = (0..200_000)
            .map(|i| entry(&format!(r"C:\some\deep\directory\path\file{i}.txt")))
            .collect();
        let mut removed = std::collections::HashSet::new();
        removed.insert(r"c:\some\deep\directory\path\file7.txt".to_string());

        let start = std::time::Instant::now();
        for _ in 0..5 {
            let mut probe = snapshot.clone();
            probe.retain(|(path, _)| !removed.contains(&path.to_lowercase()));
        }
        let allocating = start.elapsed();

        let start = std::time::Instant::now();
        for _ in 0..5 {
            let mut probe = snapshot.clone();
            probe.retain(|(path, _)| !is_removed(&removed, path));
        }
        let allocation_free = start.elapsed();

        println!("5 passes over 200k entries:");
        println!("  per-entry allocation : {allocating:?}");
        println!("  allocation-free      : {allocation_free:?}");
    }

    #[test]
    fn removal_matching_is_case_insensitive_and_length_anchored() {
        let mut removed = std::collections::HashSet::new();
        removed.insert(r"c:\src\gone.txt".to_string());

        assert!(
            is_removed(&removed, r"C:\src\Gone.txt"),
            "casing must not matter"
        );
        assert!(
            !is_removed(&removed, r"C:\src\gone.txt.bak"),
            "a longer path is a different file"
        );
        removed.insert(r"c:\src\old-dir".to_string());
        assert!(is_removed(&removed, r"C:\src\old-dir\nested\file.rs"));
        assert!(!is_removed(&removed, r"C:\src\old-directory\file.rs"));
        assert!(!is_removed(&removed, r"C:\src\stays.txt"));
    }

    #[test]
    fn coalescing_lets_a_removal_beat_an_earlier_upsert_of_the_same_path() {
        use crate::index_watcher::PathChange;

        let (removed, upserts) = coalesce_batch(vec![
            PathChange::Upsert(std::path::PathBuf::from(r"C:\src\gone.rs")),
            PathChange::Remove(std::path::PathBuf::from(r"c:\src\GONE.rs")),
        ]);
        assert!(upserts.is_empty(), "the removal must win");
        assert_eq!(
            removed.iter().collect::<Vec<_>>(),
            vec![&r"c:\src\gone.rs".to_string()]
        );
    }

    #[test]
    fn coalescing_lets_a_later_upsert_beat_an_earlier_removal() {
        use crate::index_watcher::PathChange;

        let (removed, upserts) = coalesce_batch(vec![
            PathChange::Remove(std::path::PathBuf::from(r"C:\src\back.rs")),
            PathChange::Upsert(std::path::PathBuf::from(r"C:\src\back.rs")),
        ]);
        assert!(removed.is_empty());
        assert_eq!(upserts, vec![std::path::PathBuf::from(r"C:\src\back.rs")]);
    }

    fn ready_cache_with(entries: Vec<(Arc<str>, Arc<str>)>) -> FileCache {
        let cache = test_cache();
        {
            let mut inner = cache.inner.lock().unwrap();
            inner.state = CacheState::Ready;
            inner.entries = entries;
            inner.published = Arc::new(inner.entries.clone());
        }
        cache
    }

    #[test]
    fn apply_changes_adds_an_upserted_file_that_exists_on_disk() {
        use crate::index_watcher::PathChange;

        let mut path = std::env::temp_dir();
        path.push(format!(
            "qs-apply-changes-upsert-{}.txt",
            std::process::id()
        ));
        std::fs::write(&path, b"x").unwrap();

        let cache = ready_cache_with(Vec::new());
        let generation_before = cache.generation();
        cache.apply_changes(vec![PathChange::Upsert(path.clone())]);

        assert!(cache.generation() > generation_before);
        let (state, count) = cache.status();
        assert!(state == CacheState::Ready);
        assert_eq!(count, 1);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_changes_removes_a_path_case_insensitively() {
        use crate::index_watcher::PathChange;

        let cache = ready_cache_with(vec![entry(r"C:\src\Gone.txt"), entry(r"C:\src\stays.txt")]);
        cache.apply_changes(vec![PathChange::Remove(std::path::PathBuf::from(
            r"c:\src\gone.txt",
        ))]);

        let (_, count) = cache.status();
        assert_eq!(count, 1);
        let generation = cache.generation();
        assert!(matches!(
            cache.window(generation, 0, 10),
            Window::Chunk(rows) if rows[0].0.as_ref() == r"C:\src\stays.txt"
        ));
    }

    #[test]
    fn apply_changes_is_a_noop_while_the_index_is_not_ready() {
        use crate::index_watcher::PathChange;

        let cache = test_cache(); // starts Empty
        let generation_before = cache.generation();
        cache.apply_changes(vec![PathChange::Remove(std::path::PathBuf::from(
            r"C:\anything",
        ))]);
        assert_eq!(cache.generation(), generation_before);
    }

    #[test]
    fn apply_changes_does_not_publish_a_generation_for_a_stale_removal() {
        use crate::index_watcher::PathChange;

        let cache = ready_cache_with(vec![entry(r"C:\src\still-here.txt")]);
        let generation_before = cache.generation();
        cache.apply_changes(vec![PathChange::Remove(std::path::PathBuf::from(
            r"C:\src\already-gone.txt",
        ))]);

        assert_eq!(cache.generation(), generation_before);
        assert_eq!(cache.status().1, 1);
    }

    /// The gap this exists to close: without persisting the patch, a
    /// watcher-caught create would live only in memory and vanish again the
    /// moment the app restarts and reloads the older catalog off disk.
    #[test]
    fn apply_changes_persists_the_patch_to_the_store() {
        use crate::index_store::CatalogIdentity;
        use crate::index_watcher::PathChange;

        let store = temp_sqlite_store();
        let identity = CatalogIdentity::compute(&["C:\\".into()], &[], ripgrep_version());
        store.save(&identity, &[]).unwrap();

        let cache = FileCache::new(store.clone(), None);
        {
            let mut inner = cache.inner.lock().unwrap();
            inner.state = CacheState::Ready;
        }

        let mut path = std::env::temp_dir();
        path.push(format!(
            "qs-apply-changes-persist-{}.txt",
            std::process::id()
        ));
        std::fs::write(&path, b"x").unwrap();

        cache.apply_changes(vec![PathChange::Upsert(path.clone())]);

        let CatalogLoad::Ready(loaded) = store.load(&identity).unwrap() else {
            panic!("catalog should be ready");
        };
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].path.as_ref(), path.to_string_lossy().as_ref());

        std::fs::remove_file(&path).ok();
    }

    /// The other half: a watcher-caught removal must delete the row from the
    /// store too, not just filter it out of the in-memory `Vec`.
    #[test]
    fn apply_changes_persists_a_removal_to_the_store() {
        use crate::index_store::CatalogIdentity;
        use crate::index_watcher::PathChange;

        let store = temp_sqlite_store();
        let identity = CatalogIdentity::compute(&["C:\\".into()], &[], ripgrep_version());
        store
            .save(
                &identity,
                &[CatalogEntry {
                    path: Arc::from(r"C:\src\Gone.txt"),
                    basename_folded: Arc::from("gone.txt"),
                }],
            )
            .unwrap();

        let cache = FileCache::new(store.clone(), None);
        {
            let mut inner = cache.inner.lock().unwrap();
            inner.state = CacheState::Ready;
            inner.entries = vec![entry(r"C:\src\Gone.txt")];
        }

        cache.apply_changes(vec![PathChange::Remove(std::path::PathBuf::from(
            r"c:\src\gone.txt",
        ))]);

        let CatalogLoad::Ready(loaded) = store.load(&identity).unwrap() else {
            panic!("catalog should be ready");
        };
        assert!(loaded.is_empty());
    }
}

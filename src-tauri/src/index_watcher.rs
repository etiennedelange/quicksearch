//! Real-time filename index updates.
//!
//! Backed by `notify`, which on Windows means `ReadDirectoryChangesW` — a
//! per-directory-handle change notification, not the NTFS-only USN Change
//! Journal. That distinction matters here: this development machine's `C:\`
//! is NTFS and `D:\` is ReFS, and `ReadDirectoryChangesW` works on both, so
//! both drives get real-time updates without needing per-filesystem
//! capability detection the way a USN-journal implementation would.
//!
//! Raw events are coalesced for 500ms on one worker thread, then applied to
//! [`FileCache`] as a single batch via [`FileCache::apply_changes`]. A
//! dropped/overflowed notification (a burst of changes too large for the
//! watch buffer) is treated conservatively: rather than risk an index that
//! silently never learns what it missed, it forces a full rebuild instead —
//! the same rebuild the periodic refresh already does routinely.

use notify::event::{CreateKind, ModifyKind, RenameMode};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::index_service::IndexService;

const COALESCE_WINDOW: Duration = Duration::from_millis(500);
const EVENT_QUEUE_CAPACITY: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathChange {
    Upsert(PathBuf),
    Remove(PathBuf),
}

/// Owns the OS-level watches; dropping it stops them. Kept alive for the
/// life of the app in `main.rs`.
pub struct IndexWatcher {
    _watcher: RecommendedWatcher,
}

impl IndexWatcher {
    /// Watches every root in `active_roots`, best-effort — a root that fails
    /// to register (a network path, a permissions error) is logged and
    /// simply relies on the periodic refresh instead of stopping the others.
    /// Returns `None` only if nothing could be watched at all.
    pub fn start(
        active_roots: &[String],
        exclude_dirs: Vec<String>,
        service: Arc<IndexService>,
        watcher_live: Arc<AtomicBool>,
    ) -> Option<Self> {
        let (tx, rx) = sync_channel::<notify::Result<Event>>(EVENT_QUEUE_CAPACITY);
        let epoch = service.current_epoch();
        let recovery_service = service.clone();
        let health = watcher_live.clone();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            // A callback error is a durable loss of confidence in the
            // watcher. Do not let a later successful event from another root
            // flip the shared health flag back to true and suppress fallback
            // reconciliation.
            if res.is_err() {
                health.store(false, Ordering::Release);
            }
            if let Err(TrySendError::Full(_)) = tx.try_send(res) {
                // Preserve the need for an authoritative reconciliation even
                // when the bounded queue cannot retain every event detail.
                recovery_service.request_recovery_for(epoch);
            }
        })
        .map_err(|e| {
            eprintln!(
                "[quicksearch] could not start a filesystem watcher, live updates disabled: {e}"
            )
        })
        .ok()?;

        let mut watched_any = false;
        let mut watch_failed = false;
        for root in active_roots {
            match watcher.watch(Path::new(root), RecursiveMode::Recursive) {
                Ok(()) => watched_any = true,
                Err(e) => {
                    watch_failed = true;
                    eprintln!(
                        "[quicksearch] could not watch {root} for live updates, relying on periodic refresh: {e}"
                    )
                }
            }
        }
        if !watched_any {
            return None;
        }
        watcher_live.store(!watch_failed, Ordering::Release);

        std::thread::spawn(move || coalesce_loop(rx, exclude_dirs, service, epoch));

        Some(IndexWatcher { _watcher: watcher })
    }
}

fn coalesce_loop(
    rx: Receiver<notify::Result<Event>>,
    exclude_dirs: Vec<String>,
    service: Arc<IndexService>,
    epoch: u64,
) {
    let excluded_names: Vec<String> = exclude_dirs
        .iter()
        .map(|s| s.trim().to_lowercase())
        .collect();
    loop {
        let first = match rx.recv() {
            Ok(event) => event,
            // Every sender dropped — the watcher itself was dropped (app
            // shutting down).
            Err(_) => return,
        };
        let mut batch: Vec<notify::Result<Event>> = vec![first];
        let deadline = Instant::now() + COALESCE_WINDOW;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match rx.recv_timeout(remaining) {
                Ok(event) => batch.push(event),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }

        let mut overflowed = false;
        let mut changes: Vec<PathChange> = Vec::new();
        for result in batch {
            match result {
                Ok(event) => changes.extend(classify(&event, &excluded_names)),
                Err(_) => overflowed = true,
            }
        }

        if overflowed {
            service.request_recovery_for(epoch);
            continue;
        }
        if !changes.is_empty() {
            service.submit_changes(epoch, changes);
        }
    }
}

fn classify(event: &Event, excluded_names: &[String]) -> Vec<PathChange> {
    // RenameMode::Both carries the old path first and the new path second.
    // Filter each side independently so a move out of an excluded directory
    // still removes the old indexed row when the destination is eligible.
    if matches!(
        event.kind,
        EventKind::Modify(ModifyKind::Name(RenameMode::Both))
    ) {
        let mut changes = Vec::new();
        if let Some(old) = event
            .paths
            .first()
            .filter(|p| !is_excluded(p, excluded_names))
        {
            changes.push(PathChange::Remove(old.clone()));
        }
        if let Some(new) = event
            .paths
            .get(1)
            .filter(|p| !is_excluded(p, excluded_names))
        {
            changes.push(PathChange::Upsert(new.clone()));
        }
        return changes;
    }
    let paths: Vec<PathBuf> = event
        .paths
        .iter()
        .filter(|p| !is_excluded(p, excluded_names))
        .cloned()
        .collect();
    if paths.is_empty() {
        return Vec::new();
    }

    match &event.kind {
        EventKind::Create(CreateKind::File) | EventKind::Create(CreateKind::Any) => {
            paths.into_iter().map(PathChange::Upsert).collect()
        }
        EventKind::Remove(_) => paths.into_iter().map(PathChange::Remove).collect(),
        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
            paths.into_iter().map(PathChange::Remove).collect()
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
            paths.into_iter().map(PathChange::Upsert).collect()
        }
        // RenameMode::Both is handled above so each side is filtered alone.
        EventKind::Modify(ModifyKind::Name(_)) => {
            paths.into_iter().map(PathChange::Upsert).collect()
        }
        // Content-only modifications, metadata/attribute changes, and
        // anything else don't change what a *filename* index needs to know.
        _ => Vec::new(),
    }
}

fn is_excluded(path: &Path, excluded_names: &[String]) -> bool {
    path.components().any(|c| {
        let name = c.as_os_str().to_string_lossy().to_lowercase();
        excluded_names
            .iter()
            .any(|ex| wildcard_component_match(ex, &name))
    })
}

fn wildcard_component_match(pattern: &str, value: &str) -> bool {
    let pattern = pattern.trim().trim_matches(['/', '\\']).to_lowercase();
    let pattern = pattern.as_str();
    let (mut p, mut v, mut star, mut mark) = (0usize, 0usize, None, 0usize);
    let bytes = pattern.as_bytes();
    let text = value.as_bytes();
    while v < text.len() {
        if p < bytes.len() && (bytes[p] == text[v] || bytes[p] == b'?') {
            p += 1;
            v += 1;
        } else if p < bytes.len() && bytes[p] == b'*' {
            star = Some(p);
            mark = v;
            p += 1;
        } else if let Some(s) = star {
            p = s + 1;
            mark += 1;
            v = mark;
        } else {
            return false;
        }
    }
    while p < bytes.len() && bytes[p] == b'*' {
        p += 1;
    }
    p == bytes.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::RemoveKind;

    fn event(kind: EventKind, paths: &[&str]) -> Event {
        paths
            .iter()
            .fold(Event::new(kind), |e, p| e.add_path(PathBuf::from(p)))
    }

    #[test]
    fn a_file_create_is_an_upsert() {
        let e = event(EventKind::Create(CreateKind::File), &[r"C:\src\new.rs"]);
        assert_eq!(
            classify(&e, &[]),
            vec![PathChange::Upsert(PathBuf::from(r"C:\src\new.rs"))]
        );
    }

    #[test]
    fn a_removal_is_a_remove() {
        let e = event(EventKind::Remove(RemoveKind::File), &[r"C:\src\gone.rs"]);
        assert_eq!(
            classify(&e, &[]),
            vec![PathChange::Remove(PathBuf::from(r"C:\src\gone.rs"))]
        );
    }

    #[test]
    fn a_two_path_rename_is_a_remove_then_an_upsert() {
        let e = event(
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            &[r"C:\src\old.rs", r"C:\src\new.rs"],
        );
        assert_eq!(
            classify(&e, &[]),
            vec![
                PathChange::Remove(PathBuf::from(r"C:\src\old.rs")),
                PathChange::Upsert(PathBuf::from(r"C:\src\new.rs")),
            ]
        );
    }

    #[test]
    fn a_path_under_an_excluded_directory_yields_nothing() {
        let e = event(
            EventKind::Create(CreateKind::File),
            &[r"C:\project\node_modules\pkg\index.js"],
        );
        assert_eq!(classify(&e, &["node_modules".to_string()]), Vec::new());
    }

    #[test]
    fn a_content_only_modification_yields_nothing() {
        let e = event(
            EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Any)),
            &[r"C:\src\a.rs"],
        );
        assert_eq!(classify(&e, &[]), Vec::new());
    }

    #[test]
    fn wildcard_exclusions_match_the_scan_policy() {
        let e = event(
            EventKind::Create(CreateKind::File),
            &[r"C:\project\cache-v2\x.txt"],
        );
        assert_eq!(classify(&e, &["cache*".to_string()]), Vec::new());
        let sibling = event(
            EventKind::Create(CreateKind::File),
            &[r"C:\project\cached\x.txt"],
        );
        assert_eq!(
            classify(&sibling, &["cache".to_string()]),
            vec![PathChange::Upsert(PathBuf::from(
                r"C:\project\cached\x.txt"
            ))]
        );
    }

    #[test]
    fn rename_out_of_excluded_tree_still_removes_old_path() {
        let e = event(
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            &[r"C:\project\target\old.rs", r"C:\project\src\new.rs"],
        );
        assert_eq!(
            classify(&e, &["target".to_string()]),
            vec![PathChange::Upsert(PathBuf::from(r"C:\project\src\new.rs"))]
        );
    }
}

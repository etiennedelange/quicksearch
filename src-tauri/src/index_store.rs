//! Persistent filename catalog (SQLite), backing `filecache.rs`'s in-memory
//! snapshot across app restarts.
//!
//! A saved catalog is only ever trusted under the *identity* it was saved
//! with — the active source roots, the exclusion list, and the `rg` version
//! that produced it. Any of those changing makes a stored catalog
//! [`CatalogLoad::MissingOrStale`] rather than silently mixing entries from a
//! different configuration into a search.

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct CatalogEntry {
    pub path: Arc<str>,
    pub basename_folded: Arc<str>,
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
pub struct CatalogIdentity {
    pub active_roots: Vec<String>,
    pub excluded_dirs: Vec<String>,
    pub ripgrep_version: String,
    #[serde(default)]
    pub enumeration_args: Vec<String>,
}

impl CatalogIdentity {
    fn same_policy(&self, other: &Self) -> bool {
        self.excluded_dirs == other.excluded_dirs
            && self.ripgrep_version == other.ripgrep_version
            && self.enumeration_args == other.enumeration_args
    }

    #[allow(dead_code)]
    pub fn compute(
        active_paths: &[String],
        exclude_dirs: &[String],
        ripgrep_version: String,
    ) -> Self {
        CatalogIdentity {
            active_roots: normalize(active_paths),
            excluded_dirs: normalize(exclude_dirs),
            ripgrep_version,
            enumeration_args: Vec::new(),
        }
    }

    pub fn compute_with_args(
        active_paths: &[String],
        exclude_dirs: &[String],
        ripgrep_version: String,
        enumeration_args: &[String],
    ) -> Self {
        CatalogIdentity {
            active_roots: normalize(active_paths),
            excluded_dirs: normalize(exclude_dirs),
            ripgrep_version,
            enumeration_args: enumeration_args.to_vec(),
        }
    }
}

/// Trims separators, lowercases (Windows roots and exclusion names are
/// case-insensitive), sorts, and dedups — so identity comparison doesn't
/// treat `["C:\\", "D:\\"]` and `["d:\\", "c:\\"]` as different catalogs.
fn normalize(items: &[String]) -> Vec<String> {
    let mut v: Vec<String> = items
        .iter()
        .map(|s| s.trim().trim_end_matches(['\\', '/']).to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    v.sort();
    v.dedup();
    v
}

pub enum CatalogLoad {
    Ready(Vec<CatalogEntry>),
    MissingOrStale,
}

#[derive(Debug)]
pub struct IndexError(pub String);

impl std::fmt::Display for IndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<rusqlite::Error> for IndexError {
    fn from(e: rusqlite::Error) -> Self {
        IndexError(e.to_string())
    }
}

pub trait CatalogStore: Send + Sync {
    fn load(&self, identity: &CatalogIdentity) -> Result<CatalogLoad, IndexError>;
    fn save(&self, identity: &CatalogIdentity, entries: &[CatalogEntry]) -> Result<(), IndexError>;
    /// Applies a watcher-driven patch (removed paths, upserted entries)
    /// without touching `catalog_meta` — the identity a `Ready` index is
    /// patching was already written by whichever `save`/`load` put it there.
    /// `removed` is lowercased, matching how `FileCache::apply_changes`
    /// already keys removals case-insensitively.
    fn apply(&self, removed: &[String], upserts: &[CatalogEntry]) -> Result<(), IndexError>;
}

/// Used when persistence is disabled (`filename_index_persist: false`) or the
/// database failed to open — the app must keep working exactly as it did
/// before this existed, just without surviving a restart.
pub struct NullCatalogStore;

impl CatalogStore for NullCatalogStore {
    fn load(&self, _identity: &CatalogIdentity) -> Result<CatalogLoad, IndexError> {
        Ok(CatalogLoad::MissingOrStale)
    }

    fn save(
        &self,
        _identity: &CatalogIdentity,
        _entries: &[CatalogEntry],
    ) -> Result<(), IndexError> {
        Ok(())
    }

    fn apply(&self, _removed: &[String], _upserts: &[CatalogEntry]) -> Result<(), IndexError> {
        Ok(())
    }
}

/// Bumped to `3` when the lowercase-path index was replaced by `path_key`:
/// a version-2 catalog has no such column, and its indexes are not the ones
/// this version's queries are written against.
const SCHEMA_VERSION: &str = "3";

/// A stable 64-bit key for `path`, case-folded the same way SQLite's own
/// ASCII-only `lower()` is — the two are used together in one predicate, so
/// they must agree on what "the same path" means.
///
/// FNV-1a, hand-rolled rather than `DefaultHasher`, because this value is
/// written to disk: it has to mean the same thing in the next process and
/// under the next compiler, neither of which `DefaultHasher` promises.
///
/// This exists to keep the *index* small. Indexing `lower(path)` directly
/// works, but stores a second full copy of every path — about 680MB on a
/// 2.5M-file catalog. Eight bytes per row buys the same lookup, and the
/// exact `lower(path)` comparison still decides the match, so a collision
/// costs one extra row comparison rather than deleting the wrong file.
fn path_key(path: &str) -> i64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = OFFSET_BASIS;
    for byte in path.as_bytes() {
        hash ^= byte.to_ascii_lowercase() as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash as i64
}

/// Caps the WAL at 64MB. Without it SQLite grows the journal to the
/// high-water mark of the largest transaction — a full `save` of a couple of
/// million rows — and then never shrinks the file again, which is how a
/// 1.1GB catalog ended up beside a 4.4GB journal.
const JOURNAL_SIZE_LIMIT_BYTES: i64 = 64 * 1024 * 1024;

pub struct SqliteCatalogStore {
    conn: Mutex<Connection>,
}

impl SqliteCatalogStore {
    pub fn open(path: &Path) -> Result<Self, IndexError> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "journal_size_limit", JOURNAL_SIZE_LIMIT_BYTES)?;
        // Negative means KiB rather than pages. The default ~2MB cache turns
        // a full save into page-cache thrashing against a several-hundred-
        // megabyte database; this is transient memory, only touched while a
        // save or load is actually running.
        conn.pragma_update(None, "cache_size", -131_072_i64)?;

        // An older schema's rows can never be read again (`load` rejects them
        // on the version alone), so they are pure dead weight — drop them and
        // give the disk space back rather than leave a stranded catalog
        // sitting there until something happens to overwrite it.
        if Self::stored_schema_version(&conn).is_some_and(|v| v != SCHEMA_VERSION) {
            conn.execute_batch(
                "DROP TABLE IF EXISTS file_entry; DROP TABLE IF EXISTS catalog_meta;",
            )?;
            // Cheap here precisely because the tables just went away: VACUUM
            // rewrites only the live pages, which is now almost none of them.
            conn.execute_batch("VACUUM;")?;
        }

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS catalog_meta (
                key TEXT PRIMARY KEY NOT NULL,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS file_entry (
                path TEXT PRIMARY KEY NOT NULL,
                basename_folded TEXT NOT NULL,
                path_key INTEGER NOT NULL
            );
            -- What `apply` narrows a removal by. Without some index here the
            -- `lower()` wrapper in its predicate hides the primary key from
            -- the query planner and every watcher-reported deletion becomes a
            -- full scan of the entire catalog.
            --
            -- Deliberately no index on `basename_folded`: nothing queries it.
            -- Filename matching runs in memory against the loaded snapshot,
            -- never in SQL, so indexing it only bought a second copy of every
            -- basename on disk.
            CREATE INDEX IF NOT EXISTS file_entry_path_key ON file_entry(path_key);",
        )?;
        // Reclaims a journal left oversized by a previous run (the limit above
        // only applies to journals this connection goes on to checkpoint).
        let _ = conn.pragma_update(None, "wal_checkpoint", "TRUNCATE");
        Ok(SqliteCatalogStore {
            conn: Mutex::new(conn),
        })
    }

    /// The stored version, or `None` when there is no catalog yet (a fresh
    /// database) — which is not an upgrade and must not trigger a drop.
    fn stored_schema_version(conn: &Connection) -> Option<String> {
        conn.query_row(
            "SELECT value FROM catalog_meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .ok()
    }

    fn read_identity(conn: &Connection) -> Option<CatalogIdentity> {
        if Self::stored_schema_version(conn)? != SCHEMA_VERSION {
            return None;
        }
        let identity_json: String = conn
            .query_row(
                "SELECT value FROM catalog_meta WHERE key = 'identity'",
                [],
                |row| row.get(0),
            )
            .ok()?;
        serde_json::from_str(&identity_json).ok()
    }
}

impl CatalogStore for SqliteCatalogStore {
    fn load(&self, identity: &CatalogIdentity) -> Result<CatalogLoad, IndexError> {
        let conn = self.conn.lock().unwrap();
        let all_roots = match Self::read_identity(&conn) {
            Some(stored)
                if stored.same_policy(identity)
                    && identity
                        .active_roots
                        .iter()
                        .all(|root| stored.active_roots.contains(root)) =>
            {
                stored.active_roots == identity.active_roots
            }
            _ => return Ok(CatalogLoad::MissingOrStale),
        };

        let roots =
            serde_json::to_string(&identity.active_roots).map_err(|e| IndexError(e.to_string()))?;
        let sql = if all_roots {
            "SELECT path, basename_folded FROM file_entry WHERE ?1 IS NOT NULL"
        } else {
            "SELECT path, basename_folded FROM file_entry
            WHERE EXISTS (SELECT 1 FROM json_each(?1) AS root
                WHERE lower(file_entry.path) = root.value
                   OR (lower(substr(file_entry.path, 1, length(root.value))) = root.value
                       AND substr(file_entry.path, length(root.value) + 1, 1) IN (char(92), '/')))"
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([roots], |row| {
            let path: String = row.get(0)?;
            let basename: String = row.get(1)?;
            Ok(CatalogEntry {
                path: Arc::from(path),
                basename_folded: Arc::from(basename),
            })
        })?;
        let mut entries = Vec::new();
        for row in rows {
            entries.push(row?);
        }
        Ok(CatalogLoad::Ready(entries))
    }

    /// Replaces enumerated roots in one transaction, retaining other roots
    /// when their enumeration policy matches. Rows and root coverage commit
    /// together; a failed transaction retains the previous catalog.
    fn save(&self, identity: &CatalogIdentity, entries: &[CatalogEntry]) -> Result<(), IndexError> {
        let mut conn = self.conn.lock().unwrap();
        let previous = Self::read_identity(&conn).filter(|stored| stored.same_policy(identity));
        let mut saved_identity = identity.clone();
        if let Some(stored) = &previous {
            saved_identity
                .active_roots
                .extend(stored.active_roots.iter().cloned());
            saved_identity.active_roots.sort();
            saved_identity.active_roots.dedup();
        }
        let retains_roots = saved_identity.active_roots != identity.active_roots;
        let tx = conn.transaction()?;
        // Bulk-load shape: with the secondary index in place, each of a
        // couple of million inserts pays its own descent into that btree, in
        // path order rather than key order, thrashing the page cache. Dropping
        // it first and rebuilding it once at the end is the same result for a
        // fraction of the work — measured at 251s before, and the index is
        // rebuilt inside the same transaction, so a failure still rolls back
        // to the previous complete catalog rather than a catalog with no
        // index on it.
        if !retains_roots {
            tx.execute_batch("DROP INDEX IF EXISTS file_entry_path_key;")?;
        }
        if retains_roots {
            // Replace only enumerated roots; disabled roots remain reusable.
            for root in &identity.active_roots {
                tx.execute(
                    "DELETE FROM file_entry WHERE lower(path) = ?1
                    OR (lower(substr(path, 1, length(?1))) = ?1
                        AND substr(path, length(?1) + 1, 1) IN ('\\', '/'))",
                    [root],
                )?;
            }
        } else {
            tx.execute("DELETE FROM file_entry", [])?;
        }
        {
            let mut stmt = tx.prepare(
                "INSERT OR IGNORE INTO file_entry (path, basename_folded, path_key) VALUES (?1, ?2, ?3)",
            )?;
            for entry in entries {
                stmt.execute(rusqlite::params![
                    entry.path.as_ref(),
                    entry.basename_folded.as_ref(),
                    path_key(entry.path.as_ref())
                ])?;
            }
        }
        tx.execute_batch(
            "CREATE INDEX IF NOT EXISTS file_entry_path_key ON file_entry(path_key);",
        )?;
        let identity_json =
            serde_json::to_string(&saved_identity).map_err(|e| IndexError(e.to_string()))?;
        tx.execute(
            "INSERT INTO catalog_meta (key, value) VALUES ('schema_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![SCHEMA_VERSION],
        )?;
        tx.execute(
            "INSERT INTO catalog_meta (key, value) VALUES ('identity', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![identity_json],
        )?;
        tx.commit()?;
        // A full replace is the one transaction big enough to blow the
        // journal out to gigabytes, so fold it back immediately rather than
        // waiting for an automatic checkpoint that only ever rewinds the WAL
        // in place and leaves the file at its high-water mark.
        let _ = conn.pragma_update(None, "wal_checkpoint", "TRUNCATE");
        Ok(())
    }

    /// Targeted `DELETE`/`INSERT`s instead of `save`'s full-catalog replace —
    /// a watcher patch is a handful of rows out of a multi-hundred-thousand-
    /// row catalog, and rewriting all of them on every keystroke-adjacent
    /// filesystem event would make the live watcher slower than the periodic
    /// full rebuild it exists to avoid.
    fn apply(&self, removed: &[String], upserts: &[CatalogEntry]) -> Result<(), IndexError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            // `path_key` narrows to (almost always) a single row via the
            // integer index; `lower(path)` is what actually decides the
            // match, so a hash collision costs one wasted comparison rather
            // than deleting somebody else's file.
            //
            // `lower()` is ASCII-only, same as `path_key`'s own folding and
            // as `FileCache::apply_changes`'s keys — Windows paths are
            // overwhelmingly ASCII, and all three only have to agree with
            // each other, not do general Unicode casefolding.
            let mut stmt = tx.prepare(
                "DELETE FROM file_entry
                 WHERE (path_key = ?1 AND lower(path) = ?2)
                    OR (lower(substr(path, 1, length(?2))) = ?2
                        AND substr(path, length(?2) + 1, 1) IN (char(92), '/'))",
            )?;
            for path in removed {
                stmt.execute(rusqlite::params![path_key(path), path])?;
            }
        }
        {
            let mut stmt = tx.prepare(
                "INSERT INTO file_entry (path, basename_folded, path_key) VALUES (?1, ?2, ?3)
                 ON CONFLICT(path) DO UPDATE SET basename_folded = excluded.basename_folded",
            )?;
            for entry in upserts {
                stmt.execute(rusqlite::params![
                    entry.path.as_ref(),
                    entry.basename_folded.as_ref(),
                    path_key(entry.path.as_ref())
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}

/// `rg --version`'s first line, folded into every catalog's identity so a
/// `rg` upgrade (a smart-case or glob-matching change, say) invalidates a
/// stored catalog instead of silently trusting a listing an older binary
/// produced. `"unknown"` (rather than failing) if `rg` isn't on PATH right
/// now — the catalog will simply never validate until it is, same as the
/// existing "ripgrep not found" failure the in-memory build already surfaces.
pub fn ripgrep_version() -> String {
    match crate::platform::subprocess::no_window_command("rg")
        .arg("--version")
        .output()
    {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or("unknown")
            .trim()
            .to_string(),
        _ => "unknown".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        path.push(format!(
            "qs-index-{tag}-{}-{nanos}.sqlite3",
            std::process::id()
        ));
        path
    }

    fn temp_store() -> SqliteCatalogStore {
        SqliteCatalogStore::open(&temp_path("store-test")).unwrap()
    }

    /// The catalog has to end up in the *database*, not parked in a journal
    /// beside it. The real one spent a whole session as a 0-byte database
    /// next to a 1.1GB WAL, because the checkpoint after `save` was issued
    /// through an API that errors on row-returning pragmas — and the error
    /// was discarded.
    #[test]
    fn a_full_save_leaves_the_catalog_in_the_database_not_the_journal() {
        let path = temp_path("checkpoint-test");
        let store = SqliteCatalogStore::open(&path).unwrap();

        let paths: Vec<String> = (0..20_000)
            .map(|i| format!(r"C:\some\deep\directory\path\file{i}.txt"))
            .collect();
        let entries: Vec<CatalogEntry> = paths
            .iter()
            .map(|p| CatalogEntry {
                path: Arc::from(p.as_str()),
                basename_folded: Arc::from(p.rsplit('\\').next().unwrap().to_lowercase()),
            })
            .collect();
        store.save(&identity(), &entries).unwrap();

        let db = std::fs::metadata(&path).unwrap().len();
        let wal = std::fs::metadata(path.with_extension("sqlite3-wal"))
            .map(|m| m.len())
            .unwrap_or(0);

        assert!(
            db > 500_000,
            "the catalog must land in the database: {db} bytes"
        );
        assert!(
            wal < db / 4,
            "the journal must be checkpointed away after a full save: db={db} wal={wal}"
        );
    }

    fn identity() -> CatalogIdentity {
        CatalogIdentity::compute(
            &["C:\\".into(), "D:\\".into()],
            &["node_modules".into()],
            "ripgrep 15.2.0".into(),
        )
    }

    fn entries(paths: &[&str]) -> Vec<CatalogEntry> {
        paths
            .iter()
            .map(|p| CatalogEntry {
                path: Arc::from(*p),
                basename_folded: Arc::from(p.rsplit(['\\', '/']).next().unwrap().to_lowercase()),
            })
            .collect()
    }

    #[test]
    fn a_fresh_database_reports_missing_or_stale() {
        let store = temp_store();
        assert!(matches!(
            store.load(&identity()).unwrap(),
            CatalogLoad::MissingOrStale
        ));
    }

    #[test]
    fn saved_entries_are_reloaded_under_the_same_identity() {
        let store = temp_store();
        let id = identity();
        store
            .save(&id, &entries(&[r"C:\src\a.rs", r"D:\src\b.rs"]))
            .unwrap();

        let CatalogLoad::Ready(loaded) = store.load(&id).unwrap() else {
            panic!("catalog should be ready");
        };
        let mut paths: Vec<&str> = loaded.iter().map(|e| e.path.as_ref()).collect();
        paths.sort();
        assert_eq!(paths, vec![r"C:\src\a.rs", r"D:\src\b.rs"]);
    }

    #[test]
    fn a_changed_identity_invalidates_the_saved_catalog() {
        let store = temp_store();
        store
            .save(&identity(), &entries(&[r"C:\src\a.rs"]))
            .unwrap();

        let different = CatalogIdentity::compute(&["C:\\".into()], &[], "ripgrep 15.2.0".into());
        assert!(matches!(
            store.load(&different).unwrap(),
            CatalogLoad::MissingOrStale
        ));
    }

    #[test]
    fn source_toggle_and_subset_save_preserve_disabled_roots_after_reopen() {
        let path = temp_path("toggle");
        let both = identity();
        let mut c_only = both.clone();
        c_only.active_roots = vec!["c:".into()];
        {
            let store = SqliteCatalogStore::open(&path).unwrap();
            store
                .save(&both, &entries(&[r"C:\old.rs", r"D:\keep.rs"]))
                .unwrap();
            let CatalogLoad::Ready(rows) = store.load(&c_only).unwrap() else {
                panic!("C must be reusable")
            };
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].path.as_ref(), r"C:\old.rs");
            store.save(&c_only, &entries(&[r"C:\new.rs"])).unwrap();
        }
        let store = SqliteCatalogStore::open(&path).unwrap();
        let CatalogLoad::Ready(rows) = store.load(&both).unwrap() else {
            panic!("both roots must survive")
        };
        let mut paths: Vec<_> = rows.iter().map(|row| row.path.as_ref()).collect();
        paths.sort();
        assert_eq!(paths, vec![r"C:\new.rs", r"D:\keep.rs"]);
        store.save(&c_only, &[]).unwrap();
        let CatalogLoad::Ready(rows) = store.load(&both).unwrap() else {
            panic!("empty C is still complete")
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path.as_ref(), r"D:\keep.rs");
    }

    #[test]
    fn root_filter_respects_directory_boundaries_and_rejects_unknown_roots() {
        let store = temp_store();
        let mut both = identity();
        both.active_roots = vec![r"c:\work".into(), r"c:\workspace".into()];
        store
            .save(&both, &entries(&[r"C:\work\a.rs", r"C:\workspace\b.rs"]))
            .unwrap();
        let mut selected = both.clone();
        selected.active_roots = vec![r"c:\work".into()];
        let CatalogLoad::Ready(rows) = store.load(&selected).unwrap() else {
            panic!("known root")
        };
        assert_eq!(rows.len(), 1);
        store.save(&selected, &[]).unwrap();
        let CatalogLoad::Ready(rows) = store.load(&both).unwrap() else {
            panic!("known roots")
        };
        assert_eq!(rows[0].path.as_ref(), r"C:\workspace\b.rs");
        selected.active_roots = vec!["e:".into()];
        assert!(matches!(
            store.load(&selected).unwrap(),
            CatalogLoad::MissingOrStale
        ));
        selected = both.clone();
        selected.enumeration_args.push("--hidden".into());
        assert!(matches!(
            store.load(&selected).unwrap(),
            CatalogLoad::MissingOrStale
        ));
    }

    #[test]
    fn a_second_save_replaces_the_first_entirely() {
        let store = temp_store();
        let id = identity();
        store.save(&id, &entries(&[r"C:\src\old.rs"])).unwrap();
        store.save(&id, &entries(&[r"C:\src\new.rs"])).unwrap();

        let CatalogLoad::Ready(loaded) = store.load(&id).unwrap() else {
            panic!("catalog should be ready");
        };
        let paths: Vec<&str> = loaded.iter().map(|e| e.path.as_ref()).collect();
        assert_eq!(paths, vec![r"C:\src\new.rs"]);
    }

    #[test]
    fn identity_normalization_ignores_case_order_and_trailing_slashes() {
        let a = CatalogIdentity::compute(
            &["C:\\".into(), "D:\\".into()],
            &["Node_Modules".into()],
            "ripgrep 15.2.0".into(),
        );
        let b = CatalogIdentity::compute(
            &["d:\\".into(), "c:\\".into()],
            &["node_modules".into()],
            "ripgrep 15.2.0".into(),
        );
        assert_eq!(a, b);
    }

    /// The bug that made this app unusable in the background: `apply`'s
    /// `WHERE lower(path) = ?` could not use the `path` primary key, so every
    /// single watcher-reported deletion scanned the whole catalog. Measured
    /// against a real 2.5M-row index that was 3.4s warm and 37.6s cold *per
    /// removed path*, which kept the watcher thread pegged at 100% of a core
    /// reading ~220MB/s indefinitely.
    #[test]
    fn removing_a_path_searches_an_index_instead_of_scanning_the_catalog() {
        let store = temp_store();
        let conn = store.conn.lock().unwrap();
        let plan: String = conn
            .query_row(
                "EXPLAIN QUERY PLAN DELETE FROM file_entry WHERE path_key = 1 AND lower(path) = 'x'",
                [],
                |row| row.get(3),
            )
            .unwrap();
        assert!(
            plan.contains("USING INDEX") && plan.contains("path_key"),
            "a removal must be an integer index search, not a full catalog scan: {plan}"
        );
    }

    /// The hash narrows the search; it does not decide the match. Two paths
    /// sharing a `path_key` must not take each other with them.
    #[test]
    fn a_shared_path_key_never_removes_the_wrong_file() {
        let store = temp_store();
        let id = identity();
        store.save(&id, &entries(&[r"C:\src\keep.rs"])).unwrap();

        // Force the collision rather than searching for one: give an
        // unrelated row the same key as the path about to be removed.
        let colliding_key = path_key(r"c:\src\gone.rs");
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "UPDATE file_entry SET path_key = ?1 WHERE path = ?2",
                rusqlite::params![colliding_key, r"C:\src\keep.rs"],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO file_entry (path, basename_folded, path_key) VALUES (?1, ?2, ?3)",
                rusqlite::params![r"C:\src\gone.rs", "gone.rs", colliding_key],
            )
            .unwrap();
        }

        store.apply(&[r"c:\src\gone.rs".to_string()], &[]).unwrap();

        let CatalogLoad::Ready(loaded) = store.load(&id).unwrap() else {
            panic!("catalog should be ready");
        };
        let paths: Vec<&str> = loaded.iter().map(|e| e.path.as_ref()).collect();
        assert_eq!(paths, vec![r"C:\src\keep.rs"], "only the named path may go");
    }

    #[test]
    fn removing_a_directory_removes_descendants_but_not_prefix_siblings() {
        let store = temp_store();
        let id = identity();
        store
            .save(
                &id,
                &entries(&[
                    r"C:\src\old\a.rs",
                    r"C:\src\old\nested\b.rs",
                    r"C:\src\old-files\keep.rs",
                ]),
            )
            .unwrap();
        store.apply(&[r"c:\src\old".to_string()], &[]).unwrap();
        let CatalogLoad::Ready(loaded) = store.load(&id).unwrap() else {
            panic!("catalog should be ready");
        };
        let paths: Vec<&str> = loaded.iter().map(|e| e.path.as_ref()).collect();
        assert_eq!(paths, vec![r"C:\src\old-files\keep.rs"]);
    }

    /// The key is written to disk, so it has to mean the same thing in the
    /// next process — and fold case the same way SQLite's own `lower()` does,
    /// since the two are used together in one predicate.
    #[test]
    fn the_path_key_is_stable_and_case_insensitive() {
        assert_eq!(path_key(r"C:\Src\File.TXT"), path_key(r"c:\src\file.txt"));
        assert_ne!(path_key(r"C:\src\a.txt"), path_key(r"C:\src\b.txt"));
        // A fixed expectation, so a future change to the hash cannot silently
        // orphan every catalog in the field without failing here first.
        assert_eq!(path_key("c:\\a"), 9190121232932592759_i64);
    }

    /// Nothing queries `file_entry` by basename — filename matching runs in
    /// memory against the loaded snapshot, never in SQL — so an index on it
    /// is a second copy of every basename bought for nothing.
    #[test]
    fn no_index_is_kept_for_a_column_nothing_queries() {
        let store = temp_store();
        let conn = store.conn.lock().unwrap();
        let indexes: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'file_entry'",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(
            !indexes.iter().any(|n| n.contains("basename")),
            "found an unused basename index: {indexes:?}"
        );
    }

    /// Without a journal size limit the WAL only ever grows to its high-water
    /// mark and stays there — the real catalog had a 4.4GB journal sitting
    /// beside a 1.1GB database, none of which was ever reclaimed.
    #[test]
    fn the_write_ahead_log_is_size_limited() {
        let store = temp_store();
        let conn = store.conn.lock().unwrap();
        let limit: i64 = conn
            .query_row("PRAGMA journal_size_limit", [], |row| row.get(0))
            .unwrap();
        assert!(
            limit > 0,
            "an unbounded WAL never gives its disk space back"
        );
    }

    /// A catalog written by an older schema must be dropped outright rather
    /// than left taking up space behind a `MissingOrStale` verdict that means
    /// nothing will ever read it again.
    #[test]
    fn a_catalog_from_an_older_schema_is_dropped_rather_than_stranded() {
        let mut path = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        path.push(format!(
            "qs-index-upgrade-test-{}-{nanos}.sqlite3",
            std::process::id()
        ));
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE catalog_meta (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL);
                 CREATE TABLE file_entry (path TEXT PRIMARY KEY NOT NULL, basename_folded TEXT NOT NULL);
                 INSERT INTO catalog_meta VALUES ('schema_version', '0');
                 INSERT INTO catalog_meta VALUES ('identity', '{}');
                 INSERT INTO file_entry VALUES ('C:\\old.txt', 'old.txt');",
            )
            .unwrap();
        }

        let store = SqliteCatalogStore::open(&path).unwrap();
        let conn = store.conn.lock().unwrap();
        let rows: i64 = conn
            .query_row("SELECT count(*) FROM file_entry", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            rows, 0,
            "an older schema's rows must not survive the upgrade"
        );
        let meta: i64 = conn
            .query_row("SELECT count(*) FROM catalog_meta", [], |row| row.get(0))
            .unwrap();
        assert_eq!(meta, 0, "its stale identity must go with it");
    }

    /// Dropping the rows is not enough on its own — SQLite keeps the freed
    /// pages in the file. The real catalog was a 1.1GB database that no
    /// version of the app could read any more, so the upgrade has to hand the
    /// disk space back, not just stop using it.
    #[test]
    fn upgrading_from_an_older_schema_gives_the_disk_space_back() {
        let mut path = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        path.push(format!(
            "qs-index-reclaim-test-{}-{nanos}.sqlite3",
            std::process::id()
        ));
        {
            let mut conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE catalog_meta (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL);
                 CREATE TABLE file_entry (path TEXT PRIMARY KEY NOT NULL, basename_folded TEXT NOT NULL);
                 INSERT INTO catalog_meta VALUES ('schema_version', '1');",
            )
            .unwrap();
            let tx = conn.transaction().unwrap();
            {
                let mut stmt = tx
                    .prepare("INSERT INTO file_entry (path, basename_folded) VALUES (?1, ?2)")
                    .unwrap();
                for i in 0..50_000 {
                    stmt.execute(rusqlite::params![
                        format!(r"C:\some\deep\directory\path\file{i}.txt"),
                        format!("file{i}.txt")
                    ])
                    .unwrap();
                }
            }
            tx.commit().unwrap();
        }
        let before = std::fs::metadata(&path).unwrap().len();
        assert!(before > 1_000_000, "fixture should be big enough to notice");

        // Deliberately measured with the store still open. The app holds this
        // connection for its whole life and exits via `app.exit(0)`, which
        // never drops it — so relying on close-time cleanup (which is what
        // made an earlier version of this test pass while the real 1.1GB
        // catalog stayed on disk) proves nothing.
        let store = SqliteCatalogStore::open(&path).unwrap();

        let after = std::fs::metadata(&path).unwrap().len();
        let wal = std::fs::metadata(path.with_extension("sqlite3-wal"))
            .map(|m| m.len())
            .unwrap_or(0);
        assert!(
            after < before / 10,
            "an unreadable catalog must be reclaimed, not stranded: {before} -> {after} bytes"
        );
        assert!(
            wal < before / 10,
            "and its journal must not simply inherit the bulk: {wal} bytes"
        );
        drop(store);
    }

    /// Not part of the normal suite — it writes a full-size catalog and takes
    /// minutes. Run with `cargo test -p quicksearch --release
    /// full_scale_save -- --ignored --nocapture` when the on-disk cost or the
    /// save duration needs re-measuring against a real 2.5M-file index.
    #[test]
    #[ignore]
    fn full_scale_save_reports_its_cost() {
        let path = temp_path("full-scale");
        let store = SqliteCatalogStore::open(&path).unwrap();

        let build = std::time::Instant::now();
        let entries: Vec<CatalogEntry> = (0..2_500_000)
            .map(|i| {
                let p = format!(
                    r"C:\Users\someone\projects\repo{}\src\module{}\file{}.rs",
                    i % 900,
                    i % 60,
                    i
                );
                let base = p.rsplit('\\').next().unwrap().to_lowercase();
                CatalogEntry {
                    path: Arc::from(p.as_str()),
                    basename_folded: Arc::from(base.as_str()),
                }
            })
            .collect();
        println!("built {} entries in {:?}", entries.len(), build.elapsed());

        let saved = std::time::Instant::now();
        store.save(&identity(), &entries).unwrap();
        let save_time = saved.elapsed();

        let db = std::fs::metadata(&path).unwrap().len();
        let wal = std::fs::metadata(path.with_extension("sqlite3-wal"))
            .map(|m| m.len())
            .unwrap_or(0);
        println!("save took {save_time:?}");
        println!("db  = {:.1} MB", db as f64 / 1048576.0);
        println!("wal = {:.1} MB", wal as f64 / 1048576.0);

        assert!(db > 0, "the catalog must actually commit");
        assert!(wal < db / 4, "and be checkpointed out of the journal");
    }

    #[test]
    fn null_store_never_reports_a_catalog_ready() {
        let store = NullCatalogStore;
        assert!(matches!(
            store.load(&identity()).unwrap(),
            CatalogLoad::MissingOrStale
        ));
        store.save(&identity(), &entries(&["C:\\a.rs"])).unwrap();
        assert!(matches!(
            store.load(&identity()).unwrap(),
            CatalogLoad::MissingOrStale
        ));
    }
}

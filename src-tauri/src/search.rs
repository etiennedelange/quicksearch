//! Search execution, porting quicksearch's `run_content_search`/
//! `run_files_search`/`_spawn_rg`/streaming (`_emit`/`_flush_results`).
//!
//! Unlike the Python original (which buffers on a worker thread and lets a
//! Tk `after()` timer poll+flush on the main thread), rows are emitted
//! straight to the frontend as Tauri events as soon as a batch is ready —
//! the webview's own event queue plays the role of that flush timer.

use crate::platform::subprocess::no_window_command;
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

use crate::config::Config;
use crate::filecache::{exclude_glob_args, CacheState, FileCache, Window};

fn wait_for_index(state: &SearchState, generation: u64) -> bool {
    while is_current(state, generation) {
        if !matches!(
            state.file_cache.status().0,
            CacheState::Loading | CacheState::Building
        ) {
            return true;
        }
        FileCache::poll_wait();
    }
    false
}
use crate::index_service::IndexService;

const MIN_QUERY_CHARS: usize = 3;
const EMIT_BATCH: usize = 64;
const EMIT_INTERVAL: Duration = Duration::from_millis(50);
const MAX_LINE_CHARS: usize = 300;
const DIAGNOSTIC_LIMIT: usize = 64 * 1024;
/// How many index entries the filename scan takes per pass. Bounded so a
/// pass can't hold a large temporary, and so the scan re-checks for
/// cancellation and for the index being rebuilt at a predictable cadence
/// even on an index of hundreds of thousands of files.
const SCAN_WINDOW: usize = 8192;

/// A spawned `rg` child tagged with the search generation that owns it.
/// Keeping the tag alongside the child (instead of in a separate atomic) is
/// what lets [`SearchState::install`]/[`SearchState::reap`] check ownership
/// and mutate the slot as one atomic step under a single lock — closing the
/// check-then-act windows a separate `AtomicU64` + `Mutex<Option<Child>>>`
/// pair would leave between "is this still current?" and "act on it".
struct RunningSearch {
    generation: u64,
    child: Child,
}

pub struct SearchState {
    pub generation: AtomicU64,
    running: Mutex<Option<RunningSearch>>,
    pub file_cache: Arc<FileCache>,
    pub index_service: Arc<IndexService>,
    /// Whether [`crate::index_watcher::IndexWatcher`] is actually live for at
    /// least one active root — surfaced to the frontend so the colophon's
    /// index dot can tell "watched in real time" from "relying on the
    /// periodic refresh only" apart, instead of just guessing from silence.
    pub watcher_live: Arc<std::sync::atomic::AtomicBool>,
}

impl SearchState {
    pub fn new(
        catalog_store: Arc<dyn crate::index_store::CatalogStore>,
        on_activity: Option<Arc<dyn Fn(&str) + Send + Sync>>,
    ) -> Self {
        let file_cache = Arc::new(FileCache::new(catalog_store, on_activity));
        let index_service = IndexService::start(file_cache.clone());
        SearchState {
            generation: AtomicU64::new(0),
            running: Mutex::new(None),
            file_cache,
            index_service,
            watcher_live: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Bumps the generation and unconditionally kills whatever search was
    /// previously running — a brand new search always supersedes it,
    /// regardless of which generation it belonged to.
    fn begin_generation(&self) -> u64 {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.kill_running();
        generation
    }

    fn kill_running(&self) {
        if let Some(mut running) = self.running.lock().unwrap().take() {
            let _ = running.child.kill();
            let _ = running.child.wait();
        }
    }

    /// Stops whatever's currently running — the content/filename search and,
    /// if the filename index is still mid-build, that build too (a `Ready`
    /// index is left alone: still good, and expensive to rebuild). Matches
    /// quicksearch's `cancel_search(idle=False)`, called from every path
    /// that dismisses the window (Escape, the title bar, Alt+F4, the global
    /// hotkey, or opening a result) so a whole-drive walk doesn't keep
    /// running unattended once nobody is reading the page.
    pub fn cancel(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.kill_running();
        self.index_service.cancel_build();
    }

    /// Cancels only the foreground query. Filename indexing is shared work
    /// and must continue while the user types; otherwise each debounced query
    /// can kill and restart the same whole-drive build.
    pub fn cancel_search_only(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.kill_running();
    }

    /// Installs `child` as the running search for `generation`, unless a
    /// newer generation has already started (checked under the same lock as
    /// `begin_generation`'s kill, so there's no window for a newer search to
    /// start unnoticed in between). Returns `false` if superseded — `child`
    /// is killed here before returning, since `Child::drop` does not kill
    /// the process and the caller no longer has a reference to it once this
    /// returns.
    fn install(&self, generation: u64, mut child: Child) -> bool {
        let mut guard = self.running.lock().unwrap();
        if self.generation.load(Ordering::SeqCst) != generation {
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
        *guard = Some(RunningSearch { generation, child });
        true
    }

    /// Reaps this search's own child once its worker is done reading its
    /// stdout — but only if it's still the one installed under this exact
    /// generation. A no-op if a newer search already superseded and killed
    /// it, and critically: never kills a *newer* search's child, unlike an
    /// unconditional "take whatever's there" cleanup would.
    fn reap(&self, generation: u64) -> Option<std::process::ExitStatus> {
        let mut guard = self.running.lock().unwrap();
        if guard.as_ref().is_some_and(|r| r.generation == generation) {
            if let Some(mut running) = guard.take() {
                if let Ok(Some(status)) = running.child.try_wait() {
                    return Some(status);
                }
                let _ = running.child.kill();
                return running.child.wait().ok();
            }
        }
        None
    }
}

#[derive(Serialize, Clone)]
pub struct Segment {
    text: String,
    hit: bool,
}

#[derive(Serialize, Clone)]
pub struct ResultRow {
    path: String,
    /// Directory portion of `path` (including the trailing separator),
    /// never highlighted — matches the original's `head`/`base` split,
    /// where only the filename itself can carry a rubric mark.
    dir: String,
    /// Filename portion of `path`, split into plain/highlighted segments.
    /// Populated in both search modes: "the name carries the match too,
    /// and in Filenames mode it is the only place the match can be" — but
    /// the original rubricates the filename in content search results too,
    /// not just the excerpt.
    basename_segments: Vec<Segment>,
    line: u64,
    /// KWIC excerpt segments — content search only; empty for filename
    /// search rows, which have no excerpt line to show.
    segments: Vec<Segment>,
    /// True for a Folders-mode row, or a folder row within Both mode — the
    /// frontend uses it to append a trailing separator, the only thing that
    /// tells a folder row apart from a file row once the two share one list.
    is_dir: bool,
}

/// Splits `path` into its directory prefix and filename, and rubricates
/// every literal, case-insensitive occurrence of `query` within the
/// filename — matching the original's `_draw_match_text` exactly (a plain
/// substring highlight, not a precise regex-match span, even when
/// `search_regex` is on; the original does this unconditionally too).
fn build_path_display(path: &str, query: &str) -> (String, Vec<Segment>) {
    let cut = path.rfind(['\\', '/']).map(|i| i + 1).unwrap_or(0);
    let dir = path[..cut].to_string();
    let basename = &path[cut..];
    (dir, highlight_literal_all(basename, query))
}

/// Byte length within `text`, starting at `at`, of a case-insensitive match
/// for `needle_lower` (which must already be lowercased) — or `None`.
///
/// Comparison walks both sides as `char`s and lowercases only the haystack
/// side, one character at a time, so every returned length lands on a `text`
/// char boundary by construction. That is the point: searching a lowercased
/// *copy* of the haystack and reusing those offsets in the original is wrong
/// whenever case folding changes the byte length (U+0130 `İ` lowercases to
/// two code points; U+212A `K` to one shorter one), which shifts every
/// offset after it and silently rubricates the wrong span.
fn match_len_at(text: &str, at: usize, needle_lower: &str) -> Option<usize> {
    let mut needle = needle_lower.chars();
    let mut pending = needle.next();
    let mut consumed = 0usize;

    for haystack_char in text[at..].chars() {
        if pending.is_none() {
            break;
        }
        for lowered in haystack_char.to_lowercase() {
            match pending {
                Some(want) if want == lowered => pending = needle.next(),
                // Either a mismatch, or the needle ran out partway through
                // this character's expansion — a match that ends mid-char
                // has no honest byte length, so it isn't one.
                _ => return None,
            }
        }
        consumed += haystack_char.len_utf8();
    }

    pending.is_none().then_some(consumed)
}

/// Rubricates every literal, case-insensitive occurrence of `query` in
/// `text` — matching the original's `_draw_match_text` (a plain substring
/// highlight, not a precise regex-match span, even when `search_regex` is on;
/// the original does this unconditionally too).
fn highlight_literal_all(text: &str, query: &str) -> Vec<Segment> {
    let needle_lower = query.trim().to_lowercase();
    if needle_lower.is_empty() {
        return vec![Segment {
            text: text.to_string(),
            hit: false,
        }];
    }

    let mut segments = Vec::new();
    let mut plain_from = 0usize; // start of the not-yet-emitted plain run
    let mut at = 0usize; // scan position, always a char boundary

    while at < text.len() {
        match match_len_at(text, at, &needle_lower) {
            Some(len) if len > 0 => {
                if at > plain_from {
                    segments.push(Segment {
                        text: text[plain_from..at].to_string(),
                        hit: false,
                    });
                }
                segments.push(Segment {
                    text: text[at..at + len].to_string(),
                    hit: true,
                });
                at += len;
                plain_from = at;
            }
            _ => {
                at += text[at..].chars().next().map(char::len_utf8).unwrap_or(1);
            }
        }
    }

    if plain_from < text.len() {
        segments.push(Segment {
            text: text[plain_from..].to_string(),
            hit: false,
        });
    }
    if segments.is_empty() {
        segments.push(Segment {
            text: text.to_string(),
            hit: false,
        });
    }
    segments
}

// -- typed ripgrep --json message shapes ------------------------------------
//
// Parsing every line into a generic `serde_json::Value` and then chasing
// `.get("data").get("stats")...` chains works but allocates a whole JSON
// tree per line for fields we already know the shape of. Typed structs let
// serde deserialize straight into what we need.

#[derive(Deserialize)]
#[serde(tag = "type", content = "data")]
enum RgMessage {
    // Neither carries anything the UI surfaces (see the removed
    // "rg: N files"/hover-stats reading) — parsed only far enough to be
    // recognized and skipped.
    #[serde(rename = "begin")]
    Begin(serde::de::IgnoredAny),
    #[serde(rename = "match")]
    Match(RgMatchData),
    #[serde(rename = "summary")]
    Summary(serde::de::IgnoredAny),
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct RgMatchData {
    path: RgText,
    lines: RgText,
    line_number: u64,
    #[serde(default)]
    submatches: Vec<RgSubmatch>,
}

#[derive(Deserialize)]
struct RgSubmatch {
    start: usize,
    end: usize,
}

/// rg emits `{"text": "..."}` for valid UTF-8 data and `{"bytes": "<base64>"}`
/// for paths/lines that aren't (common when searching a whole drive).
#[derive(Deserialize)]
#[serde(untagged)]
enum RgText {
    Utf8 { text: String },
    Base64 { bytes: String },
}

impl RgText {
    fn into_string(self) -> Option<String> {
        match self {
            RgText::Utf8 { text } => Some(text),
            RgText::Base64 { bytes } => {
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(bytes)
                    .ok()?;
                Some(String::from_utf8_lossy(&decoded).into_owned())
            }
        }
    }
}

pub fn start(
    app: AppHandle,
    state: Arc<SearchState>,
    pattern: String,
    mode: String,
    search_regex: bool,
    case_mode: String,
    config: Config,
) -> u64 {
    let generation = state.begin_generation();

    let _ = app.emit("qs-begin", generation);

    if pattern.trim().chars().count() < MIN_QUERY_CHARS {
        let _ = app.emit(
            "qs-done",
            serde_json::json!({ "generation": generation, "idle": true }),
        );
        return generation;
    }

    std::thread::spawn(move || {
        let result = match mode.as_str() {
            "files" => run_files_search(
                &app,
                &state,
                &pattern,
                generation,
                search_regex,
                &case_mode,
                &config,
            ),
            "folders" => run_folders_search(
                &app,
                &state,
                &pattern,
                generation,
                search_regex,
                &case_mode,
                &config,
            ),
            "both" => run_names_search(
                &app,
                &state,
                &pattern,
                generation,
                search_regex,
                &case_mode,
                &config,
            ),
            _ => run_content_search(
                &app,
                &state,
                &pattern,
                generation,
                search_regex,
                &case_mode,
                &config,
            ),
        };

        if let Err(message) = result {
            let _ = app.emit(
                "qs-error",
                serde_json::json!({ "generation": generation, "message": message }),
            );
        }
        let _ = app.emit(
            "qs-done",
            serde_json::json!({ "generation": generation, "idle": false }),
        );
    });

    generation
}

pub(crate) fn active_paths(config: &Config) -> Vec<String> {
    let disabled: std::collections::HashSet<String> = config
        .disabled_paths
        .iter()
        .map(|path| {
            path.trim()
                .trim_end_matches(['\\', '/'])
                .to_ascii_lowercase()
        })
        .collect();
    let mut candidates: Vec<(String, String)> = config
        .paths
        .iter()
        .filter_map(|path| {
            let original = path.trim().to_string();
            let key = original.trim_end_matches(['\\', '/']).to_ascii_lowercase();
            (!key.is_empty() && !disabled.contains(&key)).then_some((key, original))
        })
        .collect();

    // A parent and one of its descendants make ripgrep enumerate the
    // descendant twice. Keep the shortest configured root and retain its
    // original spelling for the subprocess and display metadata.
    candidates.sort_by(|(left, _), (right, _)| left.len().cmp(&right.len()).then(left.cmp(right)));
    let mut roots: Vec<(String, String)> = Vec::new();
    for (key, original) in candidates {
        let covered = roots.iter().any(|(parent, _)| {
            key == *parent
                || (key.starts_with(parent)
                    && matches!(key.as_bytes().get(parent.len()), Some(b'\\' | b'/')))
        });
        if !covered {
            roots.push((key, original));
        }
    }
    roots.into_iter().map(|(_, original)| original).collect()
}

fn is_current(state: &SearchState, generation: u64) -> bool {
    state.generation.load(Ordering::SeqCst) == generation
}

// -- content search (rg --json) --------------------------------------------

fn run_content_search(
    app: &AppHandle,
    state: &Arc<SearchState>,
    pattern: &str,
    generation: u64,
    search_regex: bool,
    case_mode: &str,
    config: &Config,
) -> Result<(), String> {
    let paths = active_paths(config);
    if paths.is_empty() {
        return Ok(());
    }

    let mut cmd = no_window_command("rg");
    cmd.arg("--json")
        .arg("--max-count")
        .arg(config.max_per_file.to_string());
    if !search_regex {
        cmd.arg("--fixed-strings");
    }
    if config.content_search_quiet {
        // A broad content query can otherwise fan out across every core and
        // saturate storage while the user is still typing. Keep a small pool
        // instead of forcing a serial walk: one worker made a D:\ search take
        // roughly five seconds versus well under one second with bounded
        // parallelism. Filename searches use the in-memory catalog and are
        // unaffected by this setting.
        cmd.arg("--threads").arg("4");
    }
    // Do not follow junctions/reparse points into mounted volumes while a
    // broad content search walks a drive unless the user explicitly opts in.
    // Those boundaries are a common source of permission failures and can
    // multiply the amount of storage scanned beyond the configured roots.
    if !config.content_search_follow_mounts {
        cmd.arg("--one-file-system");
    }
    cmd.args(exclude_glob_args(&config.exclude_dirs));
    cmd.args(&config.rg_extra_args);
    match case_mode {
        "sensitive" => {
            cmd.arg("--case-sensitive");
        }
        "insensitive" => {
            cmd.arg("--ignore-case");
        }
        _ => {}
    }
    cmd.arg("--").arg(pattern);
    cmd.args(&paths);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = crate::platform::subprocess::spawn_bound_to_job(&mut cmd)
        .map_err(|e| format!("ripgrep (rg) not found on PATH — install rg, then restart: {e}"))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    // Drain diagnostics while stdout is being consumed. A broad drive walk
    // can produce enough access-denied messages to fill the OS pipe; waiting
    // until stdout EOF would then block ripgrep before later matches arrive.
    let diagnostics_reader = std::thread::spawn(move || drain_diagnostics(stderr));

    if !state.install(generation, child) {
        // Superseded before we ever got to read from it — `install` already
        // killed the child, nothing else to do here.
        return Ok(());
    }

    let mut reader = BufReader::new(stdout);
    let max_results = config.max_results as usize;

    let mut batch: Vec<ResultRow> = Vec::new();
    let mut count = 0usize;
    let mut reached_limit = false;
    let mut stdout_error = None;
    let mut last_emit = Instant::now();
    // One reusable buffer rather than the fresh `String` per line
    // `reader.lines()` allocates — a content search over a whole drive is a
    // lot of lines, and none of those allocations outlive the iteration.
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Err(error) => {
                stdout_error = Some(error.to_string());
                break;
            }
            Ok(_) => {}
        }
        if !is_current(state, generation) {
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<RgMessage>(&line) else {
            continue;
        };
        match msg {
            RgMessage::Begin(_) | RgMessage::Summary(_) => {}
            RgMessage::Match(data) => {
                let Some(path) = data.path.into_string() else {
                    continue;
                };
                let text = data.lines.into_string().unwrap_or_default();

                // rg's submatch offsets are byte offsets into the ORIGINAL
                // line — they must be adjusted by however much leading
                // whitespace `.trim()` below strips, or highlighting lands
                // on the wrong substring (a bug this exact code used to
                // have: offsets computed pre-trim, applied post-trim).
                let reported_spans: Vec<(usize, usize)> = data
                    .submatches
                    .iter()
                    .map(|span| (span.start, span.end))
                    .collect();
                let (excerpt, spans) = content_excerpt(&text, &reported_spans);

                let (dir, basename_segments) = build_path_display(&path, pattern);
                batch.push(ResultRow {
                    path,
                    dir,
                    basename_segments,
                    line: data.line_number,
                    segments: build_segments(&excerpt, &spans),
                    is_dir: false,
                });
                count += 1;
                // Do not wait for a 64-row batch or another match before
                // showing the first useful result. This matters most for a
                // rare query where the next read may be a long, disk-heavy
                // walk.
                if count == 1 && !batch.is_empty() {
                    if !emit_batch(app, state, generation, &mut batch) {
                        return Ok(());
                    }
                    last_emit = Instant::now();
                }
                if batch.len() >= EMIT_BATCH || last_emit.elapsed() >= EMIT_INTERVAL {
                    if !emit_batch(app, state, generation, &mut batch) {
                        return Ok(());
                    }
                    last_emit = Instant::now();
                }
                if count >= max_results {
                    reached_limit = true;
                    if !batch.is_empty() {
                        emit_batch(app, state, generation, &mut batch);
                    }
                    // No `qs-done` emitted here: `start` emits the real one
                    // (with the `{generation, idle}` payload the frontend
                    // destructures) the moment this function returns, which
                    // is immediately. This used to emit a bare `u64` under
                    // the same event name, which the listener silently
                    // dropped — dead code that read as load-bearing.
                    //
                    // Kill `rg` here rather than draining it to real EOF for
                    // an eventual --stats summary: that summary is only
                    // reachable if nothing touches the search box again
                    // before a full two-drive walk finishes, which in
                    // practice is almost never — any further keystroke (even
                    // retyping the same text) supersedes and kills this
                    // generation anyway via begin_generation(), just later
                    // and after leaving a stale "N files so far" reading
                    // with no path to ever completing. Matches quicksearch's
                    // own `break` + immediate `_reap(proc)` here — the
                    // hover-stats tooltip simply isn't offered once capped,
                    // same documented limitation as the original.
                    break;
                }
            }
            RgMessage::Other => {}
        }
    }

    if !batch.is_empty() {
        emit_batch(app, state, generation, &mut batch);
    }

    // Always reap this search's own child once we're done reading its
    // stdout, matching the original's `finally: self._reap(proc)`. Only
    // touches it if it's still ours (see `SearchState::reap`) — never a
    // newer search's child, unlike an earlier version of this code that
    // unconditionally took whatever was in the shared slot.
    let status = state.reap(generation);
    let diagnostics = diagnostics_reader.join().unwrap_or_default();
    if let Some(error) = stdout_error {
        if is_current(state, generation) {
            let _ = app.emit(
                "qs-warning",
                serde_json::json!({
                    "generation": generation,
                    "message": format!("Content search ended early; results may be partial ({error}).")
                }),
            );
        }
        return Ok(());
    }
    if let Some(status) = status {
        // ripgrep uses exit code 1 for a valid search with no matches and 2
        // (or another non-success code) for operational/argument errors.
        if !reached_limit && !status.success() && status.code() != Some(1) {
            let detail = String::from_utf8_lossy(&diagnostics);
            let detail = detail.trim();
            let detail = if detail.len() > 2000 {
                &detail[..2000]
            } else {
                detail
            };
            if is_restricted_scan_failure(detail) {
                let _ = app.emit(
                    "qs-warning",
                    serde_json::json!({
                        "generation": generation,
                        "message": "Some protected or locked paths were skipped; the results may be partial."
                    }),
                );
                return Ok(());
            }
            return Err(if detail.is_empty() {
                format!(
                    "ripgrep failed with exit code {}",
                    status.code().unwrap_or(-1)
                )
            } else {
                format!(
                    "ripgrep failed with exit code {}: {}",
                    status.code().unwrap_or(-1),
                    detail
                )
            });
        }
    }

    Ok(())
}

fn drain_diagnostics(mut reader: impl Read) -> Vec<u8> {
    let mut retained = Vec::with_capacity(DIAGNOSTIC_LIMIT.min(8192));
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                if retained.len() < DIAGNOSTIC_LIMIT {
                    let keep = (DIAGNOSTIC_LIMIT - retained.len()).min(count);
                    retained.extend_from_slice(&buffer[..keep]);
                }
            }
            Err(_) => break,
        }
    }
    retained
}

fn is_restricted_scan_failure(detail: &str) -> bool {
    let lower = detail.to_ascii_lowercase();
    lower.contains("access is denied")
        || lower.contains("permission denied")
        || (lower.contains("panicked at") && lower.contains("ignore"))
}

fn emit_batch(
    app: &AppHandle,
    state: &Arc<SearchState>,
    generation: u64,
    batch: &mut Vec<ResultRow>,
) -> bool {
    if !is_current(state, generation) {
        return false;
    }
    let rows = std::mem::take(batch);
    let _ = app.emit(
        "qs-batch",
        serde_json::json!({ "generation": generation, "rows": rows }),
    );
    true
}

fn content_excerpt(text: &str, reported: &[(usize, usize)]) -> (String, Vec<(usize, usize)>) {
    let start = text.len() - text.trim_start().len();
    let end = text.trim_end_matches(['\r', '\n']).len();
    let source = if start <= end { &text[start..end] } else { "" };
    let spans: Vec<(usize, usize)> = reported
        .iter()
        .filter(|(_, span_end)| *span_end > start)
        .filter_map(|(span_start, span_end)| {
            let local_start = (*span_start).max(start) - start;
            let local_end = (*span_end).min(end).saturating_sub(start);
            (local_start < local_end).then_some((local_start, local_end))
        })
        .collect();

    let boundaries: Vec<usize> = std::iter::once(0)
        .chain(source.char_indices().map(|(index, _)| index).skip(1))
        .chain(std::iter::once(source.len()))
        .collect();
    let total_chars = boundaries.len().saturating_sub(1);
    if total_chars <= MAX_LINE_CHARS || spans.is_empty() {
        return (source.to_string(), spans);
    }

    let first_start = boundaries
        .partition_point(|boundary| *boundary <= spans[0].0)
        .saturating_sub(1);
    let first_end = boundaries
        .partition_point(|boundary| *boundary < spans[0].1)
        .min(total_chars);
    let mut window_start = first_start.saturating_sub(MAX_LINE_CHARS / 3);
    let mut window_end = (window_start + MAX_LINE_CHARS).min(total_chars);
    if first_end > window_end {
        window_end = first_end.min(total_chars);
        window_start = window_end.saturating_sub(MAX_LINE_CHARS);
    }

    let byte_start = boundaries[window_start];
    let byte_end = boundaries[window_end];
    let excerpt = source[byte_start..byte_end].to_string();
    let adjusted = spans
        .into_iter()
        .filter_map(|(span_start, span_end)| {
            let start = span_start.max(byte_start);
            let end = span_end.min(byte_end);
            (start < end).then_some((start - byte_start, end - byte_start))
        })
        .collect();
    (excerpt, adjusted)
}

/// Splits `text` into plain/highlighted segments using byte-offset spans
/// (rg reports UTF-8 byte offsets, which line up with Rust `str` byte
/// indices — unlike JS string indices, which are UTF-16 code units, so this
/// slicing happens here rather than being handed to the frontend as raw
/// offsets). Callers must pre-adjust `spans` for any leading trim/truncation
/// already applied to `text` — see the comment at the `match` arm above.
fn build_segments(text: &str, spans: &[(usize, usize)]) -> Vec<Segment> {
    if spans.is_empty() {
        return vec![Segment {
            text: text.to_string(),
            hit: false,
        }];
    }

    let len = text.len();
    let mut sorted: Vec<(usize, usize)> = spans
        .iter()
        .map(|&(s, e)| (s.min(len), e.min(len)))
        .filter(|&(s, e)| s < e)
        .collect();
    sorted.sort();

    let mut segments = Vec::new();
    let mut cursor = 0usize;
    for (start, end) in sorted {
        if start < cursor {
            continue; // overlapping submatch; skip
        }
        if start > cursor {
            if let Some(slice) = safe_slice(text, cursor, start) {
                segments.push(Segment {
                    text: slice.to_string(),
                    hit: false,
                });
            }
        }
        if let Some(slice) = safe_slice(text, start, end) {
            segments.push(Segment {
                text: slice.to_string(),
                hit: true,
            });
        }
        cursor = end;
    }
    if cursor < len {
        if let Some(slice) = safe_slice(text, cursor, len) {
            segments.push(Segment {
                text: slice.to_string(),
                hit: false,
            });
        }
    }
    if segments.is_empty() {
        segments.push(Segment {
            text: text.to_string(),
            hit: false,
        });
    }
    segments
}

fn safe_slice(text: &str, start: usize, end: usize) -> Option<&str> {
    text.get(start..end)
}

// -- name matching shared by the filename/folder/combined index scans -------

/// The case/regex matching rules for one search, built once per scan instead
/// of re-derived per entry. Shared by Files, Folders, and Both, which differ
/// only in *what* they feed it (file basenames, directory names, or both).
struct NameMatcher {
    regex: Option<regex::Regex>,
    sensitive: bool,
    pattern: String,
    needle_lower: String,
}

impl NameMatcher {
    fn build(pattern: &str, search_regex: bool, case_mode: &str) -> Result<Self, String> {
        let sensitive = match case_mode {
            "sensitive" => true,
            "insensitive" => false,
            _ => pattern.chars().any(|c| c.is_uppercase()),
        };
        let regex = if search_regex {
            let builder_pattern = if sensitive {
                pattern.to_string()
            } else {
                format!("(?i){pattern}")
            };
            Some(regex::Regex::new(&builder_pattern).map_err(|e| format!("invalid regex: {e}"))?)
        } else {
            None
        };
        Ok(NameMatcher {
            regex,
            sensitive,
            pattern: pattern.to_string(),
            needle_lower: pattern.to_lowercase(),
        })
    }

    fn is_match(&self, name: &str) -> bool {
        if let Some(re) = &self.regex {
            re.is_match(name)
        } else if self.sensitive {
            name.contains(&self.pattern)
        } else {
            name.to_lowercase().contains(&self.needle_lower)
        }
    }

    /// Same as [`Self::is_match`], but takes a basename the file index
    /// already lowercased at build time — avoiding a fresh lowercase alloc
    /// per entry on the (common) non-regex, case-insensitive path.
    fn is_match_cached_lower(&self, name: &str, name_lower: &str) -> bool {
        if let Some(re) = &self.regex {
            re.is_match(name)
        } else if self.sensitive {
            name.contains(&self.pattern)
        } else {
            name_lower.contains(&self.needle_lower)
        }
    }
}

// -- filename search (rg --files index) -------------------------------------

fn run_files_search(
    app: &AppHandle,
    state: &Arc<SearchState>,
    pattern: &str,
    generation: u64,
    search_regex: bool,
    case_mode: &str,
    config: &Config,
) -> Result<(), String> {
    let paths = active_paths(config);
    if paths.is_empty() {
        return Ok(());
    }

    // Validate the query before asking the index to build. A malformed regex
    // must never turn an empty cache into a whole-drive enumeration that can
    // only be abandoned after the error has already reached the UI.
    let matcher = NameMatcher::build(pattern, search_regex, case_mode)?;
    state.index_service.ensure_built(config.clone(), paths);
    // Tag this scan with the build it is about to walk. `index` below is an
    // offset into *that* build's entries, so if the index is dropped and
    // rebuilt under us — which `invalidate()` does on a source toggle,
    // without touching the *search* generation `is_current` checks — the
    // offset is meaningless and the scan has to stop rather than carry on
    // indexing into a Vec that is now shorter than it.
    if !wait_for_index(state, generation) {
        return Ok(());
    }
    let cache_generation = state.file_cache.generation();
    let stable_snapshot = state.file_cache.snapshot();

    let max_results = config.max_results as usize;
    let mut batch: Vec<ResultRow> = Vec::new();
    let mut count = 0usize;
    let mut index = 0usize;
    let mut last_emit = Instant::now();

    loop {
        if !is_current(state, generation) {
            return Ok(());
        }

        // A bounded window per pass instead of one lock acquisition per
        // entry. `FileCache::window` clamps the offset and checks the cache
        // generation under the same lock that guards the entries, so the
        // out-of-range slice this used to do (`entries[index..]` against a
        // Vec a concurrent `invalidate()` had just emptied) can't be written
        // here any more. Entries are `Arc<str>`, so the window is refcount
        // bumps rather than a copy of the index.
        let chunk = match &stable_snapshot {
            Some(snapshot) => {
                if index >= snapshot.len() {
                    break;
                }
                snapshot[index..snapshot.len().min(index.saturating_add(SCAN_WINDOW))].to_vec()
            }
            None => match state
                .file_cache
                .window(cache_generation, index, SCAN_WINDOW)
            {
                Window::Chunk(chunk) => chunk,
                Window::Exhausted => break,
                Window::Superseded => return Ok(()),
            },
        };

        if chunk.is_empty() {
            // Caught up with a build still in flight — wait for more.
            FileCache::poll_wait();
            continue;
        }

        for (path, name_lower) in &chunk {
            if !is_current(state, generation) {
                return Ok(());
            }
            let cut = path.rfind(['\\', '/']).map(|i| i + 1).unwrap_or(0);
            let basename = &path[cut..];

            if matcher.is_match_cached_lower(basename, name_lower) {
                let dir = path[..cut].to_string();
                let basename_segments = highlight_literal_all(basename, pattern);
                batch.push(ResultRow {
                    path: path.to_string(),
                    dir,
                    basename_segments,
                    line: 1,
                    segments: Vec::new(),
                    is_dir: false,
                });
                count += 1;
                if batch.len() >= EMIT_BATCH || last_emit.elapsed() >= EMIT_INTERVAL {
                    if !emit_batch(app, state, generation, &mut batch) {
                        return Ok(());
                    }
                    last_emit = Instant::now();
                }
            }
            index += 1;
            if count >= max_results {
                break;
            }
        }
        if count >= max_results {
            break;
        }
    }

    if !batch.is_empty() {
        emit_batch(app, state, generation, &mut batch);
    }

    // Only surface a build failure (rg missing from PATH) when it is
    // actually why nothing came back.
    if count == 0 {
        if let Some(err) = state.file_cache.error() {
            return Err(err);
        }
    }

    Ok(())
}

// -- folder search (derived from the same `rg --files` index) ---------------

/// Matches against every ancestor directory of each indexed file, rather
/// than running its own walk: `rg --files` never emits directory entries at
/// all, so there is no equivalent "list of folders" to build the way the
/// filename index builds a list of files. Reusing the file index means a
/// folder only surfaces here if it (recursively) contains at least one
/// non-ignored file under an active root — an empty directory, or one
/// containing only excluded entries, won't appear. That matches what this
/// search is actually for (jumping to a folder you have files in) and costs
/// nothing beyond the file index already being built for Files mode.
fn run_folders_search(
    app: &AppHandle,
    state: &Arc<SearchState>,
    pattern: &str,
    generation: u64,
    search_regex: bool,
    case_mode: &str,
    config: &Config,
) -> Result<(), String> {
    let paths = active_paths(config);
    if paths.is_empty() {
        return Ok(());
    }

    let matcher = NameMatcher::build(pattern, search_regex, case_mode)?;
    state.index_service.ensure_built(config.clone(), paths);
    if !wait_for_index(state, generation) {
        return Ok(());
    }
    let cache_generation = state.file_cache.generation();
    let stable_snapshot = state.file_cache.snapshot();

    let max_results = config.max_results as usize;
    let mut batch: Vec<ResultRow> = Vec::new();
    let mut count = 0usize;
    let mut index = 0usize;
    let mut last_emit = Instant::now();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    'outer: loop {
        if !is_current(state, generation) {
            return Ok(());
        }

        let chunk = match &stable_snapshot {
            Some(snapshot) => {
                if index >= snapshot.len() {
                    break;
                }
                snapshot[index..snapshot.len().min(index.saturating_add(SCAN_WINDOW))].to_vec()
            }
            None => match state
                .file_cache
                .window(cache_generation, index, SCAN_WINDOW)
            {
                Window::Chunk(chunk) => chunk,
                Window::Exhausted => break,
                Window::Superseded => return Ok(()),
            },
        };

        if chunk.is_empty() {
            FileCache::poll_wait();
            continue;
        }

        for (path, _) in &chunk {
            if !is_current(state, generation) {
                return Ok(());
            }
            index += 1;

            if !scan_ancestor_dirs(
                app,
                state,
                generation,
                path,
                &matcher,
                &mut seen,
                &mut batch,
                &mut count,
                &mut last_emit,
                max_results,
            ) {
                return Ok(());
            }
            if count >= max_results {
                break 'outer;
            }
        }
    }

    if !batch.is_empty() {
        emit_batch(app, state, generation, &mut batch);
    }

    if count == 0 {
        if let Some(err) = state.file_cache.error() {
            return Err(err);
        }
    }

    Ok(())
}

/// Walks every ancestor directory of `path`, innermost first, matching each
/// directory's own name against `matcher` and pushing/emitting a folder row
/// for each hit — the shared core of Folders mode and the folder half of
/// Both mode.
///
/// Stops the upward walk as soon as a directory is already in `seen`
/// (case-insensitively): every ancestor above it is guaranteed to be in
/// there too, either from this same file's earlier steps or an earlier
/// file's walk up through a shared parent, so there's nothing left to check.
///
/// Returns `false` if the search was superseded mid-emit, in which case the
/// caller must stop (its own `batch`/`count` are no longer worth finishing).
#[allow(clippy::too_many_arguments)]
fn scan_ancestor_dirs(
    app: &AppHandle,
    state: &Arc<SearchState>,
    generation: u64,
    path: &str,
    matcher: &NameMatcher,
    seen: &mut std::collections::HashSet<String>,
    batch: &mut Vec<ResultRow>,
    count: &mut usize,
    last_emit: &mut Instant,
    max_results: usize,
) -> bool {
    let mut rest: &str = path;
    while let Some(cut) = rest.rfind(['\\', '/']) {
        let dir_path = &rest[..cut];
        if dir_path.is_empty() || !seen.insert(dir_path.to_lowercase()) {
            break;
        }
        let name_cut = dir_path.rfind(['\\', '/']).map(|i| i + 1).unwrap_or(0);
        let name = &dir_path[name_cut..];

        if *count >= max_results {
            return true;
        }
        if matcher.is_match(name) {
            let dir = dir_path[..name_cut].to_string();
            let basename_segments = highlight_literal_all(name, &matcher.pattern);
            batch.push(ResultRow {
                path: dir_path.to_string(),
                dir,
                basename_segments,
                line: 1,
                segments: Vec::new(),
                is_dir: true,
            });
            *count += 1;
            if batch.len() >= EMIT_BATCH || last_emit.elapsed() >= EMIT_INTERVAL {
                if !emit_batch(app, state, generation, batch) {
                    return false;
                }
                *last_emit = Instant::now();
            }
        }
        rest = dir_path;
    }
    true
}

// -- combined filename + folder search ---------------------------------------

/// Both mode: for every indexed file, checks the file's own basename *and*
/// walks its ancestor directories — one pass over the index instead of the
/// two full passes running Files and Folders back to back would take.
fn run_names_search(
    app: &AppHandle,
    state: &Arc<SearchState>,
    pattern: &str,
    generation: u64,
    search_regex: bool,
    case_mode: &str,
    config: &Config,
) -> Result<(), String> {
    let paths = active_paths(config);
    if paths.is_empty() {
        return Ok(());
    }

    let matcher = NameMatcher::build(pattern, search_regex, case_mode)?;
    state.index_service.ensure_built(config.clone(), paths);
    if !wait_for_index(state, generation) {
        return Ok(());
    }
    let cache_generation = state.file_cache.generation();
    let stable_snapshot = state.file_cache.snapshot();

    let max_results = config.max_results as usize;
    let mut batch: Vec<ResultRow> = Vec::new();
    let mut count = 0usize;
    let mut index = 0usize;
    let mut last_emit = Instant::now();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    'outer: loop {
        if !is_current(state, generation) {
            return Ok(());
        }

        let chunk = match &stable_snapshot {
            Some(snapshot) => {
                if index >= snapshot.len() {
                    break;
                }
                snapshot[index..snapshot.len().min(index.saturating_add(SCAN_WINDOW))].to_vec()
            }
            None => match state
                .file_cache
                .window(cache_generation, index, SCAN_WINDOW)
            {
                Window::Chunk(chunk) => chunk,
                Window::Exhausted => break,
                Window::Superseded => return Ok(()),
            },
        };

        if chunk.is_empty() {
            FileCache::poll_wait();
            continue;
        }

        for (path, name_lower) in &chunk {
            if !is_current(state, generation) {
                return Ok(());
            }
            index += 1;

            let cut = path.rfind(['\\', '/']).map(|i| i + 1).unwrap_or(0);
            let basename = &path[cut..];

            if matcher.is_match_cached_lower(basename, name_lower) {
                let dir = path[..cut].to_string();
                let basename_segments = highlight_literal_all(basename, &matcher.pattern);
                batch.push(ResultRow {
                    path: path.to_string(),
                    dir,
                    basename_segments,
                    line: 1,
                    segments: Vec::new(),
                    is_dir: false,
                });
                count += 1;
                if batch.len() >= EMIT_BATCH || last_emit.elapsed() >= EMIT_INTERVAL {
                    if !emit_batch(app, state, generation, &mut batch) {
                        return Ok(());
                    }
                    last_emit = Instant::now();
                }
            }

            if !scan_ancestor_dirs(
                app,
                state,
                generation,
                path,
                &matcher,
                &mut seen,
                &mut batch,
                &mut count,
                &mut last_emit,
                max_results,
            ) {
                return Ok(());
            }
            if count >= max_results {
                break 'outer;
            }
        }
    }

    if !batch.is_empty() {
        emit_batch(app, state, generation, &mut batch);
    }

    if count == 0 {
        if let Some(err) = state.file_cache.error() {
            return Err(err);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spawn_dummy() -> Child {
        std::process::Command::new("cmd")
            .args(["/c", "ver"])
            .spawn()
            .expect("spawn dummy child for test")
    }

    #[test]
    fn invalid_filename_regex_is_rejected_before_index_work() {
        let result = NameMatcher::build("abc[", true, "smart");
        assert!(matches!(result, Err(error) if error.starts_with("invalid regex:")));
    }

    #[test]
    fn restricted_content_scan_failures_are_warnings() {
        assert!(is_restricted_scan_failure(
            r"rg: C:\Config.Msi: Access is denied."
        ));
        assert!(is_restricted_scan_failure(
            "thread 'main' panicked at crates\\ignore\\src\\walk.rs"
        ));
        assert!(!is_restricted_scan_failure(
            "regex parse error: unclosed group"
        ));
    }

    #[test]
    fn diagnostic_drain_retains_a_bounded_prefix() {
        let input = vec![b'x'; DIAGNOSTIC_LIMIT + 1024];
        let retained = drain_diagnostics(std::io::Cursor::new(input));
        assert_eq!(retained.len(), DIAGNOSTIC_LIMIT);
    }

    #[test]
    fn active_paths_deduplicates_overlapping_roots_without_breaking_drive_roots() {
        let config = Config {
            paths: vec![
                r"C:\project\".into(),
                r"c:\project\src".into(),
                r"C:\project\".into(),
                r"D:\".into(),
            ],
            disabled_paths: vec![r"d:\".into()],
            ..Config::default()
        };
        let paths = active_paths(&config);
        assert_eq!(paths.len(), 1);
        assert!(paths.contains(&r"C:\project\".to_string()));
    }

    /// Reproduces the exact bug reported against this code: rg reports byte
    /// offsets into the *original* line, but the highlighted text is the
    /// *trimmed* line — without adjusting the offsets by the trimmed
    /// leading-whitespace length, highlighting lands on the wrong substring.
    #[test]
    fn trim_offset_adjustment_matches_original_text_position() {
        let text = "    hello world"; // 4 leading spaces; "hello" is at byte 4 in the original
        let truncated: String = text.chars().take(MAX_LINE_CHARS).collect();
        let trim_offset = truncated.len() - truncated.trim_start().len();
        let trimmed = truncated.trim();
        assert_eq!(trimmed, "hello world");
        assert_eq!(trim_offset, 4);

        let original_span = (4usize, 9usize); // what rg reported, pre-trim
        let adjusted = (
            original_span.0.saturating_sub(trim_offset),
            original_span.1.saturating_sub(trim_offset),
        );
        assert_eq!(&trimmed[adjusted.0..adjusted.1], "hello");
    }

    /// Filename-search results shipped with no highlighting at all — the
    /// original always rubricates every literal, case-insensitive
    /// occurrence of the query within the basename, in both search modes.
    #[test]
    fn highlight_literal_all_marks_every_case_insensitive_occurrence() {
        let segments = highlight_literal_all("QuickSearch_Api.Quicksearch.cs", "quicksearch");
        let hits: Vec<&str> = segments
            .iter()
            .filter(|s| s.hit)
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(hits, vec!["QuickSearch", "Quicksearch"]);
        let rebuilt: String = segments.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(rebuilt, "QuickSearch_Api.Quicksearch.cs"); // segments must reconstruct the original exactly
    }

    /// A match that begins inside the whitespace `trim()` strips must stay on
    /// the characters rg actually matched. `saturating_sub` pinned its start
    /// to 0 and dragged the whole span left by the trim length, so e.g.
    /// `\s*hello` highlighted "hel" instead of "hello".
    #[test]
    fn spans_starting_inside_the_trimmed_run_are_clamped_not_shifted() {
        let text = "    hello world";
        let truncated: String = text.chars().take(MAX_LINE_CHARS).collect();
        let trim_offset = truncated.len() - truncated.trim_start().len();
        let trimmed = truncated.trim();

        // rg matched `\s*hello`: bytes 0..9 of the ORIGINAL line.
        let reported = [(0usize, 9usize)];
        let spans: Vec<(usize, usize)> = reported
            .iter()
            .filter(|(_, end)| *end > trim_offset)
            .map(|(start, end)| ((*start).max(trim_offset) - trim_offset, end - trim_offset))
            .collect();
        assert_eq!(spans, vec![(0, 5)]);

        let segments = build_segments(trimmed, &spans);
        let hits: Vec<&str> = segments
            .iter()
            .filter(|s| s.hit)
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(hits, vec!["hello"]);
    }

    /// A match lying entirely within the trimmed whitespace has nothing left
    /// to mark once the line is trimmed, and must be dropped rather than
    /// collapsed onto position 0.
    #[test]
    fn spans_entirely_inside_the_trimmed_run_are_dropped() {
        let trim_offset = 4usize;
        let reported = [(0usize, 3usize)];
        let spans: Vec<(usize, usize)> = reported
            .iter()
            .filter(|(_, end)| *end > trim_offset)
            .map(|(start, end)| ((*start).max(trim_offset) - trim_offset, end - trim_offset))
            .collect();
        assert!(spans.is_empty());

        let segments = build_segments("hello world", &spans);
        assert_eq!(segments.len(), 1);
        assert!(!segments[0].hit);
        assert_eq!(segments[0].text, "hello world");
    }

    #[test]
    fn content_excerpt_keeps_a_late_match_visible() {
        let text = format!("{}TARGET{}", "a".repeat(420), "b".repeat(120));
        let target_start = 420;
        let (excerpt, spans) = content_excerpt(&text, &[(target_start, target_start + 6)]);
        assert!(excerpt.len() <= MAX_LINE_CHARS);
        let segments = build_segments(&excerpt, &spans);
        assert!(segments
            .iter()
            .any(|segment| segment.hit && segment.text == "TARGET"));
    }

    /// Case folding that changes byte length (U+0130 lowercases to two code
    /// points) used to shift every offset after it, because the search ran
    /// over a lowercased *copy* and the offsets were reused in the original.
    /// Whatever the highlight lands on, the segments must still reconstruct
    /// the input exactly and never split a character.
    #[test]
    fn highlight_survives_case_folding_that_changes_byte_length() {
        for name in [
            "\u{0130}stanbul_notes.txt",
            "K\u{0130}TAP.md",
            "\u{212A}elvin.rs",
        ] {
            for query in ["notes", "tap", "elvin", "i", "k"] {
                let segments = highlight_literal_all(name, query);
                let rebuilt: String = segments.iter().map(|s| s.text.as_str()).collect();
                assert_eq!(
                    rebuilt, name,
                    "segments must reconstruct {name:?} for {query:?}"
                );
            }
        }
    }

    /// The lowercased-copy approach also mis-highlighted plain ASCII sitting
    /// after a character whose lowercase is longer: the offsets drifted by
    /// the difference. Anchoring the scan in the original text fixes it.
    #[test]
    fn highlight_marks_ascii_following_a_growing_character() {
        let segments = highlight_literal_all("\u{0130}_README.md", "readme");
        let hits: Vec<&str> = segments
            .iter()
            .filter(|s| s.hit)
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(hits, vec!["README"]);
    }

    #[test]
    fn highlight_of_a_query_absent_from_the_name_marks_nothing() {
        let segments = highlight_literal_all("main.rs", "zzz");
        assert!(segments.iter().all(|s| !s.hit));
        let rebuilt: String = segments.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(rebuilt, "main.rs");
    }

    #[test]
    fn highlight_marks_back_to_back_occurrences() {
        let segments = highlight_literal_all("abcabc.txt", "abc");
        let hits: Vec<&str> = segments
            .iter()
            .filter(|s| s.hit)
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(hits, vec!["abc", "abc"]);
        let rebuilt: String = segments.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(rebuilt, "abcabc.txt");
    }

    #[test]
    fn build_path_display_splits_dir_from_basename() {
        let (dir, segments) = build_path_display(r"C:\Users\me\QuickSearch_Api.cs", "quicksearch");
        assert_eq!(dir, r"C:\Users\me\");
        let hits: Vec<&str> = segments
            .iter()
            .filter(|s| s.hit)
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(hits, vec!["QuickSearch"]);
    }

    #[test]
    fn build_segments_highlights_the_adjusted_span() {
        let segments = build_segments("hello world", &[(0, 5)]);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].text, "hello");
        assert!(segments[0].hit);
        assert_eq!(segments[1].text, " world");
        assert!(!segments[1].hit);
    }

    /// Reproduces the reported race: search A installs its child, search B
    /// starts and supersedes it (killing A's child, as intended), then A's
    /// worker finally reaches its own cleanup. The old unconditional
    /// "take whatever's in the shared slot" cleanup would grab and kill B's
    /// child here. `reap` must only touch its own generation.
    #[test]
    fn reap_never_touches_a_newer_generations_child() {
        let state = SearchState::new(
            std::sync::Arc::new(crate::index_store::NullCatalogStore),
            None,
        );

        let gen_a = state.begin_generation();
        assert!(state.install(gen_a, spawn_dummy()));

        let gen_b = state.begin_generation(); // supersedes A, kills A's child
        assert!(state.install(gen_b, spawn_dummy()));

        // A's worker, unaware it was superseded, reaches its own cleanup.
        state.reap(gen_a);

        // B's child must still be the one tracked.
        let guard = state.running.lock().unwrap();
        assert!(
            guard.is_some(),
            "reap(gen_a) must not have removed B's child"
        );
        assert_eq!(guard.as_ref().unwrap().generation, gen_b);
    }

    /// Reproduces the other half of the same bug: a stale search's worker
    /// finishes spawning its child *after* a newer search has already
    /// started (and found nothing to kill, since the stale one hadn't
    /// installed yet). `install` must reject it instead of letting a stale
    /// child get tracked as if it were current.
    #[test]
    fn install_rejects_a_stale_generation() {
        let state = SearchState::new(
            std::sync::Arc::new(crate::index_store::NullCatalogStore),
            None,
        );

        let gen_a = state.begin_generation();
        let gen_b = state.begin_generation(); // newer generation already active

        let installed = state.install(gen_a, spawn_dummy());
        assert!(
            !installed,
            "installing a superseded generation's child must be rejected"
        );
        assert!(
            state.running.lock().unwrap().is_none(),
            "the rejected install must not have touched the slot"
        );

        let _ = gen_b; // silence unused warning if generation is otherwise unread
    }
}

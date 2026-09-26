#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod filecache;
mod index_service;
mod index_store;
mod index_watcher;
mod platform;
mod search;

use std::sync::Arc;
use std::time::{Duration, Instant};

use config::Config;
use index_store::{CatalogStore, NullCatalogStore, SqliteCatalogStore};
use platform::window;
use search::SearchState;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Emitter, Manager, RunEvent, WindowEvent};
use tauri_plugin_global_shortcut::{Code, Modifiers, Shortcut};

const WINDOW_LABEL: &str = "main";
const DEFAULT_HOTKEY: &str = "Ctrl+Alt+Space";

/// How long to wait for the frontend to report itself rendered before showing
/// the window anyway. Only reached if `init()` in `app.js` threw before it
/// could call `frontend_ready` — a tray-only app with an invisible window is a
/// far worse failure than one brief flash.
const REVEAL_FALLBACK: Duration = Duration::from_millis(2000);

/// Whether the launch-time reveal has already happened, so the frontend's
/// ready signal and the fallback timer — which race by design — show the
/// window exactly once between them. Without this the loser of that race
/// could re-show a window the user had already dismissed with the hotkey.
static WINDOW_REVEALED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn write_startup_trace(path: Option<&std::path::Path>, started: Instant, stage: &str) {
    let Some(path) = path else { return };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        use std::io::Write;
        let _ = writeln!(
            file,
            "stage={stage} elapsed_ms={:.1}",
            started.elapsed().as_secs_f64() * 1000.0
        );
    }
}

/// Shows the main window for the first time.
///
/// The window is declared `"visible": false` in `tauri.conf.json` and stays
/// hidden through `setup`, which is what stops the user seeing it twice: born
/// at the configured 900x560 default, painting white while WebView2 starts,
/// then jumping to the saved geometry as the UI finally rendered. Now the
/// geometry is applied while it is still hidden and it appears once, already
/// the right size, already painted.
fn reveal_main_window(app: &tauri::AppHandle) {
    use std::sync::atomic::Ordering;

    if WINDOW_REVEALED.swap(true, Ordering::SeqCst) {
        return;
    }
    if let Some(window) = app.get_webview_window(WINDOW_LABEL) {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// Ports `parse_hotkey()`: "Ctrl+Alt+Space" -> a `Shortcut`, or `None` if the
/// spec names no modifier, names an unknown key, or is otherwise malformed —
/// callers fall back to [`DEFAULT_HOTKEY`] rather than registering nothing.
fn parse_hotkey(spec: &str) -> Option<Shortcut> {
    let parts: Vec<String> = spec
        .split('+')
        .map(|p| p.trim().to_lowercase())
        .filter(|p| !p.is_empty())
        .collect();
    if parts.len() < 2 {
        return None;
    }
    let (key_name, mod_names) = parts.split_last()?;

    let mut mods = Modifiers::empty();
    for name in mod_names {
        mods |= match name.as_str() {
            "ctrl" | "control" => Modifiers::CONTROL,
            "alt" => Modifiers::ALT,
            "shift" => Modifiers::SHIFT,
            "win" | "windows" | "super" => Modifiers::SUPER,
            _ => return None,
        };
    }
    if mods.is_empty() {
        return None;
    }

    let code = named_key_code(key_name).or_else(|| single_char_code(key_name))?;
    Some(Shortcut::new(Some(mods), code))
}

fn named_key_code(name: &str) -> Option<Code> {
    Some(match name {
        "space" => Code::Space,
        "tab" => Code::Tab,
        "enter" | "return" => Code::Enter,
        "esc" | "escape" => Code::Escape,
        "insert" => Code::Insert,
        "delete" => Code::Delete,
        "home" => Code::Home,
        "end" => Code::End,
        "pageup" => Code::PageUp,
        "pagedown" => Code::PageDown,
        "up" => Code::ArrowUp,
        "down" => Code::ArrowDown,
        "left" => Code::ArrowLeft,
        "right" => Code::ArrowRight,
        "f1" => Code::F1,
        "f2" => Code::F2,
        "f3" => Code::F3,
        "f4" => Code::F4,
        "f5" => Code::F5,
        "f6" => Code::F6,
        "f7" => Code::F7,
        "f8" => Code::F8,
        "f9" => Code::F9,
        "f10" => Code::F10,
        "f11" => Code::F11,
        "f12" => Code::F12,
        _ => return None,
    })
}

fn single_char_code(name: &str) -> Option<Code> {
    if name.len() != 1 {
        return None;
    }
    let c = name.chars().next()?;
    if c.is_ascii_alphabetic() {
        Some(match c.to_ascii_uppercase() {
            'A' => Code::KeyA,
            'B' => Code::KeyB,
            'C' => Code::KeyC,
            'D' => Code::KeyD,
            'E' => Code::KeyE,
            'F' => Code::KeyF,
            'G' => Code::KeyG,
            'H' => Code::KeyH,
            'I' => Code::KeyI,
            'J' => Code::KeyJ,
            'K' => Code::KeyK,
            'L' => Code::KeyL,
            'M' => Code::KeyM,
            'N' => Code::KeyN,
            'O' => Code::KeyO,
            'P' => Code::KeyP,
            'Q' => Code::KeyQ,
            'R' => Code::KeyR,
            'S' => Code::KeyS,
            'T' => Code::KeyT,
            'U' => Code::KeyU,
            'V' => Code::KeyV,
            'W' => Code::KeyW,
            'X' => Code::KeyX,
            'Y' => Code::KeyY,
            'Z' => Code::KeyZ,
            _ => return None,
        })
    } else if c.is_ascii_digit() {
        Some(match c {
            '0' => Code::Digit0,
            '1' => Code::Digit1,
            '2' => Code::Digit2,
            '3' => Code::Digit3,
            '4' => Code::Digit4,
            '5' => Code::Digit5,
            '6' => Code::Digit6,
            '7' => Code::Digit7,
            '8' => Code::Digit8,
            '9' => Code::Digit9,
            _ => None?,
        })
    } else {
        None
    }
}

#[tauri::command]
fn get_config() -> Config {
    config::load()
}

#[derive(serde::Deserialize)]
struct RuntimeState {
    disabled_paths: Option<Vec<String>>,
    zoom: Option<f64>,
    divider_ratio: Option<f64>,
    search_regex: Option<bool>,
    case_mode: Option<String>,
    content_search_follow_mounts: Option<bool>,
}

#[tauri::command]
fn save_runtime_state(patch: RuntimeState) -> Result<Config, String> {
    // One `config::update` rather than load + mutate + save: the debounced
    // zoom and divider saves, a source toggle, and the window-geometry
    // watcher all land on this file from different threads, and only an
    // update that holds the lock across the whole read-modify-write stops
    // the last writer silently reverting the others' fields.
    config::try_update(|cfg| {
        if let Some(v) = patch.disabled_paths {
            cfg.disabled_paths = v;
        }
        if let Some(v) = patch.zoom {
            cfg.zoom = v;
        }
        if let Some(v) = patch.divider_ratio {
            cfg.divider_ratio = v;
        }
        if let Some(v) = patch.search_regex {
            cfg.search_regex = v;
        }
        if let Some(v) = patch.case_mode {
            cfg.case_mode = v;
        }
        if let Some(v) = patch.content_search_follow_mounts {
            cfg.content_search_follow_mounts = v;
        }
    })
}

#[tauri::command]
fn search(
    app: tauri::AppHandle,
    state: tauri::State<Arc<SearchState>>,
    pattern: String,
    mode: String,
    search_regex: bool,
    case_mode: String,
) -> u64 {
    let cfg = config::load();
    search::start(
        app,
        state.inner().clone(),
        pattern,
        mode,
        search_regex,
        case_mode,
        cfg,
    )
}

#[tauri::command]
fn cancel_search(state: tauri::State<Arc<SearchState>>) {
    state.cancel_search_only();
}

#[tauri::command]
fn invalidate_file_index(app: tauri::AppHandle, state: tauri::State<Arc<SearchState>>) {
    // Invalidation is queued with the same owner as watcher changes and
    // catalog loads. The service advances its epoch first, then processes the
    // cache reset before the load request below, so stale commands cannot
    // repopulate the old configuration.
    state.index_service.reconfigure();

    // Source/exclusion changes also change which filesystem events are valid.
    // Replace the watcher before the next build so an old root cannot keep
    // feeding events into the newly-invalidated cache.
    let config = config::load();
    let active_paths = search::active_paths(&config);
    if !active_paths.is_empty() {
        state
            .index_service
            .load_persisted(config.clone(), active_paths.clone());
    }
    if let Some(watcher_slot) =
        app.try_state::<std::sync::Mutex<Option<index_watcher::IndexWatcher>>>()
    {
        let mut watcher_guard = watcher_slot.lock().unwrap();
        let watcher = if active_paths.is_empty() {
            None
        } else {
            index_watcher::IndexWatcher::start(
                &active_paths,
                config.exclude_dirs.clone(),
                state.index_service.clone(),
                state.watcher_live.clone(),
            )
        };
        *watcher_guard = watcher;
    }
}

/// Cancels whatever's running, then hides the window — the single choke
/// point for every dismiss path that originates in the frontend (Escape,
/// opening a result), matching quicksearch's `hide()`. The title-bar
/// close/Alt+F4 path and the global hotkey's hide direction go through Rust
/// window events instead (see `on_window_event`/the hotkey callback in
/// `main()`), since they never reach the webview to call a command.
#[tauri::command]
fn hide_window(app: tauri::AppHandle, state: tauri::State<Arc<SearchState>>) {
    state.cancel();
    if let Some(window) = app.get_webview_window(WINDOW_LABEL) {
        let _ = window.hide();
    }
}

fn shutdown_indexing(app: &tauri::AppHandle) {
    // Drop the OS watcher first so its coalescing thread releases its sender
    // and cannot enqueue more work while the index worker is stopping.
    if let Some(watcher_slot) =
        app.try_state::<std::sync::Mutex<Option<index_watcher::IndexWatcher>>>()
    {
        watcher_slot.lock().unwrap().take();
    }
    if let Some(state) = app.try_state::<Arc<SearchState>>() {
        state.index_service.shutdown();
    }
}

#[derive(serde::Serialize)]
struct IndexStatus {
    state: String, // "cold" | "building" | "ready"
    count: usize,
    /// Whether a ready index is currently being refreshed in the background.
    refreshing: bool,
    /// Whether a filesystem watcher is actually live for at least one active
    /// root, vs. the index only ever catching up on the periodic refresh —
    /// the colophon's index dot reads this rather than assuming.
    watched: bool,
    saved: bool,
    error: Option<String>,
}

#[tauri::command]
fn get_index_status(state: tauri::State<Arc<SearchState>>) -> IndexStatus {
    // One lock for both readings — taken separately they could report a
    // count from a different build than the state they're labelled with.
    let (cache_state, count) = state.file_cache.status();
    let state_name = match cache_state {
        filecache::CacheState::Empty => "cold",
        // Reads as "building" to the frontend on purpose: from the user's
        // side both mean "results are on their way, keep waiting", and this
        // keeps the existing three-word vocabulary the colophon renders.
        filecache::CacheState::Loading | filecache::CacheState::Building => "building",
        filecache::CacheState::Ready => "ready",
    };
    IndexStatus {
        state: state_name.to_string(),
        count,
        refreshing: state.file_cache.refresh_is_active(),
        watched: state
            .watcher_live
            .load(std::sync::atomic::Ordering::Relaxed),
        saved: state.file_cache.persistence_error().is_none(),
        error: state.file_cache.persistence_error(),
    }
}

/// Called by `app.js` once the first render is complete. That — not
/// `setup` finishing, and not the webview's own load event — is the earliest
/// moment there is something worth looking at behind the window.
#[tauri::command]
fn frontend_ready(app: tauri::AppHandle) {
    reveal_main_window(&app);
}

#[tauri::command]
fn open_result(path: String, line: u64) {
    let cfg = config::load();
    let mut opened = false;

    if !cfg.editor_command.is_empty() {
        let cmd_parts: Vec<String> = cfg
            .editor_command
            .iter()
            .map(|part| {
                part.replace("{file}", &path)
                    .replace("{line}", &line.to_string())
            })
            .collect();
        if let [program, args @ ..] = cmd_parts.as_slice() {
            opened = platform::subprocess::no_window_command(program)
                .args(args)
                .spawn()
                .is_ok();
        }
    }

    if !opened {
        let _ = platform::subprocess::no_window_command("cmd")
            .args(["/C", "start", "", &path])
            .spawn();
    }
}

#[tauri::command]
fn open_result_location(path: String) -> Result<(), String> {
    use std::os::windows::process::CommandExt;

    if !std::path::Path::new(&path).exists() {
        return Err(format!("File no longer exists: {path}"));
    }

    // explorer.exe's own hand-rolled `/select` parser only recognizes the
    // switch when it's OUTSIDE quotes — a path containing a space would get
    // wrapped as one fully-quoted token by Rust's normal `.arg()` escaping,
    // which explorer then silently ignores (opens its default location
    // instead of erroring). `raw_arg` bypasses that escaping so the command
    // line is built exactly as the original app did with a literal string:
    // `/select,"<path>"` — the switch unquoted, only the path quoted.
    platform::subprocess::no_window_command("explorer")
        .raw_arg(format!("/select,\"{path}\""))
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("Failed to open Explorer: {e}"))
}

/// Puts `path` on the clipboard as a real file (Windows `CF_HDROP`), the
/// same mechanism Explorer's own right-click Copy/Cut use — pasting
/// elsewhere copies or moves the actual file, not just its path text.
#[tauri::command]
fn copy_result(path: String, cut: bool) -> Result<(), String> {
    platform::clipboard::put_files(&[path], cut).map_err(|e| e.to_string())
}

/// Wraps `window::toggle_main_window` so the tray menu and the global hotkey
/// — the two toggle paths that never reach the webview to call `hide_window`
/// — also cancel a running search/index build on the hide direction.
fn toggle_with_cancel(app: &tauri::AppHandle) {
    // A hotkey or tray toggle arriving before the launch reveal takes it
    // over: the user has now said explicitly what they want the window to
    // do, and the fallback timer must not undo it a moment later.
    WINDOW_REVEALED.store(true, std::sync::atomic::Ordering::SeqCst);
    let did_hide = window::toggle_main_window(app, WINDOW_LABEL);
    if did_hide {
        if let Some(state) = app.try_state::<Arc<SearchState>>() {
            state.cancel();
        }
    }
}

/// Opens the persistent filename catalog at
/// `app_data_dir()/filename-index.sqlite3` — deliberately not beside
/// `config.json`, which per the compatibility constraint must stay exactly
/// where quicksearch's Python original puts it (next to the binary).
/// Falls back to a no-op store (session-only, matching pre-persistence
/// behavior) if the user disabled persistence or the database couldn't be
/// opened — a locked/unwritable app-data directory must not stop the app
/// from launching.
fn open_catalog_store(app: &tauri::AppHandle, persist: bool) -> Arc<dyn CatalogStore> {
    if !persist {
        return Arc::new(NullCatalogStore);
    }
    let data_dir = match app.path().app_data_dir() {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("[quicksearch] no app data dir, filename index is session-only: {e}");
            return Arc::new(NullCatalogStore);
        }
    };
    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        eprintln!(
            "[quicksearch] could not create app data dir, filename index is session-only: {e}"
        );
        return Arc::new(NullCatalogStore);
    }
    match SqliteCatalogStore::open(&data_dir.join("filename-index.sqlite3")) {
        Ok(store) => Arc::new(store),
        Err(e) => {
            eprintln!("[quicksearch] could not open filename index database, falling back to session-only: {e}");
            Arc::new(NullCatalogStore)
        }
    }
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(main_window) = app.get_webview_window(WINDOW_LABEL) {
                window::show_and_focus(&main_window);
            }
        }))
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            get_config,
            save_runtime_state,
            search,
            cancel_search,
            invalidate_file_index,
            hide_window,
            get_index_status,
            frontend_ready,
            open_result,
            open_result_location,
            copy_result,
        ])
        .setup(|app| {
            let startup_started = Instant::now();
            let startup_trace = std::env::var_os("QUICKSEARCH_STARTUP_TRACE").map(|value| {
                let value = value.to_string_lossy();
                if value == "1" {
                    std::env::temp_dir().join(format!(
                        "quicksearch-startup-trace-{}.log",
                        std::process::id()
                    ))
                } else {
                    std::path::PathBuf::from(value.as_ref())
                }
            });
            let handle = app.handle().clone();
            let cfg = config::load();
            write_startup_trace(startup_trace.as_deref(), startup_started, "config_loaded");

            let store = open_catalog_store(&handle, cfg.filename_index_persist);
            write_startup_trace(startup_trace.as_deref(), startup_started, "catalog_opened");

            // Purely a UI signal (the colophon's activity dot) — never on the
            // path anything correctness-sensitive depends on, so a webview
            // that's gone (window closed, event channel torn down mid-quit)
            // just silently drops it rather than needing to be handled here.
            let activity_handle = handle.clone();
            let on_activity: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(move |kind: &str| {
                let _ = activity_handle.emit("qs-index-activity", kind);
            });

            let search_state = Arc::new(SearchState::new(store, Some(on_activity)));
            app.manage(search_state.clone());
            write_startup_trace(startup_trace.as_deref(), startup_started, "state_ready");

            let active_paths = search::active_paths(&cfg);
            if !active_paths.is_empty() {
                // Fast path: try the persisted catalog before anything ever
                // spawns `rg --files`. A miss/stale identity just leaves the
                // cache `Empty`, same as a first run.
                search_state
                    .index_service
                    .load_persisted(cfg.clone(), active_paths.clone());
                write_startup_trace(
                    startup_trace.as_deref(),
                    startup_started,
                    "catalog_load_queued",
                );

                // Real-time updates for as long as the watch stays alive —
                // held in managed state so it isn't dropped (and stopped)
                // the moment `setup` returns.
                let watcher = index_watcher::IndexWatcher::start(
                    &active_paths,
                    cfg.exclude_dirs.clone(),
                    search_state.index_service.clone(),
                    search_state.watcher_live.clone(),
                );
                if watcher.is_none() {
                    eprintln!(
                        "[quicksearch] no filesystem watcher could be started; filename index relies on the periodic refresh only"
                    );
                }
                app.manage(std::sync::Mutex::new(watcher));
            } else {
                app.manage(std::sync::Mutex::new(None::<index_watcher::IndexWatcher>));
            }
            // Bounded-staleness backstop underneath the watcher above: a
            // root the watcher couldn't register for, or a dropped/
            // overflowed notification, otherwise leaves the index stale for
            // as long as the app keeps running.
            let refresh_minutes = cfg.filename_index_refresh_minutes.clamp(1, 10_080);
            search_state
                .index_service
                .spawn_periodic_refresh(
                    Duration::from_secs(refresh_minutes as u64 * 60),
                    search_state.watcher_live.clone(),
                );

            let shortcut = parse_hotkey(&cfg.hotkey)
                .or_else(|| parse_hotkey(DEFAULT_HOTKEY))
                .expect("default hotkey always parses");
            if let Err(e) = window::register_toggle_hotkey(&handle, shortcut, |app| {
                toggle_with_cancel(app);
            }) {
                // Hotkey registration is a convenience feature (another instance,
                // or the original Python quicksearch, may already hold it) — the app
                // stays fully usable via the tray icon either way.
                eprintln!(
                    "[quicksearch] hotkey registration failed, tray-only for this session: {e}"
                );
            }

            let show_hide = MenuItem::with_id(app, "show_hide", "Show/Hide", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show_hide, &quit])?;
            let mut tray = TrayIconBuilder::with_id("main")
                .tooltip("QuickSearch")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "show_hide" => toggle_with_cancel(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        toggle_with_cancel(tray.app_handle());
                    }
                });
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            tray.build(app)?;

            if let Some(main_window) = app.get_webview_window(WINDOW_LABEL) {
                if let (Some(w), Some(h)) = (cfg.window_w, cfg.window_h) {
                    let geometry = window::Geometry {
                        x: cfg.window_x.unwrap_or(0),
                        y: cfg.window_y.unwrap_or(0),
                        width: w,
                        height: h,
                    };
                    window::restore(&main_window, geometry);
                }

                // Per the hard config-compatibility constraint, geometry is written
                // into config.json's existing window_w/h/x/y keys, not a separate file.
                // Only now, with the saved geometry already applied to a
                // still-hidden window, is it safe to let anything show it.
                let fallback_handle = handle.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(REVEAL_FALLBACK);
                    reveal_main_window(&fallback_handle);
                });

                window::watch(&main_window, |geometry| {
                    // Atomic read-modify-write — this fires from the geometry
                    // watcher's own thread, concurrently with whatever
                    // `save_runtime_state` the frontend is debouncing.
                    config::update(|cfg| {
                        cfg.window_x = Some(geometry.x);
                        cfg.window_y = Some(geometry.y);
                        cfg.window_w = Some(geometry.width);
                        cfg.window_h = Some(geometry.height);
                    });
                });
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                // The title bar's close button and Alt+F4 land here directly,
                // never through the `hide_window` command — cancel the same
                // way that path does, or this is the orphaned-rg bug again.
                if let Some(state) = window.try_state::<Arc<SearchState>>() {
                    state.cancel();
                }
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| match event {
            RunEvent::ExitRequested { .. } | RunEvent::Exit => shutdown_indexing(app),
            _ => {}
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test for the exact bug reported live: a path containing a
    /// space (e.g. "OneDrive - Contoso") made explorer.exe silently open
    /// its default location instead of selecting the file, because
    /// `Command::arg` auto-quotes the whole `/select,<path>` token when it
    /// contains a space — explorer's own `/select` parser only recognizes
    /// the switch outside quotes. Verifies the raw command line explorer
    /// actually receives, via a batch script that echoes it back, rather
    /// than trusting the string construction alone.
    #[test]
    fn open_result_location_command_line_is_unescaped_correctly() {
        use std::os::windows::process::CommandExt;

        let dir = std::env::temp_dir().join("qs_test_open_result_location");
        std::fs::create_dir_all(&dir).unwrap();
        let echo_bat = dir.join("echoargs.bat");
        let output_file = dir.join("output.txt");
        std::fs::write(
            &echo_bat,
            format!("@echo %* > \"{}\"\r\n", output_file.display()),
        )
        .unwrap();

        let path = r"C:\Users\someone\OneDrive - Company\a file with spaces.txt";
        let mut cmd = std::process::Command::new(&echo_bat);
        cmd.raw_arg(format!("/select,\"{path}\""));
        cmd.status().unwrap();

        let output = std::fs::read_to_string(&output_file).unwrap();
        assert_eq!(output.trim(), format!("/select,\"{path}\""));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parses_default_hotkey() {
        let shortcut = parse_hotkey("Ctrl+Alt+Space").expect("should parse");
        assert_eq!(
            shortcut,
            Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::Space)
        );
    }

    #[test]
    fn parses_single_letter_key() {
        let shortcut = parse_hotkey("ctrl+alt+j").expect("should parse");
        assert_eq!(
            shortcut,
            Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyJ)
        );
    }

    #[test]
    fn parses_function_key_with_shift() {
        let shortcut = parse_hotkey("Shift+F5").expect("should parse");
        assert_eq!(shortcut, Shortcut::new(Some(Modifiers::SHIFT), Code::F5));
    }

    #[test]
    fn rejects_missing_modifier() {
        assert!(parse_hotkey("Space").is_none());
    }

    #[test]
    fn rejects_unknown_key() {
        assert!(parse_hotkey("Ctrl+Alt+Frobnicate").is_none());
    }

    #[test]
    fn rejects_unknown_modifier() {
        assert!(parse_hotkey("Meta+Space").is_none());
    }

    #[test]
    fn rejects_empty_spec() {
        assert!(parse_hotkey("").is_none());
    }
}

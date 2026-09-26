//! `config.json` load/save, porting quicksearch's `DEFAULT_CONFIG`/`load_config`.
//!
//! Hard compatibility constraint: same keys, same
//! file, same location (next to the binary, not app-data-dir) — an existing
//! quicksearch `config.json` must load here unchanged, and this app must
//! never drop fields it doesn't itself use (`extra`, flattened, round-trips
//! anything from an older/newer config shape it doesn't otherwise know about).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::SystemTime;

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Config {
    pub paths: Vec<String>,
    pub max_results: u32,
    pub rg_extra_args: Vec<String>,
    pub max_per_file: u32,
    pub editor_command: Vec<String>,
    pub disabled_paths: Vec<String>,
    pub zoom: f64,
    pub hotkey: String,
    pub window_w: Option<u32>,
    pub window_h: Option<u32>,
    pub window_x: Option<i32>,
    pub window_y: Option<i32>,
    pub divider_ratio: f64,
    pub search_regex: bool,
    pub case_mode: String,
    pub exclude_dirs: Vec<String>,
    /// Whether the filename index survives an app restart as a SQLite
    /// catalog, instead of rebuilding from a fresh `rg --files` walk every
    /// time. Additive and defaulted `true` so an old `config.json` without
    /// this key round-trips into the new, faster behavior rather than the
    /// old one.
    pub filename_index_persist: bool,
    /// How often (in minutes) a `Ready` filename index is thrown away and
    /// rebuilt in the background, bounding how stale it can get between
    /// restarts absent a filesystem watcher. Clamped to `1..=10080` at its
    /// use site, not here, so a malformed hand-edited value can't disable
    /// reconciliation entirely or spin a busy loop.
    pub filename_index_refresh_minutes: u32,
    /// Makes broad content searches gentler on the disk: the frontend waits
    /// longer between keystrokes and ripgrep is limited to one worker thread.
    /// Filename/index searches keep their existing fast path.
    pub content_search_quiet: bool,
    /// Debounce for content searches in milliseconds. Clamped by the caller
    /// so hand-edited values cannot create a busy loop or an unusable delay.
    pub content_search_debounce_ms: u32,
    /// Whether content searches may follow junctions, symlinks, and mounted
    /// folders. The safe default keeps broad drive walks on the configured
    /// filesystem; users who intentionally search mounted trees can opt in.
    pub content_search_follow_mounts: bool,
    /// Any keys this struct doesn't model, preserved verbatim on save.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            // Whole-drive defaults rather than just the home directory: the
            // point of the tool is finding things anywhere, and `exclude_dirs`
            // below is what keeps a C:\ walk tractable.
            paths: vec!["C:\\".to_string(), "D:\\".to_string()],
            max_results: 30,
            rg_extra_args: vec![
                "--smart-case".to_string(),
                "--hidden".to_string(),
                // Dotfiles and dot-directories are worth searching; multi-
                // gigabyte VM images, ISOs and database files are not, and
                // reading them is what makes an otherwise fast scan crawl.
                "--max-filesize".to_string(),
                "50M".to_string(),
            ],
            max_per_file: 5,
            editor_command: vec![
                "code".to_string(),
                "-g".to_string(),
                "{file}:{line}".to_string(),
            ],
            disabled_paths: Vec::new(),
            zoom: 1.4,
            hotkey: "Ctrl+Alt+Space".to_string(),
            window_w: None,
            window_h: None,
            window_x: None,
            window_y: None,
            divider_ratio: 0.63,
            search_regex: false,
            case_mode: "smart".to_string(),
            exclude_dirs: vec![
                "node_modules",
                "bower_components",
                "vendor",
                "bin",
                "obj",
                "dist",
                "build",
                "target",
                "out",
                ".git",
                ".svn",
                ".hg",
                "__pycache__",
                ".venv",
                "venv",
                ".mypy_cache",
                ".pytest_cache",
                ".tox",
                ".next",
                ".nuxt",
                ".cache",
                ".gradle",
                ".idea",
                ".vs",
                "System32",
                "SysWOW64",
                "WinSxS",
                "Windows.old",
                "$Recycle.Bin",
                "DriverStore",
                "Temp",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            filename_index_persist: true,
            filename_index_refresh_minutes: 60,
            content_search_quiet: true,
            content_search_debounce_ms: 450,
            content_search_follow_mounts: false,
            extra: Map::new(),
        }
    }
}

/// Config lives next to the running binary, matching quicksearch's
/// `SCRIPT_DIR`-relative `config.json` (not an app-data-dir convention).
fn config_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("config.json")))
        .unwrap_or_else(|| PathBuf::from("config.json"))
}

/// Owns the last successfully loaded configuration as well as its file stamp.
/// Tests use their own instance and path; they never touch the installed config.
struct ConfigFile {
    path: PathBuf,
    cached: Option<(Stamp, Config)>,
    last_good: Option<Config>,
}

type Stamp = (Option<SystemTime>, u64);

fn stamp_of(path: &PathBuf) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok(), meta.len()))
}

impl ConfigFile {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            cached: None,
            last_good: None,
        }
    }

    fn read(&mut self) -> Result<Config, String> {
        self.read_with(|| {})
    }

    fn read_with(&mut self, after_read: impl FnOnce()) -> Result<Config, String> {
        let before = stamp_of(&self.path);
        if let Some((stamp, config)) = &self.cached {
            if Some(*stamp) == before {
                return Ok(config.clone());
            }
        }
        let content = match std::fs::read_to_string(&self.path) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // A removed/replaced config is not a new installation.
                if self.last_good.is_some() || before.is_some() {
                    return Err(
                        "Configuration file is missing; keeping the last valid settings".into(),
                    );
                }
                let config = Config::default();
                self.save(&config)?;
                return Ok(config);
            }
            Err(e) => return Err(format!("Cannot read configuration: {e}")),
        };
        after_read();
        let after = stamp_of(&self.path);
        if before != after {
            self.cached = None;
            return Err("Configuration changed while reading; retry the operation".into());
        }
        // serde(default) already supplies missing keys and flatten preserves
        // unknown ones. A type error must never become writable defaults.
        let value: Value = serde_json::from_str(&content)
            .map_err(|e| format!("Invalid configuration; file was not changed: {e}"))?;
        if !value.is_object() {
            return Err(
                "Invalid configuration: expected a JSON object; file was not changed".into(),
            );
        }
        let config: Config = serde_json::from_value(value)
            .map_err(|e| format!("Invalid configuration; file was not changed: {e}"))?;
        self.cached = before.map(|stamp| (stamp, config.clone()));
        self.last_good = Some(config.clone());
        Ok(config)
    }

    fn fallback(&self) -> Config {
        self.last_good.clone().unwrap_or_else(|| {
            // An unreadable existing config must not unexpectedly broaden a
            // search to two whole drives. Keep the UI usable until repaired.
            Config {
                paths: Vec::new(),
                ..Config::default()
            }
        })
    }

    fn update(&mut self, mutate: impl FnOnce(&mut Config)) -> Result<Config, String> {
        let mut config = self.read()?;
        mutate(&mut config);
        self.save(&config)?;
        Ok(config)
    }

    fn save(&mut self, config: &Config) -> Result<(), String> {
        let json = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
        let temporary = self.path.with_extension("json.tmp");
        std::fs::write(&temporary, json).map_err(|e| format!("Cannot save configuration: {e}"))?;
        if let Err(e) = std::fs::rename(&temporary, &self.path) {
            let _ = std::fs::remove_file(&temporary);
            return Err(format!("Cannot replace configuration: {e}"));
        }
        // An external writer may replace the file immediately after rename.
        // Do not associate our bytes with a stamp read from their replacement.
        self.cached = None;
        self.last_good = Some(config.clone());
        Ok(())
    }
}

static FILE: Mutex<Option<ConfigFile>> = Mutex::new(None);

fn with_file<T>(f: impl FnOnce(&mut ConfigFile) -> T) -> T {
    let mut guard = FILE.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(|| ConfigFile::new(config_path())))
}

pub fn load() -> Config {
    with_file(|file| match file.read() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("[quicksearch] {e}");
            file.fallback()
        }
    })
}

pub fn try_update(mutate: impl FnOnce(&mut Config)) -> Result<Config, String> {
    with_file(|file| file.update(mutate))
}

/// Geometry persistence cannot return an IPC error, but still keeps the last
/// valid state and reports failed saves. Interactive saves use try_update.
pub fn update(mutate: impl FnOnce(&mut Config)) -> Config {
    with_file(|file| match file.update(mutate) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("[quicksearch] {e}");
            file.fallback()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_round_trips_through_json() {
        let default = Config::default();
        let json = serde_json::to_string(&default).unwrap();
        let parsed: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.hotkey, "Ctrl+Alt+Space");
        assert_eq!(parsed.max_results, 30);
        assert_eq!(parsed.case_mode, "smart");
    }

    /// Pins the shipped defaults. These are a deliberate set, not incidental
    /// values, so a drift here should be a visible decision rather than a
    /// quiet edit.
    ///
    /// The display fields are deliberately absent: `window_*` stays `None` and
    /// `divider_ratio` keeps its own default so a fresh install sizes and
    /// positions itself, instead of inheriting one machine's saved geometry.
    #[test]
    fn shipped_defaults_are_the_intended_set() {
        let config = Config::default();

        assert_eq!(config.paths, vec!["C:\\", "D:\\"]);
        assert_eq!(
            config.rg_extra_args,
            vec!["--smart-case", "--hidden", "--max-filesize", "50M"]
        );
        assert_eq!(config.zoom, 1.4);
        assert_eq!(config.max_results, 30);
        assert_eq!(config.max_per_file, 5);
        assert_eq!(config.hotkey, "Ctrl+Alt+Space");
        assert_eq!(config.case_mode, "smart");
        assert!(!config.search_regex);
        assert!(!config.content_search_follow_mounts);
        assert!(config.exclude_dirs.contains(&"$Recycle.Bin".to_string()));
        assert!(config.exclude_dirs.contains(&"node_modules".to_string()));

        assert_eq!(config.window_w, None);
        assert_eq!(config.window_h, None);
        assert_eq!(config.window_x, None);
        assert_eq!(config.window_y, None);
        assert_eq!(config.divider_ratio, 0.63);
    }

    #[test]
    fn filename_index_settings_default_without_changing_old_config_files() {
        let config: Config = serde_json::from_str(r#"{"paths":["C:\\"]}"#).unwrap();
        assert!(config.filename_index_persist);
        assert_eq!(config.filename_index_refresh_minutes, 60);
    }

    #[test]
    fn missing_keys_fall_back_to_defaults_like_the_python_merge() {
        // Simulates an old config.json that predates the `hotkey` key.
        let partial = r#"{"paths": ["C:\\Users\\someone"], "max_results": 50}"#;
        let value: Value = serde_json::from_str(partial).unwrap();
        let default_value = serde_json::to_value(Config::default()).unwrap();
        let merged =
            if let (Value::Object(mut base), Value::Object(overlay)) = (default_value, value) {
                for (k, v) in overlay {
                    base.insert(k, v);
                }
                Value::Object(base)
            } else {
                unreachable!()
            };
        let config: Config = serde_json::from_value(merged).unwrap();
        assert_eq!(config.max_results, 50);
        assert_eq!(config.paths, vec!["C:\\Users\\someone".to_string()]);
        assert_eq!(config.hotkey, "Ctrl+Alt+Space"); // fell back to default
        assert_eq!(config.case_mode, "smart"); // fell back to default
    }

    /// A truncated file — what a `load` racing the old truncate-then-write
    /// `save` used to read — must be reported as unparseable, not silently
    /// handed back as defaults. Feeding those defaults into a read-modify-
    /// write is what overwrote the user's whole config.
    #[test]
    fn a_torn_or_corrupt_file_is_reported_unparseable_not_defaulted_silently() {
        // The tail of a pretty-printed config, as a truncated read would see.
        for corrupt in ["", "{\"paths\": [\"C:", "{\"max_results\": }"] {
            let parsed = serde_json::from_str::<Value>(corrupt);
            assert!(
                parsed.is_err(),
                "{corrupt:?} must not parse — otherwise `update` would write over it"
            );
        }
    }

    /// `read_locked`'s first-run branch turns entirely on this distinction:
    /// only `NotFound` means "no config yet". A file that exists but fails to
    /// read must report something else, or the branch would save defaults
    /// straight over settings that are still perfectly intact.
    ///
    /// Non-UTF-8 content is the deterministic case — saving `config.json` from
    /// Notepad as "Unicode" produces exactly this — and needs no race to hit.
    #[test]
    fn an_unreadable_existing_file_does_not_look_like_a_missing_one() {
        let mut path = std::env::temp_dir();
        path.push(format!("qs-config-unreadable-{}.json", std::process::id()));
        // UTF-16LE BOM + `{`: a real file on disk, not valid UTF-8.
        std::fs::write(&path, [0xFFu8, 0xFE, 0x7B, 0x00]).expect("write test file");

        let err = std::fs::read_to_string(&path).expect_err("must not decode as UTF-8");
        assert_ne!(
            err.kind(),
            std::io::ErrorKind::NotFound,
            "an existing but unreadable config must not take the first-run branch"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// The complement: a genuinely absent file must still be `NotFound`, so
    /// first run keeps writing the defaults out.
    #[test]
    fn a_missing_file_is_reported_not_found_so_first_run_still_writes_defaults() {
        let mut path = std::env::temp_dir();
        path.push(format!("qs-config-absent-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let err = std::fs::read_to_string(&path).expect_err("file must not exist");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn unknown_keys_are_preserved_in_extra() {
        let json = r#"{"paths": ["C:\\x"], "some_future_key": "value"}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert_eq!(
            config.extra.get("some_future_key").and_then(|v| v.as_str()),
            Some("value")
        );
        let round_tripped = serde_json::to_string(&config).unwrap();
        assert!(round_tripped.contains("some_future_key"));
    }

    fn fixture() -> (ConfigFile, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "qs-config-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        (ConfigFile::new(dir.join("config.json")), dir)
    }

    #[test]
    fn invalid_types_cannot_overwrite_settings_on_update() {
        for content in [
            r#"{"paths":["C:\\Work"],"max_results":"twenty"}"#,
            "null",
            "[]",
        ] {
            let (mut file, dir) = fixture();
            std::fs::write(&file.path, content).unwrap();
            assert!(file.update(|c| c.zoom = 2.0).is_err());
            assert_eq!(std::fs::read_to_string(&file.path).unwrap(), content);
            assert!(file.fallback().paths.is_empty());
            std::fs::remove_file(&file.path).unwrap();
            std::fs::remove_dir(dir).unwrap();
        }
    }

    #[test]
    fn changed_during_read_is_not_cached_as_current() {
        let (mut file, dir) = fixture();
        std::fs::write(&file.path, r#"{"max_results":10}"#).unwrap();
        let path = file.path.clone();
        assert!(file
            .read_with(|| std::fs::write(path, r#"{"max_results":999}"#).unwrap())
            .is_err());
        assert_eq!(file.read().unwrap().max_results, 999);
        std::fs::remove_file(&file.path).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn failed_save_is_reported_and_last_good_settings_survive() {
        let (mut file, dir) = fixture();
        std::fs::write(&file.path, r#"{"max_results":12,"future_setting":true}"#).unwrap();
        file.read().unwrap();
        let staging = file.path.with_extension("json.tmp");
        std::fs::create_dir(&staging).unwrap(); // deterministic write failure
        assert!(file.update(|c| c.max_results = 13).is_err());
        assert_eq!(file.fallback().max_results, 12);
        std::fs::remove_dir(staging).unwrap();
        file.update(|c| c.max_results = 14).unwrap();
        assert_eq!(
            file.read().unwrap().extra.get("future_setting"),
            Some(&Value::Bool(true))
        );
        std::fs::write(&file.path, "invalid").unwrap();
        assert!(file.read().is_err());
        assert_eq!(file.fallback().max_results, 14);
        std::fs::remove_file(&file.path).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
}

# QuickSearch

A Windows hotkey window that finds files, and lines inside files, across whole drives.

Press `Ctrl+Alt+Space` from any app, type part of a filename or some text you remember from the file, and press `Enter` to open the hit in your editor at the right line. It searches every configured root (`C:\` and `D:\` by default), not just the project that happens to be open. It is keyboard-first: the mouse works, but nothing needs it.

## Install

QuickSearch runs on Windows 10 and 11 and needs two things on `PATH`:

- [ripgrep](https://github.com/BurntSushi/ripgrep) (`rg`), which does all the searching: `winget install BurntSushi.ripgrep.MSVC`
- An editor to open results in. The default is VS Code (`code -g {file}:{line}`); see [Configuration](#configuration) to change it.

There are no prebuilt releases yet. Build it from source (below) and run `quicksearch.exe`. It starts in the tray.

## Using it

| Key | Action |
| --- | --- |
| `Ctrl+Alt+Space` | Show or hide the window (configurable) |
| `Tab` | Cycle mode: Filenames, Contents, Both, Folders |
| `Up` / `Down` | Move the selection |
| `Enter` | Open the selected result in the editor |
| `Ctrl+Enter` | Show the selected result in Explorer |
| `Alt+R` | Toggle regex (searches are literal by default) |
| `Alt+C` | Cycle case: smart, sensitive, insensitive |
| `Alt+M` | Toggle following junctions and mounted folders in content search |
| `Ctrl` + mouse wheel | Zoom |
| `Esc` | Clear the query; press again to hide |

Right-click a result for Copy and Cut, which put the file itself on the clipboard the way Explorer does.

Content results stream in as ripgrep finds them. A rare query across two full drives can take a minute or more to finish, but the first hits usually show up in well under a second.

## Configuration

Settings live in `config.json` next to `quicksearch.exe`, created with defaults on first run. Edit it by hand; there is no settings screen. Keys the app doesn't know are kept when it saves.

| Key | Default | Meaning |
| --- | --- | --- |
| `paths` | `["C:\\", "D:\\"]` | Roots to search |
| `exclude_dirs` | build output, VCS, caches, Windows system folders | Folder names skipped everywhere |
| `editor_command` | `["code", "-g", "{file}:{line}"]` | Command used by `Enter`; empty opens with the default app |
| `hotkey` | `"Ctrl+Alt+Space"` | Global show/hide shortcut |
| `max_results` | `30` | Rows per search |
| `max_per_file` | `5` | Content matches shown per file |
| `rg_extra_args` | `["--smart-case", "--hidden", "--max-filesize", "50M"]` | Passed to every ripgrep call |
| `filename_index_persist` | `true` | Keep the filename index between runs |
| `filename_index_refresh_minutes` | `60` | Background rebuild interval for the filename index |
| `content_search_quiet` | `true` | One ripgrep thread and a longer debounce for content search |

The window position, zoom, mode toggles and disabled roots are also saved here.

## Development

The app is a [Tauri 2](https://tauri.app) project: Rust in `src-tauri/`, and a plain HTML/CSS/JS frontend in `src/` with no framework and no build step. You need a stable Rust toolchain and the [Tauri prerequisites for Windows](https://tauri.app/start/prerequisites/) (WebView2 ships with Windows 11).

| Command | Purpose |
| --- | --- |
| `cargo run` (in `src-tauri/`) | Build and run a debug build |
| `.\scripts\build-release.ps1` | Release build to `src-tauri\target\release\quicksearch.exe` |
| `cargo test` (in `src-tauri/`) | Rust tests; some need `rg` on `PATH` |
| `node --test scripts/frontend-settings.test.cjs` | Frontend tests against mocked IPC |
| `cargo tauri build` | NSIS installer (needs `cargo install tauri-cli --version "^2"`) |
| `.\scripts\benchmark-quicksearch.ps1 -Executable <exe>` | Sample startup memory and I/O to CSV |

CI runs formatting, clippy, and both test suites on Windows for every push and pull request.

## How it works

**Content search** runs `rg --json --fixed-strings` over the enabled roots and streams matches to the window in batches. JSON output is used because Windows drive letters make `path:line:text` ambiguous to split. Each new keystroke kills the previous `rg`. Every `rg` is also placed in a Windows job object that is closed when the app exits, so a crash can't leave a drive walk running.

**Filename and folder search** use an in-memory index of every path under the roots, built once with `rg --files` (about 287,000 paths and 80 MB on the author's two drives). The index is saved as a SQLite catalog in `%APPDATA%\io.github.etiennedelange.quicksearch\` so later launches don't rewalk the disks. A filesystem watcher (`notify`) keeps it current while the app runs, and a periodic rebuild catches anything the watcher missed. The status line under the query reports which of those states the index is in.

The main files are `src-tauri/src/search.rs` (both search paths), `filecache.rs` (the index), `index_store.rs` (SQLite), `index_watcher.rs`, `config.rs`, and `platform/` (Windows plumbing: job objects, file clipboard, window toggle and geometry). [`docs/PRODUCT.md`](docs/PRODUCT.md) records who the app is for and the design principles behind it.

## License

[MIT](LICENSE)

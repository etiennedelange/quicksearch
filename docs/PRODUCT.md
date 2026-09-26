# Product

<!-- impeccable:product-schema 1 -->

## Platform

desktop-windows

Not `web`, `ios`, or `android`. This is a Windows 11 desktop utility built with Tauri v2:
a Rust backend and a WebView2-rendered frontend. The *rendering* technology is web
(HTML/CSS/JS, so CSS techniques and web accessibility mechanics apply literally), but the
*design conventions* that govern are Windows desktop and the OS command-palette lineage —
not web-page or mobile-app patterns. There is no browser chrome, no URL, no scroll page, no
responsive breakpoint story beyond a resizable always-on-top window.

## Stack

Existing codebase; not a greenfield choice. Tauri v2 + Rust backend, vanilla JS/HTML/CSS
frontend with no framework and no build step (`window.__TAURI__` global via
`withGlobalTauri`). Three static files: `src/index.html`, `src/app.js`, `src/style.css`.
Backend: `rusqlite` (persisted filename catalog), `notify` (filesystem watcher), `regex`,
`tauri-plugin-global-shortcut`, `tauri-plugin-single-instance`, and a small in-crate
`platform` module (show/hide toggle, window geometry, file clipboard, console-free
subprocesses bound to a kill-on-close job object).

This app is a behavior port of an earlier QuickSearch written in Python + tkinter + ctypes.
No Python source is reused; documented behavior, the `config.json` format, and the design
decisions are what carried over.

## Users

One primary user: a developer on Windows 11, working across several drives (`C:\` and `D:\`
by default), who needs to reach a specific file or a specific line of
code whose path they cannot immediately name. No second audience. No onboarding
population — the user already knows the tool. (Carried over from the source app; unchanged.)

## Product Purpose

Turn "I know some text that is in the file" into "the file is open in my editor," without
leaving whatever application currently has focus. Success is measured in seconds and
keystrokes, not in features surfaced.

## Positioning

Editor-grade search reach (whole-drive ripgrep, contents *and* filenames) at OS level, on a
global hotkey, from any application. IDE search only searches the workspace that happens to
be open; Windows Search does not read source files usefully. QuickSearch searches everything
the user owns and hands the hit straight to `code -g file:line`.

## Operating Context

- Invoked by `Ctrl+Alt+Space` from inside another application, at any moment. Also
  reachable from a tray icon; the window is `visible: false` at launch and revealed
  deliberately.
- The window appears over whatever the user was doing, takes focus, and is expected to give
  it back — an interruption the user initiated and wants to end quickly.
- ripgrep genuinely takes up to ~90s to walk `C:\` + `D:\` for a rare query. Results stream
  in; the surface is almost always in a partial, still-arriving state rather than a settled
  one. First streamed hit lands in roughly 300ms.
- Filename mode is served by an in-memory index (~287k paths after exclusions, ~80MB) that
  now **persists between runs** as a SQLite catalog in `app_data_dir()`, is reconciled by a
  `notify` filesystem watcher when watching succeeds, and is otherwise rebuilt on a
  configurable periodic refresh. Consequence: the index has a real, observable life cycle —
  cold, loading, building, ready-watched, ready-unwatched, refreshing, invalidated — which
  the surface already reports through the colophon and its activity dots.
- Search modes: Filenames (default), Contents, Both, Folders; cycled with `Tab`.

## Capabilities and Constraints

**Non-negotiable (carried from the source app, still binding here):**
- **Speed and streaming.** Rows appear as ripgrep finds them. No visual treatment may add
  per-row cost that slows the 80ms flush loop or the append path.
- **Keyboard-only operation.** Hotkey, type, `Up`/`Down`, `Enter`, `Tab`, `Escape`, and the
  rest. The mouse is never required for any action. Mouse support may be added, never
  depended on.
- **`config.json` compatibility.** Same keys, same file, same location (beside the binary,
  not app-data-dir). An existing quicksearch `config.json` must load unchanged, and unknown
  keys round-trip verbatim via a flattened `extra` map.

**Planned product direction (not yet built): a resident companion.**
The app is gaining a *pet* — a small creature living in the page's margin whose behavior is
driven by what the search engine and the filename index are genuinely doing, and which
carries persistent state of its own across sessions (it can be named, it has needs, using
the app sustains it, neglect shows). Confirmed decisions:
- Its informational job is real: it reads the machine's work, not decoration bolted on top.
- It lives in the page margin, present rather than summoned.
- It is a true pet with decaying, nameable, cross-session state — not an ambient effect.
- It is permitted to be a first-class feature with genuine presence. The user explicitly
  accepted the tension with Product Principle 1 below and directed that it be designed as a
  real feature rather than smuggled into leftover space.
- Undecided at this point: where its state is stored (`config.json` is format-locked, so
  app-data-dir alongside the SQLite catalog is the likely home), and whether it survives an
  index rebuild or a machine change.

**Technical constraints:**
- Searching is literal (`--fixed-strings`) by default; a regex mode exists in config.
- ripgrep output is parsed as `--json` because Windows drive-letter colons make
  `path:line:text` splitting ambiguous.
- Index correctness work is ongoing; anything reading index state must tolerate every state in the life cycle above, including
  interrupted, superseded, and invalidated generations.
- Frontend has a small Node test harness (`scripts/frontend-settings.test.cjs`); there is no
  end-to-end UI test suite. Verification is largely manual.

**Deliberately out of scope:** autostart, in-app settings UI, open-window switching,
per-search regex/case toggles in the UI.

## Brand Commitments

Name: **QuickSearch**. The visual world is **The Concordance**, ported from the source app's
own design document and already fully implemented here: a back-of-book concordance page,
paper-and-ink palette, one rubric red spent only on the literal match, square corners, no
chips, KWIC citation listing. That world is established and binding for this app; new work
inherits it rather than relitigating it. This project has no DESIGN.md of its own yet — a
documentation gap, not an absence of authority.

## Evidence on Hand

- Working implementation: `src-tauri/src/` (Rust core: search, filecache, index_store,
  index_watcher, config, platform) and `src/` (three-file frontend).
- Real measured figures carried from the source app: ~90s whole-drive content search, ~300ms
  to first streamed hit, ~472k files walked and ~287k indexed after exclusions, ~80MB
  resident for the filename index.
- No users beyond the author, no telemetry, no benchmarks against competing tools. None of
  these may be invented.

## Product Principles

1. **The exit is the product.** The user's goal is to stop looking at QuickSearch. Every
   element earns its place by shortening the path to `Enter`. *(The pet is an explicit,
   user-authorized exception, not a repeal: it may have presence, but it may never stand
   between the user and the row they came for.)*
2. **Design for the first row.** The confirmed scene is a locator, not a survey: the user
   usually wants the top hit, opened, immediately.
3. **Partial is the normal state.** The surface is honest about a search that is still
   running. "Still arriving" must read as progress, never as emptiness or as a finished
   result set.
4. **The keyboard is the interface.** Anything the mouse can do, a key must do first.
5. **The machine's real state is the content.** This app already treats index health,
   watcher liveness, elapsed time, and generation churn as things worth showing honestly.
   Anything new that claims to report the machine must report it truthfully — an animation
   that fakes activity is a lie about the filesystem.

## Accessibility & Inclusion

No externally imposed standard applies (single-user local tool, no procurement requirement).
Product-specific needs that do apply: the window appears abruptly over other content and is
read under whatever ambient light the user's desk has, so text contrast must hold at a
glance; because operation is keyboard-only, the focused/selected row must be unmistakable
without relying on color alone; and the whole surface scales through a single `--zoom`
custom property driven by `config.json`, so nothing may be sized in a way that ignores it.
Motion is used already (caret blink, dot breathe/pulse) and is not currently gated on
`prefers-reduced-motion` — an open item any new animated element should address rather than
compound.

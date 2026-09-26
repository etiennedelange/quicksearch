//! The Windows desktop plumbing the app needs beyond Tauri's own APIs:
//! console-free subprocesses bound to a kill-on-close job object, file
//! clipboard, the show/hide toggle, and saved window geometry.

pub mod clipboard;
pub mod subprocess;
pub mod window;

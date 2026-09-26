fn main() {
    // Cargo's `rerun-if-changed` on a directory only detects files being
    // added/removed, not existing files being edited — without this, `cargo
    // build` silently keeps serving a stale embedded copy of index.html/
    // app.js/style.css after any frontend-only edit, until some unrelated
    // Rust source change forces a real rebuild. Watch every file explicitly.
    for entry in walk_frontend_files("../src") {
        println!("cargo:rerun-if-changed={}", entry.display());
    }

    tauri_build::build()
}

fn walk_frontend_files(dir: &str) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(walk_frontend_files(path.to_str().unwrap_or_default()));
        } else {
            files.push(path);
        }
    }
    files
}

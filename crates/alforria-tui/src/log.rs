//! TUI diagnostics. While the TUI owns the terminal, `eprintln!` would
//! scribble over the alternate screen; warnings go to a log file instead
//! (upstream logs through its `Log` service into the data dir's `log/`).
//! Before [`init`] (tests, the CLI) they still go to stderr.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

static FILE: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

fn target() -> &'static Mutex<Option<PathBuf>> {
    FILE.get_or_init(|| Mutex::new(None))
}

/// Send every later [`warn`] to `path` (its directory is created).
pub fn init(path: &Path) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    *target().lock().unwrap_or_else(|e| e.into_inner()) = Some(path.to_path_buf());
}

/// The log file in use, if any.
pub fn file() -> Option<PathBuf> {
    target().lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// One `WARN` line, timestamped.
pub fn warn(message: &str) {
    let Some(path) = file() else {
        eprintln!("{message}");
        return;
    };
    let line = format!(
        "WARN  {} tui {message}\n",
        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ")
    );
    if let Ok(mut handle) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = handle.write_all(line.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn warnings_go_to_the_log_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log").join("tui.log");
        super::init(&path);
        super::warn("something broke");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("tui something broke"), "{text}");
    }
}

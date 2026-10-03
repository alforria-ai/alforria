//! Browser seam — opens a provider sign-in URL the way
//! `alforria auth login` does. Spec §6 N6 prints doc URLs instead; a
//! sign-in is different, since the user acts on the page right away.
//! The test double records the URLs.

use std::sync::Arc;

pub trait Browser: Send + Sync {
    /// Best-effort: failures are silent, because the dialog shows the URL.
    fn open(&self, url: &str);
}

/// The desktop browser: `open` (macOS), the URL handler (Windows), or
/// `xdg-open` with a display. Without a display it does nothing, since
/// `xdg-open` would fall back to a terminal browser that fights the TUI
/// for the tty. All stdio is detached.
pub struct SystemBrowser;

impl Browser for SystemBrowser {
    fn open(&self, url: &str) {
        let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
            ("open", vec![url])
        } else if cfg!(windows) {
            // Not `cmd /C start`: cmd would split the URL at `&`.
            ("rundll32", vec!["url.dll,FileProtocolHandler", url])
        } else if has_display() {
            ("xdg-open", vec![url])
        } else {
            return;
        };
        let child = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        if let Ok(mut child) = child {
            // Reap the launcher so it doesn't linger as a zombie.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

fn has_display() -> bool {
    ["DISPLAY", "WAYLAND_DISPLAY"]
        .iter()
        .any(|var| std::env::var_os(var).is_some_and(|value| !value.is_empty()))
}

/// Convenience for wiring [`App`](crate::state::App)'s seam default.
pub fn system_browser() -> Arc<dyn Browser> {
    Arc::new(SystemBrowser)
}

/// Test double: records every opened URL.
#[derive(Default)]
pub struct RecordingBrowser {
    pub opened: std::sync::Mutex<Vec<String>>,
}

impl Browser for RecordingBrowser {
    fn open(&self, url: &str) {
        self.opened
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(url.to_string());
    }
}

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
        // The URL comes from the server, which may be remote: only a plain
        // web URL reaches the desktop's URL handler.
        if !launchable_url(url) {
            return;
        }
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

/// A plain `http(s)://host…` URL with no whitespace or control characters.
/// The URL handlers launch whatever a scheme maps to (a `file:` path, an
/// app's custom protocol) and read a leading `-` as an option, so nothing
/// else is passed on. Mirrors `alforria_core::browser::launchable_url`
/// (the TUI doesn't link core).
pub fn launchable_url(url: &str) -> bool {
    let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    let rest = if lower.starts_with("https://") {
        &url[8..]
    } else if lower.starts_with("http://") {
        &url[7..]
    } else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    url.len() <= 8 * 1024
        && !host.is_empty()
        && !url.chars().any(|c| c.is_whitespace() || c.is_control())
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

#[cfg(test)]
mod tests {
    use super::launchable_url;

    #[test]
    fn only_plain_web_urls_reach_the_url_handler() {
        assert!(launchable_url(
            "https://console.libertai.io/cli?state=a&challenge=b"
        ));
        assert!(launchable_url("http://127.0.0.1:4699/cli"));
        for bad in [
            "file:///etc/passwd",
            "ms-msdt:/id PCWDiagnostic",
            "-a Calculator",
            "https://",
            "https://example.com/a b",
        ] {
            assert!(!launchable_url(bad), "{bad:?}");
        }
    }
}

//! Handing a URL to the desktop's URL handler. The handlers (`open`,
//! `xdg-open`, `url.dll`) launch whatever the scheme maps to — a `file:`
//! path, an app's custom protocol, an argument if the string starts with
//! `-` — so only plain web URLs are ever passed on. A sign-in URL can come
//! from a remote server (the TUI attached over the network), which must not
//! be able to start local programs this way.

/// A plain `http(s)://host…` URL with no whitespace or control characters.
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

/// Open `url` in the desktop browser, best-effort. Anything but a plain web
/// URL is refused (returns `false`); callers show the URL either way.
pub fn open_url(url: &str) -> bool {
    if !launchable_url(url) {
        return false;
    }
    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(windows) {
        // Not `cmd /C start`: cmd splits the URL at `&` and runs the rest.
        ("rundll32", vec!["url.dll,FileProtocolHandler", url])
    } else {
        ("xdg-open", vec![url])
    };
    std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|mut child| {
            // Reap the launcher so it doesn't linger as a zombie.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        })
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::launchable_url;

    #[test]
    fn only_plain_web_urls_launch() {
        for ok in [
            "https://console.libertai.io/cli?redirect_uri=http%3A%2F%2F127.0.0.1%3A5%2Fcallback&state=a",
            "http://127.0.0.1:4699/cli?x=1",
            "HTTPS://Example.com",
        ] {
            assert!(launchable_url(ok), "{ok}");
        }
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "ms-msdt:/id PCWDiagnostic",
            "vscode://file/x",
            "-a Calculator",
            "https://",
            "https:///path",
            "https://example.com/a b",
            "https://example.com/\nx",
            "",
        ] {
            assert!(!launchable_url(bad), "{bad:?}");
        }
    }
}

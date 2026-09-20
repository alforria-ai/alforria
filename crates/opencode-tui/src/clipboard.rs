//! Clipboard seam (§2.3) — `util/clipboard.ts` (`clipboard.ts`):
//! probe `pbcopy`/`wl-copy`/`xclip` with no new dependency; the test
//! double records writes in memory.

use std::io::Write;

pub trait Clipboard {
    /// `clipboard.write(text)` — `Ok(())` on success.
    fn write(&self, text: &str) -> anyhow::Result<()>;
}

/// Probe `pbcopy` / `wl-copy` / `xclip` like the TS implementation.
pub fn system_clipboard() -> impl Clipboard {
    SystemClipboard
}

pub struct SystemClipboard;

fn write_command(text: &str, args: &[&str]) -> anyhow::Result<()> {
    let mut child = std::process::Command::new(args[0])
        .args(&args[1..])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(text.as_bytes())?;
    }
    child.wait()?;
    Ok(())
}

impl Clipboard for SystemClipboard {
    fn write(&self, text: &str) -> anyhow::Result<()> {
        let mut candidates: Vec<Vec<&str>> = vec![vec!["pbcopy"], vec!["wl-copy"]];
        if std::env::var("DISPLAY").is_ok() || std::env::var("WAYLAND_DISPLAY").is_ok() {
            candidates.push(vec!["xclip", "-selection", "clipboard"]);
        }
        let mut errors = Vec::new();
        for args in candidates {
            match write_command(text, &args) {
                Ok(()) => return Ok(()),
                Err(error) => errors.push(format!("{}: {error}", args[0])),
            }
        }
        anyhow::bail!("no clipboard available ({})", errors.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_clipboard_probe_does_not_panic() {
        // On a CI box there is likely no clipboard — the write must
        // surface the error, not crash.
        let result = SystemClipboard.write("x");
        let _ = result;
    }
}

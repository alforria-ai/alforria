//! Clipboard seam (§2.3) — `util/clipboard.ts` (`clipboard.ts`):
//! probe `pbcopy`/`wl-copy`/`xclip` with no new dependency; the test
//! double records writes in memory.

use std::io::Write;

/// `clipboard.read()` content (`prompt/index.tsx:374-391`): the
/// `image/*` / `text/plain` union.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardContent {
    Text(String),
    Image { mime: String, data_base64: String },
    Pdf { data_base64: String },
}

pub trait Clipboard {
    /// `clipboard.write(text)` — `Ok(())` on success.
    fn write(&self, text: &str) -> anyhow::Result<()>;
    /// `clipboard.read()` — best-effort; `Err` when no clipboard
    /// tool is available.
    fn read(&self) -> anyhow::Result<ClipboardContent> {
        Err(anyhow::anyhow!("no clipboard available"))
    }
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

/// The OSC 52 escape that asks the terminal itself to set its clipboard —
/// the only copy that reaches the user's machine over SSH. Inside tmux the
/// sequence goes both plain and wrapped in tmux's passthrough; inside
/// screen only wrapped (`writeOsc52`, clipboard.ts:23-28).
pub fn osc52(text: &str, tmux: bool, screen: bool) -> String {
    let sequence = format!("\x1b]52;c;{}\x07", base64_encode(text.as_bytes()));
    let passthrough = format!("\x1bPtmux;\x1b{sequence}\x1b\\");
    if tmux {
        format!("{sequence}{passthrough}")
    } else if screen {
        passthrough
    } else {
        sequence
    }
}

/// Write [`osc52`] to the terminal the TUI draws on; `false` when stdout
/// isn't a terminal. One locked write, so it never lands inside a frame's
/// escape sequence.
fn write_osc52(text: &str) -> bool {
    use std::io::IsTerminal;
    let stdout = std::io::stdout();
    if !stdout.is_terminal() {
        return false;
    }
    let payload = osc52(
        text,
        std::env::var_os("TMUX").is_some(),
        std::env::var_os("STY").is_some(),
    );
    let mut out = stdout.lock();
    out.write_all(payload.as_bytes()).is_ok() && out.flush().is_ok()
}

impl Clipboard for SystemClipboard {
    /// OSC 52 first, then a native tool, like TS's `write` — a copy
    /// counts once either reached the terminal or a tool took it.
    fn write(&self, text: &str) -> anyhow::Result<()> {
        let via_terminal = write_osc52(text);
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
        if via_terminal {
            return Ok(());
        }
        anyhow::bail!("no clipboard available ({})", errors.join(", "))
    }

    fn read(&self) -> anyhow::Result<ClipboardContent> {
        let candidates: Vec<Vec<&str>> = vec![
            vec!["pbpaste"],
            vec!["wl-paste"],
            vec!["xclip", "-selection", "clipboard", "-o"],
        ];
        let mut last = String::new();
        for args in candidates {
            match std::process::Command::new(args[0])
                .args(&args[1..])
                .output()
            {
                Ok(output) if output.status.success() && !output.stdout.is_empty() => {
                    return Ok(sniff(output.stdout));
                }
                Ok(_) => continue,
                Err(_) => last = args[0].to_string(),
            }
        }
        anyhow::bail!("no clipboard available ({last})")
    }
}

/// Sniff binary magic like the platform clipboards do for images/pdfs
/// (`content.mime.startsWith("image/")`, `prompt/index.tsx:379`).
fn sniff(bytes: Vec<u8>) -> ClipboardContent {
    let starts_with = |magic: &[u8]| bytes.starts_with(magic);
    if starts_with(b"\x89PNG\r\n\x1a\n") {
        return ClipboardContent::Image {
            mime: "image/png".to_string(),
            data_base64: base64_encode(&bytes),
        };
    }
    if starts_with(b"\xff\xd8\xff") {
        return ClipboardContent::Image {
            mime: "image/jpeg".to_string(),
            data_base64: base64_encode(&bytes),
        };
    }
    if starts_with(b"GIF87a") || starts_with(b"GIF89a") {
        return ClipboardContent::Image {
            mime: "image/gif".to_string(),
            data_base64: base64_encode(&bytes),
        };
    }
    if starts_with(b"%PDF-") {
        return ClipboardContent::Pdf {
            data_base64: base64_encode(&bytes),
        };
    }
    ClipboardContent::Text(String::from_utf8_lossy(&bytes).into_owned())
}

fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let bytes = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let number = (bytes[0] as u32) << 16 | (bytes[1] as u32) << 8 | bytes[2] as u32;
        out.push(TABLE[(number >> 18 & 0x3f) as usize] as char);
        out.push(TABLE[(number >> 12 & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(number >> 6 & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(number & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
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

    #[test]
    fn osc52_matches_ts_including_multiplexer_passthrough() {
        // "hi" → base64 "aGk=".
        assert_eq!(osc52("hi", false, false), "\x1b]52;c;aGk=\x07");
        assert_eq!(
            osc52("hi", false, true),
            "\x1bPtmux;\x1b\x1b]52;c;aGk=\x07\x1b\\"
        );
        assert_eq!(
            osc52("hi", true, false),
            "\x1b]52;c;aGk=\x07\x1bPtmux;\x1b\x1b]52;c;aGk=\x07\x1b\\"
        );
    }
}

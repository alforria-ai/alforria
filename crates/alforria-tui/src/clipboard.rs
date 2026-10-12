//! Clipboard seam (§2.3) — `clipboard.ts`: OSC 52 plus the platform
//! tools (`osascript`, `wl-copy`/`wl-paste`, `xclip`, `xsel`), with no new
//! dependency; the test double records writes in memory.

use std::io::Write;

/// `clipboard.read()` content (`prompt/index.tsx:374-391`): the
/// `image/*` / `text/plain` union.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardContent {
    Text(String),
    /// Raw image bytes (the attachment encodes them once).
    Image {
        mime: String,
        data: Vec<u8>,
    },
    /// Raw PDF bytes.
    Pdf {
        data: Vec<u8>,
    },
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

/// Run a tool, feeding `input` on stdin; its stdout on exit status 0
/// (`command()`, clipboard.ts:9-21).
fn run_command(args: &[&str], input: Option<&str>) -> anyhow::Result<Vec<u8>> {
    let mut child = std::process::Command::new(args[0])
        .args(&args[1..])
        .stdin(if input.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        stdin.write_all(input.as_bytes())?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        anyhow::bail!("{} exited with {}", args[0], output.status);
    }
    Ok(output.stdout)
}

/// `which(name)`: an executable `name` on `$PATH`.
fn has(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(name);
        candidate.is_file()
            && std::fs::metadata(&candidate)
                .map(|meta| {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt as _;
                        meta.permissions().mode() & 0o111 != 0
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = meta;
                        true
                    }
                })
                .unwrap_or(false)
    })
}

/// `copyCommand` (clipboard.ts:83-101): the native copy tool for this
/// platform — `wl-copy` only under Wayland, then `xclip`, then `xsel`.
pub fn copy_command(
    os: &str,
    wayland: bool,
    has: impl Fn(&str) -> bool,
) -> Option<Vec<&'static str>> {
    if os == "macos" && has("osascript") {
        return Some(vec!["osascript"]);
    }
    if os == "linux" && wayland && has("wl-copy") {
        return Some(vec!["wl-copy"]);
    }
    if os == "linux" && has("xclip") {
        return Some(vec!["xclip", "-selection", "clipboard"]);
    }
    if os == "linux" && has("xsel") {
        return Some(vec!["xsel", "--clipboard", "--input"]);
    }
    if os == "windows" && has("powershell.exe") {
        return Some(vec![
            "powershell.exe",
            "-NonInteractive",
            "-NoProfile",
            "-Command",
            "[Console]::InputEncoding = [System.Text.Encoding]::UTF8; Set-Clipboard -Value ([Console]::In.ReadToEnd())",
        ]);
    }
    None
}

/// The AppleScript string literal for `set the clipboard to "…"`.
fn applescript_string(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
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
    /// OSC 52 first, then the native tool, like TS's `write` — a copy
    /// counts once either reached the terminal or a tool exited 0.
    fn write(&self, text: &str) -> anyhow::Result<()> {
        let via_terminal = write_osc52(text);
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
        let native = copy_command(std::env::consts::OS, wayland, has)
            // clipboardy's fallbacks (no osascript: pbcopy).
            .or_else(|| has("pbcopy").then(|| vec!["pbcopy"]));
        let result = match native.as_deref() {
            Some(["osascript"]) => run_command(
                &[
                    "osascript",
                    "-e",
                    &format!("set the clipboard to \"{}\"", applescript_string(text)),
                ],
                None,
            )
            .map(|_| ()),
            Some(args) => run_command(args, Some(text)).map(|_| ()),
            None => Err(anyhow::anyhow!("no clipboard tool found")),
        };
        match result {
            Ok(()) => Ok(()),
            Err(_) if via_terminal => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// `read()` (clipboard.ts:30-74): an image first (`osascript` PNGf on
    /// macOS, `wl-paste`/`xclip -t image/png` on Linux), then text.
    fn read(&self) -> anyhow::Result<ClipboardContent> {
        if cfg!(target_os = "macos") {
            if let Some(data) = macos_png() {
                return Ok(ClipboardContent::Image {
                    mime: "image/png".to_string(),
                    data,
                });
            }
        }
        if cfg!(target_os = "linux") {
            for args in [
                &["wl-paste", "-t", "image/png"][..],
                &["xclip", "-selection", "clipboard", "-t", "image/png", "-o"][..],
            ] {
                if let Ok(data) = run_command(args, None) {
                    if !data.is_empty() {
                        return Ok(ClipboardContent::Image {
                            mime: "image/png".to_string(),
                            data,
                        });
                    }
                }
            }
        }
        let candidates: [&[&str]; 4] = [
            &["pbpaste"],
            &["wl-paste", "--no-newline"],
            &["xclip", "-selection", "clipboard", "-o"],
            &["xsel", "--clipboard", "--output"],
        ];
        for args in candidates {
            if let Ok(data) = run_command(args, None) {
                if !data.is_empty() {
                    return Ok(sniff(data));
                }
            }
        }
        anyhow::bail!("no clipboard available")
    }
}

/// The macOS image read: AppleScript writes the clipboard's PNG to a
/// temporary file.
fn macos_png() -> Option<Vec<u8>> {
    let file = std::env::temp_dir().join(format!("opencode-clipboard-{}.png", std::process::id()));
    let display = file.display().to_string();
    let result = run_command(
        &[
            "osascript",
            "-e",
            "set imageData to the clipboard as \"PNGf\"",
            "-e",
            &format!(
                "set fileRef to open for access POSIX file \"{display}\" with write permission"
            ),
            "-e",
            "set eof fileRef to 0",
            "-e",
            "write imageData to fileRef",
            "-e",
            "close access fileRef",
        ],
        None,
    )
    .ok()
    .and_then(|_| std::fs::read(&file).ok())
    .filter(|data| !data.is_empty());
    let _ = std::fs::remove_file(&file);
    result
}

/// Sniff binary magic like the platform clipboards do for images/pdfs
/// (`content.mime.startsWith("image/")`, `prompt/index.tsx:379`).
fn sniff(bytes: Vec<u8>) -> ClipboardContent {
    let starts_with = |magic: &[u8]| bytes.starts_with(magic);
    let image = |mime: &str| ClipboardContent::Image {
        mime: mime.to_string(),
        data: bytes.clone(),
    };
    if starts_with(b"\x89PNG\r\n\x1a\n") {
        return image("image/png");
    }
    if starts_with(b"\xff\xd8\xff") {
        return image("image/jpeg");
    }
    if starts_with(b"GIF87a") || starts_with(b"GIF89a") {
        return image("image/gif");
    }
    if starts_with(b"%PDF-") {
        return ClipboardContent::Pdf { data: bytes };
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
    fn copy_command_matches_ts() {
        let all = |_: &str| true;
        let only = |names: &'static [&'static str]| move |name: &str| names.contains(&name);
        assert_eq!(copy_command("macos", false, all), Some(vec!["osascript"]));
        assert_eq!(copy_command("linux", true, all), Some(vec!["wl-copy"]));
        // wl-copy only under Wayland.
        assert_eq!(
            copy_command("linux", false, all),
            Some(vec!["xclip", "-selection", "clipboard"])
        );
        assert_eq!(
            copy_command("linux", false, only(&["wl-copy", "xsel"])),
            Some(vec!["xsel", "--clipboard", "--input"])
        );
        assert_eq!(copy_command("linux", false, only(&[])), None);
    }

    #[test]
    fn copy_fails_when_the_tool_exits_non_zero() {
        assert!(run_command(&["false"], Some("x")).is_err());
        assert_eq!(run_command(&["cat"], Some("x")).unwrap(), b"x");
    }

    #[test]
    fn sniffed_images_keep_raw_bytes() {
        let png = b"\x89PNG\r\n\x1a\nrest".to_vec();
        assert_eq!(
            sniff(png.clone()),
            ClipboardContent::Image {
                mime: "image/png".to_string(),
                data: png
            }
        );
    }

    #[test]
    fn applescript_text_is_escaped() {
        assert_eq!(applescript_string(r#"a"b\c"#), r#"a\"b\\c"#);
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

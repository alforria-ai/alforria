//! The terminal palette probe behind the `system` theme — the port of
//! OpenTUI's `renderer.getPalette({ size: 16 })`: OSC 10/11 ask for the
//! default foreground/background, OSC 4 for the 16 ANSI colours, and a
//! DA1 query (`ESC [ c`, which every terminal answers) ends the wait.
//!
//! The replies arrive on the terminal's input, so the probe must run
//! before anything else reads it (the TUI's input pump).

use std::time::Duration;

use super::TerminalColors;

/// The queries, BEL-terminated.
fn queries() -> String {
    let mut out = String::from("\x1b]10;?\x07\x1b]11;?\x07");
    for index in 0..16 {
        out.push_str(&format!("\x1b]4;{index};?\x07"));
    }
    out.push_str("\x1b[c");
    out
}

/// `rgb:RRRR/GGGG/BBBB` (1-4 hex digits per channel) → `#rrggbb`.
fn parse_rgb(spec: &str) -> Option<String> {
    let body = spec.strip_prefix("rgb:")?;
    let mut channels = body.split('/');
    let mut out = String::from("#");
    for _ in 0..3 {
        let channel = channels.next()?;
        if channel.is_empty() || channel.len() > 4 {
            return None;
        }
        let value = u32::from_str_radix(channel, 16).ok()?;
        let max = (1u32 << (4 * channel.len() as u32)) - 1;
        let byte = (value * 255 + max / 2) / max;
        out.push_str(&format!("{byte:02x}"));
    }
    Some(out)
}

/// Parse every OSC 4/10/11 reply in `input` (BEL or ST terminated).
pub fn parse_replies(input: &[u8]) -> TerminalColors {
    let text = String::from_utf8_lossy(input);
    let mut colors = TerminalColors::default();
    let mut palette: Vec<Option<String>> = vec![None; 16];
    for chunk in text.split("\x1b]").skip(1) {
        let end = chunk.find(['\x07', '\x1b']).unwrap_or(chunk.len());
        let reply = &chunk[..end];
        let mut fields = reply.split(';');
        match fields.next() {
            Some("10") => colors.default_foreground = fields.next().and_then(parse_rgb),
            Some("11") => colors.default_background = fields.next().and_then(parse_rgb),
            Some("4") => {
                let index = fields.next().and_then(|index| index.parse::<usize>().ok());
                let color = fields.next().and_then(parse_rgb);
                if let (Some(index), Some(color)) = (index, color) {
                    if index < 16 {
                        palette[index] = Some(color);
                    }
                }
            }
            _ => {}
        }
    }
    // The palette is usable only from index 0 on (`colors.palette[0]`
    // gates the TS `system` theme); keep the leading answered run.
    colors.palette = palette.into_iter().map_while(|color| color).collect();
    colors
}

/// Whether `input` holds the DA1 reply (`ESC [ ? … c`).
fn has_da1(input: &[u8]) -> bool {
    input.windows(3).enumerate().any(|(start, window)| {
        window == b"\x1b[?"
            && input[start + 3..]
                .iter()
                .find(|byte| !(byte.is_ascii_digit() || **byte == b';'))
                == Some(&b'c')
    })
}

/// Query the controlling terminal's colours, waiting at most `timeout`
/// for the replies. `None` when there is no terminal or it answered no
/// colour. Raw mode is enabled for the duration when it was off.
#[cfg(unix)]
pub fn probe_terminal_colors(timeout: Duration) -> Option<TerminalColors> {
    use std::io::Write;
    use std::os::fd::AsRawFd;

    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .ok()?;
    let was_raw = crossterm::terminal::is_raw_mode_enabled().unwrap_or(false);
    if !was_raw {
        crossterm::terminal::enable_raw_mode().ok()?;
    }
    let result = (|| {
        tty.write_all(queries().as_bytes()).ok()?;
        tty.flush().ok()?;
        let fd = tty.as_raw_fd();
        let deadline = std::time::Instant::now() + timeout;
        let mut input = Vec::new();
        let mut buffer = [0u8; 1024];
        while !has_da1(&input) {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            let mut poll = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd for the open tty.
            let ready = unsafe { libc::poll(&mut poll, 1, left.as_millis().max(1) as i32) };
            if ready <= 0 {
                break;
            }
            // SAFETY: `buffer` is a valid writable region of its length.
            let read = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
            if read <= 0 {
                break;
            }
            input.extend_from_slice(&buffer[..read as usize]);
        }
        let colors = parse_replies(&input);
        (colors.default_background.is_some() || !colors.palette.is_empty()).then_some(colors)
    })();
    if !was_raw {
        crossterm::terminal::disable_raw_mode().ok();
    }
    result
}

#[cfg(not(unix))]
pub fn probe_terminal_colors(_timeout: Duration) -> Option<TerminalColors> {
    None
}

/// Probe and install the result for the `system` theme; `true` when the
/// terminal answered. Call before the input pump starts reading.
pub fn probe_and_install(timeout: Duration) -> bool {
    match probe_terminal_colors(timeout) {
        Some(colors) => {
            super::registry::set_terminal_colors(colors);
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_parse_into_terminal_colors() {
        let mut input = Vec::new();
        input.extend_from_slice(b"\x1b]10;rgb:ffff/ffff/ffff\x07");
        input.extend_from_slice(b"\x1b]11;rgb:1e1e/1e1e/2e2e\x1b\\");
        input.extend_from_slice(b"\x1b]4;0;rgb:00/00/00\x07");
        input.extend_from_slice(b"\x1b]4;1;rgb:cdcd/0000/0000\x07");
        input.extend_from_slice(b"\x1b]4;3;rgb:f/f/0\x07");
        input.extend_from_slice(b"\x1b[?62;22c");
        assert!(has_da1(&input));
        let colors = parse_replies(&input);
        assert_eq!(colors.default_foreground.as_deref(), Some("#ffffff"));
        assert_eq!(colors.default_background.as_deref(), Some("#1e1e2e"));
        // Index 2 never answered: the palette stops at the gap.
        assert_eq!(colors.palette, vec!["#000000", "#cd0000"]);
        assert_eq!(parse_rgb("rgb:f/f/0").as_deref(), Some("#ffff00"));
        assert_eq!(parse_rgb("rgba:1/2/3/4"), None);
        assert!(!has_da1(b"\x1b]11;rgb:0/0/0\x07"));
    }

    #[test]
    fn the_queries_end_with_da1() {
        let queries = queries();
        assert!(queries.starts_with("\x1b]10;?\x07\x1b]11;?\x07\x1b]4;0;?\x07"));
        assert!(queries.ends_with("\x1b]4;15;?\x07\x1b[c"));
    }
}

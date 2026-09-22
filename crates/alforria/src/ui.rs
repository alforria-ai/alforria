use std::io::{self, IsTerminal, Write};
use std::sync::{Arc, Mutex};

pub mod style {
    pub const TEXT_HIGHLIGHT: &str = "\x1b[96m";
    pub const TEXT_HIGHLIGHT_BOLD: &str = "\x1b[96m\x1b[1m";
    pub const TEXT_DIM: &str = "\x1b[90m";
    pub const TEXT_DIM_BOLD: &str = "\x1b[90m\x1b[1m";
    pub const TEXT_NORMAL: &str = "\x1b[0m";
    pub const TEXT_NORMAL_BOLD: &str = "\x1b[1m";
    pub const TEXT_WARNING: &str = "\x1b[93m";
    pub const TEXT_WARNING_BOLD: &str = "\x1b[93m\x1b[1m";
    pub const TEXT_DANGER: &str = "\x1b[91m";
    pub const TEXT_DANGER_BOLD: &str = "\x1b[91m\x1b[1m";
    pub const TEXT_SUCCESS: &str = "\x1b[92m";
    pub const TEXT_SUCCESS_BOLD: &str = "\x1b[92m\x1b[1m";
    pub const TEXT_INFO: &str = "\x1b[94m";
    pub const TEXT_INFO_BOLD: &str = "\x1b[94m\x1b[1m";
}

const WORDMARK: [&str; 4] = [
    "⠀                                 ▄    ",
    "█▀▀█ █    █▀▀█ █▀▀█ █▀▀█ █▀▀█ ▀▀▀▀ █▀▀█",
    "█  █ █    █  ▀ █  █ █  ▀ █  ▀  █   █  █",
    "▀▀▀▀ █▀▀█ █    ▀▀▀▀ █▀▀█ █▀▀█ ▀▀▀▀ ▀▀▀▀",
];

const LOGO_LEFT: [&str; 4] = [
    "                   ",
    "█▀▀█ █___ █▀▀█ █▀▀█",
    "█__█ █___ █__^ █__█",
    "▀▀▀▀ █▀▀█ █___ ▀~~▀",
];

const LOGO_RIGHT: [&str; 4] = [
    "             ▄     ",
    "█▀▀█ █▀▀█ ▀▀▀▀ █▀▀█",
    "█__^ █__^ _█__ █__█",
    "█▀▀█ █▀▀█ ▀▀▀▀ ▀▀▀▀",
];

fn draw(line: &str, fg: &str, shadow: &str, bg: &str) -> String {
    let reset = "\x1b[0m";
    let mut out = String::new();
    for ch in line.chars() {
        match ch {
            '_' => {
                out.push_str(bg);
                out.push(' ');
                out.push_str(reset);
            }
            '^' => {
                out.push_str(fg);
                out.push_str(bg);
                out.push('▀');
                out.push_str(reset);
            }
            '~' => {
                out.push_str(shadow);
                out.push('▀');
                out.push_str(reset);
            }
            ' ' => out.push(' '),
            _ => {
                out.push_str(fg);
                out.push(ch);
                out.push_str(reset);
            }
        }
    }
    out
}

#[derive(Clone)]
enum Sink {
    Stdout,
    Stderr,
    Buffer(Arc<Mutex<Vec<u8>>>),
}

impl Sink {
    fn write(&self, bytes: &[u8]) {
        match self {
            Sink::Stdout => {
                let _ = io::stdout().write_all(bytes);
            }
            Sink::Stderr => {
                let _ = io::stderr().write_all(bytes);
            }
            Sink::Buffer(buffer) => {
                buffer
                    .lock()
                    .expect("sink lock poisoned")
                    .extend_from_slice(bytes);
            }
        }
    }
}

pub struct Captured {
    stdout: Arc<Mutex<Vec<u8>>>,
    stderr: Arc<Mutex<Vec<u8>>>,
}

fn captured_string(buffer: &Arc<Mutex<Vec<u8>>>) -> String {
    String::from_utf8_lossy(&buffer.lock().expect("sink lock poisoned")).into_owned()
}

impl Captured {
    pub fn stdout(&self) -> String {
        captured_string(&self.stdout)
    }

    pub fn stderr(&self) -> String {
        captured_string(&self.stderr)
    }
}

/// Styled-output seam (cli/ui.ts): UI writes go to stderr, data to stdout.
pub struct Ui {
    stdout: Sink,
    stderr: Sink,
    tty: bool,
    blank: bool,
}

impl Ui {
    pub fn production() -> Self {
        let tty = io::stdout().is_terminal() || io::stderr().is_terminal();
        Self {
            stdout: Sink::Stdout,
            stderr: Sink::Stderr,
            tty,
            blank: false,
        }
    }

    pub fn capture(tty: bool) -> (Self, Captured) {
        let stdout = Arc::new(Mutex::new(Vec::new()));
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let ui = Self {
            stdout: Sink::Buffer(stdout.clone()),
            stderr: Sink::Buffer(stderr.clone()),
            tty,
            blank: false,
        };
        (ui, Captured { stdout, stderr })
    }

    pub fn is_tty(&self) -> bool {
        self.tty
    }

    /// A second handle over the same sinks (fresh blank state) — used by
    /// seams that print from inside service callbacks (the MCP OAuth
    /// browser redirect).
    pub fn share(&self) -> Ui {
        Ui {
            stdout: self.stdout.clone(),
            stderr: self.stderr.clone(),
            tty: self.tty,
            blank: false,
        }
    }

    pub fn print(&mut self, message: &str) {
        self.blank = false;
        self.stderr.write(message.as_bytes());
    }

    pub fn println(&mut self, message: &str) {
        self.print(message);
        self.stderr.write(b"\n");
    }

    pub fn empty(&mut self) {
        if self.blank {
            return;
        }
        self.println(style::TEXT_NORMAL);
        self.blank = true;
    }

    pub fn error(&mut self, message: &str) {
        let message = message.strip_prefix("Error: ").unwrap_or(message);
        let line = format!(
            "{}Error: {}{}",
            style::TEXT_DANGER_BOLD,
            style::TEXT_NORMAL,
            message
        );
        self.println(&line);
    }

    pub fn logo(&self, pad: Option<&str>) -> String {
        let pad = pad.unwrap_or("");
        let mut out = String::new();
        if !self.tty {
            for row in WORDMARK {
                out.push_str(pad);
                out.push_str(row);
                out.push('\n');
            }
            return out.trim_end().to_string();
        }
        let reset = "\x1b[0m";
        let left = ("\x1b[90m", "\x1b[38;5;235m", "\x1b[48;5;235m");
        let right = (reset, "\x1b[38;5;238m", "\x1b[48;5;238m");
        for (index, row) in LOGO_LEFT.iter().enumerate() {
            out.push_str(pad);
            out.push_str(&draw(row, left.0, left.1, left.2));
            out.push(' ');
            let other = LOGO_RIGHT.get(index).copied().unwrap_or("");
            out.push_str(&draw(other, right.0, right.1, right.2));
            out.push('\n');
        }
        out.trim_end().to_string()
    }

    pub fn write_stdout(&mut self, message: &str) {
        self.stdout.write(message.as_bytes());
    }

    pub fn write_stderr(&self, message: &str) {
        self.stderr.write(message.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn style_codes_are_verbatim() {
        assert_eq!(style::TEXT_HIGHLIGHT, "\x1b[96m");
        assert_eq!(style::TEXT_HIGHLIGHT_BOLD, "\x1b[96m\x1b[1m");
        assert_eq!(style::TEXT_DIM, "\x1b[90m");
        assert_eq!(style::TEXT_DIM_BOLD, "\x1b[90m\x1b[1m");
        assert_eq!(style::TEXT_NORMAL, "\x1b[0m");
        assert_eq!(style::TEXT_NORMAL_BOLD, "\x1b[1m");
        assert_eq!(style::TEXT_WARNING, "\x1b[93m");
        assert_eq!(style::TEXT_WARNING_BOLD, "\x1b[93m\x1b[1m");
        assert_eq!(style::TEXT_DANGER, "\x1b[91m");
        assert_eq!(style::TEXT_DANGER_BOLD, "\x1b[91m\x1b[1m");
        assert_eq!(style::TEXT_SUCCESS, "\x1b[92m");
        assert_eq!(style::TEXT_SUCCESS_BOLD, "\x1b[92m\x1b[1m");
        assert_eq!(style::TEXT_INFO, "\x1b[94m");
        assert_eq!(style::TEXT_INFO_BOLD, "\x1b[94m\x1b[1m");
    }

    #[test]
    fn print_writes_to_stderr_and_resets_blank() {
        let (mut ui, captured) = Ui::capture(false);
        ui.print("hello");
        assert_eq!(captured.stderr(), "hello");
        ui.empty();
        ui.print("x");
        ui.empty();
        assert_eq!(captured.stderr(), "hello\x1b[0m\nx\x1b[0m\n");
    }

    #[test]
    fn println_appends_eol() {
        let (mut ui, captured) = Ui::capture(false);
        ui.println("hello");
        assert_eq!(captured.stderr(), "hello\n");
        assert_eq!(captured.stdout(), "");
    }

    #[test]
    fn empty_prints_blank_line_only_once() {
        let (mut ui, captured) = Ui::capture(false);
        ui.empty();
        ui.empty();
        ui.empty();
        assert_eq!(captured.stderr(), "\x1b[0m\n");
    }

    #[test]
    fn println_resets_blank_flag() {
        let (mut ui, captured) = Ui::capture(false);
        ui.empty();
        ui.println("text");
        ui.empty();
        assert_eq!(captured.stderr(), "\x1b[0m\ntext\n\x1b[0m\n");
    }

    #[test]
    fn error_strips_existing_error_prefix() {
        let (mut ui, captured) = Ui::capture(false);
        ui.error("Error: boom");
        assert_eq!(captured.stderr(), "\x1b[91m\x1b[1mError: \x1b[0mboom\n");
    }

    #[test]
    fn error_without_prefix_prints_message() {
        let (mut ui, captured) = Ui::capture(false);
        ui.error("boom");
        assert_eq!(captured.stderr(), "\x1b[91m\x1b[1mError: \x1b[0mboom\n");
    }

    #[test]
    fn logo_uses_wordmark_without_tty() {
        let (ui, _captured) = Ui::capture(false);
        assert!(!ui.is_tty());
        let logo = ui.logo(None);
        let lines: Vec<&str> = logo.split('\n').collect();
        assert_eq!(lines[0], "⠀                                 ▄    ");
        assert_eq!(lines.len(), 4);
        assert!(!logo.contains("\x1b["));
    }

    #[test]
    fn logo_uses_glyphs_with_tty() {
        let (ui, _captured) = Ui::capture(true);
        assert!(ui.is_tty());
        let logo = ui.logo(None);
        assert!(logo.contains("\x1b[90m"));
        assert!(logo.contains("\x1b[38;5;235m"));
        assert!(logo.contains("\x1b[48;5;238m"));
        assert!(logo.contains("▀"));
    }

    #[test]
    fn logo_pad_prefixes_every_row() {
        let (ui, _captured) = Ui::capture(true);
        let logo = ui.logo(Some("> "));
        assert!(logo.lines().all(|line| line.starts_with("> ")));
        let wordmark = ui.logo(Some("> "));
        assert!(wordmark.lines().all(|line| line.starts_with("> ")));
    }

    #[test]
    fn stdout_and_stderr_are_separate_sinks() {
        let (mut ui, captured) = Ui::capture(false);
        ui.write_stdout("data\n");
        ui.write_stderr("ui\n");
        assert_eq!(captured.stdout(), "data\n");
        assert_eq!(captured.stderr(), "ui\n");
    }
}

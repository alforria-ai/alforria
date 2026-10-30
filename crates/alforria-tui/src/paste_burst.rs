//! Multi-line pastes on terminals without bracketed paste.
//!
//! Windows consoles (crossterm has no bracketed paste there), `tmux
//! paste-buffer` without `-p` and some remote shells deliver a paste as
//! plain keystrokes, so every line ending reads as Enter and submits the
//! prompt. A person cannot produce several keys a few milliseconds apart,
//! so the input thread gathers such a burst and hands the ones holding a
//! line break to [`crate::state::Msg::Paste`].

use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// The longest silence inside a burst. Far below key-repeat intervals, and
/// too short to feel on a keystroke, which waits this long before delivery.
pub const GAP: Duration = Duration::from_millis(8);

/// A later burst this soon after a long one is the same paste, split by the
/// transport.
pub const CONTINUATION: Duration = Duration::from_millis(50);

/// Shorter bursts are typing: `o` and Enter rolled over on the keyboard.
const MIN_PASTE_KEYS: usize = 4;

/// Whether `key` can be part of pasted text.
pub fn is_text(key: &KeyEvent) -> bool {
    if key.kind == KeyEventKind::Release {
        return false;
    }
    match key.code {
        KeyCode::Enter | KeyCode::Tab => true,
        KeyCode::Char('j') if key.modifiers == KeyModifiers::CONTROL => true,
        // CONTROL + ALT is how AltGr characters arrive on Windows.
        KeyCode::Char(_) => {
            key.modifiers.difference(KeyModifiers::SHIFT).is_empty()
                || key
                    .modifiers
                    .contains(KeyModifiers::CONTROL | KeyModifiers::ALT)
        }
        _ => false,
    }
}

/// What the input thread delivers for a gathered burst.
#[derive(Debug, PartialEq, Eq)]
pub enum Burst {
    /// Typing, or a paste with nothing to protect: the keys, unchanged.
    Keys(Vec<KeyEvent>),
    /// A paste holding a line break. A trailing line break is dropped, so
    /// the paste stops short of what would have been a submit.
    Paste(String),
    /// Nothing but line breaks in the middle of a split paste.
    Ignore,
}

fn is_line_break(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Enter)
        || (key.code == KeyCode::Char('j') && key.modifiers == KeyModifiers::CONTROL)
}

/// Classify a burst; `continuing` says it follows a long burst closely.
pub fn resolve(keys: Vec<KeyEvent>, continuing: bool) -> Burst {
    if !keys.iter().any(is_line_break) || (keys.len() < MIN_PASTE_KEYS && !continuing) {
        return Burst::Keys(keys);
    }
    let mut text = String::new();
    let mut after_enter = false;
    for key in &keys {
        // A CRLF arrives as Enter + ctrl+j: one line break.
        if !(after_enter && key.code == KeyCode::Char('j')) {
            match key.code {
                _ if is_line_break(key) => text.push('\n'),
                KeyCode::Tab => text.push('\t'),
                KeyCode::Char(char) => text.push(char),
                _ => {}
            }
        }
        after_enter = key.code == KeyCode::Enter;
    }
    let text = text.trim_end_matches('\n');
    if text.is_empty() {
        return Burst::Ignore;
    }
    Burst::Paste(text.to_string())
}

/// Whether a burst of `len` keys makes the next one a [`CONTINUATION`].
pub fn is_long(len: usize) -> bool {
    len >= MIN_PASTE_KEYS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(text: &str) -> Vec<KeyEvent> {
        text.chars()
            .map(|char| match char {
                '\r' => KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                '\n' => KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
                '\t' => KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
                char if char.is_uppercase() => {
                    KeyEvent::new(KeyCode::Char(char), KeyModifiers::SHIFT)
                }
                char => KeyEvent::new(KeyCode::Char(char), KeyModifiers::NONE),
            })
            .collect()
    }

    #[test]
    fn line_endings_in_a_burst_are_pasted_not_submitted() {
        assert_eq!(
            resolve(keys("One\r\rTwo\r"), false),
            Burst::Paste("One\n\nTwo".into())
        );
        assert_eq!(
            resolve(keys("a\n\n\tb"), false),
            Burst::Paste("a\n\n\tb".into())
        );
        assert_eq!(
            resolve(keys("a\r\n\r\nb\r\n"), false),
            Burst::Paste("a\n\nb".into())
        );
    }

    #[test]
    fn typing_stays_keys() {
        assert_eq!(resolve(keys("a"), false), Burst::Keys(keys("a")));
        assert_eq!(resolve(keys("\r"), false), Burst::Keys(keys("\r")));
        assert_eq!(resolve(keys("o\r"), false), Burst::Keys(keys("o\r")));
        assert_eq!(
            resolve(keys("no break"), false),
            Burst::Keys(keys("no break"))
        );
    }

    #[test]
    fn a_split_paste_never_submits() {
        assert_eq!(resolve(keys("\r"), true), Burst::Ignore);
        assert_eq!(resolve(keys("\rb"), true), Burst::Paste("\nb".into()));
        assert_eq!(resolve(keys("more"), true), Burst::Keys(keys("more")));
    }

    #[test]
    fn only_text_keys_join_a_burst() {
        assert!(is_text(&KeyEvent::new(
            KeyCode::Char('A'),
            KeyModifiers::SHIFT
        )));
        assert!(is_text(&KeyEvent::new(
            KeyCode::Char('@'),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        )));
        assert!(!is_text(&KeyEvent::new(
            KeyCode::Char('p'),
            KeyModifiers::CONTROL
        )));
        assert!(!is_text(&KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(!is_text(&KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)));
        let mut release = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert!(!is_text(&release));
    }
}

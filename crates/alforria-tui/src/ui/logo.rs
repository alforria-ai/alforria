//! The alforria ASCII wordmark (left `ALFO` + right `RRIA` halves) and the
//! spinner frames (`component/spinner.tsx`).
//!
//! Half-block glyphs in the reference's two-tone idiom — left half muted,
//! right half bold — but on a taller 5-row face than `logo.ts` so the
//! letters stay legible for a name the reference face never had to spell.

/// The wordmark halves: `left` renders muted, `right` renders bold.
pub const LOGO: Logo = Logo {
    left: [
        " ███  █     █████  ███ ",
        "█   █ █     █     █   █",
        "█████ █     ████  █   █",
        "█   █ █     █     █   █",
        "█   █ █████ █      ███ ",
    ],
    right: [
        "████  ████  █████  ███ ",
        "█   █ █   █   █   █   █",
        "████  ████    █   █████",
        "█  █  █  █    █   █   █",
        "█   █ █   █ █████ █   █",
    ],
};

/// `SPINNER_FRAMES` (`component/spinner.tsx:8`).
pub const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub struct Logo {
    pub left: [&'static str; 5],
    pub right: [&'static str; 5],
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logo_rows_pair_up() {
        assert_eq!(LOGO.left.len(), LOGO.right.len());
        for (left, right) in LOGO.left.iter().zip(LOGO.right.iter()) {
            assert_eq!(left.chars().count(), right.chars().count());
            assert_eq!(left.chars().count(), 23);
        }
    }
}

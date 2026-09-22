//! The alforria ASCII wordmark (left `ALFO` + right `RRIA` halves) and the
//! spinner frames (`component/spinner.tsx`).
//!
//! The half-block letters share opencode's `logo.ts` idiom — left half muted,
//! right half bold — but `ALFORRIA` needs glyphs the reference never had
//! (`A`,`L`,`F`,`R`,`I`). Those are drawn below with a custom shadow
//! treatment: `_` is a shadowed space, `^` a shadowed `▀`, `~` a shadow `▀`
//! and `,` a shadow `▄` (see `component/logo.tsx`).

/// The wordmark halves: `left` renders muted, `right` renders bold.
pub const LOGO: Logo = Logo {
    left: [
        "                   ",
        "█▀▀█ █___ █▀▀█ █▀▀█",
        "█▄▄█ █___ █▀▀▀ █__█",
        "█__█ █▀▀█ █___ ▀~~▀",
    ],
    right: [
        "             ▄     ",
        "█▀▀█ █▀▀█ ▀▀▀▀ █▀▀█",
        "█__^ █__^ _█__ █▄▄█",
        "█__█ █__█ ▀▀▀▀ █__█",
    ],
};

/// `SPINNER_FRAMES` (`component/spinner.tsx:8`).
pub const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub struct Logo {
    pub left: [&'static str; 4],
    pub right: [&'static str; 4],
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logo_rows_pair_up() {
        assert_eq!(LOGO.left.len(), LOGO.right.len());
        for (left, right) in LOGO.left.iter().zip(LOGO.right.iter()) {
            assert_eq!(left.chars().count(), right.chars().count());
            assert_eq!(left.chars().count(), 19);
        }
    }
}

//! The alforria ASCII wordmark (left `ALFO` + right `RRIA` halves, in the
//! style of the TS `logo.ts`) and the spinner frames
//! (`component/spinner.tsx`).

/// The wordmark halves: `left` renders muted, `right` renders bold.
pub const LOGO: Logo = Logo {
    left: [
        "                   ",
        "█▀▀█ █___ █▀▀█ █▀▀█",
        "█__█ █___ █__^ █__█",
        "▀▀▀▀ █▀▀█ █___ ▀~~▀",
    ],
    right: [
        "             ▄     ",
        "█▀▀█ █▀▀█ ▀▀▀▀ █▀▀█",
        "█__^ █__^ _█__ █__█",
        "█▀▀█ █▀▀█ ▀▀▀▀ ▀▀▀▀",
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
        }
    }
}

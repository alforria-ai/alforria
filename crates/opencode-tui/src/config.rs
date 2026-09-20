//! TUI config (`config/index.tsx`) — the resolved shape `run()` receives.
//!
//! The `tui` schema lives in the user's `opencode.json`; the CLI resolves it
//! and hands it to the TUI as part of [`crate::TuiInput`]. Only the fields
//! the M8.3 runtime consumes are here; keybinds land with the keymap
//! (TODO(M8.4)) and attention with the attention seam (TODO(M8.8)).

/// `prompt.max_width` (`config/index.tsx:54-59`): a fixed column cap or
/// `"auto"` — `max(75, 70% of terminal width)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMaxWidth {
    Auto,
    Fixed(u16),
}

/// `TuiConfig.Resolved` (the subset the runtime needs).
#[derive(Debug, Clone)]
pub struct TuiConfig {
    /// `mouse` (`config/index.tsx:74`) — default `true`.
    pub mouse: bool,
    /// `theme` (`config/index.tsx:63`).
    pub theme: Option<String>,
    /// `prompt.max_width` — default 75 (`routes/home.tsx:33-37`).
    pub prompt_max_width: PromptMaxWidth,
}

impl Default for TuiConfig {
    fn default() -> TuiConfig {
        TuiConfig {
            mouse: true,
            theme: None,
            prompt_max_width: PromptMaxWidth::Fixed(75),
        }
    }
}

impl TuiConfig {
    /// `promptMaxWidth` (`routes/home.tsx:33-37`).
    pub fn prompt_max_width(&self, terminal_width: u16) -> u16 {
        match self.prompt_max_width {
            PromptMaxWidth::Auto => std::cmp::max(75, terminal_width * 7 / 10),
            PromptMaxWidth::Fixed(width) => width,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_max_width_defaults_to_75() {
        let config = TuiConfig::default();
        assert_eq!(config.prompt_max_width(60), 75);
        assert_eq!(config.prompt_max_width(200), 75);
    }

    #[test]
    fn prompt_max_width_auto_scales_with_the_terminal() {
        let config = TuiConfig {
            prompt_max_width: PromptMaxWidth::Auto,
            ..TuiConfig::default()
        };
        assert_eq!(config.prompt_max_width(80), 75);
        assert_eq!(config.prompt_max_width(120), 84);
        assert_eq!(config.prompt_max_width(200), 140);
    }
}

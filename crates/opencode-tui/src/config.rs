//! TUI config (`config/index.tsx`) — the resolved shape `run()` receives.
//!
//! The `tui` schema lives in the user's `opencode.json`; the CLI resolves it
//! and hands it to the TUI as part of [`crate::TuiInput`]. `keybinds` carries
//! the user overrides resolved by the keymap (TODO(M8.7): dialog config).

/// `prompt.max_width` (`config/index.tsx:54-59`): a fixed column cap or
/// `"auto"` — `max(75, 70% of terminal width)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMaxWidth {
    Auto,
    Fixed(u16),
}

/// `attention` (`config/index.tsx:113-122`): defaults
/// `{enabled: false, notifications: true, sound: true, volume: 0.4}`.
/// `volume` only scales sound-pack playback — a non-goal (spec §6 N5) —
/// so the field is carried for parity but never read.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AttentionConfig {
    pub enabled: bool,
    pub notifications: bool,
    pub sound: bool,
    pub volume: f64,
}

impl Default for AttentionConfig {
    fn default() -> AttentionConfig {
        AttentionConfig {
            enabled: false,
            notifications: true,
            sound: true,
            volume: 0.4,
        }
    }
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
    /// `keybinds` (`config/index.tsx:67`) — user overrides,
    /// keybind name → binding string (`"none"`, `"false"`, …).
    pub keybinds: std::collections::BTreeMap<String, String>,
    /// `leader_timeout` (`config/index.tsx:66`) — default 2000.
    pub leader_timeout_ms: Option<u64>,
    /// The `ResolveOptions.terminalSuspend` flag (`config/index.tsx:86-89`)
    /// — `process.platform !== "win32"`.
    pub terminal_suspend_supported: bool,
    /// `scroll_speed` (`config/index.tsx:70`) — default 3.
    pub scroll_speed: f64,
    /// `scroll_acceleration.enabled` (`config/index.tsx:71`).
    pub scroll_acceleration_enabled: bool,
    /// `diff_style` (`config/index.tsx:72`): `"auto" | "stacked"`.
    pub diff_style: String,
    /// `prompt.max_height` (`config/index.tsx:55`).
    pub prompt_max_height: Option<u16>,
    /// `attention` (`config/index.tsx:113-122`).
    pub attention: AttentionConfig,
}

impl Default for TuiConfig {
    fn default() -> TuiConfig {
        TuiConfig {
            mouse: true,
            theme: None,
            prompt_max_width: PromptMaxWidth::Fixed(75),
            prompt_max_height: None,
            keybinds: std::collections::BTreeMap::new(),
            leader_timeout_ms: None,
            terminal_suspend_supported: cfg!(not(target_os = "windows")),
            scroll_speed: 3.0,
            scroll_acceleration_enabled: false,
            diff_style: "auto".to_string(),
            attention: AttentionConfig::default(),
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

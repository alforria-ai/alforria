//! TUI config (`config/index.tsx`) — the resolved shape `run()` receives.
//!
//! The CLI finds and merges the `tui.json` files (`config/tui.ts`), then
//! [`TuiConfig::from_info`] validates and resolves the merged document
//! (`TuiConfig.resolve`). `keybinds` carries the user overrides resolved by
//! the keymap.

use serde_json::Value;

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
    /// `cursor` (`config/index.tsx:33-42`), resolved only when set.
    pub cursor: Option<CursorConfig>,
}

/// `cursor.style` (`config/index.tsx:34`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorStyle {
    Block,
    Underline,
    Line,
    /// Keep the terminal's own setting.
    Default,
}

/// The resolved `cursor` (`config/index.tsx:129-134`): style defaults to
/// `block`, blinking to `true`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorConfig {
    pub style: CursorStyle,
    pub blinking: bool,
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
            cursor: None,
        }
    }
}

/// One keybind override value as the binding string the keymap parses:
/// `false` → `"false"`, an array joins with `,`, a key-stroke object
/// becomes `ctrl+shift+name`, a binding object contributes its `key`.
fn binding_string(value: &Value) -> Option<String> {
    fn item(value: &Value) -> Option<String> {
        match value {
            Value::String(text) => Some(text.clone()),
            Value::Object(object) => match object.get("key") {
                Some(key) => item(key),
                None => {
                    let name = object.get("name")?.as_str()?;
                    let mut parts = Vec::new();
                    for modifier in ["ctrl", "shift", "meta", "super", "hyper"] {
                        if object.get(modifier).and_then(Value::as_bool) == Some(true) {
                            parts.push(modifier.to_string());
                        }
                    }
                    parts.push(name.to_string());
                    Some(parts.join("+"))
                }
            },
            _ => None,
        }
    }
    match value {
        Value::Bool(false) => Some("false".to_string()),
        Value::Array(items) => items
            .iter()
            .map(item)
            .collect::<Option<Vec<_>>>()
            .map(|items| items.join(",")),
        other => item(other),
    }
}

/// `ConfigParse.schema(Info, …)`: the merged-file shape checks. A file
/// that fails is skipped whole, as in TS (`config/tui.ts` `load`).
pub fn validate_info(info: &Value) -> Result<(), String> {
    let Some(object) = info.as_object() else {
        return Err("expected an object".to_string());
    };
    let kind = |key: &str, ok: fn(&Value) -> bool| -> Result<(), String> {
        match object.get(key) {
            Some(value) if !ok(value) => Err(format!("invalid `{key}`")),
            _ => Ok(()),
        }
    };
    kind("$schema", Value::is_string)?;
    kind("theme", Value::is_string)?;
    kind("mouse", Value::is_boolean)?;
    kind("leader_timeout", |v| v.as_u64().is_some_and(|n| n > 0))?;
    kind("scroll_speed", |v| v.as_f64().is_some_and(|n| n >= 0.001))?;
    kind("diff_style", |v| {
        matches!(v.as_str(), Some("auto" | "stacked"))
    })?;
    kind("scroll_acceleration", |v| {
        v.get("enabled").is_some_and(Value::is_boolean)
    })?;
    kind("cursor", |v| {
        v.is_object()
            && v.get("style").is_none_or(|style| {
                matches!(
                    style.as_str(),
                    Some("block" | "underline" | "line" | "default")
                )
            })
            && v.get("blinking").is_none_or(Value::is_boolean)
    })?;
    kind("prompt", |v| {
        let size = |v: &Value| v.as_u64().is_some_and(|n| n > 0);
        v.is_object()
            && v.get("max_height").is_none_or(size)
            && v.get("max_width")
                .is_none_or(|w| size(w) || w.as_str() == Some("auto"))
    })?;
    kind("attention", |v| {
        v.is_object()
            && ["enabled", "notifications", "sound"]
                .iter()
                .all(|key| v.get(key).is_none_or(Value::is_boolean))
            && v.get("volume")
                .is_none_or(|n| n.as_f64().is_some_and(|n| (0.0..=1.0).contains(&n)))
    })?;
    kind("keybinds", |v| {
        // Unknown names are dropped before validation (`dropUnknownKeybinds`).
        v.as_object().is_some_and(|binds| {
            binds
                .iter()
                .filter(|(name, _)| {
                    crate::keymap::bindings::DEFINITIONS
                        .iter()
                        .any(|definition| definition.name == name.as_str())
                })
                .all(|(_, value)| binding_string(value).is_some())
        })
    })?;
    Ok(())
}

impl TuiConfig {
    /// `TuiConfig.resolve(info, { terminalSuspend })`
    /// (`config/index.tsx:95-136`) over an already validated document.
    pub fn from_info(info: &Value, terminal_suspend_supported: bool) -> TuiConfig {
        let mut config = TuiConfig {
            terminal_suspend_supported,
            ..TuiConfig::default()
        };
        let get = |key: &str| info.get(key);
        config.theme = get("theme").and_then(Value::as_str).map(str::to_string);
        if let Some(mouse) = get("mouse").and_then(Value::as_bool) {
            config.mouse = mouse;
        }
        config.leader_timeout_ms = get("leader_timeout").and_then(Value::as_u64);
        if let Some(speed) = get("scroll_speed").and_then(Value::as_f64) {
            config.scroll_speed = speed;
        }
        config.scroll_acceleration_enabled = get("scroll_acceleration")
            .and_then(|v| v.get("enabled"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if let Some(style) = get("diff_style").and_then(Value::as_str) {
            config.diff_style = style.to_string();
        }
        if let Some(prompt) = get("prompt") {
            config.prompt_max_height = prompt
                .get("max_height")
                .and_then(Value::as_u64)
                .map(|n| n.min(u16::MAX as u64) as u16);
            match prompt.get("max_width") {
                Some(Value::String(auto)) if auto == "auto" => {
                    config.prompt_max_width = PromptMaxWidth::Auto
                }
                Some(width) => {
                    if let Some(width) = width.as_u64() {
                        config.prompt_max_width =
                            PromptMaxWidth::Fixed(width.min(u16::MAX as u64) as u16);
                    }
                }
                None => {}
            }
        }
        if let Some(attention) = get("attention") {
            let flag = |key: &str, default: bool| {
                attention
                    .get(key)
                    .and_then(Value::as_bool)
                    .unwrap_or(default)
            };
            config.attention = AttentionConfig {
                enabled: flag("enabled", false),
                notifications: flag("notifications", true),
                sound: flag("sound", true),
                volume: attention
                    .get("volume")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.4),
            };
        }
        if let Some(cursor) = get("cursor").filter(|cursor| cursor.is_object()) {
            config.cursor = Some(CursorConfig {
                style: match cursor.get("style").and_then(Value::as_str) {
                    Some("underline") => CursorStyle::Underline,
                    Some("line") => CursorStyle::Line,
                    Some("default") => CursorStyle::Default,
                    _ => CursorStyle::Block,
                },
                blinking: cursor
                    .get("blinking")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
            });
        }
        if let Some(keybinds) = get("keybinds").and_then(Value::as_object) {
            for (name, value) in keybinds {
                // `dropUnknownKeybinds` (`config/tui.ts`): unknown names go.
                if !crate::keymap::bindings::DEFINITIONS
                    .iter()
                    .any(|definition| definition.name == name)
                {
                    continue;
                }
                if let Some(binding) = binding_string(value) {
                    config.keybinds.insert(name.clone(), binding);
                }
            }
        }
        config
    }

    /// Whether the prompt cursor blinks (`cursor.blinking`, default on).
    pub fn cursor_blinks(&self) -> bool {
        self.cursor.is_none_or(|cursor| cursor.blinking)
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
    fn info_resolves_like_ts() {
        let info = serde_json::json!({
            "theme": "nord",
            "mouse": false,
            "leader_timeout": 500,
            "scroll_speed": 5,
            "scroll_acceleration": {"enabled": true},
            "prompt": {"max_width": "auto", "max_height": 8},
            "cursor": {"style": "underline"},
            "attention": {"enabled": true},
            "keybinds": {
                "command_list": ["ctrl+k", {"name": "p", "ctrl": true, "shift": true}],
                "session_new": false,
                "no_such_binding": "ctrl+q",
            },
        });
        validate_info(&info).expect("valid");
        let config = TuiConfig::from_info(&info, true);
        assert_eq!(config.theme.as_deref(), Some("nord"));
        assert!(!config.mouse);
        assert_eq!(config.leader_timeout_ms, Some(500));
        assert_eq!(config.scroll_speed, 5.0);
        assert!(config.scroll_acceleration_enabled);
        assert_eq!(config.prompt_max_width, PromptMaxWidth::Auto);
        assert_eq!(config.prompt_max_height, Some(8));
        assert_eq!(
            config.cursor,
            Some(CursorConfig {
                style: CursorStyle::Underline,
                blinking: true
            })
        );
        assert!(config.attention.enabled && config.attention.notifications);
        assert_eq!(
            config.keybinds.get("command_list").map(String::as_str),
            Some("ctrl+k,ctrl+shift+p")
        );
        assert_eq!(
            config.keybinds.get("session_new").map(String::as_str),
            Some("false")
        );
        assert!(!config.keybinds.contains_key("no_such_binding"));
    }

    #[test]
    fn invalid_info_is_rejected() {
        for bad in [
            serde_json::json!({"mouse": "no"}),
            serde_json::json!({"scroll_speed": 0}),
            serde_json::json!({"diff_style": "split"}),
            serde_json::json!({"prompt": {"max_width": 0}}),
            serde_json::json!({"keybinds": {"leader": 3}}),
        ] {
            assert!(validate_info(&bad).is_err(), "{bad}");
        }
    }

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

//! Tool ID and shell kind — port of `tool/shell/id.ts`.
//!
//! Keep the exposed tool ID and permission key as `"bash"` for compatibility
//! with existing plugins, users, and saved permissions (rename with opencode
//! 2.0 — the TS compat note is binding).

/// The exposed tool ID and permission key (shell/id.ts:14-17).
pub const TOOL_ID: &str = "bash";

/// `type Kind = "bash" | "pwsh" | "powershell" | "cmd"` (shell/id.ts:1-2).
pub type Kind = &'static str;

fn is_kind(value: &str) -> bool {
    matches!(value, "bash" | "pwsh" | "powershell" | "cmd")
}

/// `toKind` (shell/id.ts:10-12): unknown shells parse as `bash`.
pub fn to_kind(value: &str) -> Kind {
    if is_kind(value) {
        // Safety: is_kind constrains the value to the four kind literals.
        match value {
            "pwsh" => "pwsh",
            "powershell" => "powershell",
            "cmd" => "cmd",
            _ => "bash",
        }
    } else {
        "bash"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_id_is_bash() {
        assert_eq!(TOOL_ID, "bash");
    }

    #[test]
    fn known_kinds_pass_through() {
        assert_eq!(to_kind("bash"), "bash");
        assert_eq!(to_kind("pwsh"), "pwsh");
        assert_eq!(to_kind("powershell"), "powershell");
        assert_eq!(to_kind("cmd"), "cmd");
    }

    #[test]
    fn unknown_kind_falls_back_to_bash() {
        assert_eq!(to_kind("zsh"), "bash");
        assert_eq!(to_kind(""), "bash");
        assert_eq!(to_kind("nu"), "bash");
    }
}

/// User-visible command failure surfaced from a handler (effect-cmd.ts:13-18).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError {
    pub message: String,
    pub exit_code: i32,
}

impl CliError {
    pub fn new(message: impl Into<String>) -> Self {
        Self::with_exit_code(message, 1)
    }

    pub fn with_exit_code(message: impl Into<String>, exit_code: i32) -> Self {
        Self {
            message: message.into(),
            exit_code,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigIssue {
    pub message: String,
    pub path: Vec<String>,
}

/// Typed errors recognized by FormatError (cli/error.ts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypedError {
    Cli(CliError),
    McpFailed {
        name: Option<String>,
    },
    AccountService {
        message: Option<String>,
    },
    AccountTransport {
        message: Option<String>,
    },
    ProviderModelNotFound {
        provider_id: Option<String>,
        model_id: Option<String>,
        suggestions: Vec<String>,
    },
    ProviderInit {
        provider_id: Option<String>,
    },
    ConfigJson {
        path: Option<String>,
        message: Option<String>,
    },
    ConfigDirectoryTypo {
        dir: Option<String>,
        path: Option<String>,
        suggestion: Option<String>,
    },
    ConfigFrontmatter {
        message: Option<String>,
    },
    ConfigRemoteAuth {
        url: Option<String>,
        remote: Option<String>,
    },
    ConfigInvalid {
        path: Option<String>,
        message: Option<String>,
        issues: Vec<ConfigIssue>,
    },
    UiCancelled,
    Unknown {
        raw: String,
    },
}

impl From<CliError> for TypedError {
    fn from(error: CliError) -> Self {
        TypedError::Cli(error)
    }
}

impl TypedError {
    pub fn exit_code(&self) -> i32 {
        match self {
            TypedError::Cli(err) => err.exit_code,
            _ => 1,
        }
    }

    pub fn raw(&self) -> &str {
        match self {
            TypedError::Unknown { raw } => raw,
            _ => "",
        }
    }
}

fn required(value: &Option<String>) -> String {
    value.clone().unwrap_or_else(|| "undefined".to_string())
}

fn optional(value: &Option<String>) -> String {
    value.clone().unwrap_or_default()
}

/// cli/error.ts FormatError: Some(message) for recognized errors,
/// None for unknown ones (caller falls back to format_unknown).
pub fn format_error(err: &TypedError) -> Option<String> {
    match err {
        TypedError::Cli(err) => Some(err.message.clone()),
        TypedError::McpFailed { name } => Some(format!(
            "MCP server \"{name}\" failed. Note, opencode does not support MCP authentication yet.",
            name = required(name)
        )),
        TypedError::AccountService { message } | TypedError::AccountTransport { message } => {
            Some(optional(message))
        }
        TypedError::ProviderModelNotFound {
            provider_id,
            model_id,
            suggestions,
        } => {
            let mut lines = vec![format!(
                "Model not found: {}/{}",
                required(provider_id),
                required(model_id)
            )];
            if !suggestions.is_empty() {
                lines.push(format!("Did you mean: {}", suggestions.join(", ")));
            }
            lines.push("Try: `opencode models` to list available models".to_string());
            lines.push("Or check your config (opencode.json) provider/model names".to_string());
            Some(lines.join("\n"))
        }
        TypedError::ProviderInit { provider_id } => Some(format!(
            "Failed to initialize provider \"{}\". Check credentials and configuration.",
            required(provider_id)
        )),
        TypedError::ConfigJson { path, message } => {
            let mut out = format!("Config file at {} is not valid JSON(C)", required(path));
            if let Some(message) = message {
                out.push_str(&format!(": {message}"));
            }
            Some(out)
        }
        TypedError::ConfigDirectoryTypo {
            dir,
            path,
            suggestion,
        } => Some(format!(
            "Directory \"{}\" in {} is not valid. Rename the directory to \"{}\" or remove it. This is a common typo.",
            required(dir),
            required(path),
            required(suggestion)
        )),
        TypedError::ConfigFrontmatter { message } => Some(optional(message)),
        TypedError::ConfigRemoteAuth { url, remote } => {
            let from_remote = remote
                .as_deref()
                .filter(|remote| !remote.is_empty())
                .map(|remote| format!(" from {remote}"))
                .unwrap_or_default();
            let mut lines = vec![format!(
                "Failed to load remote config{from_remote}: the server returned a login page instead of JSON."
            )];
            lines.push(
                "Authentication is missing or has expired (the endpoint is likely behind an SSO or identity-aware proxy)."
                    .to_string(),
            );
            if let Some(url) = url {
                lines.push(format!("Run `opencode auth login {url}` to re-authenticate."));
            }
            Some(lines.join("\n"))
        }
        TypedError::ConfigInvalid {
            path,
            message,
            issues,
        } => {
            let mut line = String::from("Configuration is invalid");
            if let Some(path) = path {
                if path != "config" {
                    line.push_str(&format!(" at {path}"));
                }
            }
            if let Some(message) = message {
                line.push_str(&format!(": {message}"));
            }
            let mut lines = vec![line];
            for issue in issues {
                lines.push(format!("↳ {} {}", issue.message, issue.path.join(".")));
            }
            Some(lines.join("\n"))
        }
        TypedError::UiCancelled => Some(String::new()),
        TypedError::Unknown { .. } => None,
    }
}

/// FormatUnknownError (util/error.ts errorFormat): generic formatter for
/// unrecognized errors.
pub fn format_unknown(raw: &str) -> String {
    raw.to_string()
}

// ---------------------------------------------------------------------------
// FormatError over a JSON error body (error.ts) — the prompt/command
// result-tuple error shape (run.ts formatRunError).
/// `isTaggedError(input, tag)` — checks `_tag` only.
fn is_tagged(value: &serde_json::Value, tag: &str) -> bool {
    value.get("_tag").and_then(|v| v.as_str()) == Some(tag)
}

/// `NamedError.hasName(input, name)` — checks `name`.
fn has_name(value: &serde_json::Value, name: &str) -> bool {
    value.get("name").and_then(|v| v.as_str()) == Some(name)
}

fn json_str<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(|v| v.as_str())
}

/// `configData(input, tag)`: `{name: tag, data: {...}}` unwraps `data`;
/// `{_tag: tag}` is the payload itself.
fn config_data<'a>(value: &'a serde_json::Value, tag: &str) -> Option<&'a serde_json::Value> {
    if has_name(value, tag) {
        if let Some(data) = value.get("data").filter(|data| data.is_object()) {
            return Some(data);
        }
    }
    if is_tagged(value, tag) {
        return Some(value);
    }
    None
}

/// FormatError over a parsed JSON error body — `Some(message)` for
/// recognized tags, `None` for unknown ones (caller falls back to
/// format_json_unknown).
pub fn format_json_error(input: &serde_json::Value) -> Option<String> {
    if is_tagged(input, "CliError") {
        return Some(json_str(input, "message").unwrap_or_default().to_string());
    }
    if has_name(input, "MCPFailed") {
        let name = input
            .get("data")
            .and_then(|data| json_str(data, "name"))
            .unwrap_or_default();
        return Some(format!(
            "MCP server \"{name}\" failed. Note, opencode does not support MCP authentication yet."
        ));
    }
    if is_tagged(input, "AccountServiceError") || is_tagged(input, "AccountTransportError") {
        return Some(json_str(input, "message").unwrap_or_default().to_string());
    }
    if let Some(data) = config_data(input, "ProviderModelNotFoundError") {
        let mut lines = vec![format!(
            "Model not found: {}/{}",
            json_str(data, "providerID").unwrap_or_default(),
            json_str(data, "modelID").unwrap_or_default()
        )];
        let suggestions = data
            .get("suggestions")
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if !suggestions.is_empty() {
            lines.push(format!("Did you mean: {}", suggestions.join(", ")));
        }
        lines.push("Try: `opencode models` to list available models".to_string());
        lines.push("Or check your config (opencode.json) provider/model names".to_string());
        return Some(lines.join("\n"));
    }
    if let Some(data) = config_data(input, "ProviderInitError") {
        return Some(format!(
            "Failed to initialize provider \"{}\". Check credentials and configuration.",
            json_str(data, "providerID").unwrap_or_default()
        ));
    }
    if let Some(data) = config_data(input, "ConfigJsonError") {
        let message = json_str(data, "message");
        return Some(format!(
            "Config file at {} is not valid JSON(C){}",
            json_str(data, "path").unwrap_or_default(),
            message.map(|m| format!(": {m}")).unwrap_or_default()
        ));
    }
    if let Some(data) = config_data(input, "ConfigDirectoryTypoError") {
        return Some(format!(
            "Directory \"{}\" in {} is not valid. Rename the directory to \"{}\" or remove it. This is a common typo.",
            json_str(data, "dir").unwrap_or_default(),
            json_str(data, "path").unwrap_or_default(),
            json_str(data, "suggestion").unwrap_or_default()
        ));
    }
    if let Some(data) = config_data(input, "ConfigFrontmatterError") {
        return Some(json_str(data, "message").unwrap_or_default().to_string());
    }
    if let Some(data) = config_data(input, "ConfigRemoteAuthError") {
        let url = json_str(data, "url").unwrap_or_default();
        let remote = json_str(data, "remote").unwrap_or_default();
        let from_remote = if remote.is_empty() {
            String::new()
        } else {
            format!(" from {remote}")
        };
        let mut lines = vec![format!(
            "Failed to load remote config{from_remote}: the server returned a login page instead of JSON."
        )];
        lines.push(
            "Authentication is missing or has expired (the endpoint is likely behind an SSO or identity-aware proxy)."
                .to_string(),
        );
        if !url.is_empty() {
            lines.push(format!(
                "Run `opencode auth login {url}` to re-authenticate."
            ));
        }
        return Some(lines.join("\n"));
    }
    if let Some(data) = config_data(input, "ConfigInvalidError") {
        let path = json_str(data, "path").unwrap_or_default();
        let message = json_str(data, "message");
        let mut line = format!(
            "Configuration is invalid{}",
            if path.is_empty() || path == "config" {
                String::new()
            } else {
                format!(" at {path}")
            }
        );
        if let Some(message) = message {
            line.push_str(&format!(": {message}"));
        }
        let mut lines = vec![line];
        if let Some(issues) = data.get("issues").and_then(|v| v.as_array()) {
            for issue in issues {
                let message = json_str(issue, "message").unwrap_or_default();
                let path = issue
                    .get("path")
                    .and_then(|v| v.as_array())
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| item.as_str())
                            .collect::<Vec<_>>()
                            .join(".")
                    })
                    .unwrap_or_default();
                lines.push(format!("↳ {message} {path}"));
            }
        }
        return Some(lines.join("\n"));
    }
    if is_tagged(input, "UICancelledError") || has_name(input, "UICancelledError") {
        return Some(String::new());
    }
    None
}

/// `FormatUnknownError` over a parsed JSON body — `errorFormat`
/// (tui/util/error.ts:108): objects pretty-print as `JSON.stringify(value,
/// null, 2)`, scalars via `String(value)`.
pub fn format_json_unknown(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Null => "null".to_string(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn some(value: &str) -> Option<String> {
        Some(value.to_string())
    }

    #[test]
    fn cli_error_returns_message() {
        let err = TypedError::Cli(CliError::new("boom"));
        assert_eq!(format_error(&err), Some("boom".to_string()));
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn cli_error_carries_exit_code() {
        let err = TypedError::Cli(CliError::with_exit_code("custom", 42));
        assert_eq!(format_error(&err), Some("custom".to_string()));
        assert_eq!(err.exit_code(), 42);
    }

    #[test]
    fn mcp_failed_includes_name() {
        let err = TypedError::McpFailed {
            name: some("server-one"),
        };
        assert_eq!(
            format_error(&err),
            Some(
                "MCP server \"server-one\" failed. Note, opencode does not support MCP authentication yet."
                    .to_string()
            )
        );
    }

    #[test]
    fn account_errors_return_message() {
        let err = TypedError::AccountService {
            message: some("service failed"),
        };
        assert_eq!(format_error(&err), Some("service failed".to_string()));
        let err = TypedError::AccountTransport { message: None };
        assert_eq!(format_error(&err), Some(String::new()));
    }

    #[test]
    fn provider_model_not_found_with_suggestions() {
        let err = TypedError::ProviderModelNotFound {
            provider_id: some("anthropic"),
            model_id: some("claude-x"),
            suggestions: vec!["claude-4".to_string(), "claude-3".to_string()],
        };
        assert_eq!(
            format_error(&err),
            Some(
                "Model not found: anthropic/claude-x\nDid you mean: claude-4, claude-3\nTry: `opencode models` to list available models\nOr check your config (opencode.json) provider/model names"
                    .to_string()
            )
        );
    }

    #[test]
    fn provider_model_not_found_without_suggestions() {
        let err = TypedError::ProviderModelNotFound {
            provider_id: None,
            model_id: some("claude-x"),
            suggestions: vec![],
        };
        let formatted = format_error(&err).unwrap();
        assert!(
            formatted.starts_with("Model not found: undefined/claude-x\n"),
            "{formatted}"
        );
        assert!(!formatted.contains("Did you mean"));
    }

    #[test]
    fn provider_init_error() {
        let err = TypedError::ProviderInit {
            provider_id: some("openai"),
        };
        assert_eq!(
            format_error(&err),
            Some(
                "Failed to initialize provider \"openai\". Check credentials and configuration."
                    .to_string()
            )
        );
    }

    #[test]
    fn config_json_error_with_and_without_message() {
        let err = TypedError::ConfigJson {
            path: some("/home/user/opencode.json"),
            message: None,
        };
        assert_eq!(
            format_error(&err),
            Some("Config file at /home/user/opencode.json is not valid JSON(C)".to_string())
        );
        let err = TypedError::ConfigJson {
            path: some("/home/user/opencode.json"),
            message: some("unexpected token"),
        };
        assert_eq!(
            format_error(&err),
            Some(
                "Config file at /home/user/opencode.json is not valid JSON(C): unexpected token"
                    .to_string()
            )
        );
    }

    #[test]
    fn config_directory_typo_error() {
        let err = TypedError::ConfigDirectoryTypo {
            dir: some("auths"),
            path: some("/home/user/opencode.json"),
            suggestion: some("auth"),
        };
        assert_eq!(
            format_error(&err),
            Some(
                "Directory \"auths\" in /home/user/opencode.json is not valid. Rename the directory to \"auth\" or remove it. This is a common typo."
                    .to_string()
            )
        );
    }

    #[test]
    fn config_frontmatter_error() {
        let err = TypedError::ConfigFrontmatter {
            message: some("bad frontmatter"),
        };
        assert_eq!(format_error(&err), Some("bad frontmatter".to_string()));
    }

    #[test]
    fn config_remote_auth_error_with_url() {
        let err = TypedError::ConfigRemoteAuth {
            url: some("https://example.com"),
            remote: some("https://config.example.com/config.json"),
        };
        assert_eq!(
            format_error(&err),
            Some(
                "Failed to load remote config from https://config.example.com/config.json: the server returned a login page instead of JSON.\nAuthentication is missing or has expired (the endpoint is likely behind an SSO or identity-aware proxy).\nRun `opencode auth login https://example.com` to re-authenticate."
                    .to_string()
            )
        );
    }

    #[test]
    fn config_remote_auth_error_without_url() {
        let err = TypedError::ConfigRemoteAuth {
            url: None,
            remote: None,
        };
        assert_eq!(
            format_error(&err),
            Some(
                "Failed to load remote config: the server returned a login page instead of JSON.\nAuthentication is missing or has expired (the endpoint is likely behind an SSO or identity-aware proxy)."
                    .to_string()
            )
        );
    }

    #[test]
    fn config_invalid_error_with_issues() {
        let err = TypedError::ConfigInvalid {
            path: some("/home/user/opencode.json"),
            message: some("2 errors"),
            issues: vec![
                ConfigIssue {
                    message: "unknown option".to_string(),
                    path: vec!["model".to_string()],
                },
                ConfigIssue {
                    message: "expected string".to_string(),
                    path: vec!["share".to_string(), "level".to_string()],
                },
            ],
        };
        assert_eq!(
            format_error(&err),
            Some(
                "Configuration is invalid at /home/user/opencode.json: 2 errors\n↳ unknown option model\n↳ expected string share.level"
                    .to_string()
            )
        );
    }

    #[test]
    fn config_invalid_error_hides_config_path() {
        let err = TypedError::ConfigInvalid {
            path: some("config"),
            message: None,
            issues: vec![],
        };
        assert_eq!(
            format_error(&err),
            Some("Configuration is invalid".to_string())
        );
    }

    #[test]
    fn ui_cancelled_is_silent_empty() {
        let err = TypedError::UiCancelled;
        assert_eq!(format_error(&err), Some(String::new()));
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn unknown_error_returns_none() {
        let err = TypedError::Unknown {
            raw: "whatever".to_string(),
        };
        assert_eq!(format_error(&err), None);
        assert_eq!(err.raw(), "whatever");
        assert_eq!(format_unknown(err.raw()), "whatever");
    }

    #[test]
    fn json_error_cli_tag() {
        let value = serde_json::json!({"_tag": "CliError", "message": "boom", "exitCode": 3});
        assert_eq!(format_json_error(&value), Some("boom".to_string()));
    }

    #[test]
    fn json_error_mcp_failed_uses_data_name() {
        let value = serde_json::json!({"name": "MCPFailed", "data": {"name": "remote"}});
        assert_eq!(
            format_json_error(&value),
            Some(
                "MCP server \"remote\" failed. Note, opencode does not support MCP authentication yet."
                    .to_string()
            )
        );
    }

    #[test]
    fn json_error_account_tags() {
        assert_eq!(
            format_json_error(
                &serde_json::json!({"_tag": "AccountServiceError", "message": "down"})
            ),
            Some("down".to_string())
        );
        assert_eq!(
            format_json_error(&serde_json::json!({"_tag": "AccountTransportError"})),
            Some(String::new())
        );
    }

    #[test]
    fn json_error_provider_model_not_found() {
        let value = serde_json::json!({
            "name": "ProviderModelNotFoundError",
            "data": {"providerID": "anthropic", "modelID": "x", "suggestions": ["a", "b"]},
        });
        assert_eq!(
            format_json_error(&value),
            Some("Model not found: anthropic/x\nDid you mean: a, b\nTry: `opencode models` to list available models\nOr check your config (opencode.json) provider/model names".to_string())
        );
        // `_tag` form carries the fields on the object itself.
        let value = serde_json::json!({
            "_tag": "ProviderInitError",
            "providerID": "openai",
        });
        assert_eq!(
            format_json_error(&value),
            Some(
                "Failed to initialize provider \"openai\". Check credentials and configuration."
                    .to_string()
            )
        );
    }

    #[test]
    fn json_error_config_tags() {
        let value = serde_json::json!({"_tag": "ConfigJsonError", "path": "/c/opencode.json", "message": "bad"});
        assert_eq!(
            format_json_error(&value),
            Some("Config file at /c/opencode.json is not valid JSON(C): bad".to_string())
        );
        let value = serde_json::json!({
            "name": "ConfigDirectoryTypoError",
            "data": {"dir": "auths", "path": "/c/opencode.json", "suggestion": "auth"},
        });
        assert_eq!(
            format_json_error(&value),
            Some("Directory \"auths\" in /c/opencode.json is not valid. Rename the directory to \"auth\" or remove it. This is a common typo.".to_string())
        );
        let value = serde_json::json!({"_tag": "ConfigRemoteAuthError", "url": "https://u", "remote": "https://r"});
        assert_eq!(
            format_json_error(&value),
            Some("Failed to load remote config from https://r: the server returned a login page instead of JSON.\nAuthentication is missing or has expired (the endpoint is likely behind an SSO or identity-aware proxy).\nRun `opencode auth login https://u` to re-authenticate.".to_string())
        );
        let value = serde_json::json!({
            "_tag": "ConfigInvalidError",
            "path": "/c/opencode.json",
            "message": "1 error",
            "issues": [{"message": "unknown option", "path": ["model"]}],
        });
        assert_eq!(
            format_json_error(&value),
            Some(
                "Configuration is invalid at /c/opencode.json: 1 error\n↳ unknown option model"
                    .to_string()
            )
        );
    }

    #[test]
    fn json_error_ui_cancelled_and_unknown() {
        assert_eq!(
            format_json_error(&serde_json::json!({"_tag": "UICancelledError"})),
            Some(String::new())
        );
        assert_eq!(
            format_json_error(&serde_json::json!({"name": "UICancelledError"})),
            Some(String::new())
        );
        assert_eq!(format_json_error(&serde_json::json!({"nope": 1})), None);
        assert_eq!(format_json_error(&serde_json::json!("plain")), None);
    }

    #[test]
    fn json_unknown_formats_values() {
        assert_eq!(
            format_json_unknown(&serde_json::json!({"a": 1})),
            "{\n  \"a\": 1\n}"
        );
        assert_eq!(format_json_unknown(&serde_json::json!("plain")), "plain");
        assert_eq!(format_json_unknown(&serde_json::Value::Null), "null");
    }
}

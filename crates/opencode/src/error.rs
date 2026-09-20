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
}

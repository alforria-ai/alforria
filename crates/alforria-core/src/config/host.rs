//! Host-level settings for an unattended server (alforria extension).
//!
//! These keys have no upstream counterpart. They describe how the *server
//! process* behaves when nobody is at a terminal: how it stops, who may talk
//! to it, what the agent's shell may see. They are read once, from the host
//! config only (the global config directory, `~/.opencode` and the managed
//! directory, see [`load_host_config`]): a project `opencode.json` — a file
//! the agent itself can write — never weakens them.

use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::config::{Config, ConfigFlags, ConfigLoader, LoadParams};
use crate::{CoreError, GlobalPaths};

/// A duration written as `"500ms"`, `"30s"`, `"5m"`, `"2h"`, `"1d"`, or as a
/// bare number of milliseconds (upstream's convention for timeouts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurationSetting(pub Duration);

impl DurationSetting {
    pub fn get(&self) -> Duration {
        self.0
    }

    /// Parse the string form; `None` when the text is not a duration.
    pub fn parse(text: &str) -> Option<DurationSetting> {
        let text = text.trim();
        let split = text
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(text.len());
        let (number, unit) = text.split_at(split);
        let number: f64 = number.parse().ok()?;
        if !number.is_finite() || number < 0.0 {
            return None;
        }
        let millis = match unit.trim() {
            "ms" => 1.0,
            "s" | "sec" | "secs" => 1_000.0,
            "m" | "min" | "mins" => 60_000.0,
            "h" | "hr" | "hrs" => 3_600_000.0,
            "d" | "day" | "days" => 86_400_000.0,
            _ => return None,
        };
        Some(DurationSetting(Duration::from_millis(
            (number * millis).round() as u64,
        )))
    }
}

impl Serialize for DurationSetting {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(self.0.as_millis() as u64)
    }
}

impl<'de> Deserialize<'de> for DurationSetting {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        match &value {
            serde_json::Value::Number(number) => number
                .as_u64()
                .map(|millis| DurationSetting(Duration::from_millis(millis)))
                .ok_or_else(|| {
                    serde::de::Error::custom("expected a non-negative number of milliseconds")
                }),
            serde_json::Value::String(text) => DurationSetting::parse(text).ok_or_else(|| {
                serde::de::Error::custom(format!(
                    "invalid duration {text:?}: expected a number followed by ms, s, m, h or d"
                ))
            }),
            _ => Err(serde::de::Error::custom(
                "expected a duration string such as \"30s\" or a number of milliseconds",
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// server.shutdown
// ---------------------------------------------------------------------------

/// What happens to running turns when the server is told to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShutdownSessions {
    /// Cancel them at once.
    #[default]
    Abort,
    /// Let them finish, cancelling whatever is still running at the deadline.
    Drain,
}

/// `server.shutdown` — how `alforria serve` handles SIGTERM / SIGINT.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ShutdownInfo {
    /// How long shutdown may take before the process exits anyway
    /// (default 10 s).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<DurationSetting>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sessions: Option<ShutdownSessions>,
}

// ---------------------------------------------------------------------------
// server.auth.bearer, server.directories
// ---------------------------------------------------------------------------

/// One string or a list of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

impl OneOrMany {
    pub fn to_vec(&self) -> Vec<String> {
        match self {
            OneOrMany::One(value) => vec![value.clone()],
            OneOrMany::Many(values) => values.clone(),
        }
    }
}

/// A static bearer token, held as the SHA-256 of the token: a bare hex
/// digest (scope `admin`), or a digest with the scope it grants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum BearerTokenInfo {
    Digest(String),
    Scoped {
        sha256: String,
        /// Space-separated scopes (`hook`, `chat`, `connector`, `admin`);
        /// `admin` when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
        /// A label for logs.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// The one webhook trigger the token delivers to. Required for a
        /// `hook` token: without it the token is refused.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trigger: Option<String>,
    },
}

/// `server.auth.bearer` — bearer authentication next to HTTP Basic.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct BearerAuthInfo {
    /// Ed25519 public keys that sign the JWTs this server accepts: PEM
    /// (`BEGIN PUBLIC KEY`), or base64 / hex of the raw 32-byte key or of
    /// its SPKI DER.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keys: Option<Vec<String>>,
    /// The `aud` a JWT must carry (any of these). Required with `keys`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience: Option<OneOrMany>,
    /// The `iss` a JWT must carry, when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
    /// Clock skew tolerated on `exp` / `nbf` (default 30 s).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leeway: Option<DurationSetting>,
    /// Static tokens, as SHA-256 digests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_sha256: Option<Vec<BearerTokenInfo>>,
    /// Let a credential below `admin` answer a permission request `always`
    /// (a standing allow rule). Off by default: `once` and `reject` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_always: Option<bool>,
}

/// `server.auth`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ServerAuthInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer: Option<BearerAuthInfo>,
}

/// `server.upload` — limits on `PUT /file/content`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct UploadInfo {
    /// The largest upload accepted, in bytes (default 32 MiB).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
    /// The directories, relative to the project, a token below `admin` may
    /// upload into (default `["uploads"]`). `"."` is the whole project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directories: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// server.trust
// ---------------------------------------------------------------------------

/// `server.trust.project` — how much a project's own config may decide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProjectTrust {
    /// Today's standalone behaviour: a project's `opencode.json` and
    /// `.opencode/` set everything upstream lets them set.
    Full,
    /// Capability-bearing keys from a project's or a remote config are
    /// dropped (`config::precedence::UPSTREAM_KEYS`), and `{env:}` reads
    /// only the names in `project_env`.
    Restricted,
}

/// `server.trust.host` — which files may carry host keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostSources {
    /// The global config directory, `~/.opencode` and the managed
    /// directory (the default).
    User,
    /// The managed directory only. Read from the managed directory itself:
    /// set anywhere else it is ignored.
    Managed,
}

/// `server.trust` — what the server believes of the configs it merges.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TrustInfo {
    /// `"restricted"` when `server.auth.bearer` is configured, `"full"`
    /// otherwise, unless set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectTrust>,
    /// Keys a restricted project config may set after all: dotted paths
    /// with `*` for any one name (`"mcp"`, `"agent.*.permission"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_keys: Option<Vec<String>>,
    /// Environment variables `{env:NAME}` may read from a restricted
    /// project config. None by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_env: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostSources>,
}

// ---------------------------------------------------------------------------
// permission.unattended, question.unattended
// ---------------------------------------------------------------------------

/// What a permission ask resolves to when nobody answers it in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionOnTimeout {
    /// Keep waiting (upstream behaviour).
    #[default]
    Wait,
    /// Deny, telling the model nobody was there to answer.
    Deny,
    /// Allow, once.
    Allow,
}

/// `permission.unattended` — a bound on how long a permission ask waits for
/// a person. Read from the host config only: a project file cannot turn
/// asks into allows.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PermissionUnattendedInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<DurationSetting>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_timeout: Option<PermissionOnTimeout>,
}

/// What a question resolves to when nobody answers it in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QuestionOnTimeout {
    /// Keep waiting (upstream behaviour).
    #[default]
    Wait,
    /// Dismiss it, telling the model the user is unavailable.
    Reject,
}

/// `question.unattended`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct QuestionUnattendedInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<DurationSetting>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_timeout: Option<QuestionOnTimeout>,
}

/// `question` — settings for the question tool's asks.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct QuestionInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unattended: Option<QuestionUnattendedInfo>,
}

// ---------------------------------------------------------------------------
// hook.webhook
// ---------------------------------------------------------------------------

/// One webhook: where selected events are delivered, and how each delivery
/// is signed.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct WebhookInfo {
    /// A label for this hook; defaults to its URL. Deliveries are tied to
    /// it, so renaming a hook abandons its undelivered events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub url: String,
    /// Event types to deliver, by name or `*` / `?` pattern:
    /// `"permission.asked"`, `"session.*"`, `"trigger.*"`.
    pub events: Vec<String>,
    /// A shared secret: deliveries carry an HMAC-SHA256 of the timestamp
    /// and body. Exactly one of `secret` and `signing_key` is required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    /// An Ed25519 private key (base64 or hex of the 32-byte seed):
    /// deliveries carry a signature the receiver checks with the public
    /// key, so the receiver holds nothing that can forge one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing_key: Option<String>,
    /// Deliver a trigger run's final answer (`trigger.completed`, when its
    /// `report` is `"final"`) as `properties.text`, cut to 16 KiB. Off by
    /// default: a delivery carries ids, never content. This is the one
    /// exception, for a receiver whose job is to pass that answer on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_text: Option<bool>,
}

/// `hook.outbox` — how deliveries are retried and how long they are kept.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct OutboxInfo {
    /// How long a delivered (or abandoned) delivery is kept (default 24h).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<DurationSetting>,
    /// How long an undelivered event is retried before it is abandoned
    /// (default 72h).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age: Option<DurationSetting>,
    /// The first retry delay, doubling up to `retry_max` (defaults 5s, 5m).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_initial: Option<DurationSetting>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_max: Option<DurationSetting>,
    /// How long one delivery attempt may take (default 10s).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<DurationSetting>,
    /// The most undelivered events kept; beyond it the oldest are dropped
    /// (default 10000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_pending: Option<u64>,
}

/// `hook`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct HookInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook: Option<Vec<WebhookInfo>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outbox: Option<OutboxInfo>,
}

// ---------------------------------------------------------------------------
// shell_env
// ---------------------------------------------------------------------------

/// `shell_env.inherit` — `"all"`, `"none"`, or a list of names / patterns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellEnvInherit {
    All,
    None,
    Names(Vec<String>),
}

impl Serialize for ShellEnvInherit {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            ShellEnvInherit::All => serializer.serialize_str("all"),
            ShellEnvInherit::None => serializer.serialize_str("none"),
            ShellEnvInherit::Names(names) => names.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for ShellEnvInherit {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        match &value {
            serde_json::Value::String(mode) if mode == "all" => Ok(ShellEnvInherit::All),
            serde_json::Value::String(mode) if mode == "none" => Ok(ShellEnvInherit::None),
            serde_json::Value::Array(_) => serde_json::from_value(value)
                .map(ShellEnvInherit::Names)
                .map_err(serde::de::Error::custom),
            _ => Err(serde::de::Error::custom(
                "expected \"all\", \"none\" or a list of variable names",
            )),
        }
    }
}

/// `shell_env` — what the agent's shells see of the server's environment
/// (`crate::shell_env`). Upstream's `shell` key is a string (the shell to
/// run), so this cannot live under it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ShellEnvInfo {
    /// Which of the server's variables a shell starts with. Unset: all of
    /// them on a standalone server, a short allowlist on one with bearer
    /// auth (`shell_env::RESTRICTED_INHERIT`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherit: Option<ShellEnvInherit>,
    /// Exact names exempt from the built-in deny list, and inherited on a
    /// server with bearer auth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow: Option<Vec<String>>,
    /// Variables set in every shell, over anything inherited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set: Option<std::collections::BTreeMap<String, String>>,
    /// Names or `*` / `?` patterns never inherited, on top of the built-in
    /// list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deny: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// The host config: everything the regular loader merges *except* project
/// files and project `.opencode` directories.
///
/// Unlike the per-directory load, a global file that fails to parse is an
/// error here rather than an empty config: these settings carry the server's
/// access policy, and silently dropping them would change who may connect.
pub fn load_host_config(paths: &GlobalPaths) -> Result<Config, CoreError> {
    let params = LoadParams::new(paths.home.clone(), paths.clone()).flags(ConfigFlags {
        disable_project_config: true,
        ..ConfigFlags::default()
    });
    let loader = ConfigLoader::new();
    loader.check_global(&params)?;
    let (config, _directories) = loader.load(&params)?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_parse_units_and_milliseconds() {
        let parse = |text: &str| DurationSetting::parse(text).map(|d| d.get());
        assert_eq!(parse("500ms"), Some(Duration::from_millis(500)));
        assert_eq!(parse("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse("1.5s"), Some(Duration::from_millis(1500)));
        assert_eq!(parse("5m"), Some(Duration::from_secs(300)));
        assert_eq!(parse("2h"), Some(Duration::from_secs(7200)));
        assert_eq!(parse("1d"), Some(Duration::from_secs(86_400)));
        assert_eq!(parse(" 10 s "), Some(Duration::from_secs(10)));
        assert_eq!(parse("10"), None, "a bare string needs a unit");
        assert_eq!(parse("-1s"), None);
        assert_eq!(parse("soon"), None);

        let from_json = |value: serde_json::Value| {
            serde_json::from_value::<DurationSetting>(value).map(|d| d.get())
        };
        assert_eq!(
            from_json(serde_json::json!(1500)).unwrap(),
            Duration::from_millis(1500)
        );
        assert_eq!(
            from_json(serde_json::json!("2m")).unwrap(),
            Duration::from_secs(120)
        );
        assert!(from_json(serde_json::json!("2 fortnights")).is_err());
        assert!(from_json(serde_json::json!(true)).is_err());
    }

    #[test]
    fn host_config_ignores_project_files_and_rejects_broken_global_ones() {
        let dir = tempfile::tempdir().unwrap();
        let paths = GlobalPaths::resolve_with(dir.path().join("home"), |_| None);
        std::fs::create_dir_all(&paths.config).unwrap();
        std::fs::create_dir_all(&paths.home).unwrap();
        std::fs::write(
            paths.config.join("opencode.json"),
            r#"{ "server": { "shutdown": { "timeout": "3s", "sessions": "drain" } } }"#,
        )
        .unwrap();
        // A project-style file in the home directory is not host config.
        std::fs::write(
            paths.home.join("opencode.json"),
            r#"{ "server": { "shutdown": { "timeout": "99s" } } }"#,
        )
        .unwrap();
        let config = load_host_config(&paths).unwrap();
        let shutdown = config.server.unwrap().shutdown.unwrap();
        assert_eq!(shutdown.timeout.unwrap().get(), Duration::from_secs(3));
        assert_eq!(shutdown.sessions, Some(ShutdownSessions::Drain));

        std::fs::write(paths.config.join("opencode.json"), "{ not json").unwrap();
        assert!(load_host_config(&paths).is_err());
    }
}

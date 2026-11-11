//! The server's host policy (alforria extension): the unattended-server
//! settings resolved once from the host config
//! (`alforria_core::config::host`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use alforria_core::config::host::ShutdownSessions;
use alforria_core::config::Config;

/// How a listener stops ([`crate::Listener::shutdown`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShutdownOptions {
    /// The whole budget: stop accepting, settle the sessions, close the
    /// connections, checkpoint the database.
    pub timeout: Duration,
    pub sessions: ShutdownSessions,
}

/// `server.shutdown.timeout` when unset.
pub const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

impl Default for ShutdownOptions {
    fn default() -> Self {
        ShutdownOptions {
            timeout: DEFAULT_SHUTDOWN_TIMEOUT,
            sessions: ShutdownSessions::Abort,
        }
    }
}

/// The project directories a credential narrower than `admin` may open
/// (`server.directories`): exactly the listed ones. Unset, that is the
/// server's working directory alone, so a `chat` token can never be pointed
/// at an arbitrary path with `?directory=`.
///
/// A directory *under* a listed one is not allowed: an instance opened
/// there loads the config, agents and commands found from that directory
/// upwards, so a file dropped into a subdirectory (an upload, something the
/// agent wrote) would become policy for whoever opened it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryAllowlist {
    roots: Vec<PathBuf>,
}

impl DirectoryAllowlist {
    /// The allowlist for configured entries (`~/` expands against `home`);
    /// no entries means the working directory.
    pub fn new(configured: &[String], home: &Path) -> DirectoryAllowlist {
        let mut roots: Vec<PathBuf> = configured
            .iter()
            .filter(|entry| !entry.is_empty())
            .map(|entry| match entry.strip_prefix("~/") {
                Some(rest) => home.join(rest),
                None if entry == "~" => home.to_path_buf(),
                None => PathBuf::from(entry),
            })
            .map(|path| crate::state::resolve_directory(&path))
            .collect();
        if roots.is_empty() {
            roots.push(crate::state::resolve_directory(&crate::state::cwd()));
        }
        DirectoryAllowlist { roots }
    }

    /// Whether `directory` is one of the allowed directories. Both sides
    /// are resolved (symlinks followed where the path exists), so another
    /// spelling of an allowed directory is allowed and a link elsewhere is
    /// not.
    pub fn allows(&self, directory: &Path) -> bool {
        let directory = crate::state::resolve_directory(directory);
        self.roots.contains(&directory)
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }
}

impl Default for DirectoryAllowlist {
    fn default() -> Self {
        DirectoryAllowlist::new(&[], Path::new(""))
    }
}

/// `server.upload.directories` when unset.
pub const DEFAULT_UPLOAD_DIRECTORY: &str = "uploads";

/// `server.upload.max_bytes` when unset: 32 MiB.
pub const DEFAULT_UPLOAD_MAX_BYTES: u64 = 32 * 1024 * 1024;

/// Everything the server resolved from the host config.
#[derive(Debug, Clone)]
pub struct ServerPolicy {
    pub shutdown: ShutdownOptions,
    /// `server.auth.bearer`, when it configures anything.
    pub bearer: Option<Arc<crate::bearer::BearerAuth>>,
    /// `server.directories`.
    pub directories: DirectoryAllowlist,
    /// `server.trust.host: "managed"`: the user's config files carry no
    /// host key.
    pub managed_only: bool,
    /// `server.auth.bearer.reply_always`: whether a credential below
    /// `admin` may answer a permission request `always`.
    pub reply_always: bool,
    /// `server.upload.max_bytes`: the largest body `PUT /file/content`
    /// takes.
    pub upload_max_bytes: u64,
    /// `server.upload.directories`: where, inside the project, a
    /// credential below `admin` may upload.
    pub upload_directories: Vec<String>,
    /// `permission.unattended`: what an unanswered permission ask
    /// resolves to, and when. `None`: it waits.
    pub unattended_permission: Option<alforria_core::session::permission::Unattended>,
    /// `question.unattended`: how long a question waits before it is
    /// dismissed. `None`: it waits.
    pub unattended_question: Option<Duration>,
    /// `hook`: webhooks and their outbox settings, as configured.
    pub hook: Option<alforria_core::config::host::HookInfo>,
    /// What the agent's shells see of the server's environment.
    pub shell_env: alforria_core::shell_env::ShellEnvPolicy,
    /// Why the host config could not be used, if it could not. A listener
    /// refuses to bind with this set ([`crate::listen_with`]): the policy
    /// in force would not be the one written down.
    pub config_error: Option<String>,
}

impl Default for ServerPolicy {
    fn default() -> Self {
        ServerPolicy {
            shutdown: ShutdownOptions::default(),
            bearer: None,
            directories: DirectoryAllowlist::default(),
            managed_only: false,
            reply_always: false,
            upload_max_bytes: DEFAULT_UPLOAD_MAX_BYTES,
            upload_directories: vec![DEFAULT_UPLOAD_DIRECTORY.to_string()],
            unattended_permission: None,
            unattended_question: None,
            hook: None,
            shell_env: alforria_core::shell_env::ShellEnvPolicy::default(),
            config_error: None,
        }
    }
}

/// The variables the providers of `config` read their keys from
/// (`provider.*.env[]`): withheld from the agent's processes whatever they
/// are called (`alforria_core::shell_env::learn`).
pub(crate) fn provider_env_names(config: &Config) -> impl Iterator<Item = &String> {
    config
        .provider
        .iter()
        .flatten()
        .flat_map(|(_, provider)| provider.env.iter().flatten())
}

impl ServerPolicy {
    /// Resolve the policy from a loaded host config, expanding `~/`
    /// against `home` (the context's, never the process's).
    pub fn from_config_in(config: &Config, home: &Path) -> Result<ServerPolicy, String> {
        let server = config.server.as_ref();
        let bearer = match server
            .and_then(|server| server.auth.as_ref())
            .and_then(|auth| auth.bearer.as_ref())
        {
            Some(info) => crate::bearer::BearerAuth::from_config(info)?.map(Arc::new),
            None => None,
        };
        let directories = DirectoryAllowlist::new(
            server
                .and_then(|server| server.directories.as_deref())
                .unwrap_or_default(),
            home,
        );
        let mut shutdown = ShutdownOptions::default();
        if let Some(info) = server.and_then(|server| server.shutdown.as_ref()) {
            if let Some(timeout) = info.timeout {
                shutdown.timeout = timeout.get();
            }
            if let Some(sessions) = info.sessions {
                shutdown.sessions = sessions;
            }
        }
        use alforria_core::config::host::{PermissionOnTimeout, QuestionOnTimeout};
        use alforria_core::session::permission::{OnTimeout, Unattended};
        let unattended_permission = match config
            .permission
            .as_ref()
            .and_then(|permission| permission.unattended.as_ref())
        {
            None => None,
            Some(info) => {
                let on_timeout = match info.on_timeout.unwrap_or_default() {
                    PermissionOnTimeout::Wait => None,
                    PermissionOnTimeout::Deny => Some(OnTimeout::Deny),
                    PermissionOnTimeout::Allow => Some(OnTimeout::Allow),
                };
                match (on_timeout, info.timeout) {
                    (None, _) => None,
                    (Some(on_timeout), Some(timeout)) => Some(Unattended {
                        timeout: timeout.get(),
                        on_timeout,
                    }),
                    (Some(_), None) => return Err(
                        "permission.unattended.timeout is required unless on_timeout is \"wait\""
                            .to_string(),
                    ),
                }
            }
        };
        let unattended_question = match config
            .question
            .as_ref()
            .and_then(|question| question.unattended.as_ref())
        {
            None => None,
            Some(info) => match (info.on_timeout.unwrap_or_default(), info.timeout) {
                (QuestionOnTimeout::Wait, _) => None,
                (QuestionOnTimeout::Reject, Some(timeout)) => Some(timeout.get()),
                (QuestionOnTimeout::Reject, None) => {
                    return Err(
                        "question.unattended.timeout is required unless on_timeout is \"wait\""
                            .to_string(),
                    )
                }
            },
        };
        // The restricted posture — bearer auth, or the host's own switch
        // (`server.trust.project: "restricted"`): credential-like names
        // are denied and a child starts from an allowlist. A standalone
        // install keeps the user's environment. In either one the
        // variables the host's providers read their keys from are withheld
        // (`ServerPolicy::load` adds the `{env:}` references, the server
        // the model catalog's and each instance's).
        let restricted = bearer.is_some()
            || server
                .and_then(|server| server.trust.as_ref())
                .and_then(|trust| trust.project)
                == Some(alforria_core::config::host::ProjectTrust::Restricted);
        let mut shell_env =
            alforria_core::shell_env::ShellEnvPolicy::from_config(config.shell_env.as_ref())
                .restricted(restricted);
        shell_env.learn(
            alforria_core::shell_env::SOURCE_HOST_PROVIDERS,
            provider_env_names(config),
        );
        Ok(ServerPolicy {
            shutdown,
            unattended_permission,
            unattended_question,
            hook: config.hook.clone(),
            bearer,
            directories,
            // Decided by the loader, from the managed directory alone
            // (`ServerPolicy::load`).
            managed_only: false,
            reply_always: server
                .and_then(|server| server.auth.as_ref())
                .and_then(|auth| auth.bearer.as_ref())
                .and_then(|bearer| bearer.reply_always)
                .unwrap_or(false),
            upload_max_bytes: server
                .and_then(|server| server.upload.as_ref())
                .and_then(|upload| upload.max_bytes)
                .unwrap_or(DEFAULT_UPLOAD_MAX_BYTES),
            upload_directories: server
                .and_then(|server| server.upload.as_ref())
                .and_then(|upload| upload.directories.clone())
                .unwrap_or_else(|| vec![DEFAULT_UPLOAD_DIRECTORY.to_string()]),
            shell_env,
            config_error: None,
        })
    }

    /// Load the host config under `paths` and resolve the policy. A config
    /// that does not load, or does not resolve, leaves the defaults in
    /// place with `config_error` set.
    pub fn load(paths: &alforria_core::GlobalPaths) -> ServerPolicy {
        let resolved = alforria_core::config::host::load_host_config(paths)
            .map_err(|err| err.to_string())
            .and_then(|config| ServerPolicy::from_config_in(&config, &paths.home));
        match resolved {
            Ok(mut policy) => {
                let loader = alforria_core::ConfigLoader::new();
                let params = alforria_core::LoadParams::new(paths.home.clone(), paths.clone());
                policy.managed_only = loader.host_mode(&params).managed_only;
                // What the host config reads through `{env:NAME}` stays
                // out of the agent's shells.
                policy.shell_env.learn(
                    alforria_core::shell_env::SOURCE_HOST_REFERENCES,
                    loader.host_env_references(&params),
                );
                policy
            }
            Err(error) => {
                tracing::warn!("host config not applied: {error}");
                ServerPolicy {
                    config_error: Some(error),
                    ..ServerPolicy::default()
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(value: serde_json::Value) -> Config {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn shutdown_defaults_to_a_ten_second_abort() {
        let policy =
            ServerPolicy::from_config_in(&config(serde_json::json!({})), Path::new("/home/test"))
                .unwrap();
        assert_eq!(policy.shutdown, ShutdownOptions::default());
        assert_eq!(policy.shutdown.timeout, Duration::from_secs(10));
        assert_eq!(policy.shutdown.sessions, ShutdownSessions::Abort);
    }

    #[test]
    fn shutdown_reads_the_configured_deadline_and_mode() {
        let policy = ServerPolicy::from_config_in(
            &config(serde_json::json!({
                "server": { "shutdown": { "timeout": "45s", "sessions": "drain" } }
            })),
            Path::new("/home/test"),
        )
        .unwrap();
        assert_eq!(policy.shutdown.timeout, Duration::from_secs(45));
        assert_eq!(policy.shutdown.sessions, ShutdownSessions::Drain);
    }

    fn names(env: &[(std::ffi::OsString, std::ffi::OsString)]) -> Vec<String> {
        let mut names: Vec<String> = env
            .iter()
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// The server's environment in these tests: made up, and passed in.
    fn server_env() -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
        [
            ("PATH", "/usr/bin"),
            ("HOME", "/home/agent"),
            ("TERM", "xterm"),
            ("EDITOR", "vi"),
            ("GITHUB_TOKEN", "ghp-leak"),
            ("github_token", "ghp-lower-leak"),
            ("AWS_SECRET_ACCESS_KEY", "aws-leak"),
            ("HF_TOKEN", "hf-leak"),
            ("OPENCODE_SERVER_PASSWORD", "password-leak"),
            ("ALFORRIA_WEBHOOK_SECRET", "webhook-leak"),
            ("LIBERTAI_API_KEY", "builtin-leak"),
            ("ACME_GATEWAY_AUTH", "provider-leak"),
            ("acme_gateway_auth", "provider-lower-leak"),
            ("ACME_REFERENCED", "referenced-leak"),
        ]
        .into_iter()
        .map(|(name, value)| (name.into(), value.into()))
        .collect()
    }

    // sha256("operator-token")
    const DIGEST: &str = "0850123315d21ab90f4f7236408a52ef6dbd6a02a6550e5c10dc73f4d993680e";

    /// A standalone server — no bearer auth, no host switch — is the
    /// user's own machine: its shells keep the user's credentials. What
    /// they lose is alforria's own (the password, the webhook secret) and
    /// the variables a provider's key is read from, built in or of any
    /// name the host's config gives one.
    #[test]
    fn a_standalone_shell_keeps_the_users_credentials_and_loses_provider_key_variables() {
        let policy = ServerPolicy::from_config_in(
            &config(serde_json::json!({
                "provider": { "acme": { "env": ["ACME_GATEWAY_AUTH"] } }
            })),
            Path::new("/home/test"),
        )
        .unwrap();
        assert!(!policy.shell_env.is_restricted());
        let kept = [
            "ACME_REFERENCED",
            "AWS_SECRET_ACCESS_KEY",
            "EDITOR",
            "GITHUB_TOKEN",
            "HF_TOKEN",
            "HOME",
            "PATH",
            "TERM",
            "github_token",
        ];
        assert_eq!(names(&policy.shell_env.shell_env_from(server_env())), kept);
        // And so do the helpers the server starts.
        assert_eq!(
            names(&policy.shell_env.scrubbed_env_from(server_env())),
            kept
        );

        // `shell_env.allow` hands a provider's variable over, and
        // `shell_env.deny` takes a credential away.
        let policy = ServerPolicy::from_config_in(
            &config(serde_json::json!({
                "provider": { "acme": { "env": ["ACME_GATEWAY_AUTH"] } },
                "shell_env": { "allow": ["ACME_GATEWAY_AUTH"], "deny": ["*_TOKEN", "AWS_*"] }
            })),
            Path::new("/home/test"),
        )
        .unwrap();
        assert_eq!(
            names(&policy.shell_env.shell_env_from(server_env())),
            [
                "ACME_GATEWAY_AUTH",
                "ACME_REFERENCED",
                "EDITOR",
                "HOME",
                "PATH",
                "TERM"
            ]
        );
    }

    /// The host's own switch (`server.trust.project: "restricted"`) puts
    /// the shell environment in the restricted posture without bearer
    /// auth; `"full"` beside bearer auth does not take it out.
    #[test]
    fn the_host_switch_restricts_the_shell_environment() {
        let policy = ServerPolicy::from_config_in(
            &config(serde_json::json!({ "server": { "trust": { "project": "restricted" } } })),
            Path::new("/home/test"),
        )
        .unwrap();
        assert!(policy.bearer.is_none());
        assert!(policy.shell_env.is_restricted());
        assert_eq!(
            names(&policy.shell_env.shell_env_from(server_env())),
            ["HOME", "PATH", "TERM"]
        );
        assert_eq!(
            names(&policy.shell_env.scrubbed_env_from(server_env())),
            ["HOME", "PATH", "TERM"]
        );

        let policy = ServerPolicy::from_config_in(
            &config(serde_json::json!({ "server": { "trust": { "project": "full" } } })),
            Path::new("/home/test"),
        )
        .unwrap();
        assert!(!policy.shell_env.is_restricted());

        let policy = ServerPolicy::from_config_in(
            &config(serde_json::json!({ "server": {
                "auth": { "bearer": { "tokens_sha256": [DIGEST] } },
                "trust": { "project": "full" }
            } })),
            Path::new("/home/test"),
        )
        .unwrap();
        assert!(policy.shell_env.is_restricted());
    }

    /// With bearer auth the shell — and a helper — starts from the
    /// allowlist and what the host adds to it.
    #[test]
    fn a_bearer_server_shell_starts_from_the_allowlist() {
        let bearer = serde_json::json!({ "auth": { "bearer": { "tokens_sha256": [DIGEST] } } });
        let policy = ServerPolicy::from_config_in(
            &config(serde_json::json!({ "server": bearer })),
            Path::new("/home/test"),
        )
        .unwrap();
        assert!(policy.bearer.is_some());
        assert!(policy.shell_env.is_restricted());
        assert_eq!(
            names(&policy.shell_env.shell_env_from(server_env())),
            ["HOME", "PATH", "TERM"]
        );
        assert_eq!(
            names(&policy.shell_env.scrubbed_env_from(server_env())),
            ["HOME", "PATH", "TERM"]
        );

        let policy = ServerPolicy::from_config_in(
            &config(serde_json::json!({
                "server": bearer,
                "shell_env": { "allow": ["EDITOR", "HF_TOKEN"] }
            })),
            Path::new("/home/test"),
        )
        .unwrap();
        assert_eq!(
            names(&policy.shell_env.shell_env_from(server_env())),
            ["EDITOR", "HF_TOKEN", "HOME", "PATH", "TERM"]
        );
    }

    /// A variable the host config reads through `{env:NAME}` is one the
    /// host keeps a secret in: the loaded policy withholds it.
    #[test]
    fn a_variable_the_host_config_refers_to_is_withheld() {
        let dir = tempfile::tempdir().unwrap();
        let paths = alforria_core::GlobalPaths::resolve_with(dir.path().join("home"), |_| None);
        std::fs::create_dir_all(&paths.config).unwrap();
        std::fs::create_dir_all(&paths.home).unwrap();
        std::fs::write(
            paths.config.join("opencode.json"),
            r#"{
                "username": "{env:ACME_REFERENCED}",
                "provider": { "acme": { "env": ["ACME_GATEWAY_AUTH"] } }
            }"#,
        )
        .unwrap();
        let policy = ServerPolicy::load(&paths);
        assert_eq!(policy.config_error, None);
        assert_eq!(
            names(&policy.shell_env.shell_env_from(server_env())),
            [
                "AWS_SECRET_ACCESS_KEY",
                "EDITOR",
                "GITHUB_TOKEN",
                "HF_TOKEN",
                "HOME",
                "PATH",
                "TERM",
                "github_token"
            ]
        );
        assert!(policy.shell_env.denied("acme_referenced"));
    }
}

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

/// One reason a server counts as unattended or remotely reachable
/// ([`ServerPolicy::unattended_or_remote`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PostureReason {
    /// `server.auth.bearer` is configured.
    Bearer,
    /// `server.trust.project` is `"restricted"`: the host's own switch.
    HostSwitch,
    /// The listener is bound to an address that is not loopback.
    RemoteBind,
    /// `permission.unattended` answers an ask nobody answered.
    UnattendedPermission,
    /// `question.unattended` dismisses a question nobody answered.
    UnattendedQuestion,
    /// `hook.webhook` delivers events to another machine.
    Webhook,
    /// Triggers are enabled and a `webhook` or `watch` trigger exists, in
    /// the host config or stored: runs start with nobody asking.
    OutsideTrigger,
}

impl PostureReason {
    /// The name the log line and `GET /server/posture` give it.
    pub fn as_str(self) -> &'static str {
        match self {
            PostureReason::Bearer => "bearer-auth",
            PostureReason::HostSwitch => "trust-project-restricted",
            PostureReason::RemoteBind => "non-loopback-bind",
            PostureReason::UnattendedPermission => "permission-unattended",
            PostureReason::UnattendedQuestion => "question-unattended",
            PostureReason::Webhook => "hook-webhook",
            PostureReason::OutsideTrigger => "webhook-or-watch-trigger",
        }
    }
}

/// Whether `hostname` — what a listener is asked to bind — is a loopback
/// address: `localhost`, or an IP address in `127.0.0.0/8` or `::1`. Any
/// other name or address can be reached from another machine.
pub fn is_loopback_bind(hostname: &str) -> bool {
    let host = hostname
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']');
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .is_ok_and(|address| address.is_loopback())
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
            .map(|path| Self::landing(&path))
            .collect();
        if roots.is_empty() {
            roots.push(Self::landing(&crate::state::cwd()));
        }
        DirectoryAllowlist { roots }
    }

    /// Where an operator's entry is: the directory an instance opened under
    /// that spelling acts in ([`crate::state::resolve_directory`]), as the
    /// kernel finds it (`protect::resolve` — an entry that cannot be
    /// resolved keeps its spelling, which then matches nothing resolved).
    fn landing(entry: &Path) -> PathBuf {
        alforria_core::protect::resolve(&crate::state::resolve_directory(entry))
    }

    /// Whether `directory` — a spelling off a request, or a directory
    /// already resolved — is one of the allowed directories.
    ///
    /// Two readings of it exist and both must be the same allowed place.
    /// A handler acts in [`crate::state::resolve_directory`] of the
    /// spelling, which is upstream's `FSUtil.resolve` and stays as it is
    /// for every other caller: `..` is folded out of the spelling first,
    /// and a directory that does not exist yet keeps its spelling, links in
    /// it unread. The kernel reads the spelling its own way
    /// (`protect::resolve_path`): `allowed/link/../x` is next to where
    /// `link` leads, and `/a/link/new` is created where `link` leads. So
    /// the directory the handler would act in is resolved as the kernel
    /// will resolve it when the instance touches its files, the spelling
    /// itself likewise, and a spelling the two readings disagree about —
    /// or that cannot be resolved — is not allowed.
    pub fn allows(&self, directory: &Path) -> bool {
        use alforria_core::protect::resolve_path;
        let spelled = if directory.is_absolute() {
            directory.to_path_buf()
        } else {
            crate::state::cwd().join(directory)
        };
        let acted_in = crate::state::resolve_directory(directory);
        match (resolve_path(&spelled), resolve_path(&acted_in)) {
            (Ok(spelled), Ok(acted_in)) => spelled == acted_in && self.roots.contains(&acted_in),
            _ => false,
        }
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
    /// Why this server counts as unattended or remotely reachable; empty
    /// on a developer's own loopback server
    /// ([`ServerPolicy::unattended_or_remote`]). Sorted, each once.
    pub posture: Vec<PostureReason>,
    /// Whether triggers are enabled (`triggers.enabled`, on by default):
    /// what a stored trigger needs to count ([`ServerPolicy::observe`]).
    pub triggers_enabled: bool,
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
            posture: Vec::new(),
            triggers_enabled: true,
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
        // What the host config alone says of `unattended_or_remote`; the
        // listener and the stored triggers are added by
        // `ServerPolicy::observe`.
        let triggers_enabled = config
            .triggers
            .as_ref()
            .and_then(|triggers| triggers.enabled)
            .unwrap_or(true);
        let mut posture = Vec::new();
        if bearer.is_some() {
            posture.push(PostureReason::Bearer);
        }
        if server
            .and_then(|server| server.trust.as_ref())
            .and_then(|trust| trust.project)
            == Some(alforria_core::config::host::ProjectTrust::Restricted)
        {
            posture.push(PostureReason::HostSwitch);
        }
        if unattended_permission.is_some() {
            posture.push(PostureReason::UnattendedPermission);
        }
        if unattended_question.is_some() {
            posture.push(PostureReason::UnattendedQuestion);
        }
        if config
            .hook
            .as_ref()
            .and_then(|hook| hook.webhook.as_ref())
            .is_some_and(|webhooks| !webhooks.is_empty())
        {
            posture.push(PostureReason::Webhook);
        }
        if triggers_enabled
            && config
                .trigger
                .iter()
                .flatten()
                .any(|(_, spec)| spec.starts_from_outside())
        {
            posture.push(PostureReason::OutsideTrigger);
        }
        // The restricted posture of the shell environment: credential-like
        // names are denied and a child starts from an allowlist. A
        // developer's own loopback server keeps the user's environment. In
        // either one the variables the host's providers read their keys
        // from are withheld (`ServerPolicy::load` adds the `{env:}`
        // references, the server the model catalog's and each instance's).
        let mut shell_env =
            alforria_core::shell_env::ShellEnvPolicy::from_config(config.shell_env.as_ref())
                .restricted(!posture.is_empty());
        shell_env.deny_names(provider_env_names(config));
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
            posture,
            triggers_enabled,
            config_error: None,
        })
    }

    /// Whether this server is unattended or can be reached from another
    /// machine: bearer auth is configured; or the listener is bound to a
    /// non-loopback address; or an unattended answer
    /// (`permission.unattended`, `question.unattended`), a webhook
    /// (`hook.webhook`) or a `webhook` / `watch` trigger is configured; or
    /// the host said so itself (`server.trust.project: "restricted"`).
    ///
    /// It decides ONE thing: the posture of the shell environment
    /// (`alforria_core::shell_env`) — nobody is at such a server's terminal
    /// to own the credentials in its environment. What a project's config
    /// may set, what `{env:}` and `{file:}` it may read and what a token
    /// may do are decided elsewhere and more narrowly
    /// (`alforria_core::config::HostMode::restricted`, the bearer scopes),
    /// and do not follow this.
    pub fn unattended_or_remote(&self) -> bool {
        !self.posture.is_empty()
    }

    /// Add what only the running server knows to
    /// [`ServerPolicy::unattended_or_remote`]: the address the listener
    /// binds (`hostname`) and whether a stored trigger is started from
    /// outside (`outside_trigger`, counted while triggers are enabled).
    /// The shell environment's posture follows.
    pub fn observe(&mut self, hostname: &str, outside_trigger: bool) {
        if !is_loopback_bind(hostname) {
            self.posture.push(PostureReason::RemoteBind);
        }
        if outside_trigger && self.triggers_enabled {
            self.posture.push(PostureReason::OutsideTrigger);
        }
        self.posture.sort();
        self.posture.dedup();
        self.shell_env = std::mem::take(&mut self.shell_env).restricted(!self.posture.is_empty());
    }

    /// One line for the log: the shell environment's posture and why.
    pub fn posture_summary(&self) -> String {
        if self.posture.is_empty() {
            "shell environment: standalone (a loopback server with no unattended feature): \
             the agent's processes inherit the environment except alforria's own secrets and \
             provider keys"
                .to_string()
        } else {
            let reasons: Vec<&str> = self.posture.iter().map(|reason| reason.as_str()).collect();
            format!(
                "shell environment: restricted ({}): the agent's processes start from the \
                 allowlist and credential-like variables are withheld; see shell_env in the \
                 host config",
                reasons.join(", ")
            )
        }
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
                policy
                    .shell_env
                    .deny_names(loader.host_env_references(&params));
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

    /// The allowlist is asked where the kernel lands a spelling, and where
    /// the kernel finds the directory a handler would act in for it; a
    /// spelling the two disagree about is not allowed.
    #[cfg(unix)]
    #[test]
    fn the_allowlist_judges_a_directory_where_the_kernel_lands_it() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        for sub in ["repo", "b/deep", "c"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        symlink(root.join("b/deep"), root.join("repo/out")).unwrap();
        symlink(root.join("b"), root.join("link")).unwrap();
        let text = |path: PathBuf| path.display().to_string();
        let list = DirectoryAllowlist::new(
            &[text(root.join("repo")), text(root.join("link/new"))],
            Path::new(""),
        );
        // An allowed directory under its own and under an honest spelling.
        assert!(list.allows(&root.join("repo")));
        assert!(list.allows(&root.join("repo/../repo/.")));
        // `repo/out/..` is `b` for the kernel and `repo` once `..` is
        // folded out of the spelling: the two readings disagree.
        assert!(!list.allows(&root.join("repo/out/..")));
        assert!(!list.allows(&root.join("b")));
        // What cannot be resolved is not allowed.
        assert!(!list.allows(&root.join("nowhere/../repo")));

        // A listed directory that does not exist yet, behind a symlinked
        // parent, is where the link leads: under either name…
        assert_eq!(list.roots()[1], root.join("b/new"));
        assert!(list.allows(&root.join("link/new")));
        assert!(list.allows(&root.join("b/new")));
        // …and it is still allowed once it exists.
        std::fs::create_dir_all(root.join("b/new")).unwrap();
        assert!(list.allows(&root.join("link/new")));
        assert!(list.allows(&root.join("b/new")));
        std::fs::remove_dir(root.join("b/new")).unwrap();
        // A link repointed after the list was built does not carry the
        // entry with it: the spelling now leads somewhere never listed.
        std::fs::remove_file(root.join("link")).unwrap();
        symlink(root.join("c"), root.join("link")).unwrap();
        assert!(!list.allows(&root.join("link/new")));
        assert!(list.allows(&root.join("b/new")));
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

    fn observed(config_value: serde_json::Value, hostname: &str, stored: bool) -> ServerPolicy {
        let mut policy =
            ServerPolicy::from_config_in(&config(config_value), Path::new("/home/test")).unwrap();
        policy.observe(hostname, stored);
        policy
    }

    /// `unattended_or_remote`, one trigger of it at a time: each puts the
    /// shell environment in the restricted posture, and a developer's own
    /// loopback server with none of them stays standalone.
    #[test]
    fn each_sign_of_an_unattended_or_remote_server_restricts_the_shell_environment() {
        use PostureReason::*;
        let webhook_trigger = serde_json::json!({ "trigger": {
            "inbox": { "on": { "webhook": true }, "prompt": "Triage" }
        } });
        let watch_trigger = serde_json::json!({ "trigger": {
            "drop": { "on": { "watch": { "path": "inbox" } }, "prompt": "Triage" }
        } });
        let cases: Vec<(&str, serde_json::Value, &str, bool, Vec<PostureReason>)> = vec![
            (
                "bearer auth",
                serde_json::json!({ "server": { "auth": { "bearer": { "tokens_sha256": [DIGEST] } } } }),
                "127.0.0.1",
                false,
                vec![Bearer],
            ),
            (
                "the host switch",
                serde_json::json!({ "server": { "trust": { "project": "restricted" } } }),
                "127.0.0.1",
                false,
                vec![HostSwitch],
            ),
            (
                "every interface",
                serde_json::json!({}),
                "0.0.0.0",
                false,
                vec![RemoteBind],
            ),
            (
                "a LAN address",
                serde_json::json!({}),
                "192.168.1.20",
                false,
                vec![RemoteBind],
            ),
            (
                "a host name",
                serde_json::json!({}),
                "agent.example.org",
                false,
                vec![RemoteBind],
            ),
            (
                "every IPv6 interface",
                serde_json::json!({}),
                "::",
                false,
                vec![RemoteBind],
            ),
            (
                "an unattended permission answer",
                serde_json::json!({ "permission": { "unattended": { "timeout": "5m", "on_timeout": "deny" } } }),
                "127.0.0.1",
                false,
                vec![UnattendedPermission],
            ),
            (
                "an unattended question answer",
                serde_json::json!({ "question": { "unattended": { "timeout": "5m", "on_timeout": "reject" } } }),
                "127.0.0.1",
                false,
                vec![UnattendedQuestion],
            ),
            (
                "a webhook",
                serde_json::json!({ "hook": { "webhook": [
                    { "url": "https://hooks.example.org/a", "events": ["session.idle"] }
                ] } }),
                "127.0.0.1",
                false,
                vec![Webhook],
            ),
            (
                "a configured webhook trigger",
                webhook_trigger.clone(),
                "127.0.0.1",
                false,
                vec![OutsideTrigger],
            ),
            (
                "a configured watch trigger",
                watch_trigger,
                "127.0.0.1",
                false,
                vec![OutsideTrigger],
            ),
            (
                "a stored webhook or watch trigger",
                serde_json::json!({}),
                "127.0.0.1",
                true,
                vec![OutsideTrigger],
            ),
            (
                "several at once",
                serde_json::json!({
                    "server": { "auth": { "bearer": { "tokens_sha256": [DIGEST] } } },
                    "trigger": { "inbox": { "on": { "webhook": true }, "prompt": "Triage" } }
                }),
                "0.0.0.0",
                true,
                vec![Bearer, RemoteBind, OutsideTrigger],
            ),
        ];
        for (what, config_value, hostname, stored, reasons) in cases {
            let policy = observed(config_value, hostname, stored);
            assert_eq!(policy.posture, reasons, "{what}");
            assert!(policy.unattended_or_remote(), "{what}");
            assert!(policy.shell_env.is_restricted(), "{what}");
            assert_eq!(
                names(&policy.shell_env.shell_env_from(server_env())),
                ["HOME", "PATH", "TERM"],
                "{what}"
            );
            assert!(policy.posture_summary().contains("restricted"), "{what}");
            for reason in &policy.posture {
                assert!(policy.posture_summary().contains(reason.as_str()), "{what}");
            }
        }

        // None of them: a developer's own server.
        let local: Vec<(&str, serde_json::Value, &str, bool)> = vec![
            ("the default", serde_json::json!({}), "127.0.0.1", false),
            ("localhost", serde_json::json!({}), "localhost", false),
            ("IPv6 loopback", serde_json::json!({}), "::1", false),
            (
                "bracketed IPv6 loopback",
                serde_json::json!({}),
                "[::1]",
                false,
            ),
            (
                "asks that wait",
                serde_json::json!({
                    "permission": { "unattended": { "on_timeout": "wait" } },
                    "question": { "unattended": { "on_timeout": "wait" } }
                }),
                "127.0.0.1",
                false,
            ),
            (
                "a clock trigger",
                serde_json::json!({ "trigger": {
                    "nightly": { "on": { "cron": "0 3 * * *" }, "prompt": "Tidy up" }
                } }),
                "127.0.0.1",
                false,
            ),
            (
                "a disabled webhook trigger",
                serde_json::json!({ "trigger": {
                    "inbox": { "on": { "webhook": true }, "prompt": "Triage", "enabled": false }
                } }),
                "127.0.0.1",
                false,
            ),
            (
                "triggers turned off",
                {
                    let mut value = webhook_trigger.clone();
                    value["triggers"] = serde_json::json!({ "enabled": false });
                    value
                },
                "127.0.0.1",
                true,
            ),
            (
                "no webhooks listed",
                serde_json::json!({ "hook": { "webhook": [] } }),
                "127.0.0.1",
                false,
            ),
            (
                "project config trusted in full",
                serde_json::json!({ "server": { "trust": { "project": "full" } } }),
                "127.0.0.1",
                false,
            ),
        ];
        for (what, config_value, hostname, stored) in local {
            let policy = observed(config_value, hostname, stored);
            assert_eq!(policy.posture, [], "{what}");
            assert!(!policy.unattended_or_remote(), "{what}");
            assert!(!policy.shell_env.is_restricted(), "{what}");
            assert!(
                names(&policy.shell_env.shell_env_from(server_env()))
                    .contains(&"GITHUB_TOKEN".to_string()),
                "{what}"
            );
            assert!(policy.posture_summary().contains("standalone"), "{what}");
        }
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

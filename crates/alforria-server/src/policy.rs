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
    /// `server.auth.bearer.reply_always`: whether a credential below
    /// `admin` may answer a permission request `always`.
    pub reply_always: bool,
    /// `server.upload.max_bytes`: the largest body `PUT /file/content`
    /// takes.
    pub upload_max_bytes: u64,
    /// `server.upload.directories`: where, inside the project, a
    /// credential below `admin` may upload.
    pub upload_directories: Vec<String>,
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
            reply_always: false,
            upload_max_bytes: DEFAULT_UPLOAD_MAX_BYTES,
            upload_directories: vec![DEFAULT_UPLOAD_DIRECTORY.to_string()],
            shell_env: alforria_core::shell_env::ShellEnvPolicy::default(),
            config_error: None,
        }
    }
}

impl ServerPolicy {
    /// Resolve the policy from a loaded host config.
    pub fn from_config(config: &Config) -> Result<ServerPolicy, String> {
        ServerPolicy::from_config_in(config, &alforria_core::GlobalPaths::home_from_env())
    }

    /// [`ServerPolicy::from_config`], expanding `~/` against `home`.
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
        Ok(ServerPolicy {
            shutdown,
            bearer,
            directories,
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
            shell_env: alforria_core::shell_env::ShellEnvPolicy::from_config(
                config.shell_env.as_ref(),
            ),
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
            Ok(policy) => policy,
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
        let policy = ServerPolicy::from_config(&config(serde_json::json!({}))).unwrap();
        assert_eq!(policy.shutdown, ShutdownOptions::default());
        assert_eq!(policy.shutdown.timeout, Duration::from_secs(10));
        assert_eq!(policy.shutdown.sessions, ShutdownSessions::Abort);
    }

    #[test]
    fn shutdown_reads_the_configured_deadline_and_mode() {
        let policy = ServerPolicy::from_config(&config(serde_json::json!({
            "server": { "shutdown": { "timeout": "45s", "sessions": "drain" } }
        })))
        .unwrap();
        assert_eq!(policy.shutdown.timeout, Duration::from_secs(45));
        assert_eq!(policy.shutdown.sessions, ShutdownSessions::Drain);
    }
}

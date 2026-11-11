//! The server's host policy (alforria extension): the unattended-server
//! settings resolved once from the host config
//! (`alforria_core::config::host`).

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

/// Everything the server resolved from the host config.
#[derive(Debug, Clone, Default)]
pub struct ServerPolicy {
    pub shutdown: ShutdownOptions,
    /// Why the host config could not be used, if it could not. A listener
    /// refuses to bind with this set ([`crate::listen_with`]): the policy
    /// in force would not be the one written down.
    pub config_error: Option<String>,
}

impl ServerPolicy {
    /// Resolve the policy from a loaded host config.
    pub fn from_config(config: &Config) -> Result<ServerPolicy, String> {
        let server = config.server.as_ref();
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
            config_error: None,
        })
    }

    /// Load the host config under `paths` and resolve the policy. A config
    /// that does not load, or does not resolve, leaves the defaults in
    /// place with `config_error` set.
    pub fn load(paths: &alforria_core::GlobalPaths) -> ServerPolicy {
        let resolved = alforria_core::config::host::load_host_config(paths)
            .map_err(|err| err.to_string())
            .and_then(|config| ServerPolicy::from_config(&config));
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

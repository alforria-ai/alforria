//! cli/cmd/serve.ts port — the `serve` command.

use std::ffi::OsString;

use clap::ArgMatches;

use crate::error::TypedError;
use crate::network::{self, NetworkOptions};
use crate::ui::Ui;

/// serve.ts:16-17 — printed via `console.log` (stdout) before listening.
/// Only reached with `OPENCODE_SERVER_PASSWORD` set but empty (the explicit
/// opt-out); unset, a password is generated instead.
pub const UNSECURED_WARNING: &str =
    "Warning: OPENCODE_SERVER_PASSWORD is not set; server is unsecured.";

/// `!Flag.OPENCODE_SERVER_PASSWORD` (flag.ts:32) — unset or empty is falsy.
pub fn password_set_from(value: Option<&str>) -> bool {
    value.is_some_and(|value| !value.is_empty())
}

pub fn password_set() -> bool {
    password_set_from(std::env::var("OPENCODE_SERVER_PASSWORD").ok().as_deref())
}

/// The password `serve` must generate: deliberate deviation (decided
/// 2026-10-12, `review-findings.md`) — TS only warns and serves anyone who
/// can reach the port. An unset `OPENCODE_SERVER_PASSWORD` gets a random
/// one; set (even empty, the explicit opt-out) is left alone.
pub fn generated_password_for(env: Option<&str>) -> Option<String> {
    match env {
        None => Some(alforria_server::AuthConfig::generate_password()),
        Some(_) => None,
    }
}

/// Whether the host config sets up bearer authentication
/// (`server.auth.bearer`). Such a server is not unsecured without a
/// password, and is not given a generated one: the tokens are its access
/// control, and a password printed to a supervisor's log would be a second,
/// weaker way in.
pub fn bearer_configured() -> bool {
    let paths = alforria_core::GlobalPaths::from_env();
    alforria_server::ServerPolicy::load(&paths).bearer.is_some()
}

/// The lines after the handshake telling the user how to sign in.
pub fn credentials_lines(username: &str, password: &str) -> String {
    format!("  username: {username}\n  password: {password}\n")
}

/// serve.ts:20 — the handshake line. Formats the configured hostname and the
/// actual bound port, not the listener URL.
pub fn handshake(hostname: &str, port: u16) -> String {
    format!("alforria server listening on http://{hostname}:{port}")
}

/// `--print-logs` wires `OPENCODE_PRINT_LOGS` (TS `Logger.pretty` prints to
/// stderr); `--log-level` caps the filter (`Logger.pretty`'s `level`).
/// Without `--print-logs` the subscriber stays unset and all
/// `tracing` output is dropped, matching the TS quiet server.
pub fn init_logging_from_env() {
    if std::env::var_os("OPENCODE_PRINT_LOGS").is_none() {
        return;
    }
    let level = match std::env::var("OPENCODE_LOG_LEVEL")
        .unwrap_or_default()
        .to_ascii_uppercase()
        .as_str()
    {
        "TRACE" => tracing_subscriber::filter::LevelFilter::TRACE,
        "DEBUG" => tracing_subscriber::filter::LevelFilter::DEBUG,
        "WARN" | "WARNING" => tracing_subscriber::filter::LevelFilter::WARN,
        "ERROR" => tracing_subscriber::filter::LevelFilter::ERROR,
        _ => tracing_subscriber::filter::LevelFilter::INFO,
    };
    let _ = tracing_subscriber::fmt()
        .with_max_level(level)
        .with_writer(std::io::stderr)
        .try_init();
}

pub fn run(matches: &ArgMatches, ui: &mut Ui, raw: &[OsString]) -> Result<(), TypedError> {
    init_logging_from_env();
    let env_password = std::env::var("OPENCODE_SERVER_PASSWORD").ok();
    let bearer = bearer_configured();
    let generated = if bearer {
        None
    } else {
        generated_password_for(env_password.as_deref())
    };
    if generated.is_none() && !password_set() && !bearer {
        ui.write_stdout(&format!("{UNSECURED_WARNING}\n"));
    }
    let args = NetworkOptions::from_matches(matches);
    let config = network::global_server_config();
    let opts = network::resolve(&args, raw, &config);
    let mut listen = opts.listen_options();
    listen.password = generated.clone();
    listen.schedule = true;
    let runtime = super::runtime()?;
    let listener = runtime
        .block_on(alforria_server::listen(&listen))
        .map_err(|err| TypedError::Unknown {
            raw: err.to_string(),
        })?;
    ui.write_stdout(&format!(
        "{}\n",
        handshake(&listener.hostname, listener.port)
    ));
    if let Some(password) = &generated {
        let username = alforria_server::AuthConfig::from_env().username;
        ui.write_stdout(&credentials_lines(&username, password));
    }
    // serve.ts:22 `Effect.never` — run until told to stop.
    run_until_stopped(runtime, listener);
    Ok(())
}

/// Resolves on the first request to stop: SIGTERM (a supervisor) or SIGINT
/// (ctrl-c) on unix, ctrl-c elsewhere.
pub async fn stop_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let (Ok(mut terminate), Ok(mut interrupt)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) else {
            let _ = tokio::signal::ctrl_c().await;
            return "SIGINT";
        };
        tokio::select! {
            _ = terminate.recv() => "SIGTERM",
            _ = interrupt.recv() => "SIGINT",
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "ctrl-c"
    }
}

/// Serve until a stop signal, then shut down the way the host config asks
/// (`server.shutdown`): running turns are settled, their processes killed
/// and the database checkpointed before the process exits 0. A second
/// signal skips the wait.
pub fn run_until_stopped(runtime: tokio::runtime::Runtime, listener: alforria_server::Listener) {
    runtime.block_on(async {
        let signal = stop_signal().await;
        let options = listener.shutdown_options();
        tracing::info!(signal, timeout = ?options.timeout, "shutting down");
        tokio::select! {
            report = listener.shutdown(options) => {
                tracing::info!(?report, "shutdown complete");
            }
            signal = stop_signal() => {
                tracing::warn!(signal, "second signal: exiting without waiting");
                alforria_core::process_group::kill_all();
            }
        }
    });
    // Whatever is still blocked on a thread (a tool's file read, a pipe
    // reader) must not hold the exit.
    runtime.shutdown_timeout(std::time::Duration::from_secs(1));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_line_is_byte_exact() {
        assert_eq!(
            handshake("127.0.0.1", 4096),
            "alforria server listening on http://127.0.0.1:4096"
        );
        assert_eq!(
            handshake("0.0.0.0", 1234),
            "alforria server listening on http://0.0.0.0:1234"
        );
    }

    #[test]
    fn unsecured_warning_is_byte_exact() {
        assert_eq!(
            UNSECURED_WARNING,
            "Warning: OPENCODE_SERVER_PASSWORD is not set; server is unsecured."
        );
    }

    #[test]
    fn a_password_is_generated_only_when_the_env_is_unset() {
        let first = generated_password_for(None).unwrap();
        let second = generated_password_for(None).unwrap();
        assert_eq!(first.len(), 32);
        assert_ne!(first, second);
        assert_eq!(generated_password_for(Some("")), None);
        assert_eq!(generated_password_for(Some("secret")), None);
        assert_eq!(
            credentials_lines("alforria", "pw"),
            "  username: alforria\n  password: pw\n"
        );
    }

    #[test]
    fn password_set_treats_unset_or_empty_as_missing() {
        assert!(!password_set_from(None));
        assert!(!password_set_from(Some("")));
        assert!(password_set_from(Some("0")));
        assert!(password_set_from(Some("secret")));
    }
}

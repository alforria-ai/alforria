//! `attach <url>` — attach to a running opencode server
//! (`cli/cmd/attach.ts:6-147`).

use std::ffi::OsString;

use clap::ArgMatches;

use super::tui::{handoff, validate_session};
use crate::error::{CliError, TypedError};
use crate::ui::Ui;

fn cli_error(message: impl Into<String>) -> TypedError {
    TypedError::Cli(CliError {
        message: message.into(),
        exit_code: 1,
    })
}

/// attach.ts:74-84 — chdir locally when possible; a directory that does
/// not exist locally passes through untouched (remote attach).
pub fn resolve_directory(dir: Option<&str>) -> Option<String> {
    let dir = dir?;
    if std::env::set_current_dir(dir).is_ok() {
        std::env::current_dir()
            .ok()
            .map(|cwd| cwd.to_string_lossy().into_owned())
    } else {
        Some(dir.to_string())
    }
}

pub fn run(matches: &ArgMatches, ui: &mut Ui, _raw: &[OsString]) -> Result<(), TypedError> {
    if matches.get_flag("replay") {
        return Err(cli_error(
            "--replay is not supported; replay is enabled by default",
        ));
    }
    let no_replay = matches.get_flag("no-replay");
    let replay_limit = matches.get_one::<f64>("replay-limit").copied();
    let mini = matches.get_flag("mini");

    let directory = resolve_directory(matches.get_one::<String>("dir").map(String::as_str));

    if mini {
        // N2: the mini tree is not ported; the flag falls through to
        // the full TUI with a warning (attach.ts:86-100).
        ui.error("--mini is not available; starting the full TUI");
    } else {
        let unsupported = [
            ("--no-replay", no_replay),
            ("--replay-limit", replay_limit.is_some()),
        ]
        .into_iter()
        .find_map(|(flag, shown)| if shown { Some(flag) } else { None });
        if let Some(flag) = unsupported {
            return Err(cli_error(format!("{flag} requires --mini")));
        }
    }

    let continue_ = matches.get_flag("continue");
    let session = matches.get_one::<String>("session").cloned();
    let fork = matches.get_flag("fork");
    if fork && !continue_ && session.is_none() {
        return Err(cli_error("--fork requires --continue or --session"));
    }

    let url = matches.get_one::<String>("url").expect("url").clone();
    let password = matches
        .get_one::<String>("password")
        .cloned()
        .or_else(|| std::env::var("OPENCODE_SERVER_PASSWORD").ok());
    let username = matches
        .get_one::<String>("username")
        .cloned()
        .or_else(|| std::env::var("OPENCODE_SERVER_USERNAME").ok());

    validate_session(
        &url,
        password.as_deref(),
        username.as_deref(),
        directory.as_deref(),
        session.as_deref(),
    )?;

    let headers = crate::client::auth_header(password.as_deref(), username.as_deref())
        .map(|header| vec![("authorization".to_string(), header)])
        .unwrap_or_default();

    handoff(
        &url,
        directory,
        headers,
        opencode_tui::state::Args {
            model: None,
            agent: None,
            prompt: None,
            continue_,
            session_id: session,
            fork,
            auto: false,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_directory_passes_through_when_chdir_fails() {
        let missing = resolve_directory(Some("/nonexistent-attach-dir-xyz/"));
        assert_eq!(missing.as_deref(), Some("/nonexistent-attach-dir-xyz/"));
    }
}

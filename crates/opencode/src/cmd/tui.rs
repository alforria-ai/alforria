//! `$0 [project]` — the default command: start the TUI (`cli/cmd/tui.ts`).

use std::ffi::OsString;
use std::path::Path;

use clap::ArgMatches;

use super::network;
use crate::client::{auth_header, OpencodeClient};
use crate::error::{CliError, TypedError};
use crate::network::has_arg;
use crate::ui::Ui;

/// The TUI handoff seam. The TS worker-thread + RPC machinery
/// (tui.ts:24-57,210-228) is an implementation detail; the Rust port
/// runs `opencode_tui::run` in-process. Tests replace the runner.
pub type TuiRunner = Box<
    dyn Fn(
            &str,
            Option<String>,
            Vec<(String, String)>,
            opencode_tui::state::Args,
        ) -> Result<(), TypedError>
        + Sync,
>;

fn default_tui_runner(
    url: &str,
    directory: Option<String>,
    headers: Vec<(String, String)>,
    args: opencode_tui::state::Args,
) -> Result<(), TypedError> {
    // `paths.state` (`global.ts:14`): TUI state (pinned sessions, recent
    // models, favorites, prompt history) persists under the XDG state dir.
    let state_dir = opencode_core::paths::GlobalPaths::from_env().state;
    let _exit = opencode_tui::run(opencode_tui::TuiInput {
        url: url.to_string(),
        directory,
        headers,
        args,
        config: opencode_tui::config::TuiConfig::default(),
        state_dir: Some(state_dir),
    })
    .map_err(|err| TypedError::Unknown {
        raw: err.to_string(),
    })?;
    Ok(())
}

thread_local! {
    static TUI_RUNNER: std::cell::RefCell<TuiRunner> =
        std::cell::RefCell::new(Box::new(default_tui_runner));
}

/// Replace the TUI runner for the duration of `f` (tests record the
/// handoff instead of opening a terminal).
pub fn with_tui_runner<T>(runner: TuiRunner, f: impl FnOnce() -> T) -> T {
    TUI_RUNNER.with(|cell| *cell.borrow_mut() = runner);
    let result = f();
    TUI_RUNNER.with(|cell| *cell.borrow_mut() = Box::new(default_tui_runner));
    result
}

/// Run the installed TUI runner (the seam both `$0` and `attach` use).
pub fn handoff(
    url: &str,
    directory: Option<String>,
    headers: Vec<(String, String)>,
    args: opencode_tui::state::Args,
) -> Result<(), TypedError> {
    TUI_RUNNER.with(|cell| {
        let runner = cell.borrow();
        runner(url, directory, headers, args)
    })
}

fn cli_error(message: impl Into<String>) -> TypedError {
    TypedError::Cli(CliError {
        message: message.into(),
        exit_code: 1,
    })
}

/// `resolveThreadDirectory` (tui.ts:66-70): a relative `[project]`
/// resolves from `$PWD` (not the process cwd).
pub fn resolve_thread_directory(project: Option<&str>, env_pwd: Option<&str>, cwd: &str) -> String {
    let root = Path::new(env_pwd.unwrap_or(cwd));
    let next = match project {
        Some(project) if Path::new(project).is_absolute() => Path::new(project).to_path_buf(),
        Some(project) => root.join(project),
        None => Path::new(cwd).to_path_buf(),
    };
    next.to_string_lossy().into_owned()
}

/// `input()` (tui.ts:59-64): piped stdin merged with `--prompt`.
pub fn merged_prompt(prompt: Option<&str>, piped: Option<&str>) -> Option<String> {
    match (prompt, piped) {
        (Some(value), Some(piped)) => Some(format!("{piped}\n{value}")),
        (Some(value), None) => Some(value.to_string()),
        (None, piped) => piped.map(str::to_string),
    }
}

fn piped_stdin() -> Option<String> {
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        let mut buffer = String::new();
        match std::io::Read::read_to_string(&mut std::io::stdin(), &mut buffer) {
            Ok(0) => None,
            Ok(_) => Some(buffer),
            Err(_) => None,
        }
    } else {
        None
    }
}

/// `validateSession` (`tui/validate-session.ts`): the `ses…` prefix
/// check, then `session.get` with `throwOnError`.
pub fn validate_session(
    url: &str,
    password: Option<&str>,
    username: Option<&str>,
    directory: Option<&str>,
    session_id: Option<&str>,
) -> Result<(), TypedError> {
    let Some(session_id) = session_id else {
        return Ok(());
    };
    if !session_id.starts_with("ses") {
        return Err(cli_error(format!(
            "Invalid session ID: Expected a string starting with 'ses', but got '{session_id}'"
        )));
    }
    let client = OpencodeClient::new(
        url.to_string(),
        directory.map(str::to_string),
        password,
        username,
    );
    let runtime = super::runtime()?;
    let found = runtime
        .block_on(client.session_get(session_id))
        .map_err(|error| cli_error(error.message))?;
    if found.is_none() {
        return Err(cli_error("Session not found"));
    }
    Ok(())
}

pub fn run(matches: &ArgMatches, ui: &mut Ui, raw: &[OsString]) -> Result<(), TypedError> {
    if matches.get_flag("replay") {
        return Err(cli_error(
            "--replay is not supported; replay is enabled by default",
        ));
    }
    let no_replay = matches.get_flag("no-replay");
    let replay_limit = matches.get_one::<f64>("replay-limit").copied();
    let demo = matches.get_flag("demo");
    let mini = matches.get_flag("mini");

    let argv = network::raw_args(raw);
    if mini {
        let network_flag = [
            "--port",
            "--hostname",
            "--mdns",
            "--no-mdns",
            "--mdns-domain",
            "--cors",
        ]
        .into_iter()
        .find(|option| has_arg(argv, option));
        if let Some(flag) = network_flag {
            return Err(cli_error(format!("{flag} cannot be used with --mini")));
        }
        // N2: the interactive mini tree (`cli/cmd/run/`, run.ts) is not
        // ported; the flag falls through to the full TUI with a warning.
        ui.error("--mini is not available; starting the full TUI");
    } else {
        let unsupported = [
            ("--no-replay", no_replay),
            ("--replay-limit", replay_limit.is_some()),
            ("--demo", demo),
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
    let model = matches.get_one::<String>("model").cloned();
    let agent = matches.get_one::<String>("agent").cloned();
    let prompt = matches.get_one::<String>("prompt").cloned();
    let auto = matches.get_flag("auto")
        || matches.get_flag("yolo")
        || matches.get_flag("dangerously-skip-permissions");

    if fork && !continue_ && session.is_none() {
        return Err(cli_error("--fork requires --continue or --session"));
    }

    let cwd = std::env::current_dir()
        .map_err(|err| TypedError::Unknown {
            raw: err.to_string(),
        })?
        .to_string_lossy()
        .into_owned();
    let project = matches.get_one::<String>("project").map(String::as_str);
    let next = resolve_thread_directory(project, std::env::var("PWD").ok().as_deref(), &cwd);
    if std::env::set_current_dir(&next).is_err() {
        // tui.ts:203-206 — the message prints but the exit code stays 0.
        ui.error(&format!("Failed to change directory to {next}"));
        return Ok(());
    }
    let cwd = std::env::current_dir()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or(next);

    let prompt = merged_prompt(prompt.as_deref(), piped_stdin().as_deref());

    // `resolveNetworkOptionsNoConfig` + the external-transport check
    // (tui.ts:232-234,251): an explicit `--port`/`--hostname` (or a
    // `--mdns` flag that resolves to true) boots a real server the TUI
    // targets over the network.
    let options = network::NetworkOptions::from_matches(matches);
    let resolved = network::resolve(&options, raw, &network::empty_server_config());
    let external = has_arg(argv, "--port") || has_arg(argv, "--hostname") || resolved.mdns;

    let password = std::env::var("OPENCODE_SERVER_PASSWORD").ok();
    let username = std::env::var("OPENCODE_SERVER_USERNAME").ok();
    let headers = auth_header(password.as_deref(), username.as_deref())
        .map(|header| vec![("authorization".to_string(), header)])
        .unwrap_or_default();

    let runtime = super::runtime()?;
    let listener = runtime
        .block_on(opencode_server::listen(&resolved.listen_options()))
        .map_err(|err| TypedError::Unknown {
            raw: err.to_string(),
        })?;
    let url = format!("http://127.0.0.1:{}", listener.port);

    let (url, directory) = if external {
        (url, None)
    } else {
        (url, Some(cwd.clone()))
    };

    validate_session(
        &url,
        password.as_deref(),
        username.as_deref(),
        directory.as_deref(),
        session.as_deref(),
    )?;

    handoff(
        &url,
        directory,
        headers,
        opencode_tui::state::Args {
            model,
            agent,
            prompt,
            continue_,
            session_id: session,
            fork,
            auto,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_thread_directory_is_pwd_relative() {
        assert_eq!(
            resolve_thread_directory(Some("foo"), Some("/root"), "/cwd"),
            "/root/foo"
        );
        assert_eq!(
            resolve_thread_directory(Some("/abs"), Some("/root"), "/cwd"),
            "/abs"
        );
        assert_eq!(
            resolve_thread_directory(None, Some("/root"), "/cwd"),
            "/cwd"
        );
        assert_eq!(resolve_thread_directory(None, None, "/cwd"), "/cwd");
    }

    #[test]
    fn merged_prompt_joins_piped_and_value() {
        assert_eq!(
            merged_prompt(Some("hello"), Some("piped")),
            Some("piped\nhello".to_string())
        );
        assert_eq!(
            merged_prompt(Some("hello"), None),
            Some("hello".to_string())
        );
        assert_eq!(
            merged_prompt(None, Some("piped")),
            Some("piped".to_string())
        );
        assert_eq!(merged_prompt(None, None), None);
    }
    fn err_text(error: &TypedError) -> String {
        crate::error::format_error(error).unwrap_or_default()
    }

    fn parse_args(parts: &[&str]) -> clap::ArgMatches {
        super::super::cli()
            .try_get_matches_from(std::iter::once("opencode").chain(parts.iter().copied()))
            .expect("parse")
    }

    #[test]
    fn replay_true_is_rejected() {
        let matches = parse_args(&["--replay"]);
        let (mut ui, _captured) = crate::ui::Ui::capture(false);
        let err = run(&matches, &mut ui, &[]).unwrap_err();
        let message = err_text(&err);
        assert!(message.contains("--replay is not supported"), "{message}");
    }

    #[test]
    fn no_replay_requires_mini() {
        let matches = parse_args(&["--no-replay"]);
        let (mut ui, _captured) = crate::ui::Ui::capture(false);
        let err = run(&matches, &mut ui, &[]).unwrap_err();
        assert!(err_text(&err).contains("--no-replay requires --mini"));
    }

    #[test]
    fn replay_limit_requires_mini() {
        let matches = parse_args(&["--replay-limit", "10"]);
        let (mut ui, _captured) = crate::ui::Ui::capture(false);
        let err = run(&matches, &mut ui, &[]).unwrap_err();
        assert!(err_text(&err).contains("--replay-limit requires --mini"));
    }

    #[test]
    fn demo_requires_mini() {
        let matches = parse_args(&["--demo"]);
        let (mut ui, _captured) = crate::ui::Ui::capture(false);
        let err = run(&matches, &mut ui, &[]).unwrap_err();
        assert!(err_text(&err).contains("--demo requires --mini"));
    }

    #[test]
    fn mini_with_network_flag_is_rejected() {
        let raw: Vec<std::ffi::OsString> = ["--mini", "--port", "1234"]
            .iter()
            .map(std::ffi::OsString::from)
            .collect();
        let matches = parse_args(&["--mini", "--port", "1234"]);
        let (mut ui, _captured) = crate::ui::Ui::capture(false);
        let err = run(&matches, &mut ui, &raw).unwrap_err();
        assert!(
            err_text(&err).contains("--port cannot be used with --mini"),
            "{}",
            err_text(&err)
        );
    }

    #[test]
    fn fork_requires_continue_or_session() {
        let matches = parse_args(&["--fork"]);
        let (mut ui, _captured) = crate::ui::Ui::capture(false);
        let err = run(&matches, &mut ui, &[]).unwrap_err();
        assert!(
            err_text(&err).contains("--fork requires --continue or --session"),
            "{}",
            err_text(&err)
        );
    }
}

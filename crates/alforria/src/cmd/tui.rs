//! `$0 [project]` — the default command: start the TUI (`cli/cmd/tui.ts`).

use std::ffi::OsString;
use std::path::Path;

use clap::ArgMatches;

use super::network;
use crate::client::{auth_header, OpencodeClient};
use crate::error::{CliError, TypedError};
use crate::network::has_arg;
use crate::ui::Ui;

/// Where the TUI's server lives: a URL over HTTP (`attach`, the network
/// flags) or the router called in memory (plain `alforria`).
#[derive(Debug, Clone)]
pub enum TuiTarget {
    Url(String),
    InProcess(alforria_tui::transport::backend::SharedBackend),
}

impl TuiTarget {
    /// The base URL the clients address (`http://localhost` in process).
    pub fn url(&self) -> &str {
        match self {
            TuiTarget::Url(url) => url,
            TuiTarget::InProcess(_) => alforria_tui::transport::backend::IN_PROCESS_BASE_URL,
        }
    }
}

/// The TUI handoff seam. The TS worker-thread + RPC machinery
/// (tui.ts:24-57,210-228) becomes the in-memory router backend; the
/// Rust port runs `alforria_tui::run` in-process. Tests replace the runner.
pub type TuiRunner = Box<
    dyn Fn(
            &TuiTarget,
            Option<String>,
            Vec<(String, String)>,
            alforria_tui::state::Args,
        ) -> Result<(), TypedError>
        + Sync,
>;

fn default_tui_runner(
    target: &TuiTarget,
    directory: Option<String>,
    headers: Vec<(String, String)>,
    args: alforria_tui::state::Args,
) -> Result<(), TypedError> {
    // `paths.state` (`global.ts:14`): TUI state (pinned sessions, recent
    // models, favorites, prompt history) persists under the XDG state dir.
    let paths = alforria_core::paths::GlobalPaths::from_env();
    let state_dir = paths.state.clone();
    let backend = match target {
        TuiTarget::Url(_) => None,
        TuiTarget::InProcess(backend) => Some(backend.clone()),
    };
    let _exit = alforria_tui::run(alforria_tui::TuiInput {
        url: target.url().to_string(),
        directory,
        headers,
        args,
        config: alforria_tui::config::TuiConfig::default(),
        state_dir: Some(state_dir),
        backend,
        // `Global.Path.log` (`global.ts`): the data dir's `log/`.
        log_file: Some(paths.data.join("log").join("tui.log")),
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
    target: &TuiTarget,
    directory: Option<String>,
    headers: Vec<(String, String)>,
    args: alforria_tui::state::Args,
) -> Result<(), TypedError> {
    TUI_RUNNER.with(|cell| {
        let runner = cell.borrow();
        runner(target, directory, headers, args)
    })
}

/// The URL a TUI reaches a listener on: the bound address, except that
/// an unspecified bind (`0.0.0.0`, `::`) is reached over loopback. A
/// specific IP is used as is: a server bound to `192.168.1.5` does not
/// answer on `127.0.0.1`.
pub fn listener_url(hostname: &str, port: u16) -> String {
    let host = match hostname.parse::<std::net::IpAddr>() {
        Ok(ip) if ip.is_unspecified() && ip.is_ipv4() => "127.0.0.1".to_string(),
        Ok(ip) if ip.is_unspecified() => "[::1]".to_string(),
        Ok(std::net::IpAddr::V6(ip)) => format!("[{ip}]"),
        Ok(std::net::IpAddr::V4(ip)) => ip.to_string(),
        Err(_) => hostname.to_string(),
    };
    format!("http://{host}:{port}")
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
    target: &TuiTarget,
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
    let runtime = super::runtime()?;
    let found = match target {
        TuiTarget::Url(url) => {
            let client = OpencodeClient::new(
                url.to_string(),
                directory.map(str::to_string),
                password,
                username,
            );
            runtime
                .block_on(client.session_get(session_id))
                .map_err(|error| cli_error(error.message))?
                .is_some()
        }
        TuiTarget::InProcess(backend) => {
            use alforria_tui::transport::api::percent_encode;
            use alforria_tui::transport::backend::BackendRequest;
            let mut url = format!("{}/session/{}", target.url(), percent_encode(session_id));
            if let Some(directory) = directory {
                url.push_str(&format!("?directory={}", percent_encode(directory)));
            }
            let headers = auth_header(password, username)
                .map(|header| vec![("authorization".to_string(), header)])
                .unwrap_or_default();
            let response = runtime
                .block_on(backend.0.send(BackendRequest {
                    method: reqwest::Method::GET,
                    url,
                    headers,
                    json: None,
                }))
                .map_err(|_| cli_error("session get failed"))?;
            response.is_success()
        }
    };
    if !found {
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

    // Plain `alforria` runs the server in process with no listener
    // (upstream: a worker reached over RPC, tui.ts:236-246); only the
    // network flags bind a real socket. The runtime stays alive for the
    // whole TUI: tasks the server spawns while booting run on it.
    let runtime = super::runtime()?;
    // Held until the TUI exits: dropping it stops the server.
    let mut _listener = None;
    let (target, directory) = if external {
        let listener = runtime
            .block_on(alforria_server::listen(&resolved.listen_options()))
            .map_err(|err| TypedError::Unknown {
                raw: err.to_string(),
            })?;
        let url = listener_url(&listener.hostname, listener.port);
        _listener = Some(listener);
        (TuiTarget::Url(url), None)
    } else {
        let router = runtime
            .block_on(async { alforria_server::in_process_router(&resolved.listen_options()) })
            .map_err(|err| TypedError::Unknown {
                raw: err.to_string(),
            })?;
        let backend = alforria_tui::transport::backend::ServiceBackend::new(router);
        (
            TuiTarget::InProcess(alforria_tui::transport::backend::SharedBackend(
                std::sync::Arc::new(backend),
            )),
            Some(cwd.clone()),
        )
    };

    validate_session(
        &target,
        password.as_deref(),
        username.as_deref(),
        directory.as_deref(),
        session.as_deref(),
    )?;

    handoff(
        &target,
        directory,
        headers,
        alforria_tui::state::Args {
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
            Path::new("/root").join("foo").to_string_lossy()
        );
        // "/abs" is only absolute on unix (no drive on Windows).
        #[cfg(unix)]
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
            .try_get_matches_from(std::iter::once("alforria").chain(parts.iter().copied()))
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
    fn listener_url_uses_the_bound_address() {
        assert_eq!(listener_url("127.0.0.1", 4096), "http://127.0.0.1:4096");
        assert_eq!(listener_url("0.0.0.0", 4096), "http://127.0.0.1:4096");
        assert_eq!(listener_url("::", 4096), "http://[::1]:4096");
        assert_eq!(listener_url("192.168.1.5", 80), "http://192.168.1.5:80");
        assert_eq!(listener_url("fe80::1", 80), "http://[fe80::1]:80");
        assert_eq!(listener_url("localhost", 80), "http://localhost:80");
    }

    /// Inodes of the TCP sockets this process has in LISTEN state.
    #[cfg(target_os = "linux")]
    fn listening_inodes() -> std::collections::BTreeSet<String> {
        let owned: std::collections::BTreeSet<String> = std::fs::read_dir("/proc/self/fd")
            .map(|dir| {
                dir.filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
                    .filter_map(|link| {
                        let link = link.to_string_lossy().into_owned();
                        let inode = link.strip_prefix("socket:[")?.strip_suffix(']')?;
                        Some(inode.to_string())
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut listening = std::collections::BTreeSet::new();
        for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
            let Ok(text) = std::fs::read_to_string(table) else {
                continue;
            };
            for line in text.lines().skip(1) {
                let fields: Vec<&str> = line.split_whitespace().collect();
                // `st` 0A = LISTEN; field 9 is the inode.
                if fields.len() > 9 && fields[3] == "0A" && owned.contains(fields[9]) {
                    listening.insert(fields[9].to_string());
                }
            }
        }
        listening
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn plain_tui_runs_the_server_in_process_without_a_listener() {
        let before = listening_inodes();
        let matches = parse_args(&[]);
        let (mut ui, _captured) = crate::ui::Ui::capture(false);
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let record = std::sync::Arc::clone(&seen);
        let result = with_tui_runner(
            Box::new(move |target, directory, _headers, _args| {
                let TuiTarget::InProcess(backend) = target else {
                    panic!("plain alforria must not use HTTP: {target:?}");
                };
                // The router answers in memory.
                let runtime = super::super::runtime().expect("runtime");
                let response =
                    runtime
                        .block_on(backend.0.send(
                            alforria_tui::transport::backend::BackendRequest {
                                method: reqwest::Method::GET,
                                url: format!("{}/global/health", target.url()),
                                headers: Vec::new(),
                                json: None,
                            },
                        ))
                        .expect("in-process request");
                let status = response.status;
                *record.lock().unwrap() = Some((status, directory.is_some(), listening_inodes()));
                Ok(())
            }),
            || run(&matches, &mut ui, &[]),
        );
        result.expect("tui run");
        let (status, has_directory, during) = seen.lock().unwrap().take().expect("runner ran");
        assert_eq!(status, 200);
        assert!(has_directory, "the in-process TUI is scoped to the cwd");
        let opened: Vec<_> = during.difference(&before).collect();
        assert!(opened.is_empty(), "no TCP listener opened: {opened:?}");
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

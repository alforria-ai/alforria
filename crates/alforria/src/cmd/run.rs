//! cli/cmd/run.ts — the `run` command: flags, validation, session flow and
//! the loopback-server execution driver.

use std::path::{Path, PathBuf};

use clap::ArgMatches;
use serde_json::{json, Value};

use crate::client::{ClientError, OpencodeClient};
use crate::error::{CliError, TypedError};
use crate::ui::{self, Ui};

use super::run_events::{self, apply, map_event, LoopState, Output};
use super::{run_files, run_output};

/// The parsed `run` flag surface (run.ts builder 135-262).
#[derive(Debug, Clone, Default)]
pub struct RunArgs {
    pub message: Vec<String>,
    pub command: Option<String>,
    pub r#continue: bool,
    pub session: Option<String>,
    pub fork: bool,
    pub share: bool,
    pub model: Option<String>,
    pub agent: Option<String>,
    pub format: String,
    pub file: Vec<String>,
    pub title: Option<String>,
    pub attach: Option<String>,
    pub password: Option<String>,
    pub username: Option<String>,
    pub dir: Option<String>,
    pub port: u16,
    pub variant: Option<String>,
    pub thinking: bool,
    pub mini: bool,
    pub auto: bool,
    pub yolo: bool,
    pub dangerously_skip_permissions: bool,
    pub demo: bool,
    pub replay_limit: Option<f64>,
}

impl RunArgs {
    pub fn from_matches(matches: &ArgMatches) -> Self {
        let get = |name: &str| {
            matches
                .get_one::<String>(name)
                .cloned()
                .filter(|value| !value.is_empty())
        };
        RunArgs {
            message: matches
                .get_many::<String>("message")
                .map(|values| values.cloned().collect())
                .unwrap_or_default(),
            command: get("command"),
            r#continue: matches.get_flag("continue"),
            session: get("session"),
            fork: matches.get_flag("fork"),
            share: matches.get_flag("share"),
            model: get("model"),
            agent: get("agent"),
            format: get("format").unwrap_or_else(|| "default".to_string()),
            file: matches
                .get_many::<String>("file")
                .map(|values| values.cloned().collect())
                .unwrap_or_default(),
            // `--title ""` is distinct from an absent title (run.ts:450-454).
            title: matches.get_one::<String>("title").cloned(),
            attach: get("attach"),
            password: get("password"),
            username: get("username"),
            dir: get("dir"),
            port: matches.get_one::<u16>("port").copied().unwrap_or_default(),
            variant: get("variant"),
            thinking: matches.get_flag("thinking"),
            mini: matches.get_flag("mini"),
            auto: matches.get_flag("auto"),
            yolo: matches.get_flag("yolo"),
            dangerously_skip_permissions: matches.get_flag("dangerously-skip-permissions"),
            demo: matches.get_flag("demo"),
            replay_limit: matches.get_one::<f64>("replay-limit").copied(),
        }
    }
}

/// run.ts:288-290 — message words joined with spaces; words containing
/// spaces are `"`-quoted with embedded quotes escaped.
pub fn build_message(words: &[String]) -> String {
    words
        .iter()
        .map(|arg| {
            if arg.contains(' ') {
                format!("\"{}\"", arg.replace('"', "\\\""))
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// run.ts:288-321 — the mini/replay validation sequence. `die` order is
/// binding; every failure renders via `UI.error` + exit 1.
pub fn validate(args: &RunArgs, via_run_subcommand: bool, tty: bool) -> Result<(), CliError> {
    let interactive = args.mini;
    if interactive && args.command.is_some() {
        return Err(CliError::new("--mini cannot be used with --command"));
    }
    if interactive && via_run_subcommand {
        return Err(CliError::new(
            "--mini must be used without the run subcommand",
        ));
    }
    if args.demo && !interactive {
        return Err(CliError::new("--demo requires --mini"));
    }
    if interactive && args.format == "json" {
        return Err(CliError::new("--mini cannot be used with --format json"));
    }
    if args.replay_limit.is_some() && !interactive {
        return Err(CliError::new("--replay-limit requires --mini"));
    }
    if let Some(limit) = args.replay_limit {
        if limit.fract() != 0.0 || limit <= 0.0 {
            return Err(CliError::new("--replay-limit must be a positive integer"));
        }
    }
    if interactive && !tty {
        return Err(CliError::new("--mini requires a TTY stdout"));
    }
    Ok(())
}

/// run.ts:420-423 — the message-presence check (runs after directory
/// resolution, so it lives separately from `validate`).
pub fn require_message(args: &RunArgs, message: &str) -> Result<(), CliError> {
    if message.trim().is_empty() && args.command.is_none() && !args.mini {
        return Err(CliError::new("You must provide a message or a command"));
    }
    Ok(())
}

/// run.ts:425-428.
pub fn require_fork(args: &RunArgs) -> Result<(), CliError> {
    if args.fork && !args.r#continue && args.session.is_none() {
        return Err(CliError::new("--fork requires --continue or --session"));
    }
    Ok(())
}

/// run.ts:450-454 — `undefined` passes no title, `""` truncates the message.
pub fn session_title(title: Option<&str>, message: &str) -> Option<String> {
    match title {
        None => None,
        Some("") => Some(truncate(message, 50)),
        Some(title) => Some(title.to_string()),
    }
}

fn truncate(message: &str, len: usize) -> String {
    let chars = message.chars().collect::<Vec<_>>();
    if chars.len() > len {
        let head: String = chars.into_iter().take(len).collect();
        return format!("{head}...");
    }
    message.to_string()
}

/// run.ts:430-448 — non-interactive sessions deny question/plan tools for
/// pattern `*`.
pub fn rules(interactive: bool) -> Value {
    if interactive {
        return json!([]);
    }
    json!([
        {"permission": "question", "action": "deny", "pattern": "*"},
        {"permission": "plan_enter", "action": "deny", "pattern": "*"},
        {"permission": "plan_exit", "action": "deny", "pattern": "*"},
    ])
}

// The HTTP client maps onto the session-flow seam with the TS result-tuple
// semantics: any error degrades to "no data" (`{data, error}` → `.data`).
impl SessionApi for OpencodeClient {
    async fn get(&self, id: &str) -> Option<Value> {
        self.session_get(id).await.ok().flatten()
    }

    async fn list(&self) -> Vec<Value> {
        self.session_list().await.unwrap_or_default()
    }

    async fn fork(&self, id: &str) -> Option<Value> {
        self.session_fork(id).await.ok()
    }

    async fn create(&self, body: &Value) -> Option<Value> {
        self.session_create(body.clone()).await.ok()
    }
}

/// run.ts:31-38 — split `provider/model` on the first slash.
pub fn pick_model(value: Option<&str>) -> Option<(String, String)> {
    let value = value?;
    match value.split_once('/') {
        Some((provider, rest)) => Some((provider.to_string(), rest.to_string())),
        None => Some((value.to_string(), String::new())),
    }
}

/// `FSUtil.resolve` (core/fs-util.ts:94-99) — realpath, lexical fallback.
pub fn fs_resolve(path: PathBuf) -> PathBuf {
    std::fs::canonicalize(&path).unwrap_or(path)
}

fn resolve_root() -> PathBuf {
    let base = std::env::var("PWD")
        .map(PathBuf::from)
        .or_else(|_| std::env::current_dir())
        .unwrap_or_default();
    if base.is_absolute() {
        return fs_resolve(base);
    }
    let joined = std::env::current_dir().unwrap_or_default().join(base);
    fs_resolve(joined)
}

/// run.ts:331-345 — root resolution + the `--dir` chdir (local mode only);
/// in attach mode `--dir` is a path on the remote, never a local chdir.
pub fn resolve_directory(
    root: &Path,
    dir: Option<&str>,
    attach: bool,
) -> Result<Option<PathBuf>, CliError> {
    let Some(dir) = dir else {
        return Ok(if attach {
            None
        } else {
            Some(root.to_path_buf())
        });
    };
    if attach {
        return Ok(Some(PathBuf::from(dir)));
    }
    let target = if Path::new(dir).is_absolute() {
        PathBuf::from(dir)
    } else {
        root.join(dir)
    };
    if std::env::set_current_dir(&target).is_err() {
        return Err(CliError::new(format!(
            "Failed to change directory to {dir}"
        )));
    }
    Ok(Some(std::env::current_dir().unwrap_or(target)))
}

/// run.ts:40-50 — `resolveRunInput`: piped stdin is appended to the CLI
/// message with `\n` (message wins positionally).
pub fn resolve_run_input(value: Option<&str>, piped: Option<&str>) -> Option<String> {
    let falsy = |input: Option<&str>| input.is_none_or(|input| input.is_empty());
    if falsy(value) {
        return piped.map(str::to_string);
    }
    if falsy(piped) {
        return value.map(str::to_string);
    }
    Some(format!("{}\n{}", value.unwrap(), piped.unwrap()))
}

/// run.ts:416 — piped stdin, `undefined` when stdin is a TTY.
fn read_piped_stdin() -> Option<String> {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        return None;
    }
    let mut buffer = Vec::new();
    std::io::Read::read_to_end(&mut std::io::stdin(), &mut buffer)
        .ok()
        .map(|_| String::from_utf8_lossy(&buffer).into_owned())
}

/// One resolved session (run.ts:67-71 `SessionInfo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: String,
    pub title: Option<String>,
    pub directory: Option<String>,
}

/// The session-flow seam — the `OpencodeClient` implements this against
/// the HTTP API; tests drive it with fixture data.
pub trait SessionApi {
    /// `sdk.session.get(...).catch(() => undefined)` — any failure is None.
    fn get(&self, id: &str) -> impl std::future::Future<Output = Option<Value>> + Send;
    /// `sdk.session.list()` errors degrade to "no base session".
    fn list(&self) -> impl std::future::Future<Output = Vec<Value>> + Send;
    fn fork(&self, id: &str) -> impl std::future::Future<Output = Option<Value>> + Send;
    fn create(&self, body: &Value) -> impl std::future::Future<Output = Option<Value>> + Send;
}

fn info_field(info: &Value, key: &str) -> Option<String> {
    match info.get(key) {
        Some(Value::String(value)) => Some(value.clone()),
        _ => None,
    }
}

/// `if (!id)` — an empty id is missing too (run.ts:471-475).
fn truthy_field(info: &Value, key: &str) -> Option<String> {
    info_field(info, key).filter(|value| !value.is_empty())
}

fn parentless(info: &Value) -> bool {
    info.get("parentID")
        .and_then(|v| v.as_str())
        .is_none_or(|parent| parent.is_empty())
}

/// run.ts:456-533 — the session create/continue/fork resolution. `Err`
/// maps to the caller's `Session not found` exit.
pub async fn resolve_session(
    api: &impl SessionApi,
    args: &RunArgs,
    message: &str,
) -> Result<SessionInfo, CliError> {
    if let Some(session_id) = args.session.as_deref() {
        let Some(current) = api.get(session_id).await else {
            return Err(CliError::new("Session not found"));
        };
        if args.fork {
            let Some(forked) = api.fork(session_id).await else {
                return Err(CliError::new("Session not found"));
            };
            return Ok(SessionInfo {
                id: truthy_field(&forked, "id")
                    .ok_or_else(|| CliError::new("Session not found"))?,
                title: info_field(&forked, "title").or_else(|| info_field(&current, "title")),
                directory: info_field(&forked, "directory")
                    .or_else(|| info_field(&current, "directory")),
            });
        }
        return Ok(SessionInfo {
            id: truthy_field(&current, "id").ok_or_else(|| CliError::new("Session not found"))?,
            title: info_field(&current, "title"),
            directory: info_field(&current, "directory"),
        });
    }
    let base = if args.r#continue {
        api.list().await.into_iter().find(parentless)
    } else {
        None
    };
    if let Some(base) = base {
        if args.fork {
            let Some(forked) = api
                .fork(&truthy_field(&base, "id").unwrap_or_default())
                .await
            else {
                return Err(CliError::new("Session not found"));
            };
            return Ok(SessionInfo {
                id: truthy_field(&forked, "id")
                    .ok_or_else(|| CliError::new("Session not found"))?,
                title: info_field(&forked, "title").or_else(|| info_field(&base, "title")),
                directory: info_field(&forked, "directory")
                    .or_else(|| info_field(&base, "directory")),
            });
        }
        return Ok(SessionInfo {
            id: truthy_field(&base, "id").ok_or_else(|| CliError::new("Session not found"))?,
            title: info_field(&base, "title"),
            directory: info_field(&base, "directory"),
        });
    }
    let title = session_title(args.title.as_deref(), message);
    let mut body = json!({"permission": rules(args.mini)});
    if let Some(title) = &title {
        body["title"] = json!(title);
    }
    let Some(created) = api.create(&body).await else {
        return Err(CliError::new("Session not found"));
    };
    let id = truthy_field(&created, "id").ok_or_else(|| CliError::new("Session not found"))?;
    Ok(SessionInfo {
        id,
        title: info_field(&created, "title").or(title),
        directory: info_field(&created, "directory"),
    })
}

/// run.ts:593-668 — agent validation (`localAgent`/`attachAgent` behind
/// `pickAgent`): unknown or subagent agents fall back to the default with
/// a warning line.
pub fn pick_agent(
    listing: Result<Vec<Value>, ClientError>,
    name: Option<&str>,
    attach: Option<&str>,
    ui: &mut Ui,
) -> Option<String> {
    let agent = name?;
    let agents = match listing {
        Ok(agents) => agents,
        Err(_) => {
            if let Some(attach) = attach {
                warn_line(
                    ui,
                    &format!("failed to list agents from {attach}. Falling back to default agent"),
                );
                return None;
            }
            Vec::new()
        }
    };
    let found = agents
        .iter()
        .find(|entry| entry.get("name").and_then(|v| v.as_str()) == Some(agent));
    let Some(found) = found else {
        warn_line(
            ui,
            &format!("agent \"{agent}\" not found. Falling back to default agent"),
        );
        return None;
    };
    if found.get("mode").and_then(|v| v.as_str()) == Some("subagent") {
        warn_line(
            ui,
            &format!("agent \"{agent}\" is a subagent, not a primary agent. Falling back to default agent"),
        );
        return None;
    }
    Some(agent.to_string())
}

/// `UI.println(TEXT_WARNING_BOLD + "!", TEXT_NORMAL, message)` — joined
/// with a single space (run.ts:608-611).
fn warn_line(ui: &mut Ui, message: &str) {
    ui.println(&format!(
        "{}! {}{}",
        ui::style::TEXT_WARNING_BOLD,
        ui::style::TEXT_NORMAL,
        message
    ));
}

/// run.ts:538 — `cfg.share === "auto" || flags.autoShare || args.share`.
pub fn share_gate(config_share: Option<&str>, auto_share_env: bool, share_flag: bool) -> bool {
    config_share == Some("auto") || auto_share_env || share_flag
}

/// run.ts:546 — `~  {url}` info-bold.
pub fn share_success_line(url: &str) -> String {
    format!("{}~  {url}", ui::style::TEXT_INFO_BOLD)
}

/// run.ts:542 — `!  {message}` danger-bold.
pub fn share_disabled_line(message: &str) -> String {
    format!("{}!  {message}", ui::style::TEXT_DANGER_BOLD)
}

/// run.ts:535-548 — after session creation (and in `execute`), if config
/// `share == "auto"` or the flag/env is set → call `session.share`; share
/// failures are non-fatal.
async fn share(client: &OpencodeClient, session_id: &str, share_flag: bool, ui: &mut Ui) {
    let Ok(config) = client.config_get().await else {
        return;
    };
    let config_share = config.get("share").and_then(|v| v.as_str());
    if !share_gate(
        config_share,
        alforria_server::engine::bool_env("OPENCODE_AUTO_SHARE"),
        share_flag,
    ) {
        return;
    }
    match client.session_share(session_id).await {
        Ok(data) => {
            if let Some(url) = data
                .get("share")
                .and_then(|share| share.get("url"))
                .and_then(|v| v.as_str())
            {
                ui.println(&share_success_line(url));
            }
        }
        Err(err) => {
            let message = error_message(&err);
            if message.contains("disabled") {
                ui.println(&share_disabled_line(&message));
            }
        }
    }
}

/// `current(sdk)` — attach-mode remote directory resolution (run.ts:583-593).
async fn resolve_remote_directory(client: &OpencodeClient) -> Result<String, CliError> {
    match client.path_get().await {
        Ok(value) => value
            .get("directory")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| CliError::new("Failed to resolve remote directory")),
        Err(_) => Err(CliError::new("Failed to resolve remote directory")),
    }
}

/// run.ts:845-861 — the `session.command` body. Note the raw `--model`
/// string, not the `pick()`-ed model (matches the TS reference).
pub fn command_body(args: &RunArgs, agent: Option<&str>, message: &str) -> Value {
    let mut body = json!({
        "command": args.command,
        "arguments": message,
    });
    if let Some(agent) = agent {
        body["agent"] = json!(agent);
    }
    if let Some(model) = &args.model {
        body["model"] = json!(model);
    }
    if let Some(variant) = &args.variant {
        body["variant"] = json!(variant);
    }
    body
}

/// run.ts:863-877 — the `session.prompt` body: parts = files + text.
pub fn prompt_body(args: &RunArgs, agent: Option<&str>, files: Vec<Value>, message: &str) -> Value {
    let mut parts = files;
    parts.push(json!({"type": "text", "text": message}));
    let mut body = json!({"parts": parts});
    if let Some((provider_id, model_id)) = pick_model(args.model.as_deref()) {
        body["model"] = json!({"providerID": provider_id, "modelID": model_id});
    }
    if let Some(agent) = agent {
        body["agent"] = json!(agent);
    }
    if let Some(variant) = &args.variant {
        body["variant"] = json!(variant);
    }
    body
}

/// run.ts:86 — `formatRunError` = `FormatError(error) ?? FormatUnknownError(error)`
/// over the parsed result-tuple error body.
fn format_run_error(error: &ClientError) -> String {
    if let Some(body) = &error.body {
        if let Ok(value) = serde_json::from_str::<Value>(body) {
            if let Some(formatted) = crate::error::format_json_error(&value) {
                return formatted;
            }
            return crate::error::format_json_unknown(&value);
        }
    }
    error.message.clone()
}

/// The error body as a JSON payload for `emit("error", { error })` —
/// `wrapClientError` keeps the parsed body for the result-tuple path.
fn error_payload(error: &ClientError) -> Value {
    if let Some(body) = &error.body {
        if let Ok(value) = serde_json::from_str::<Value>(body) {
            return value;
        }
    }
    Value::String(error.message.clone())
}

/// `wrapClientError`'s message extraction (`data.message` → `message`).
fn error_message(error: &ClientError) -> String {
    if let Some(body) = &error.body {
        if let Ok(value) = serde_json::from_str::<Value>(body) {
            if let Some(message) = value
                .get("data")
                .and_then(|data| data.get("message"))
                .and_then(|message| message.as_str())
            {
                return message.to_string();
            }
            if let Some(message) = value.get("message").and_then(|m| m.as_str()) {
                return message.to_string();
            }
        }
    }
    error.message.clone()
}

/// Entry point for the `run` subcommand.
pub fn run(matches: &ArgMatches, ui: &mut Ui) -> Result<(), TypedError> {
    let args = RunArgs::from_matches(matches);
    handler(&args, ui)
}

fn handler(args: &RunArgs, ui: &mut Ui) -> Result<(), TypedError> {
    let message = build_message(&args.message);
    validate(args, true, ui.is_tty())?;
    // The `--mini` interactive mode (`run/runtime.ts`) is not ported; fail
    // loudly rather than silently consuming the prompt single-shot.
    if args.mini {
        return Err(CliError::new("--mini interactive mode is not supported").into());
    }

    let root = resolve_root();
    let directory = resolve_directory(&root, args.dir.as_deref(), args.attach.is_some())?;

    // File paths resolve against the local root in attach mode
    // (run.ts:368 — the remote never sees the local chdir).
    let file_base = if args.attach.is_some() {
        root.clone()
    } else {
        directory.clone().unwrap_or_else(|| root.clone())
    };
    let files = run_files::resolve_files(args.attach.is_some(), &file_base, &args.file)?;

    let piped = read_piped_stdin();
    let message = resolve_run_input(Some(&message), piped.as_deref()).unwrap_or_default();
    require_message(args, &message)?;
    require_fork(args)?;

    let runtime = super::runtime()?;
    runtime.block_on(execute(args, ui, &root, directory, files, &message))
}

async fn execute(
    args: &RunArgs,
    ui: &mut Ui,
    root: &Path,
    directory: Option<PathBuf>,
    files: Vec<Value>,
    message: &str,
) -> Result<(), TypedError> {
    let attach = args.attach.clone();
    // `_listener` keeps the loopback server bound for the command's life.
    let _listener = if attach.is_none() {
        Some(
            alforria_server::listen(&alforria_server::ListenOptions {
                port: args.port,
                hostname: "127.0.0.1".to_string(),
                cors: Vec::new(),
            })
            .await
            .map_err(|err| TypedError::Unknown {
                raw: err.to_string(),
            })?,
        )
    } else {
        None
    };
    let base_client = if let Some(url) = &attach {
        OpencodeClient::new(
            url,
            args.dir.clone(),
            args.password.as_deref(),
            args.username.as_deref(),
        )
    } else {
        let url = format!(
            "http://127.0.0.1:{}",
            _listener.as_ref().map(|l| l.port).unwrap_or_default()
        );
        OpencodeClient::new(
            url,
            Some(
                directory
                    .clone()
                    .unwrap_or_else(|| root.to_path_buf())
                    .display()
                    .to_string(),
            ),
            args.password.as_deref(),
            args.username.as_deref(),
        )
    };

    let sess = resolve_session(&base_client, args, message).await?;
    let session_id = sess.id;

    // run.ts:826-828 — attach rebinds the client onto the session's
    // directory after the session is resolved.
    let client = if let Some(url) = &attach {
        let cwd = match args.dir.clone().or(sess.directory) {
            Some(cwd) => cwd,
            None => resolve_remote_directory(&base_client).await?,
        };
        OpencodeClient::new(
            url,
            Some(cwd),
            args.password.as_deref(),
            args.username.as_deref(),
        )
    } else {
        base_client
    };

    let agent = pick_agent(
        client.agent_list().await,
        args.agent.as_deref(),
        attach.as_deref(),
        ui,
    );
    share(&client, &session_id, args.share, ui).await;

    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    client
        .subscribe_events(tx)
        .await
        .map_err(|err| TypedError::Cli(CliError::new(format_run_error(&err))))?;

    let command = args.command.is_some();
    let body = if command {
        command_body(args, agent.as_deref(), message)
    } else {
        prompt_body(args, agent.as_deref(), files, message)
    };
    let mut prompt = {
        let client = client.clone();
        let session_id = session_id.clone();
        tokio::spawn(async move {
            if command {
                client.session_command(&session_id, body).await
            } else {
                client.session_prompt(&session_id, body).await
            }
        })
    };

    let auto = args.auto || args.yolo || args.dangerously_skip_permissions;
    let thinking = args.thinking;
    let mut state = LoopState::new(
        session_id.clone(),
        auto,
        thinking,
        args.format == "json",
        ui.is_tty(),
    );
    let mut prompt_error = None;
    let mut prompt_done = false;
    loop {
        tokio::select! {
            maybe = rx.recv() => {
                let Some(event) = maybe else { break };
                let outputs = map_event(&mut state, &event);
                let mut replies = Vec::new();
                apply(ui, &outputs, &mut |request, reply| {
                    replies.push((request.to_string(), reply.to_string()));
                });
                for (request, reply) in replies {
                    let client = client.clone();
                    tokio::spawn(async move {
                        let _ = client.permission_reply(&request, &reply).await;
                    });
                }
                if outputs.iter().any(|output| matches!(output, Output::Stop)) {
                    break;
                }
            }
            result = &mut prompt, if !prompt_done => {
                prompt_done = true;
                match result {
                    Ok(Err(err)) => prompt_error = Some(err),
                    Ok(Ok(_)) => {}
                    Err(err) => prompt_error = Some(ClientError {
                        status: None,
                        body: None,
                        message: err.to_string(),
                    }),
                }
                if prompt_error.is_some() {
                    break;
                }
            }
        }
    }
    if let Some(err) = prompt_error {
        // run.ts:855-857, 872-874 — emit the error event in json mode,
        // format onto stderr otherwise; exit 1 either way.
        if args.format == "json" {
            let line = run_output::emit_envelope(
                run_events::now_ms(),
                "error",
                &session_id,
                &json!({"error": error_payload(&err)}),
            );
            ui.write_stdout(&format!("{line}\n"));
        } else {
            ui.error(&format_run_error(&err));
        }
        return Err(TypedError::Cli(CliError::with_exit_code("", 1)));
    }
    // `finish()` returns early in attach mode (run.ts:839-843) — the
    // loop's error exit code only applies to the local server.
    if state.error.is_some() && args.attach.is_none() {
        // Errors already rendered per event; exit code 1 without a new line.
        return Err(TypedError::Cli(CliError::with_exit_code("", 1)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> RunArgs {
        RunArgs::default()
    }

    #[test]
    fn build_message_joins_and_quotes() {
        assert_eq!(
            build_message(&[
                "hello".to_string(),
                "two words".to_string(),
                "say \"hi\"".to_string(),
            ]),
            "hello \"two words\" \"say \\\"hi\\\"\""
        );
        assert_eq!(build_message(&[]), "");
    }

    #[test]
    fn run_command_parses_the_full_flag_matrix() {
        let matches = crate::cmd::cli().try_get_matches_from([
            "alforria",
            "run",
            "hello",
            "two words",
            "--continue",
            "--fork",
            "--session",
            "ses_1",
            "--model",
            "anthropic/claude",
            "--agent",
            "build",
            "--format",
            "json",
            "--file",
            "/tmp/a",
            "--file",
            "/tmp/b",
            "--title",
            "",
            "--port",
            "9000",
            "--variant",
            "high",
            "--thinking",
            "--auto",
        ]);
        let matches = match matches {
            Ok(matches) => matches.subcommand_matches("run").unwrap().clone(),
            Err(err) => panic!("parse failed: {err}"),
        };
        let parsed = RunArgs::from_matches(&matches);
        assert_eq!(
            parsed.message,
            vec!["hello".to_string(), "two words".to_string()]
        );
        assert!(parsed.r#continue);
        assert!(parsed.fork);
        assert_eq!(parsed.session.as_deref(), Some("ses_1"));
        assert_eq!(parsed.model.as_deref(), Some("anthropic/claude"));
        assert_eq!(parsed.agent.as_deref(), Some("build"));
        assert_eq!(parsed.format, "json");
        assert_eq!(
            parsed.file,
            vec!["/tmp/a".to_string(), "/tmp/b".to_string()]
        );
        assert_eq!(parsed.title, Some(String::new()));
        assert_eq!(parsed.port, 9000);
        assert_eq!(parsed.variant.as_deref(), Some("high"));
        assert!(parsed.thinking);
        assert!(parsed.auto);
        assert_eq!(build_message(&parsed.message), "hello \"two words\"");
    }

    #[test]
    fn run_command_parses_hidden_flags() {
        let matches = crate::cmd::cli()
            .try_get_matches_from([
                "alforria",
                "run",
                "hi",
                "--mini",
                "--demo",
                "--yolo",
                "--dangerously-skip-permissions",
                "--replay-limit",
                "5",
            ])
            .unwrap();
        let parsed = RunArgs::from_matches(matches.subcommand_matches("run").unwrap());
        assert!(parsed.mini);
        assert!(parsed.demo);
        assert!(parsed.yolo);
        assert!(parsed.dangerously_skip_permissions);
        assert_eq!(parsed.replay_limit, Some(5.0));
    }

    #[test]
    fn validate_rejects_mini_with_command() {
        let mut run_args = args();
        run_args.mini = true;
        run_args.command = Some("x".to_string());
        assert_eq!(
            validate(&run_args, true, true).unwrap_err().message,
            "--mini cannot be used with --command"
        );
    }

    #[test]
    fn validate_rejects_mini_via_run_subcommand() {
        let mut run_args = args();
        run_args.mini = true;
        assert_eq!(
            validate(&run_args, true, true).unwrap_err().message,
            "--mini must be used without the run subcommand"
        );
        assert!(validate(&run_args, false, true).is_ok());
    }

    #[test]
    fn validate_rejects_demo_without_mini() {
        let mut run_args = args();
        run_args.demo = true;
        assert_eq!(
            validate(&run_args, true, true).unwrap_err().message,
            "--demo requires --mini"
        );
    }

    #[test]
    fn validate_rejects_mini_with_json_format() {
        let mut run_args = args();
        run_args.mini = true;
        run_args.format = "json".to_string();
        let err = validate(&run_args, false, true).unwrap_err();
        assert_eq!(err.message, "--mini cannot be used with --format json");
    }

    #[test]
    fn validate_rejects_replay_limit_without_mini() {
        let mut run_args = args();
        run_args.replay_limit = Some(10.0);
        assert_eq!(
            validate(&run_args, true, true).unwrap_err().message,
            "--replay-limit requires --mini"
        );
    }

    #[test]
    fn validate_rejects_non_positive_replay_limit() {
        let mut run_args = args();
        run_args.mini = true;
        run_args.replay_limit = Some(0.0);
        assert_eq!(
            validate(&run_args, false, true).unwrap_err().message,
            "--replay-limit must be a positive integer"
        );
        let mut fractional = run_args.clone();
        fractional.replay_limit = Some(1.5);
        assert_eq!(
            validate(&fractional, false, true).unwrap_err().message,
            "--replay-limit must be a positive integer"
        );
        let mut negative = run_args.clone();
        negative.replay_limit = Some(-3.0);
        assert_eq!(
            validate(&negative, false, true).unwrap_err().message,
            "--replay-limit must be a positive integer"
        );
        run_args.replay_limit = Some(3.0);
        assert!(validate(&run_args, false, true).is_ok());
    }

    #[test]
    fn validate_rejects_mini_without_tty() {
        let mut run_args = args();
        run_args.mini = true;
        assert_eq!(
            validate(&run_args, false, false).unwrap_err().message,
            "--mini requires a TTY stdout"
        );
        assert!(validate(&run_args, false, true).is_ok());
    }

    #[test]
    fn message_or_command_is_required() {
        let empty = args();
        assert_eq!(
            require_message(&empty, "").unwrap_err().message,
            "You must provide a message or a command"
        );
        assert!(require_message(&empty, "hello").is_ok());
        let mut command = args();
        command.command = Some("init".to_string());
        assert!(require_message(&command, "").is_ok());
    }

    #[test]
    fn fork_requires_continue_or_session() {
        let mut fork = args();
        fork.fork = true;
        assert_eq!(
            require_fork(&fork).unwrap_err().message,
            "--fork requires --continue or --session"
        );
        fork.session = Some("ses_1".to_string());
        assert!(require_fork(&fork).is_ok());
        let mut fork = args();
        fork.fork = true;
        fork.r#continue = true;
        assert!(require_fork(&fork).is_ok());
    }

    #[test]
    fn title_rules_follow_ts() {
        assert_eq!(session_title(None, "msg"), None);
        assert_eq!(session_title(Some("hi"), "msg"), Some("hi".to_string()));
        assert_eq!(session_title(Some(""), "short"), Some("short".to_string()));
        let long = "x".repeat(60);
        let expected = format!("{}...", "x".repeat(50));
        assert_eq!(session_title(Some(""), &long), Some(expected));
        let exactly = "y".repeat(50);
        assert_eq!(
            session_title(Some(""), &exactly),
            Some(exactly),
            "50 chars get no ellipsis"
        );
    }

    #[test]
    fn rules_deny_question_and_plan_tools_when_not_interactive() {
        let deny = rules(false);
        assert_eq!(
            deny,
            json!([
                {"permission": "question", "action": "deny", "pattern": "*"},
                {"permission": "plan_enter", "action": "deny", "pattern": "*"},
                {"permission": "plan_exit", "action": "deny", "pattern": "*"},
            ])
        );
        assert_eq!(rules(true), json!([]));
    }

    #[test]
    fn pick_model_splits_on_first_slash() {
        assert_eq!(
            pick_model(Some("anthropic/claude-4")),
            Some(("anthropic".to_string(), "claude-4".to_string()))
        );
        assert_eq!(
            pick_model(Some("a/b/c")),
            Some(("a".to_string(), "b/c".to_string()))
        );
        assert_eq!(
            pick_model(Some("noprovider")),
            Some(("noprovider".to_string(), String::new()))
        );
        assert_eq!(pick_model(None), None);
    }

    #[test]
    fn resolve_directory_without_dir_is_root() {
        let root = PathBuf::from("/tmp");
        assert_eq!(resolve_directory(&root, None, false).unwrap(), Some(root));
    }

    #[test]
    fn resolve_directory_chdir_failure_is_error() {
        let root = PathBuf::from("/tmp");
        let err = resolve_directory(&root, Some("/definitely/not/here"), false).unwrap_err();
        assert_eq!(
            err.message,
            "Failed to change directory to /definitely/not/here"
        );
    }

    #[test]
    fn resolve_directory_attach_never_chdirs() {
        let root = PathBuf::from("/tmp");
        // An unchdir-able path is fine in attach mode — it is remote data.
        assert_eq!(
            resolve_directory(&root, Some("/remote/path"), true).unwrap(),
            Some(PathBuf::from("/remote/path"))
        );
        assert_eq!(resolve_directory(&root, None, true).unwrap(), None);
    }

    struct FakeApi {
        sessions: Vec<Value>,
        fork_result: Option<Value>,
        created: Vec<Value>,
    }

    impl FakeApi {
        fn get_by_id(&self, id: &str) -> Option<Value> {
            self.sessions
                .iter()
                .find(|session| session.get("id").and_then(|v| v.as_str()) == Some(id))
                .cloned()
        }
    }

    impl SessionApi for FakeApi {
        async fn get(&self, id: &str) -> Option<Value> {
            self.get_by_id(id)
        }

        async fn list(&self) -> Vec<Value> {
            self.sessions.clone()
        }

        async fn fork(&self, id: &str) -> Option<Value> {
            let _base = self.get_by_id(id)?;
            self.fork_result.clone()
        }

        async fn create(&self, _body: &Value) -> Option<Value> {
            self.created.first().cloned()
        }
    }

    fn session(id: &str, parent: Option<&str>) -> Value {
        let mut value = json!({"id": id, "title": format!("Title {id}"), "directory": "/dir"});
        if let Some(parent) = parent {
            value["parentID"] = json!(parent);
        }
        value
    }

    #[tokio::test]
    async fn session_resolution_by_id() {
        let api = FakeApi {
            sessions: vec![session("ses_1", None)],
            fork_result: None,
            created: vec![],
        };
        let mut run_args = args();
        run_args.session = Some("ses_1".to_string());
        let resolved = resolve_session(&api, &run_args, "m").await.unwrap();
        assert_eq!(resolved.id, "ses_1");
        assert_eq!(resolved.title.as_deref(), Some("Title ses_1"));
    }

    #[tokio::test]
    async fn session_resolution_missing_session_errors() {
        let api = FakeApi {
            sessions: vec![],
            fork_result: None,
            created: vec![],
        };
        let mut run_args = args();
        run_args.session = Some("ses_missing".to_string());
        let err = resolve_session(&api, &run_args, "m").await.unwrap_err();
        assert_eq!(err.message, "Session not found");
    }

    #[tokio::test]
    async fn session_resolution_fork_by_id() {
        let api = FakeApi {
            sessions: vec![session("ses_1", None)],
            fork_result: Some(json!({"id": "ses_fork", "title": "Forked"})),
            created: vec![],
        };
        let mut run_args = args();
        run_args.session = Some("ses_1".to_string());
        run_args.fork = true;
        let resolved = resolve_session(&api, &run_args, "m").await.unwrap();
        assert_eq!(resolved.id, "ses_fork");
        assert_eq!(resolved.title.as_deref(), Some("Forked"));
    }

    #[tokio::test]
    async fn session_resolution_continue_picks_first_parentless() {
        let api = FakeApi {
            sessions: vec![session("ses_child", Some("ses_1")), session("ses_2", None)],
            fork_result: None,
            created: vec![],
        };
        let mut run_args = args();
        run_args.r#continue = true;
        let resolved = resolve_session(&api, &run_args, "m").await.unwrap();
        assert_eq!(resolved.id, "ses_2");
    }

    #[tokio::test]
    async fn session_resolution_continue_creates_when_no_root() {
        let api = FakeApi {
            sessions: vec![session("ses_child", Some("ses_1"))],
            fork_result: None,
            created: vec![json!({"id": "ses_new", "title": "Fresh"})],
        };
        let mut run_args = args();
        run_args.r#continue = true;
        run_args.title = Some("Fresh".to_string());
        let resolved = resolve_session(&api, &run_args, "msg").await.unwrap();
        assert_eq!(resolved.id, "ses_new");
        assert_eq!(resolved.title.as_deref(), Some("Fresh"));
    }

    #[tokio::test]
    async fn session_resolution_create_new() {
        let api = FakeApi {
            sessions: vec![],
            fork_result: None,
            created: vec![json!({"id": "ses_new"})],
        };
        let mut run_args = args();
        run_args.title = Some("".to_string());
        let resolved = resolve_session(&api, &run_args, "hello world")
            .await
            .unwrap();
        assert_eq!(resolved.id, "ses_new");
        assert_eq!(resolved.title.as_deref(), Some("hello world"));
    }

    #[tokio::test]
    async fn session_resolution_create_failure_errors() {
        let api = FakeApi {
            sessions: vec![],
            fork_result: None,
            created: vec![],
        };
        let err = resolve_session(&api, &args(), "m").await.unwrap_err();
        assert_eq!(err.message, "Session not found");
    }

    #[test]
    fn resolve_run_input_matrix() {
        assert_eq!(resolve_run_input(None, None), None);
        assert_eq!(
            resolve_run_input(Some("msg"), None),
            Some("msg".to_string())
        );
        assert_eq!(
            resolve_run_input(None, Some("piped")),
            Some("piped".to_string())
        );
        assert_eq!(
            resolve_run_input(Some("msg"), Some("piped")),
            Some("msg\npiped".to_string())
        );
        // Empty strings are falsy (`resolveRunInput`, run.ts:40-46).
        assert_eq!(resolve_run_input(Some(""), None), None);
        assert_eq!(
            resolve_run_input(Some(""), Some("piped")),
            Some("piped".to_string())
        );
        assert_eq!(
            resolve_run_input(Some("msg"), Some("")),
            Some("msg".to_string())
        );
    }

    #[test]
    fn pick_agent_matrix() {
        let (mut ui, captured) = crate::ui::Ui::capture(false);
        let agents = vec![
            json!({"name": "build", "mode": "primary"}),
            json!({"name": "search", "mode": "subagent"}),
        ];
        // No agent requested → no validation.
        assert_eq!(pick_agent(Ok(agents.clone()), None, None, &mut ui), None);
        // Unknown agent falls back with a warning.
        assert_eq!(
            pick_agent(Ok(agents.clone()), Some("nope"), None, &mut ui),
            None
        );
        // Subagents fall back too.
        assert_eq!(
            pick_agent(Ok(agents.clone()), Some("search"), None, &mut ui),
            None
        );
        // Primary agents pass through.
        assert_eq!(
            pick_agent(Ok(agents.clone()), Some("build"), None, &mut ui),
            Some("build".to_string())
        );
        let stderr = captured.stderr();
        assert!(
            stderr.contains("agent \"nope\" not found. Falling back to default agent"),
            "{stderr}"
        );
        assert!(
            stderr.contains(
                "agent \"search\" is a subagent, not a primary agent. Falling back to default agent"
            ),
            "{stderr}"
        );
    }

    #[test]
    fn pick_agent_attach_listing_failure_warns() {
        let (mut ui, captured) = crate::ui::Ui::capture(false);
        let err = ClientError {
            status: Some(500),
            body: None,
            message: "boom".to_string(),
        };
        assert_eq!(
            pick_agent(Err(err), Some("build"), Some("http://remote"), &mut ui),
            None
        );
        let stderr = captured.stderr();
        assert!(
            stderr.contains(
                "failed to list agents from http://remote. Falling back to default agent"
            ),
            "{stderr}"
        );
    }

    #[test]
    fn share_gate_matrix() {
        assert!(share_gate(Some("auto"), false, false));
        assert!(share_gate(None, true, false));
        assert!(share_gate(None, false, true));
        assert!(!share_gate(None, false, false));
        assert!(!share_gate(Some("manual"), false, false));
    }

    #[test]
    fn share_lines_follow_ts_styles() {
        assert_eq!(
            share_success_line("https://s.opncd.ai/x"),
            "\u{1b}[94m\u{1b}[1m~  https://s.opncd.ai/x"
        );
        assert_eq!(
            share_disabled_line("Sharing is disabled in configuration"),
            "\u{1b}[91m\u{1b}[1m!  Sharing is disabled in configuration"
        );
    }

    fn client_error(body: Option<&str>) -> ClientError {
        ClientError {
            status: Some(400),
            body: body.map(str::to_string),
            message: "400 Bad Request".to_string(),
        }
    }

    #[test]
    fn format_run_error_formats_tagged_bodies() {
        let err = client_error(Some(
            "{\"name\":\"MCPFailed\",\"data\":{\"name\":\"remote\"}}",
        ));
        assert_eq!(
            format_run_error(&err),
            "MCP server \"remote\" failed. Note, alforria does not support MCP authentication yet."
        );
    }

    #[test]
    fn format_run_error_pretty_prints_unknown_bodies() {
        let err = client_error(Some("{\"name\":\"APIError\"}"));
        assert_eq!(format_run_error(&err), "{\n  \"name\": \"APIError\"\n}");
    }

    #[test]
    fn format_run_error_falls_back_to_message() {
        let err = client_error(None);
        assert_eq!(format_run_error(&err), "400 Bad Request");
    }

    #[test]
    fn error_payload_parses_body_or_falls_back() {
        assert_eq!(
            error_payload(&client_error(Some("{\"a\":1}"))),
            json!({"a": 1})
        );
        assert_eq!(
            error_payload(&client_error(None)),
            Value::String("400 Bad Request".to_string())
        );
    }

    #[test]
    fn command_body_matches_ts_shape() {
        let mut run_args = args();
        run_args.command = Some("compact".to_string());
        run_args.agent = Some("build".to_string());
        run_args.model = Some("anthropic/claude".to_string());
        run_args.variant = Some("high".to_string());
        assert_eq!(
            command_body(&run_args, Some("build"), "extra args"),
            json!({
                "command": "compact",
                "arguments": "extra args",
                "agent": "build",
                "model": "anthropic/claude",
                "variant": "high",
            })
        );
    }

    #[test]
    fn prompt_body_matches_ts_shape() {
        let mut run_args = args();
        run_args.model = Some("anthropic/claude".to_string());
        let files = vec![
            json!({"type": "file", "url": "file:///a", "filename": "a", "mime": "text/plain"}),
        ];
        let body = prompt_body(&run_args, None, files, "hello");
        assert_eq!(
            body["parts"],
            json!([
                {"type": "file", "url": "file:///a", "filename": "a", "mime": "text/plain"},
                {"type": "text", "text": "hello"},
            ])
        );
        assert_eq!(
            body["model"],
            json!({"providerID": "anthropic", "modelID": "claude"})
        );
    }
}

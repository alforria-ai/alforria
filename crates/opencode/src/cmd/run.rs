//! cli/cmd/run.ts — the `run` command: flags, validation, session flow and
//! the loopback-server execution driver.

use std::path::{Path, PathBuf};

use clap::ArgMatches;
use serde_json::{json, Value};

use crate::client::{ClientError, OpencodeClient};
use crate::error::{CliError, TypedError};
use crate::ui::Ui;

use super::run_events::{apply, map_event, LoopState, Output};

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

/// run.ts:331-345 — root resolution + the `--dir` chdir (local mode only).
pub fn resolve_directory(root: &Path, dir: Option<&str>) -> Result<PathBuf, CliError> {
    let Some(dir) = dir else {
        return Ok(root.to_path_buf());
    };
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
    Ok(std::env::current_dir().unwrap_or(target))
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

// TODO(C4): formatRunError parity (wrapClientError + FormatError chain).
fn format_run_error(error: &ClientError) -> String {
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

    let root = resolve_root();
    let directory = resolve_directory(&root, args.dir.as_deref())?;
    require_message(args, &message)?;
    require_fork(args)?;

    if args.command.is_some() {
        // TODO(C4): command mode (run.ts:845-861).
        return Err(TypedError::Cli(CliError::new(
            "--command is not supported yet",
        )));
    }

    let runtime = super::runtime()?;
    runtime.block_on(execute(args, ui, &directory, &message))
}

async fn execute(
    args: &RunArgs,
    ui: &mut Ui,
    directory: &Path,
    message: &str,
) -> Result<(), TypedError> {
    let listener = opencode_server::listen(&opencode_server::ListenOptions {
        port: args.port,
        hostname: "127.0.0.1".to_string(),
        cors: Vec::new(),
    })
    .await
    .map_err(|err| TypedError::Unknown {
        raw: err.to_string(),
    })?;
    let client = OpencodeClient::new(
        format!("http://127.0.0.1:{}", listener.port),
        Some(directory.display().to_string()),
        args.password.as_deref(),
        args.username.as_deref(),
    );
    let sess = resolve_session(&client, args, message).await?;
    let session_id = sess.id;

    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    client
        .subscribe_events(tx)
        .await
        .map_err(|err| TypedError::Cli(CliError::new(format_run_error(&err))))?;

    let mut body = json!({"parts": [{"type": "text", "text": message}]});
    if let Some((provider_id, model_id)) = pick_model(args.model.as_deref()) {
        body["model"] = json!({"providerID": provider_id, "modelID": model_id});
    }
    if let Some(agent) = &args.agent {
        body["agent"] = json!(agent);
    }
    if let Some(variant) = &args.variant {
        body["variant"] = json!(variant);
    }
    let mut prompt = {
        let client = client.clone();
        let url = format!("/session/{session_id}/message");
        tokio::spawn(async move { client.session_prompt(&url, body).await })
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
        return Err(TypedError::Cli(CliError::new(format_run_error(&err))));
    }
    if state.error.is_some() {
        // Errors already rendered per event; exit code 1 without a new line.
        return Err(TypedError::Cli(CliError::with_exit_code("", 1)));
    }
    Ok(())
}

// TODO(C4): --attach remote transport; --file parts; stdin merge; share flow;
// agent validation; --format json emission.

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
            "opencode",
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
                "opencode",
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
        assert_eq!(resolve_directory(&root, None).unwrap(), root);
    }

    #[test]
    fn resolve_directory_chdir_failure_is_error() {
        let root = PathBuf::from("/tmp");
        let err = resolve_directory(&root, Some("/definitely/not/here")).unwrap_err();
        assert_eq!(
            err.message,
            "Failed to change directory to /definitely/not/here"
        );
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
}

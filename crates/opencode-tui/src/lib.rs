//! ratatui TUI — a stateless HTTP+SSE client of the opencode server (M8).
//!
//! TS reference: `packages/tui` (whole package is ground truth; see
//! `scratchpad/specs/M8.md`). Elm-style model: one `update()`, full redraw
//! per change. This crate is transport-first: everything above
//! `transport::api` is testable against a `FakeApi` + fake event source.
//!
//! The terminal runtime (this file) owns the three long-lived task
//! families of spec §2.2: the SSE loop (`transport::events`), the input
//! pump (crossterm) and the 40 ms tick; effects returned by `update`
//! execute against the server seam between messages.

pub mod app;
pub mod clipboard;
pub mod command;
pub mod config;
pub mod keymap;
pub mod state;
pub mod transport;
pub mod ui;

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::clipboard::Clipboard as _;
use crate::state::route::Route;
use crate::state::{App, Args, Effect, Msg, Toast, ToastVariant};
use crate::transport::api::{HttpClientConfig, HttpServerApi, Location, ServerApi};
use crate::transport::events::{spawn_event_loop, EventSource, SseEventSource, TokioClock};
use crate::ui::view;

/// The 40 ms frame tick (the TS spinner `interval={40}`).
const FRAME_INTERVAL_MS: u64 = 40;

/// `TuiInput` (`app.tsx:142-152`): the CLI handoff.
#[derive(Debug, Clone, Default)]
pub struct TuiInput {
    pub url: String,
    pub directory: Option<String>,
    pub headers: Vec<(String, String)>,
    pub args: Args,
    pub config: config::TuiConfig,
    pub state_dir: Option<PathBuf>,
}

/// `run()`'s result (`app.tsx:188, 354`).
#[derive(Debug, Default)]
pub struct Exit {
    pub epilogue: Option<String>,
    pub reason: Option<String>,
}

pub fn run(input: TuiInput) -> Result<Exit> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run_inner(input))
}

async fn run_inner(input: TuiInput) -> Result<Exit> {
    let _guard = TerminalGuard::enter(input.config.mouse)?;

    let http_config = HttpClientConfig {
        base_url: input.url.clone(),
        directory: input.directory.clone(),
        headers: input.headers.clone(),
    };
    let api: Arc<dyn ServerApi> = Arc::new(HttpServerApi::new(http_config.clone())?);
    let source: Arc<dyn EventSource> = Arc::new(SseEventSource::new(http_config)?);

    let mut app = App::new(input.config, input.args.clone(), input.state_dir.as_deref());
    app::apply_args(&mut app);

    let (msg_tx, msg_rx) = tokio::sync::mpsc::unbounded_channel::<Msg>();
    spawn_sse(source, msg_tx.clone());
    spawn_input_pump(msg_tx.clone());
    spawn_tick(msg_tx.clone());
    spawn_sighup(msg_tx);

    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(io::stdout()))?;
    let exit = event_loop(&mut app, api, msg_rx, &mut terminal).await;

    // §6 N6: `docs.open` prints the URL instead of opening a browser —
    // after the alternate screen is gone.
    let opened_urls = std::mem::take(&mut app.ui.opened_urls);
    drop(_guard);
    for url in opened_urls {
        println!("{url}");
    }

    let exit = exit?;
    print_exit(&exit);
    Ok(exit)
}

/// One `update` pass per message; effects execute between messages (the
/// TS handlers fire-and-forget `sdk.client.*` calls — HTTP latency is the
/// same bound as the SSE reconnect).
async fn event_loop(
    app: &mut App,
    api: Arc<dyn ServerApi>,
    mut messages: tokio::sync::mpsc::UnboundedReceiver<Msg>,
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<io::Stdout>>,
) -> Result<Exit> {
    let title_disabled = app::terminal_title_disabled_from_env();
    let mut last_title: Option<String> = None;
    let mut effects = vec![Effect::Bootstrap { fatal: true }];

    loop {
        execute_effects(app, api.clone(), &mut effects).await?;
        if app.ui.exit {
            break;
        }
        terminal.draw(|frame| view(app, frame))?;
        apply_title(app, title_disabled, &mut last_title)?;

        let Some(msg) = messages.recv().await else {
            app.exit(None);
            break;
        };
        effects = state::update(app, msg);
    }

    Ok(Exit {
        epilogue: app::epilogue(app),
        reason: app.ui.exit_reason.clone(),
    })
}

async fn execute_effects(
    app: &mut App,
    api: Arc<dyn ServerApi>,
    effects: &mut Vec<Effect>,
) -> Result<()> {
    for effect in effects.drain(..) {
        execute_effect(app, api.clone(), effect).await?;
    }
    Ok(())
}

async fn execute_effect(app: &mut App, api: Arc<dyn ServerApi>, effect: Effect) -> Result<()> {
    match effect {
        Effect::Bootstrap { fatal } => {
            if let Err(error) = state::sync::bootstrap(&mut app.state, api.as_ref(), fatal).await {
                if fatal {
                    app.exit(Some(format!("{error:#}")));
                }
            }
        }
        Effect::PermissionAutoReply {
            request_id,
            reply,
            metadata,
        } => {
            let loc = Location {
                directory: metadata.directory,
                workspace: metadata.workspace,
            };
            let _ = api.permission_reply(&loc, &request_id, reply, None).await;
        }
        Effect::LspStatusRefetch { workspace } => {
            let loc = Location {
                directory: None,
                workspace,
            };
            if let Ok(status) = api.lsp_status(&loc).await {
                app.state.sync.lsp = match status {
                    serde_json::Value::Array(items) => items,
                    _ => Vec::new(),
                };
            }
        }
        Effect::SessionFork {
            session_id,
            navigate,
        } => {
            match api
                .session_fork(&Location::default(), &session_id, None)
                .await
            {
                Ok(session) => {
                    if navigate {
                        app.state.route.navigate(Route::Session {
                            session_id: session.id.clone(),
                            prompt: None,
                        });
                    }
                }
                Err(_) => app.show_toast(Toast {
                    title: None,
                    variant: ToastVariant::Error,
                    message: "Failed to fork session".to_string(),
                    duration_ms: 5000,
                }),
            }
        }
        Effect::SuspendTerminal => {
            suspend::terminal_suspend_and_resume().await;
        }
        Effect::SessionShare { session_id } => {
            match api.session_share(&Location::default(), &session_id).await {
                Ok(session) => {
                    if let Some(share) = session.share {
                        let _ = clipboard::system_clipboard().write(&share.url);
                        app.show_toast(Toast {
                            title: None,
                            variant: ToastVariant::Success,
                            message: "Share URL copied to clipboard!".to_string(),
                            duration_ms: 5000,
                        });
                    }
                }
                Err(_) => app.show_toast(Toast {
                    title: None,
                    variant: ToastVariant::Error,
                    message: "Failed to share session".to_string(),
                    duration_ms: 5000,
                }),
            }
        }
        Effect::SessionUnshare { session_id } => {
            match api.session_unshare(&Location::default(), &session_id).await {
                Ok(_) => app.show_toast(Toast {
                    title: None,
                    variant: ToastVariant::Success,
                    message: "Session unshared successfully".to_string(),
                    duration_ms: 5000,
                }),
                Err(_) => app.show_toast(Toast {
                    title: None,
                    variant: ToastVariant::Error,
                    message: "Failed to unshare session".to_string(),
                    duration_ms: 5000,
                }),
            }
        }
        Effect::SessionSummarize {
            session_id,
            provider_id,
            model_id,
        } => {
            let _ = api
                .session_summarize(&Location::default(), &session_id, &provider_id, &model_id)
                .await;
        }
        Effect::SessionAbort { session_id } => {
            let _ = api.session_abort(&Location::default(), &session_id).await;
        }
        Effect::SessionRevert {
            session_id,
            message_id,
        } => {
            let _ = api
                .session_revert(&Location::default(), &session_id, &message_id, None)
                .await;
        }
        Effect::SessionUnrevert { session_id } => {
            let _ = api
                .session_unrevert(&Location::default(), &session_id)
                .await;
        }
        Effect::SessionRefresh => {
            let sessions = state::sync::list_sessions(
                api.as_ref(),
                &Location::default(),
                &app.state.kv,
                &app.state.project,
            )
            .await;
            app.state.sync.session = sessions;
        }
        Effect::SessionBackground { session_id } => {
            let _ = api
                .experimental_session_background(&Location::default(), &session_id)
                .await;
        }
        Effect::SessionCopyTranscript { .. } => {
            // TODO(M8.8): formatTranscript + clipboard write.
        }
        Effect::SessionMount {
            session_id,
            previous_workspace,
        } => {
            // `createEffect` (`session/index.tsx:286-324`).
            match api.session_get(&Location::default(), &session_id).await {
                Ok(info) => {
                    if info.workspace_id != previous_workspace {
                        app.state.project.workspace.current = info.workspace_id.clone();
                        // Non-fatal: the workspace may no longer exist —
                        // the session still renders, non-interactive.
                        let _ = state::sync::bootstrap(&mut app.state, api.as_ref(), false).await;
                    }
                    let _ =
                        state::sync::session_sync(&mut app.state, api.as_ref(), &session_id).await;
                    // `scroll.scrollBy(100_000)` — snap to bottom.
                    app.ui.session_scroll.snap_to_bottom();
                }
                Err(_) => {
                    app.show_toast(Toast {
                        title: None,
                        variant: ToastVariant::Error,
                        message: format!("Session not found: {session_id}"),
                        duration_ms: 5000,
                    });
                    app.state.route.navigate(Route::Home { prompt: None });
                }
            }
        }
        Effect::SessionHydrate { session_id } => {
            let _ = state::sync::session_sync(&mut app.state, api.as_ref(), &session_id).await;
        }
        Effect::ClipboardWrite {
            text,
            success,
            failure,
        } => match clipboard::system_clipboard().write(&text) {
            Ok(()) => {
                if let Some(toast) = success {
                    app.show_toast(toast);
                }
            }
            Err(_) => {
                if let Some(toast) = failure {
                    app.show_toast(toast);
                }
            }
        },
        Effect::OpenUrl { url } => {
            // §6 N6: no browser from a TUI — the URL prints on exit.
            app.ui.opened_urls.push(url);
        }
    }
    Ok(())
}

/// The terminal title effect (`app.tsx:455-478`): set on change, cleared
/// when disabled (OSC 2).
fn apply_title(app: &App, env_disabled: bool, last_title: &mut Option<String>) -> Result<()> {
    let title = app::terminal_title(app, env_disabled);
    if title == *last_title {
        return Ok(());
    }
    let sequence = match &title {
        Some(title) => format!("\x1b]2;{title}\x07"),
        None => "\x1b]2;\x07".to_string(),
    };
    write_ansi(&sequence)?;
    *last_title = title;
    Ok(())
}

fn write_ansi(sequence: &str) -> Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(sequence.as_bytes())?;
    stdout.flush()?;
    Ok(())
}

/// `app.tsx:357-364`: reason → stderr + exit code 1; epilogue → stdout.
fn print_exit(exit: &Exit) {
    if let Some(reason) = &exit.reason {
        eprintln!("{reason}");
    }
    if let Some(epilogue) = &exit.epilogue {
        println!("{epilogue}");
    }
    if exit.reason.is_some() {
        // TS sets `process.exitCode = 1` and lets the process end; the
        // process is fully torn down here.
        std::process::exit(1);
    }
}

fn spawn_sse(source: Arc<dyn EventSource>, messages: tokio::sync::mpsc::UnboundedSender<Msg>) {
    let (bus_tx, mut bus_rx) = tokio::sync::mpsc::unbounded_channel();
    spawn_event_loop(source, Arc::new(TokioClock::new()), bus_tx);
    tokio::spawn(async move {
        while let Some(batch) = bus_rx.recv().await {
            if messages.send(Msg::Bus(batch)).is_err() {
                return;
            }
        }
    });
}

fn spawn_input_pump(messages: tokio::sync::mpsc::UnboundedSender<Msg>) {
    std::thread::spawn(move || loop {
        match crossterm::event::read() {
            Ok(crossterm::event::Event::Key(key)) => {
                if messages.send(Msg::Key(key)).is_err() {
                    return;
                }
            }
            Ok(crossterm::event::Event::Mouse(mouse)) => {
                if messages.send(Msg::Mouse(mouse)).is_err() {
                    return;
                }
            }
            Ok(crossterm::event::Event::Resize(columns, rows)) => {
                if messages.send(Msg::Resize(columns, rows)).is_err() {
                    return;
                }
            }
            // TODO(M8.6): bracketed paste (Event::Paste).
            Ok(_) => {}
            Err(_) => return,
        }
    });
}

fn spawn_tick(messages: tokio::sync::mpsc::UnboundedSender<Msg>) {
    tokio::spawn(async move {
        let start = Instant::now();
        let mut interval = tokio::time::interval(Duration::from_millis(FRAME_INTERVAL_MS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if messages.send(Msg::Tick(start.elapsed())).is_err() {
                return;
            }
        }
    });
}

/// SIGHUP destroys the renderer (`app.tsx:231-235`) — here it exits the
/// loop for the scoped teardown.
fn spawn_sighup(messages: tokio::sync::mpsc::UnboundedSender<Msg>) {
    tokio::spawn(async move {
        let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
            .expect("SIGHUP handler");
        sighup.recv().await;
        let _ = messages.send(Msg::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('d'),
            crossterm::event::KeyModifiers::CONTROL,
        )));
    });
}

/// Raw mode + alternate screen + optional mouse capture + the kitty
/// keyboard protocol (`app.tsx:186-213`).
struct TerminalGuard {
    mouse: bool,
}

impl TerminalGuard {
    fn enter(mouse: bool) -> Result<TerminalGuard> {
        crossterm::terminal::enable_raw_mode()?;
        crossterm::execute!(io::stdout(), crossterm::terminal::EnterAlternateScreen)?;
        let mut mouse = mouse;
        if mouse_disabled_from_env() {
            mouse = false;
        }
        if mouse {
            crossterm::execute!(io::stdout(), crossterm::event::EnableMouseCapture)?;
        }
        #[cfg(unix)]
        crossterm::execute!(
            io::stdout(),
            crossterm::event::PushKeyboardEnhancementFlags(
                crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            )
        )?;
        Ok(TerminalGuard { mouse })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = write_ansi("\x1b]2;\x07");
        #[cfg(unix)]
        let _ = crossterm::execute!(io::stdout(), crossterm::event::PopKeyboardEnhancementFlags);
        if self.mouse {
            let _ = crossterm::execute!(io::stdout(), crossterm::event::DisableMouseCapture);
        }
        let _ = crossterm::execute!(io::stdout(), crossterm::terminal::LeaveAlternateScreen);
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// `Flag.OPENCODE_DISABLE_MOUSE` (`app.tsx:202`).
fn mouse_disabled_from_env() -> bool {
    std::env::var("OPENCODE_DISABLE_MOUSE").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// `terminal.suspend` (`app.tsx:870-879`): leave raw mode, `SIGTSTP` the
/// process group, then restore — `SIGCONT` resumes mid-`kill`.
mod suspend {
    #[cfg(unix)]
    pub async fn terminal_suspend_and_resume() {
        crossterm::terminal::disable_raw_mode().ok();
        crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen).ok();
        unsafe {
            libc::kill(0, libc::SIGTSTP);
        }
        crossterm::terminal::enable_raw_mode().ok();
        crossterm::execute!(std::io::stdout(), crossterm::terminal::EnterAlternateScreen).ok();
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use serde_json::Value;

    use crate::app::fast_boot_from_env;
    use crate::state;
    use crate::state::route::Route;
    use crate::state::{App, Args, Effect, ToastVariant};
    use crate::transport::api::{
        Location, MessageWithParts, MoveSession, ServerApi, SessionCommand, SessionCreate,
        SessionListQuery, SessionPrompt, SessionShell,
    };

    use super::*;

    #[test]
    fn fast_boot_env_flag_defaults_off() {
        std::env::remove_var("OPENCODE_FAST_BOOT");
        assert!(!fast_boot_from_env());
        std::env::set_var("OPENCODE_FAST_BOOT", "1");
        assert!(fast_boot_from_env());
        std::env::remove_var("OPENCODE_FAST_BOOT");
    }

    #[test]
    fn fast_boot_overlay_stays_hidden_when_ready_is_immediate() {
        let mut app = App::new(crate::config::TuiConfig::default(), Args::default(), None);
        state::update(&mut app, state::Msg::Tick(Default::default()));
        assert!(!app.ui.startup_loading.visible());
    }

    /// A `FakeApi` scripting the fork endpoint and (optionally) failing the
    /// first bootstrap call (`spec §8.1`).
    struct FakeApi {
        fork_result: Result<opencode_schema::session_v1::V1SessionInfo, String>,
        fail_bootstrap: bool,
    }

    #[async_trait]
    impl ServerApi for FakeApi {
        async fn path_get(&self, _loc: &Location) -> Result<Value> {
            if self.fail_bootstrap {
                return Err(anyhow::anyhow!("connect refused"));
            }
            unreachable!("not under test")
        }
        async fn session_fork(
            &self,
            _loc: &Location,
            _session_id: &str,
            _message_id: Option<&str>,
        ) -> Result<opencode_schema::session_v1::V1SessionInfo> {
            let mut result = self.fork_result.clone().map_err(|e| anyhow::anyhow!(e))?;
            result.id = "ses_forked".into();
            Ok(result)
        }
        async fn project_current(&self, _loc: &Location) -> Result<Value> {
            if self.fail_bootstrap {
                return Err(anyhow::anyhow!("connect refused"));
            }
            unreachable!("not under test")
        }
        async fn project_directories(&self, _loc: &Location, _project_id: &str) -> Result<Value> {
            unreachable!("not under test")
        }
        async fn experimental_workspace_list(&self, _loc: &Location) -> Result<Value> {
            unreachable!("not under test")
        }
        async fn experimental_workspace_status(&self, _loc: &Location) -> Result<Value> {
            unreachable!("not under test")
        }
        async fn config_providers(&self, _loc: &Location) -> Result<Value> {
            unreachable!("not under test")
        }
        async fn config_get(&self, _loc: &Location) -> Result<Value> {
            unreachable!("not under test")
        }
        async fn provider_list(&self, _loc: &Location) -> Result<Value> {
            unreachable!("not under test")
        }
        async fn provider_auth(&self, _loc: &Location) -> Result<Value> {
            unreachable!("not under test")
        }
        async fn app_agents(&self, _loc: &Location) -> Result<Value> {
            unreachable!("not under test")
        }
        async fn command_list(&self, _loc: &Location) -> Result<Value> {
            unreachable!("not under test")
        }
        async fn lsp_status(&self, _loc: &Location) -> Result<Value> {
            unreachable!("not under test")
        }
        async fn mcp_status(&self, _loc: &Location) -> Result<Value> {
            unreachable!("not under test")
        }
        async fn mcp_connect(&self, _loc: &Location, _name: &str) -> Result<bool> {
            unreachable!("not under test")
        }
        async fn mcp_disconnect(&self, _loc: &Location, _name: &str) -> Result<bool> {
            unreachable!("not under test")
        }
        async fn formatter_status(&self, _loc: &Location) -> Result<Value> {
            unreachable!("not under test")
        }
        async fn vcs_get(&self, _loc: &Location) -> Result<Value> {
            unreachable!("not under test")
        }
        async fn experimental_capabilities(&self, _loc: &Location) -> Result<Value> {
            unreachable!("not under test")
        }
        async fn experimental_console(&self, _loc: &Location) -> Result<Value> {
            unreachable!("not under test")
        }
        async fn experimental_resource_list(&self, _loc: &Location) -> Result<Value> {
            unreachable!("not under test")
        }
        async fn experimental_session_background(
            &self,
            _loc: &Location,
            _session_id: &str,
        ) -> Result<bool> {
            unreachable!("not under test")
        }
        async fn sync_start(&self, _loc: &Location) -> Result<bool> {
            unreachable!("not under test")
        }
        async fn global_upgrade(&self, _loc: &Location, _target: &str) -> Result<Value> {
            unreachable!("not under test")
        }
        async fn experimental_move_session(
            &self,
            _loc: &Location,
            _req: MoveSession,
        ) -> Result<()> {
            unreachable!("not under test")
        }
        async fn session_list(
            &self,
            _loc: &Location,
            _query: SessionListQuery,
        ) -> Result<Vec<opencode_schema::session_v1::V1SessionInfo>> {
            unreachable!("not under test")
        }
        async fn session_get(
            &self,
            _loc: &Location,
            _session_id: &str,
        ) -> Result<opencode_schema::session_v1::V1SessionInfo> {
            unreachable!("not under test")
        }
        async fn session_messages(
            &self,
            _loc: &Location,
            _session_id: &str,
            _limit: Option<u64>,
        ) -> Result<Vec<MessageWithParts>> {
            unreachable!("not under test")
        }
        async fn session_todo(
            &self,
            _loc: &Location,
            _session_id: &str,
        ) -> Result<Vec<opencode_schema::session_todo::TodoInfo>> {
            unreachable!("not under test")
        }
        async fn session_diff(
            &self,
            _loc: &Location,
            _session_id: &str,
            _message_id: Option<&str>,
        ) -> Result<Vec<opencode_schema::file_diff::SnapshotFileDiff>> {
            unreachable!("not under test")
        }
        async fn session_create(
            &self,
            _loc: &Location,
            _req: SessionCreate,
        ) -> Result<opencode_schema::session_v1::V1SessionInfo> {
            unreachable!("not under test")
        }
        async fn session_prompt(
            &self,
            _loc: &Location,
            _session_id: &str,
            _req: SessionPrompt,
        ) -> Result<MessageWithParts> {
            unreachable!("not under test")
        }
        async fn session_command(
            &self,
            _loc: &Location,
            _session_id: &str,
            _req: SessionCommand,
        ) -> Result<MessageWithParts> {
            unreachable!("not under test")
        }
        async fn session_shell(
            &self,
            _loc: &Location,
            _session_id: &str,
            _req: SessionShell,
        ) -> Result<MessageWithParts> {
            unreachable!("not under test")
        }
        async fn session_abort(&self, _loc: &Location, _session_id: &str) -> Result<bool> {
            unreachable!("not under test")
        }
        async fn session_revert(
            &self,
            _loc: &Location,
            _session_id: &str,
            _message_id: &str,
            _part_id: Option<&str>,
        ) -> Result<opencode_schema::session_v1::V1SessionInfo> {
            unreachable!("not under test")
        }
        async fn session_unrevert(
            &self,
            _loc: &Location,
            _session_id: &str,
        ) -> Result<opencode_schema::session_v1::V1SessionInfo> {
            unreachable!("not under test")
        }
        async fn session_summarize(
            &self,
            _loc: &Location,
            _session_id: &str,
            _provider_id: &str,
            _model_id: &str,
        ) -> Result<bool> {
            unreachable!("not under test")
        }
        async fn session_share(
            &self,
            _loc: &Location,
            _session_id: &str,
        ) -> Result<opencode_schema::session_v1::V1SessionInfo> {
            unreachable!("not under test")
        }
        async fn session_unshare(
            &self,
            _loc: &Location,
            _session_id: &str,
        ) -> Result<opencode_schema::session_v1::V1SessionInfo> {
            unreachable!("not under test")
        }
        async fn session_rename(
            &self,
            _loc: &Location,
            _session_id: &str,
            _title: &str,
        ) -> Result<opencode_schema::session_v1::V1SessionInfo> {
            unreachable!("not under test")
        }
        async fn session_delete(&self, _loc: &Location, _session_id: &str) -> Result<bool> {
            unreachable!("not under test")
        }
        async fn session_status(
            &self,
            _loc: &Location,
        ) -> Result<
            std::collections::BTreeMap<String, opencode_schema::session_status::SessionStatusInfo>,
        > {
            unreachable!("not under test")
        }
        async fn permission_reply(
            &self,
            _loc: &Location,
            _request_id: &str,
            _reply: opencode_schema::permission_v1::PermissionV1Reply,
            _message: Option<&str>,
        ) -> Result<bool> {
            unreachable!("not under test")
        }
        async fn question_reply(
            &self,
            _loc: &Location,
            _request_id: &str,
            _answers: Vec<opencode_schema::question_v1::QuestionV1Answer>,
        ) -> Result<bool> {
            unreachable!("not under test")
        }
        async fn question_reject(&self, _loc: &Location, _request_id: &str) -> Result<bool> {
            unreachable!("not under test")
        }
    }

    fn forked_session() -> opencode_schema::session_v1::V1SessionInfo {
        opencode_schema::session_v1::V1SessionInfo {
            id: "ses_x".into(),
            slug: "x".into(),
            project_id: "prj".into(),
            workspace_id: None,
            directory: "/x".into(),
            path: None,
            parent_id: None,
            summary: None,
            cost: None,
            tokens: None,
            share: None,
            title: "X".into(),
            agent: None,
            model: None,
            version: "1".into(),
            metadata: None,
            time: opencode_schema::session_v1::V1SessionTime {
                created: 1,
                updated: 1,
                compacting: None,
                archived: None,
            },
            permission: None,
            revert: None,
        }
    }

    fn fake_app() -> App {
        App::new(crate::config::TuiConfig::default(), Args::default(), None)
    }

    #[tokio::test]
    async fn session_fork_effect_navigates_on_success() {
        let mut app = fake_app();
        let api = FakeApi {
            fork_result: Ok(forked_session()),
            fail_bootstrap: false,
        };
        execute_effect(
            &mut app,
            Arc::new(api),
            Effect::SessionFork {
                session_id: "ses_a".into(),
                navigate: true,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            app.state.route.data,
            Route::Session {
                session_id: "ses_forked".into(),
                prompt: None,
            }
        );
        assert!(app.ui.toasts.is_empty());
    }

    #[tokio::test]
    async fn session_fork_effect_toasts_on_failure() {
        let mut app = fake_app();
        let api = FakeApi {
            fork_result: Err("fork failed".into()),
            fail_bootstrap: false,
        };
        execute_effect(
            &mut app,
            Arc::new(api),
            Effect::SessionFork {
                session_id: "ses_a".into(),
                navigate: true,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            app.state.route.data,
            Route::Home { prompt: None },
            "no navigation on failure"
        );
        assert_eq!(app.ui.toasts.len(), 1);
        assert_eq!(app.ui.toasts[0].variant, ToastVariant::Error);
        assert_eq!(app.ui.toasts[0].message, "Failed to fork session");
    }

    #[tokio::test]
    async fn bootstrap_fatal_failure_exits_with_reason() {
        // `app.tsx:546-551`: a fatal phase-1 error exits the TUI with the
        // error as the exit reason.
        let mut app = fake_app();
        let api = FakeApi {
            fork_result: Ok(forked_session()),
            fail_bootstrap: true,
        };
        execute_effect(&mut app, Arc::new(api), Effect::Bootstrap { fatal: true })
            .await
            .unwrap();
        assert!(app.ui.exit);
        assert!(app
            .ui
            .exit_reason
            .as_deref()
            .expect("reason")
            .contains("connect refused"));
    }
}

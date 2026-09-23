//! ratatui TUI — a stateless HTTP+SSE client of the alforria server (M8).
//!
//! TS reference: `packages/tui` (whole package is ground truth; see
//! `scratchpad/specs/M8.md`). Elm-style model: one `update()`, full redraw
//! per change. This crate is transport-first: everything above
//! `transport::api` is testable against a `FakeApi` + fake event source.
//!
//! The terminal runtime (this file) owns the three long-lived task
//! families of spec §2.2: the SSE loop (`transport::events`), the input
//! pump (crossterm) and the 40 ms tick; effects returned by `update`
//! execute as spawned tasks (spec §2.1) — the driver executor.

pub mod app;
pub mod attention;
pub mod clipboard;
pub mod command;
pub mod config;
pub mod editor;
pub mod keymap;
pub mod state;
pub mod transcript;
pub mod transport;
pub mod ui;

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::clipboard::Clipboard as _;
use crate::state::local::McpAction;
use crate::state::route::Route;
use crate::state::sync::object_of;
use crate::state::{App, Args, Effect, Msg, Toast, ToastVariant};
use crate::transport::api::{
    HttpClientConfig, HttpServerApi, Location, MoveSession, MoveSessionDestination, ServerApi,
};
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

/// Build the forked route prompt from the message parts
/// (`dialog-message.tsx:38-51`): text input + stripped file parts.
fn prompt_info_from_parts(
    parts: &Vec<alforria_schema::session_v1::V1Part>,
) -> crate::state::route::PromptInfo {
    let mut input = String::new();
    let mut file_parts = Vec::new();
    for part in parts {
        match part {
            alforria_schema::session_v1::V1Part::Text {
                text,
                synthetic: Some(false),
                ..
            } => {
                input.push_str(text);
            }
            alforria_schema::session_v1::V1Part::File { .. } => {
                if let Ok(value) = serde_json::to_value(part) {
                    file_parts.push(value);
                }
            }
            _ => {}
        }
    }
    crate::state::route::PromptInfo {
        input,
        mode: None,
        parts: file_parts,
    }
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

    let mut app = App::new(
        input.config.clone(),
        input.args.clone(),
        input.state_dir.as_deref(),
    );
    app::apply_args(&mut app);
    let app = Arc::new(tokio::sync::Mutex::new(app));

    let (msg_tx, msg_rx) = tokio::sync::mpsc::unbounded_channel::<Msg>();
    spawn_sse(source, msg_tx.clone());
    spawn_input_pump(msg_tx.clone());
    spawn_tick(msg_tx.clone());
    spawn_sighup(msg_tx.clone());
    // crossterm only reports resizes (SIGWINCH) — the terminal size at
    // startup has to be fed to the app explicitly, or the layout stays
    // at the 80x1 default until the user resizes.
    if let Ok((columns, rows)) = crossterm::terminal::size() {
        let _ = msg_tx.send(Msg::Resize(columns, rows));
    }

    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(io::stdout()))?;
    let exit = event_loop(&app, api, msg_rx, &mut terminal).await;

    // §6 N6: `docs.open` prints the URL instead of opening a browser —
    // after the alternate screen is gone.
    let app = app.lock().await;
    let mut app = app;
    let opened_urls = std::mem::take(&mut app.ui.opened_urls);
    drop(_guard);
    for url in opened_urls {
        println!("{url}");
    }
    drop(app);

    let exit = exit?;
    print_exit(&exit);
    Ok(exit)
}

/// One `update` pass per message. Effects run as spawned tasks
/// (spec §2.1 — "a driver executor turns each into a task"): a turn
/// that blocks on a permission/question reply keeps the message pump
/// running, exactly like the TS fire-and-forget `sdk.client.*` calls.
async fn event_loop(
    app: &Arc<tokio::sync::Mutex<App>>,
    api: Arc<dyn ServerApi>,
    mut messages: tokio::sync::mpsc::UnboundedReceiver<Msg>,
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<io::Stdout>>,
) -> Result<Exit> {
    let title_disabled = app::terminal_title_disabled_from_env();
    let mut last_title: Option<String> = None;
    let mut pending = vec![Effect::Bootstrap { fatal: true }];

    loop {
        for effect in pending.drain(..) {
            let app = Arc::clone(app);
            let api = Arc::clone(&api);
            tokio::spawn(async move {
                execute_effect(&app, api, effect).await;
            });
        }
        let exited = {
            let mut app = app.lock().await;
            if app.ui.exit {
                true
            } else {
                terminal.draw(|frame| view(&mut app, frame))?;
                apply_title(&app, title_disabled, &mut last_title)?;
                false
            }
        };
        if exited {
            break;
        }

        let Some(msg) = messages.recv().await else {
            app.lock().await.exit(None);
            break;
        };
        pending = {
            let mut app = app.lock().await;
            state::update(&mut app, msg)
        };
    }

    let app = app.lock().await;
    Ok(Exit {
        epilogue: app::epilogue(&app),
        reason: app.ui.exit_reason.clone(),
    })
}

/// The formatted transcript of the route session — the
/// `session.copy`/`session.export` input (`session/index.tsx:923-1014`).
fn route_transcript(app: &App, options: &transcript::TranscriptOptions) -> Option<String> {
    let Route::Session { session_id, .. } = &app.state.route.data else {
        return None;
    };
    let session = app.state.sync.session(session_id)?;
    let messages = app
        .state
        .sync
        .message
        .get(session_id)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let messages = messages
        .iter()
        .map(|info| {
            let parts = app
                .state
                .sync
                .part
                .get(match info {
                    alforria_schema::session_v1::V1Message::User { id, .. }
                    | alforria_schema::session_v1::V1Message::Assistant { id, .. } => id,
                })
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            transcript::MessageParts {
                info: info.clone(),
                parts: parts.to_vec(),
            }
        })
        .collect::<Vec<_>>();
    Some(transcript::format_transcript(
        session,
        &messages,
        options,
        &app.state.sync.provider,
    ))
}

/// `session.copy` (`session/index.tsx:916-947`): clipboard write of the
/// formatted transcript with the fixed success/failure toasts.
async fn copy_transcript(
    app: &Arc<tokio::sync::Mutex<App>>,
    options: transcript::TranscriptOptions,
) {
    let (text, clipboard) = {
        let app = app.lock().await;
        (
            route_transcript(&app, &options),
            clipboard::system_clipboard(),
        )
    };
    let copied = text.is_some_and(|text| clipboard.write(&text).is_ok());
    let mut app = app.lock().await;
    app.show_toast(Toast {
        title: None,
        variant: if copied {
            ToastVariant::Success
        } else {
            ToastVariant::Error
        },
        message: if copied {
            "Session transcript copied to clipboard!".to_string()
        } else {
            "Failed to copy session transcript".to_string()
        },
        duration_ms: 5000,
    });
}

/// `session.export` (`session/index.tsx:946-1020`): the export-options
/// confirm — `writeExport` + the `$EDITOR` open (the renderer suspends
/// around the child process).
async fn export_transcript(
    app: &Arc<tokio::sync::Mutex<App>>,
    filename: String,
    options: transcript::TranscriptOptions,
    open_without_saving: bool,
) {
    let text = {
        let app = app.lock().await;
        route_transcript(&app, &options)
    };
    let Some(text) = text else {
        return;
    };
    // `paths.cwd` — the export directory.
    let export_dir = std::env::current_dir()
        .ok()
        .unwrap_or_else(|| PathBuf::from("."));
    let cwd = {
        let app = app.lock().await;
        app.state
            .project
            .instance_path
            .worktree
            .clone()
            .filter(|worktree| worktree != "/")
            .or_else(|| app.state.project.instance_path.directory.clone())
    };
    let cwd = cwd
        .filter(|cwd| Path::new(cwd).exists())
        .map(PathBuf::from)
        .unwrap_or_else(|| export_dir.clone());

    if open_without_saving {
        let _ = editor::open_editor(&text, Some(&cwd));
        return;
    }

    let filename = filename.trim().to_string();
    let filepath = export_dir.join(&filename);
    let write_result = std::fs::write(&filepath, &text);
    let edited = editor::open_editor(&text, Some(&cwd));
    let mut app = app.lock().await;
    if write_result.is_err() {
        app.show_toast(Toast {
            title: None,
            variant: ToastVariant::Error,
            message: "Failed to export session".to_string(),
            duration_ms: 5000,
        });
        return;
    }
    if let Some(edited) = edited {
        let _ = std::fs::write(&filepath, edited);
    }
    app.show_toast(Toast {
        title: None,
        variant: ToastVariant::Success,
        message: format!("Session exported to {filename}"),
        duration_ms: 5000,
    });
}

/// Execute one effect against the server seam. Effects that can block
/// on a user reply (the submit pipeline) release the app lock across
/// their awaits; the rest hold it for the duration.
pub async fn execute_effect(
    app: &Arc<tokio::sync::Mutex<App>>,
    api: Arc<dyn ServerApi>,
    effect: Effect,
) {
    match effect {
        Effect::Bootstrap { fatal } => {
            let mut app = app.lock().await;
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
                let mut app = app.lock().await;
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
            let result = api
                .session_fork(&Location::default(), &session_id, None)
                .await;
            let mut app = app.lock().await;
            match result {
                Ok(session) => {
                    if navigate {
                        app.state.route.navigate(Route::Session {
                            session_id: session.id.clone(),
                            prompt: None,
                        });
                    }
                }
                Err(_) => {
                    app.show_toast(Toast {
                        title: None,
                        variant: ToastVariant::Error,
                        message: "Failed to fork session".to_string(),
                        duration_ms: 5000,
                    });
                }
            }
        }
        Effect::SuspendTerminal => {
            suspend::terminal_suspend_and_resume().await;
        }
        Effect::SessionShare { session_id } => {
            let result = api.session_share(&Location::default(), &session_id).await;
            let mut app = app.lock().await;
            match result {
                Ok(session) => {
                    if let Some(share) = session.share {
                        // `copy()` (`session/index.tsx:468-473`): the
                        // toast reflects the clipboard result.
                        let toast = match clipboard::system_clipboard().write(&share.url) {
                            Ok(()) => Toast {
                                title: None,
                                variant: ToastVariant::Success,
                                message: "Share URL copied to clipboard!".to_string(),
                                duration_ms: 5000,
                            },
                            Err(_) => Toast {
                                title: None,
                                variant: ToastVariant::Error,
                                message: "Failed to copy URL to clipboard".to_string(),
                                duration_ms: 5000,
                            },
                        };
                        app.show_toast(toast);
                    }
                }
                Err(_) => {
                    app.show_toast(Toast {
                        title: None,
                        variant: ToastVariant::Error,
                        message: "Failed to share session".to_string(),
                        duration_ms: 5000,
                    });
                }
            }
        }
        Effect::SessionUnshare { session_id } => {
            let result = api.session_unshare(&Location::default(), &session_id).await;
            let mut app = app.lock().await;
            app.show_toast(match result {
                Ok(_) => Toast {
                    title: None,
                    variant: ToastVariant::Success,
                    message: "Session unshared successfully".to_string(),
                    duration_ms: 5000,
                },
                Err(_) => Toast {
                    title: None,
                    variant: ToastVariant::Error,
                    message: "Failed to unshare session".to_string(),
                    duration_ms: 5000,
                },
            });
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
        Effect::PermissionReply {
            request_id,
            reply,
            message,
        } => {
            // `workspace: project.workspace.current()` on every reply
            // (permission.tsx:172, 185; question.tsx:48-62).
            let workspace = app.lock().await.state.project.workspace.current.clone();
            let loc = Location {
                directory: None,
                workspace,
            };
            let _ = api
                .permission_reply(&loc, &request_id, reply, message.as_deref())
                .await;
        }
        Effect::QuestionReply {
            request_id,
            answers,
        } => {
            let workspace = app.lock().await.state.project.workspace.current.clone();
            let loc = Location {
                directory: None,
                workspace,
            };
            let _ = api.question_reply(&loc, &request_id, answers).await;
        }
        Effect::QuestionReject { request_id } => {
            let workspace = app.lock().await.state.project.workspace.current.clone();
            let loc = Location {
                directory: None,
                workspace,
            };
            let _ = api.question_reject(&loc, &request_id).await;
        }
        Effect::SessionRename { session_id, title } => {
            let error = api
                .session_rename(&Location::default(), &session_id, &title)
                .await
                .err();
            if let Some(error) = error {
                let mut app = app.lock().await;
                app.show_toast(Toast {
                    title: None,
                    variant: ToastVariant::Error,
                    message: format!("{error:#}"),
                    duration_ms: 5000,
                });
            }
        }
        Effect::AuthSet { provider_id, key } => {
            let result = api.auth_set(&Location::default(), &provider_id, &key).await;
            {
                let mut app = app.lock().await;
                app.show_toast(match result {
                    Ok(_) => Toast {
                        title: None,
                        variant: ToastVariant::Success,
                        message: format!("Saved credential for {provider_id}"),
                        duration_ms: 5000,
                    },
                    Err(error) => Toast {
                        title: None,
                        variant: ToastVariant::Error,
                        message: format!("{error:#}"),
                        duration_ms: 5000,
                    },
                });
            }
            // `instance.dispose()` + `sync.bootstrap()`
            // (`dialog-provider.tsx:406-407`) — the connected-provider
            // state is credential-derived. Box::pin: the recursive async
            // call needs indirection.
            Box::pin(execute_effect(app, api, Effect::Bootstrap { fatal: false })).await;
        }
        Effect::SessionDelete { session_id } => {
            let error = api
                .session_delete(&Location::default(), &session_id)
                .await
                .err();
            if let Some(error) = error {
                let mut app = app.lock().await;
                app.show_toast(Toast {
                    title: None,
                    variant: ToastVariant::Error,
                    message: format!("{error:#}"),
                    duration_ms: 5000,
                });
            }
        }
        Effect::McpToggle { name } => {
            // `dialog-mcp.tsx:49-66`: toggle then refresh the MCP
            // status from the server.
            let mut app = app.lock().await;
            let action = app.state.local.mcp_toggle(&app.state.sync, &name);
            let result = match &action {
                McpAction::Connect(name) => api.mcp_connect(&Location::default(), name).await,
                McpAction::Disconnect(name) => api.mcp_disconnect(&Location::default(), name).await,
            };
            if result.is_ok() {
                if let Ok(status) = api.mcp_status(&Location::default()).await {
                    app.state.sync.mcp = object_of(status);
                }
            }
        }
        Effect::GlobalUpgrade { target } => {
            let _ = api.global_upgrade(&Location::default(), &target).await;
        }
        Effect::SessionMove {
            session_id,
            directory,
        } => {
            let _ = api
                .experimental_move_session(
                    &Location::default(),
                    MoveSession {
                        session_id: session_id.clone(),
                        destination: MoveSessionDestination {
                            directory: directory.clone(),
                        },
                        move_changes: None,
                    },
                )
                .await;
        }
        Effect::ProjectDirectories { project_id } => {
            // `dialog-move-session.tsx:60-78`.
            if let Ok(value) = api
                .project_directories(&Location::default(), &project_id)
                .await
            {
                let directories = match value {
                    serde_json::Value::Array(items) => items,
                    _ => Vec::new(),
                };
                let mut app = app.lock().await;
                app.ui.move_directories = Some(directories);
            }
        }

        Effect::SessionForkFromMessage {
            session_id,
            message_id,
            seed_prompt,
        } => {
            let result = api
                .session_fork(&Location::default(), &session_id, message_id.as_deref())
                .await;
            let mut app = app.lock().await;
            match result {
                Ok(forked) => {
                    // Seed the prompt from the forked message parts
                    // (dialog-message.tsx:38-51).
                    let prompt = if seed_prompt {
                        message_id
                            .as_deref()
                            .and_then(|id| app.state.sync.part.get(id))
                            .map(prompt_info_from_parts)
                    } else {
                        None
                    };
                    app.state.route.navigate(Route::Session {
                        session_id: forked.id.clone(),
                        prompt,
                    });
                }
                Err(error) => app.show_toast(Toast {
                    title: None,
                    variant: ToastVariant::Error,
                    message: format!("{error:#}"),
                    duration_ms: 5000,
                }),
            }
        }
        Effect::SessionExport {
            filename,
            thinking,
            tool_details,
            assistant_metadata,
            open_without_saving,
        } => {
            export_transcript(
                app,
                filename,
                transcript::TranscriptOptions {
                    thinking,
                    tool_details,
                    assistant_metadata,
                },
                open_without_saving,
            )
            .await;
        }
        Effect::SessionRefresh => {
            let mut app = app.lock().await;
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
        Effect::SessionCopyTranscript {
            thinking,
            tool_details,
            assistant_metadata,
        } => {
            copy_transcript(
                app,
                transcript::TranscriptOptions {
                    thinking,
                    tool_details,
                    assistant_metadata,
                },
            )
            .await;
        }
        Effect::SessionMount {
            session_id,
            previous_workspace,
        } => {
            // `createEffect` (`session/index.tsx:286-324`).
            let info = api.session_get(&Location::default(), &session_id).await;
            let mut app = app.lock().await;
            match info {
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
            let mut app = app.lock().await;
            let _ = state::sync::session_sync(&mut app.state, api.as_ref(), &session_id).await;
        }
        Effect::ClipboardWrite {
            text,
            success,
            failure,
        } => match clipboard::system_clipboard().write(&text) {
            Ok(()) => {
                if let Some(toast) = success {
                    let mut app = app.lock().await;
                    app.show_toast(toast);
                }
            }
            Err(_) => {
                if let Some(toast) = failure {
                    let mut app = app.lock().await;
                    app.show_toast(toast);
                }
            }
        },
        Effect::OpenUrl { url } => {
            // §6 N6: no browser from a TUI — the URL prints on exit.
            let mut app = app.lock().await;
            app.ui.opened_urls.push(url);
        }
        Effect::PromptPaste => {
            let pasted = crate::clipboard::system_clipboard().read();
            let mut app = app.lock().await;
            match pasted {
                Ok(crate::clipboard::ClipboardContent::Text(text)) => {
                    crate::state::prompt::paste_input_text(&mut app, &text);
                }
                Ok(crate::clipboard::ClipboardContent::Image { mime, data_base64 }) => {
                    crate::state::prompt::paste_attachment(
                        &mut app,
                        &crate::state::prompt::Attachment {
                            filename: Some("clipboard".to_string()),
                            filepath: None,
                            mime,
                            content: data_base64.into_bytes(),
                        },
                    );
                }
                Ok(crate::clipboard::ClipboardContent::Pdf { data_base64 }) => {
                    crate::state::prompt::paste_attachment(
                        &mut app,
                        &crate::state::prompt::Attachment {
                            filename: Some("clipboard".to_string()),
                            filepath: None,
                            mime: "application/pdf".to_string(),
                            content: data_base64.into_bytes(),
                        },
                    );
                }
                Err(_) => {}
            }
        }
        Effect::OpenPromptEditor { value } => {
            // `openEditor` suspends the renderer around the child
            // process (`editor.ts:40`).
            let cwd = {
                let app = app.lock().await;
                app.state
                    .project
                    .instance_path
                    .worktree
                    .clone()
                    .or_else(|| app.state.project.instance_path.directory.clone())
            };
            crossterm::terminal::disable_raw_mode().ok();
            crossterm::execute!(io::stdout(), crossterm::terminal::LeaveAlternateScreen).ok();
            let edited = editor::open_editor(&value, cwd.as_deref().map(Path::new));
            crossterm::terminal::enable_raw_mode().ok();
            crossterm::execute!(io::stdout(), crossterm::terminal::EnterAlternateScreen).ok();
            if let Some(content) = edited {
                let normalized = editor::normalize_prompt_content(&content);
                let mut app = app.lock().await;
                crate::state::prompt::apply_editor_content(&mut app, &normalized);
            }
        }
        Effect::PromptSubmit { payload } => {
            crate::state::prompt::run_submit(app, api.as_ref(), *payload).await;
        }
        Effect::Attention {
            title,
            message,
            notification,
            bell,
        } => {
            let app = app.lock().await;
            crate::attention::notify(
                &crate::attention::NotifyRequest {
                    title,
                    message,
                    notification,
                    bell,
                },
                &app.config.attention,
                app.attention.as_ref(),
            );
        }
    }
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
            // Bracketed paste (`prompt/index.tsx:1396-1420`).
            Ok(crossterm::event::Event::Paste(text)) => {
                if messages.send(Msg::Paste(text)).is_err() {
                    return;
                }
            }
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
#[cfg(unix)]
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

#[cfg(not(unix))]
fn spawn_sighup(_messages: tokio::sync::mpsc::UnboundedSender<Msg>) {}

/// Raw mode + alternate screen + optional mouse capture + the kitty
/// keyboard protocol (`app.tsx:186-213`).
struct TerminalGuard {
    mouse: bool,
}

impl TerminalGuard {
    fn enter(mouse: bool) -> Result<TerminalGuard> {
        crossterm::terminal::enable_raw_mode()?;
        crossterm::execute!(io::stdout(), crossterm::terminal::EnterAlternateScreen)?;
        let _ = crossterm::execute!(io::stdout(), crossterm::event::EnableBracketedPaste);
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
        let _ = crossterm::execute!(io::stdout(), crossterm::event::DisableBracketedPaste);
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

    // No terminal suspend on Windows — the keybinding is already disabled
    // (`terminal_suspend_supported`).
    #[cfg(not(unix))]
    pub async fn terminal_suspend_and_resume() {}
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use serde_json::Value;

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
        assert!(!crate::app::fast_boot_from_env());
        std::env::set_var("OPENCODE_FAST_BOOT", "1");
        assert!(crate::app::fast_boot_from_env());
        std::env::remove_var("OPENCODE_FAST_BOOT");
    }

    #[test]
    fn fast_boot_overlay_stays_hidden_when_ready_is_immediate() {
        let mut app = App::new(crate::config::TuiConfig::default(), Args::default(), None);
        state::update(&mut app, state::Msg::Tick(Default::default()));
        assert!(!app.ui.startup_loading.visible());
    }

    /// A `FakeApi` scripting the fork endpoint and (optionally) failing
    /// the first bootstrap call (spec §8.1).
    struct FakeApi {
        fork_result: Result<alforria_schema::session_v1::V1SessionInfo, String>,
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
        ) -> Result<alforria_schema::session_v1::V1SessionInfo> {
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
        ) -> Result<Vec<alforria_schema::session_v1::V1SessionInfo>> {
            unreachable!("not under test")
        }
        async fn session_get(
            &self,
            _loc: &Location,
            _session_id: &str,
        ) -> Result<alforria_schema::session_v1::V1SessionInfo> {
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
        ) -> Result<Vec<alforria_schema::session_todo::TodoInfo>> {
            unreachable!("not under test")
        }
        async fn session_diff(
            &self,
            _loc: &Location,
            _session_id: &str,
            _message_id: Option<&str>,
        ) -> Result<Vec<alforria_schema::file_diff::SnapshotFileDiff>> {
            unreachable!("not under test")
        }
        async fn session_create(
            &self,
            _loc: &Location,
            _req: SessionCreate,
        ) -> Result<alforria_schema::session_v1::V1SessionInfo> {
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
        ) -> Result<alforria_schema::session_v1::V1SessionInfo> {
            unreachable!("not under test")
        }
        async fn session_unrevert(
            &self,
            _loc: &Location,
            _session_id: &str,
        ) -> Result<alforria_schema::session_v1::V1SessionInfo> {
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
        ) -> Result<alforria_schema::session_v1::V1SessionInfo> {
            unreachable!("not under test")
        }
        async fn session_unshare(
            &self,
            _loc: &Location,
            _session_id: &str,
        ) -> Result<alforria_schema::session_v1::V1SessionInfo> {
            unreachable!("not under test")
        }
        async fn session_rename(
            &self,
            _loc: &Location,
            _session_id: &str,
            _title: &str,
        ) -> Result<alforria_schema::session_v1::V1SessionInfo> {
            unreachable!("not under test")
        }
        async fn session_delete(&self, _loc: &Location, _session_id: &str) -> Result<bool> {
            unreachable!("not under test")
        }
        async fn session_status(
            &self,
            _loc: &Location,
        ) -> Result<
            std::collections::BTreeMap<String, alforria_schema::session_status::SessionStatusInfo>,
        > {
            unreachable!("not under test")
        }
        async fn permission_reply(
            &self,
            _loc: &Location,
            _request_id: &str,
            _reply: alforria_schema::permission_v1::PermissionV1Reply,
            _message: Option<&str>,
        ) -> Result<bool> {
            unreachable!("not under test")
        }
        async fn question_reply(
            &self,
            _loc: &Location,
            _request_id: &str,
            _answers: Vec<alforria_schema::question_v1::QuestionV1Answer>,
        ) -> Result<bool> {
            unreachable!("not under test")
        }
        async fn question_reject(&self, _loc: &Location, _request_id: &str) -> Result<bool> {
            unreachable!("not under test")
        }
        async fn auth_set(&self, _loc: &Location, _provider_id: &str, _key: &str) -> Result<bool> {
            unreachable!("not under test")
        }
    }

    fn forked_session() -> alforria_schema::session_v1::V1SessionInfo {
        alforria_schema::session_v1::V1SessionInfo {
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
            time: alforria_schema::session_v1::V1SessionTime {
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
        let app = Arc::new(tokio::sync::Mutex::new(fake_app()));
        let api: Arc<dyn ServerApi> = Arc::new(FakeApi {
            fork_result: Ok(forked_session()),
            fail_bootstrap: false,
        });
        execute_effect(
            &app,
            api,
            Effect::SessionFork {
                session_id: "ses_a".into(),
                navigate: true,
            },
        )
        .await;
        let app = app.lock().await;
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
        let app = Arc::new(tokio::sync::Mutex::new(fake_app()));
        let api: Arc<dyn ServerApi> = Arc::new(FakeApi {
            fork_result: Err("fork failed".into()),
            fail_bootstrap: false,
        });
        execute_effect(
            &app,
            api,
            Effect::SessionFork {
                session_id: "ses_a".into(),
                navigate: true,
            },
        )
        .await;
        let app = app.lock().await;
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
        let app = Arc::new(tokio::sync::Mutex::new(fake_app()));
        let api: Arc<dyn ServerApi> = Arc::new(FakeApi {
            fork_result: Ok(forked_session()),
            fail_bootstrap: true,
        });
        execute_effect(&app, api, Effect::Bootstrap { fatal: true }).await;
        let app = app.lock().await;
        assert!(app.ui.exit);
        assert!(app
            .ui
            .exit_reason
            .as_deref()
            .expect("reason")
            .contains("connect refused"));
    }
}

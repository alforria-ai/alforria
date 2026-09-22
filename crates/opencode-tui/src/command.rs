//! The command registry — the three `useBindings({ commands })` sets
//! (`app.tsx:561-962`, `routes/session/index.tsx:465-1084`,
//! `component/prompt/index.tsx:335-559,736-798`) plus the home tips
//! command. Commands are pure lookup + run — they never read the
//! terminal; effects do the I/O.

use opencode_schema::session_status::SessionStatusInfo;
use opencode_schema::session_v1::{V1Message, V1Part, V1SessionInfo};
use serde_json::{json, Value};

use crate::keymap::COMMAND_PALETTE_COMMAND;
use crate::state::kv::keys;
use crate::state::route::Route;
use crate::state::{App, Effect, PendingDialog, Toast, ToastVariant};
use crate::ui::theme::Mode as ThemeMode;

#[cfg(test)]
#[path = "command_tests.rs"]
mod tests;

/// One palette entry — the `getCommandEntries` shape
/// (`command-palette.tsx:43-61`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandInfo {
    pub name: &'static str,
    pub title: String,
    pub category: &'static str,
    pub hidden: bool,
    pub suggested: bool,
    pub slash_name: Option<&'static str>,
    pub slash_aliases: &'static [&'static str],
    /// The `enabled` predicate evaluated at registry build time.
    pub enabled: bool,
}

impl CommandInfo {
    fn new(name: &'static str, title: impl Into<String>, category: &'static str) -> CommandInfo {
        CommandInfo {
            name,
            title: title.into(),
            category,
            hidden: false,
            suggested: false,
            slash_name: None,
            slash_aliases: &[],
            enabled: true,
        }
    }
}

/// `Flag.OPENCODE_EXPERIMENTAL_WORKSPACES` (`core/src/flag/flag.ts:50`).
pub fn experimental_workspaces() -> bool {
    std::env::var("OPENCODE_EXPERIMENTAL_WORKSPACES").is_ok_and(|v| !v.is_empty() && v != "0")
}

fn current_session(app: &App) -> Option<&V1SessionInfo> {
    match &app.state.route.data {
        Route::Session { session_id, .. } => app.state.sync.session(session_id),
        _ => None,
    }
}

fn route_is_session(app: &App) -> bool {
    matches!(app.state.route.data, Route::Session { .. })
}

/// `dialog.replace(...)` — the opened dialog's effects are dropped here
/// (`open` only returns effects for the move-session fetch, which
/// opens via its own arm below).
fn show_dialog(app: &mut App, dialog: PendingDialog) {
    let _ = crate::ui::dialogs::open(app, dialog);
}

fn clear_dialog(app: &mut App) {
    crate::ui::dialogs::clear(app);
}

fn toast(app: &mut App, variant: ToastVariant, message: impl Into<String>, duration_ms: u64) {
    app.show_toast(Toast {
        title: None,
        variant,
        message: message.into(),
        duration_ms,
    });
}

// --------------------------------------------------- transcript scroll

/// `scroll.scrollBy(±scroll.height / 2)` — a page is half the
/// viewport (`session/index.tsx:756-759`).
fn scroll_by_half(app: &mut App, sign: i64) {
    let step = (app.ui.session_scroll.viewport_height as i64 / 4).max(1);
    app.ui.session_scroll.scroll_by(step * sign);
}

/// `scrollToMessage` + `findNextVisibleMessage`
/// (`session/index.tsx:378-421`): only messages with a non-synthetic
/// non-ignored text part count, ±10px threshold.
fn scroll_to_next_visible_message(app: &mut App, direction: i64) {
    let scroll = &app.ui.session_scroll;
    let top = scroll.effective_y() as i64;
    let threshold = 10;
    let target = if direction > 0 {
        scroll
            .children
            .iter()
            .find(|(_, y)| (*y as i64) > top + threshold)
    } else {
        scroll
            .children
            .iter()
            .rev()
            .find(|(_, y)| (*y as i64) < top - threshold)
    };
    match target {
        Some((_, y)) => app.ui.session_scroll.scroll_by(*y as i64 - top - 1),
        None => app
            .ui
            .session_scroll
            .scroll_by(direction * scroll.viewport_height as i64),
    }
}

/// `session.messages_last_user` (`session/index.tsx:832-861`).
fn scroll_to_last_user(app: &mut App) {
    let Some((_, y)) = app.ui.session_scroll.children.last().cloned() else {
        return;
    };
    let top = app.ui.session_scroll.effective_y() as i64;
    app.ui.session_scroll.scroll_by(y as i64 - top - 1);
}

/// `useConnected()` (`component/use-connected.tsx`) — see
/// `state::connected`.
fn connected(app: &App) -> bool {
    crate::state::connected(app)
}

/// `sidebarVisible()` (`routes/session/index.tsx:271-276`).
pub fn sidebar_visible(app: &App) -> bool {
    let Some(session) = current_session(app) else {
        return false;
    };
    if session.parent_id.is_some() {
        return false;
    }
    if app.ui.sidebar_open {
        return true;
    }
    let sidebar = app
        .state
        .kv
        .get(keys::SIDEBAR, json!("auto"))
        .as_str()
        .unwrap_or("auto")
        .to_string();
    sidebar == "auto" && app.ui.terminal_width > 120
}

fn message_id(message: &V1Message) -> &str {
    match message {
        V1Message::User { id, .. } | V1Message::Assistant { id, .. } => id,
    }
}

fn tool_state_metadata(
    state: &opencode_schema::session_v1::V1ToolState,
) -> Option<&serde_json::Map<String, Value>> {
    use opencode_schema::session_v1::V1ToolState;
    match state {
        V1ToolState::Running { metadata, .. } | V1ToolState::Error { metadata, .. } => {
            metadata.as_ref()
        }
        V1ToolState::Pending { .. } | V1ToolState::Completed { .. } => None,
    }
}

/// `foregroundTasks()` (`routes/session/index.tsx:219-229`) — running
/// `task` tool parts that are not backgrounded.
pub fn foreground_tasks(app: &App) -> usize {
    if !app.state.sync.capabilities {
        return 0;
    }
    let Route::Session { session_id, .. } = &app.state.route.data else {
        return 0;
    };
    app.state
        .sync
        .message
        .get(session_id)
        .map(|messages| {
            messages
                .iter()
                .flat_map(|message| {
                    app.state
                        .sync
                        .part
                        .get(message_id(message))
                        .map(Vec::as_slice)
                        .unwrap_or(&[])
                })
                .filter(|part| {
                    matches!(part,
                        V1Part::Tool { tool, state, .. }
                        if tool == "task"
                            && matches!(
                                state,
                                opencode_schema::session_v1::V1ToolState::Running { .. }
                            )
                            && tool_state_metadata(state).and_then(|m| m.get("background"))
                                != Some(&json!(true)))
                })
                .count()
        })
        .unwrap_or(0)
}

/// `messagesBeforeRevert()` (`routes/session/index.tsx:213-218`).
fn messages_before_revert(app: &App) -> Vec<&V1Message> {
    let Route::Session { session_id, .. } = &app.state.route.data else {
        return Vec::new();
    };
    let messages: Vec<&V1Message> = app
        .state
        .sync
        .message
        .get(session_id)
        .map(|messages| messages.iter().collect())
        .unwrap_or_default();
    let Some(revert) = current_session(app).and_then(|session| session.revert.as_ref()) else {
        return messages;
    };
    match messages
        .iter()
        .position(|message| message_id(message) == revert.message_id)
    {
        Some(index) => messages[..index].to_vec(),
        None => messages,
    }
}

/// `prompt?.set(...)` for undo (`session/index.tsx:623-643`): input from
/// non-synthetic text parts, file parts carried along.
fn set_prompt_from_parts(app: &mut App, target: &str) {
    let mut input = String::new();
    let mut parts = Vec::new();
    for part in app
        .state
        .sync
        .part
        .get(target)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        match part {
            V1Part::Text {
                text, synthetic, ..
            } if !synthetic.unwrap_or(false) => {
                input.push_str(text);
            }
            V1Part::File { .. } => {
                if let Ok(value) = serde_json::to_value(part) {
                    if let Some(part) = crate::state::prompt::PromptPart::from_value(&value) {
                        parts.push(part);
                    }
                }
            }
            _ => {}
        }
    }
    set_prompt(app, input, parts);
}

fn set_prompt(app: &mut App, input: String, parts: Vec<crate::state::prompt::PromptPart>) {
    app.ui.prompt.textarea.set_text(&input);
    app.ui.prompt.parts = parts;
    app.ui.prompt.restore_extmarks_from_parts();
    app.ui.prompt.textarea.buffer_end(false);
}

fn set_mode(app: &mut App, mode: ThemeMode) {
    app.ui.theme.set_mode(&mut app.state.kv, mode);
}

fn session_id(app: &App) -> Option<String> {
    match &app.state.route.data {
        Route::Session { session_id, .. } => Some(session_id.clone()),
        _ => None,
    }
}

fn clipboard_toast(message: &str, variant: ToastVariant) -> Toast {
    Toast {
        title: None,
        variant,
        message: message.to_string(),
        duration_ms: 5000,
    }
}

// ------------------------------------------------------------- helpers

const QUICK_SWITCH_COMMANDS: [&str; 9] = [
    "session.quick_switch.1",
    "session.quick_switch.2",
    "session.quick_switch.3",
    "session.quick_switch.4",
    "session.quick_switch.5",
    "session.quick_switch.6",
    "session.quick_switch.7",
    "session.quick_switch.8",
    "session.quick_switch.9",
];

/// `pasteSummaryEnabled()` (`prompt/index.tsx:1209`).
fn paste_summary_enabled(app: &App) -> bool {
    let disabled = app
        .state
        .sync
        .config
        .get("experimental")
        .and_then(|experimental| experimental.get("disable_paste_summary"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    app.state
        .kv
        .get(keys::PASTE_SUMMARY_ENABLED, json!(!disabled))
        .as_bool()
        .unwrap_or(!disabled)
}

/// `currentWorktreeWorkspace()` (`app.tsx:571-578`).
fn current_worktree_workspace(app: &App) -> Option<Value> {
    let workspace_id = app.state.project.workspace.current.as_deref()?;
    app.state
        .project
        .workspace
        .list
        .iter()
        .find(|workspace| {
            workspace.get("workspaceID").and_then(Value::as_str) == Some(workspace_id)
                || workspace.get("id").and_then(Value::as_str) == Some(workspace_id)
        })
        .filter(|workspace| {
            workspace.get("type").and_then(Value::as_str) == Some("worktree")
                && workspace
                    .get("directory")
                    .and_then(Value::as_str)
                    .is_some_and(|directory| !directory.is_empty())
        })
        .cloned()
}

fn interrupt_enabled(app: &App) -> bool {
    let Route::Session { session_id, .. } = &app.state.route.data else {
        return false;
    };
    matches!(
        app.state.sync.session_status.get(session_id),
        Some(SessionStatusInfo::Busy) | Some(SessionStatusInfo::Retry { .. })
    )
}

fn share_enabled(app: &App) -> bool {
    app.state.sync.config.get("share") != Some(&json!("disabled"))
}

fn current_share_url(app: &App) -> Option<String> {
    current_session(app)?
        .share
        .as_ref()
        .map(|share| share.url.clone())
}

/// `thinkingMode()` (`context/thinking.tsx:36-55`).
fn thinking_current(app: &App) -> &'static str {
    match app
        .state
        .kv
        .get(keys::THINKING_MODE, json!("hide"))
        .as_str()
    {
        Some("show") => "show",
        _ => "hide",
    }
}

/// `nextThinkingMode` (`context/thinking.tsx:36-38`).
fn thinking_next(app: &App) -> &'static str {
    if thinking_current(app) == "show" {
        "hide"
    } else {
        "show"
    }
}

fn thinking_title(app: &App) -> &'static str {
    if thinking_next(app) == "hide" {
        "Collapse thinking"
    } else {
        "Expand thinking"
    }
}

// ------------------------------------------------------------ registry

/// The palette-reachable registry (`visibility: "reachable"`): app +
/// tips (home) + prompt + stash + session commands.
pub fn registry(app: &App) -> Vec<CommandInfo> {
    let mut commands: Vec<CommandInfo> = Vec::new();

    // ---- app commands (`app.tsx:561-962`)
    commands.push({
        let mut command =
            CommandInfo::new(COMMAND_PALETTE_COMMAND, "Show command palette", "System");
        command.hidden = true;
        command
    });
    commands.push({
        let mut command = CommandInfo::new("session.list", "Switch session", "Session");
        command.suggested = !app.state.sync.session.is_empty();
        command.slash_name = Some("sessions");
        command.slash_aliases = &["resume", "continue"];
        command
    });
    commands.push({
        let mut command = CommandInfo::new("session.new", "New session", "Session");
        command.suggested = route_is_session(app);
        command.slash_name = Some("new");
        command.slash_aliases = &["clear"];
        command
    });
    commands.push({
        let mut command =
            CommandInfo::new("workspace.copy_path", "Copy worktree path", "Workspace");
        command.enabled = current_worktree_workspace(app).is_some();
        command
    });
    for name in QUICK_SWITCH_COMMANDS {
        let mut command = CommandInfo::new(name, quick_switch_title(name), "Session");
        command.hidden = true;
        commands.push(command);
    }
    commands.push({
        let mut command = CommandInfo::new("model.list", "Switch model", "Agent");
        command.suggested = true;
        command.slash_name = Some("models");
        command.slash_aliases = &["mo"];
        command
    });
    for (name, title) in [
        ("model.cycle_recent", "Model cycle"),
        ("model.cycle_recent_reverse", "Model cycle reverse"),
        ("model.cycle_favorite", "Favorite cycle"),
        ("model.cycle_favorite_reverse", "Favorite cycle reverse"),
    ] {
        let mut command = CommandInfo::new(name, title, "Agent");
        command.hidden = true;
        commands.push(command);
    }
    commands.push({
        let mut command = CommandInfo::new("agent.list", "Switch agent", "Agent");
        command.slash_name = Some("agents");
        command
    });
    commands.push({
        let mut command = CommandInfo::new("mcp.list", "Toggle MCPs", "Agent");
        command.slash_name = Some("mcps");
        command
    });
    for (name, title) in [
        ("agent.cycle", "Agent cycle"),
        ("agent.cycle.reverse", "Agent cycle reverse"),
    ] {
        let mut command = CommandInfo::new(name, title, "Agent");
        command.hidden = true;
        commands.push(command);
    }
    commands.push(CommandInfo::new("variant.cycle", "Variant cycle", "Agent"));
    commands.push({
        let mut command = CommandInfo::new("variant.list", "Switch model variant", "Agent");
        command.hidden = app
            .state
            .local
            .variant_list(&app.state.sync, &app.state.args)
            .is_empty();
        command.slash_name = Some("variants");
        command
    });
    commands.push({
        let mut command = CommandInfo::new("provider.connect", "Connect provider", "Provider");
        command.suggested = !connected(app);
        command.slash_name = Some("connect");
        command
    });
    if app
        .state
        .sync
        .console_state
        .get("switchableOrgCount")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        > 1
    {
        let mut command = CommandInfo::new("console.org.switch", "Switch org", "Provider");
        command.suggested = app
            .state
            .sync
            .console_state
            .get("activeOrgName")
            .and_then(Value::as_str)
            .is_some();
        command.slash_name = Some("org");
        command.slash_aliases = &["orgs", "switch-org"];
        commands.push(command);
    }
    commands.push({
        let mut command = CommandInfo::new("opencode.status", "View status", "System");
        command.slash_name = Some("status");
        command
    });
    commands.push({
        let mut command = CommandInfo::new("opencode.debug", "View debug info", "System");
        command.slash_name = Some("debug");
        command
    });
    commands.push({
        let mut command = CommandInfo::new("theme.switch", "Switch theme", "System");
        command.slash_name = Some("themes");
        command
    });
    commands.push(CommandInfo::new(
        "theme.switch_mode",
        if app.ui.theme.mode == ThemeMode::Dark {
            "Switch to light mode"
        } else {
            "Switch to dark mode"
        },
        "System",
    ));
    commands.push(CommandInfo::new(
        "theme.mode.lock",
        if app.ui.theme.lock.is_some() {
            "Unlock theme mode"
        } else {
            "Lock theme mode"
        },
        "System",
    ));
    commands.push({
        let mut command = CommandInfo::new("help.show", "Help", "System");
        command.slash_name = Some("help");
        command
    });
    commands.push(CommandInfo::new("docs.open", "Open docs", "System"));
    commands.push({
        let mut command = CommandInfo::new("app.exit", "Exit the app", "System");
        command.slash_name = Some("exit");
        command.slash_aliases = &["quit", "q"];
        // The re-registered `app_exit` gate (`app.tsx:977-985`): exit is
        // disabled while the prompt is focused and non-empty.
        command.enabled = !app.ui.prompt_focused || app.ui.prompt.input().is_empty();
        command
    });
    commands.push(CommandInfo::new(
        "app.debug",
        "Toggle debug panel",
        "System",
    ));
    commands.push(CommandInfo::new("app.console", "Toggle console", "System"));
    commands.push(CommandInfo::new(
        "app.heap_snapshot",
        "Write heap snapshot",
        "System",
    ));
    commands.push({
        let mut command = CommandInfo::new("terminal.suspend", "Suspend terminal", "System");
        command.hidden = true;
        command.enabled = app.config.terminal_suspend_supported;
        command
    });
    commands.push(CommandInfo::new(
        "terminal.title.toggle",
        if app.state.kv.get_bool(keys::TERMINAL_TITLE_ENABLED, true) {
            "Disable terminal title"
        } else {
            "Enable terminal title"
        },
        "System",
    ));
    commands.push(CommandInfo::new(
        "app.toggle.animations",
        if app.state.kv.get_bool(keys::ANIMATIONS_ENABLED, true) {
            "Disable animations"
        } else {
            "Enable animations"
        },
        "System",
    ));
    commands.push(CommandInfo::new(
        "app.toggle.file_context",
        if app.state.kv.get_bool(keys::FILE_CONTEXT_ENABLED, true) {
            "Disable file context"
        } else {
            "Enable file context"
        },
        "System",
    ));
    commands.push(CommandInfo::new(
        "app.toggle.diffwrap",
        if app.state.kv.get(keys::DIFF_WRAP_MODE, json!("word")) == json!("word") {
            "Disable diff wrapping"
        } else {
            "Enable diff wrapping"
        },
        "System",
    ));
    commands.push(CommandInfo::new(
        "app.toggle.paste_summary",
        if paste_summary_enabled(app) {
            "Disable paste summary"
        } else {
            "Enable paste summary"
        },
        "System",
    ));
    commands.push(CommandInfo::new(
        "app.toggle.session_directory_filter",
        if app
            .state
            .kv
            .get_bool(keys::SESSION_DIRECTORY_FILTER_ENABLED, true)
        {
            "Disable session directory filtering"
        } else {
            "Enable session directory filtering"
        },
        "System",
    ));
    commands.push(CommandInfo::new(
        "permission.mode",
        if app.state.permission_mode == crate::state::PermissionMode::Auto {
            "Disable auto-approve permissions"
        } else {
            "Enable auto-approve permissions"
        },
        "System",
    ));

    // ---- tips (`feature-plugins/home/tips.tsx:13-31`, home slot)
    commands.push(CommandInfo::new(
        "tips.toggle",
        if app.state.kv.get_bool("tips_hidden", false) {
            "Show tips"
        } else {
            "Hide tips"
        },
        "System",
    ));

    // ---- prompt commands (`component/prompt/index.tsx:335-559`)
    commands.push({
        let mut command = CommandInfo::new("prompt.clear", "Clear prompt", "Prompt");
        command.hidden = true;
        // The `prompt.clear` binding gate (`prompt/index.tsx:810-814`).
        command.enabled = app.ui.prompt_focused && !app.ui.prompt.input().is_empty();
        command
    });
    commands.push({
        let mut command = CommandInfo::new("prompt.submit", "Submit prompt", "Prompt");
        command.hidden = true;
        command
    });
    // `inputCommands` (`keymap.tsx:134-173`): the keymap layer's
    // textarea command name for Enter — an alias of `prompt.submit`.
    commands.push({
        let mut command = CommandInfo::new("input.submit", "Submit prompt", "Prompt");
        command.hidden = true;
        command
    });
    commands.push({
        let mut command = CommandInfo::new(
            "prompt.editor_context.clear",
            "Remove editor context",
            "Prompt",
        );
        // The editor bridge seam always returns `None` (spec §6 N3).
        command.enabled = false;
        command
    });
    commands.push({
        let mut command = CommandInfo::new("prompt.paste", "Paste", "Prompt");
        command.hidden = true;
        command
    });
    commands.push({
        let mut command = CommandInfo::new("session.interrupt", "Interrupt session", "Session");
        command.hidden = true;
        command.enabled = interrupt_enabled(app);
        command
    });
    commands.push({
        let mut command = CommandInfo::new("prompt.editor", "Open editor", "Session");
        command.slash_name = Some("editor");
        command
    });
    commands.push({
        let mut command = CommandInfo::new("prompt.skills", "Skills", "Prompt");
        command.slash_name = Some("skills");
        command
    });
    commands.push({
        let mut command = CommandInfo::new("workspace.set", "Warp", "Session");
        command.enabled = experimental_workspaces();
        command.slash_name = Some("warp");
        command
    });
    commands.push({
        let mut command = CommandInfo::new("session.move", "Move session", "Session");
        command.slash_name = Some("move");
        command
    });

    // ---- stash commands (`component/prompt/index.tsx:736-798`)
    commands.push({
        let mut command = CommandInfo::new("prompt.stash", "Stash prompt", "Prompt");
        command.enabled = !app.ui.prompt.input().is_empty();
        command
    });
    commands.push({
        let mut command = CommandInfo::new("prompt.stash.pop", "Stash pop", "Prompt");
        command.enabled = !app.ui.prompt.stash.is_empty();
        command
    });
    commands.push({
        let mut command = CommandInfo::new("prompt.stash.list", "Stash list", "Prompt");
        command.enabled = !app.ui.prompt.stash.is_empty();
        command
    });

    // ---- session commands (`routes/session/index.tsx:465-1084`)
    if route_is_session(app) {
        commands.extend(session_commands(app));
    }

    commands
}

fn quick_switch_title(name: &str) -> String {
    let slot = name
        .rsplit('.')
        .next()
        .and_then(|slot| slot.parse::<usize>().ok())
        .unwrap_or(0);
    format!("Switch to session in quick slot {slot}")
}

fn session_commands(app: &App) -> Vec<CommandInfo> {
    let mut commands = Vec::new();
    commands.push({
        let mut command = CommandInfo::new(
            "session.share",
            if current_share_url(app).is_some() {
                "Copy share link"
            } else {
                "Share session"
            },
            "Session",
        );
        command.suggested = true;
        command.enabled = share_enabled(app);
        command.slash_name = Some("share");
        command
    });
    commands.push({
        let mut command = CommandInfo::new("session.rename", "Rename session", "Session");
        command.slash_name = Some("rename");
        command
    });
    commands.push({
        let mut command = CommandInfo::new("session.timeline", "Jump to message", "Session");
        command.slash_name = Some("timeline");
        command
    });
    commands.push({
        let mut command = CommandInfo::new("session.fork", "Fork session", "Session");
        command.slash_name = Some("fork");
        command
    });
    commands.push({
        let mut command = CommandInfo::new("session.compact", "Compact session", "Session");
        command.slash_name = Some("compact");
        command.slash_aliases = &["summarize"];
        command
    });
    commands.push({
        let mut command = CommandInfo::new("session.unshare", "Unshare session", "Session");
        command.enabled = current_share_url(app).is_some();
        command.slash_name = Some("unshare");
        command
    });
    commands.push({
        let mut command = CommandInfo::new("session.undo", "Undo previous message", "Session");
        command.slash_name = Some("undo");
        command
    });
    commands.push({
        let mut command = CommandInfo::new("session.redo", "Redo", "Session");
        command.enabled = current_session(app)
            .and_then(|session| session.revert.as_ref())
            .is_some();
        command.slash_name = Some("redo");
        command
    });
    commands.push(CommandInfo::new(
        "session.sidebar.toggle",
        if sidebar_visible(app) {
            "Hide sidebar"
        } else {
            "Show sidebar"
        },
        "Session",
    ));
    commands.push(CommandInfo::new(
        "session.toggle.conceal",
        if app.ui.conceal {
            "Disable code concealment"
        } else {
            "Enable code concealment"
        },
        "Session",
    ));
    commands.push({
        let mut command = CommandInfo::new(
            "session.toggle.timestamps",
            if app.state.kv.get(keys::TIMESTAMPS, json!("hide")) == json!("show") {
                "Hide timestamps"
            } else {
                "Show timestamps"
            },
            "Session",
        );
        command.slash_name = Some("timestamps");
        command.slash_aliases = &["toggle-timestamps"];
        command
    });
    commands.push({
        let mut command =
            CommandInfo::new("session.toggle.thinking", thinking_title(app), "Session");
        command.slash_name = Some("thinking");
        command.slash_aliases = &["toggle-thinking"];
        command
    });
    commands.push(CommandInfo::new(
        "session.toggle.actions",
        if app.state.kv.get_bool(keys::TOOL_DETAILS_VISIBILITY, true) {
            "Hide tool details"
        } else {
            "Show tool details"
        },
        "Session",
    ));
    commands.push(CommandInfo::new(
        "session.toggle.scrollbar",
        "Toggle session scrollbar",
        "Session",
    ));
    commands.push(CommandInfo::new(
        "session.toggle.generic_tool_output",
        if app
            .state
            .kv
            .get_bool(keys::GENERIC_TOOL_OUTPUT_VISIBILITY, false)
        {
            "Hide generic tool output"
        } else {
            "Show generic tool output"
        },
        "Session",
    ));
    for (name, title) in [
        ("session.page.up", "Page up"),
        ("session.page.down", "Page down"),
        ("session.line.up", "Line up"),
        ("session.line.down", "Line down"),
        ("session.half.page.up", "Half page up"),
        ("session.half.page.down", "Half page down"),
        ("session.first", "First message"),
        ("session.last", "Last message"),
        ("session.messages_last_user", "Jump to last user message"),
        ("session.message.next", "Next message"),
        ("session.message.previous", "Previous message"),
    ] {
        let mut command = CommandInfo::new(name, title, "Session");
        command.hidden = true;
        commands.push(command);
    }
    commands.push(CommandInfo::new(
        "messages.copy",
        "Copy last assistant message",
        "Session",
    ));
    commands.push({
        let mut command = CommandInfo::new("session.copy", "Copy session transcript", "Session");
        command.slash_name = Some("copy");
        command
    });
    commands.push({
        let mut command =
            CommandInfo::new("session.export", "Export session transcript", "Session");
        command.slash_name = Some("export");
        command
    });
    commands.push({
        let mut command = CommandInfo::new("session.background", "Background subagents", "Session");
        command.hidden = true;
        command.enabled = foreground_tasks(app) > 0;
        command
    });
    // `session.child.first` has no `enabled` predicate; the other three
    // are `enabled: !!session()?.parentID` (`session/index.tsx:1039-1083`).
    commands.push(CommandInfo::new(
        "session.child.first",
        "Go to child session",
        "Session",
    ));
    let child_enabled = current_session(app)
        .and_then(|session| session.parent_id.as_deref())
        .is_some();
    for name in [
        "session.parent",
        "session.child.next",
        "session.child.previous",
    ] {
        let title = match name {
            "session.parent" => "Go to parent session",
            "session.child.next" => "Next child session",
            _ => "Previous child session",
        };
        let mut command = CommandInfo::new(name, title, "Session");
        command.hidden = true;
        command.enabled = child_enabled;
        commands.push(command);
    }
    commands
}

/// `useCommandSlashes()` (`keymap.tsx:260-289`): the `/`-command list.
pub fn slash_commands(app: &App) -> Vec<CommandInfo> {
    registry(app)
        .into_iter()
        .filter(|command| command.slash_name.is_some())
        .collect()
}

/// The command-palette list (`command-palette.tsx:15-17`): reachable
/// commands that are not hidden, minus the palette command itself.
pub fn palette(app: &App) -> Vec<CommandInfo> {
    registry(app)
        .into_iter()
        // `visibility: "reachable"` (command-palette.tsx:33-41): only
        // commands whose `enabled` conditions pass.
        .filter(|command| {
            !command.hidden && command.enabled && command.name != COMMAND_PALETTE_COMMAND
        })
        .collect()
}

/// The `enabled` predicate of a registered command; unknown commands
/// are disabled.
pub fn is_enabled(app: &App, name: &str) -> bool {
    registry(app)
        .into_iter()
        .find(|command| command.name == name)
        .map(|command| command.enabled)
        .unwrap_or(false)
}

// ------------------------------------------------------------------ run

/// `tui.command.execute` → `keymap.dispatchCommand(name)`
/// (`app.tsx:986-990`). Commands that are not registered right now
/// (route-gated) no-op.
pub fn run(app: &mut App, name: &str) -> Vec<Effect> {
    if !registry(app).iter().any(|command| command.name == name) {
        return Vec::new();
    }
    // `session.quick_switch.N` — dynamic name before the literal match.
    if let Some(slot) = name.strip_prefix("session.quick_switch.") {
        if let Ok(slot) = slot.parse::<usize>() {
            if let Some(route) =
                app.state
                    .local
                    .session_quick_switch(&app.state.sync, &app.state.route.data, slot)
            {
                app.state.route.navigate(route);
            }
            return Vec::new();
        }
    }
    match name {
        // ---- app commands
        "command.palette.show" => show_dialog(app, PendingDialog::CommandPalette),
        "session.list" => show_dialog(app, PendingDialog::SessionList),
        "session.new" => {
            app.state.route.navigate(Route::Home { prompt: None });
            clear_dialog(app);
        }
        "workspace.copy_path" => {
            let Some(workspace) = current_worktree_workspace(app) else {
                return Vec::new();
            };
            if let Some(path) = workspace.get("directory").and_then(Value::as_str) {
                return vec![Effect::ClipboardWrite {
                    text: path.to_string(),
                    success: Some(clipboard_toast("Copied worktree path", ToastVariant::Info)),
                    failure: None,
                }];
            }
        }
        "model.list" => show_dialog(app, PendingDialog::Model),
        "model.cycle_recent" => app
            .state
            .local
            .model_cycle(1, &app.state.sync, &app.state.args),
        "model.cycle_recent_reverse" => {
            app.state
                .local
                .model_cycle(-1, &app.state.sync, &app.state.args)
        }
        "model.cycle_favorite" => {
            app.state
                .local
                .model_cycle_favorite(1, &app.state.sync, &app.state.args);
        }
        "model.cycle_favorite_reverse" => {
            app.state
                .local
                .model_cycle_favorite(-1, &app.state.sync, &app.state.args);
        }
        "agent.list" => show_dialog(app, PendingDialog::Agent),
        "mcp.list" => show_dialog(app, PendingDialog::Mcp),
        "agent.cycle" => app.state.local.agent_move(1, &app.state.sync),
        "agent.cycle.reverse" => app.state.local.agent_move(-1, &app.state.sync),
        "variant.cycle" => app
            .state
            .local
            .variant_cycle(&app.state.sync, &app.state.args),
        "variant.list" => {
            if app
                .state
                .local
                .variant_list(&app.state.sync, &app.state.args)
                .is_empty()
            {
                app.show_toast(Toast {
                    title: Some("No variants available".to_string()),
                    variant: ToastVariant::Info,
                    message: "The current model does not support any variants.".to_string(),
                    duration_ms: 5000,
                });
            } else {
                show_dialog(app, PendingDialog::Variant);
            }
        }
        "provider.connect" => show_dialog(app, PendingDialog::ProviderConnect),
        "console.org.switch" => show_dialog(app, PendingDialog::ConsoleOrg),
        "opencode.status" => show_dialog(app, PendingDialog::Status),
        "opencode.debug" => show_dialog(app, PendingDialog::Debug),
        "theme.switch" => show_dialog(app, PendingDialog::ThemeList),
        "theme.switch_mode" => {
            let next = if app.ui.theme.mode == ThemeMode::Dark {
                ThemeMode::Light
            } else {
                ThemeMode::Dark
            };
            set_mode(app, next);
            clear_dialog(app);
        }
        "theme.mode.lock" => {
            if app.ui.theme.lock.is_some() {
                app.ui.theme.unlock(&mut app.state.kv);
            } else {
                set_mode(app, app.ui.theme.mode);
            }
            clear_dialog(app);
        }
        "help.show" => show_dialog(app, PendingDialog::Help),
        "docs.open" => {
            clear_dialog(app);
            return vec![Effect::OpenUrl {
                url: "https://opencode.ai/docs".to_string(),
            }];
        }
        "app.exit" => app.exit(None),
        // §5.8: OpenTUI debug console/overlay are no-op stubs.
        "app.debug" | "app.console" | "app.heap_snapshot" => clear_dialog(app),
        "terminal.suspend" => {
            return vec![Effect::SuspendTerminal];
        }
        "terminal.title.toggle" => {
            let next = !app.state.kv.get_bool(keys::TERMINAL_TITLE_ENABLED, true);
            app.state.kv.set(keys::TERMINAL_TITLE_ENABLED, json!(next));
            clear_dialog(app);
        }
        "app.toggle.animations" => {
            let next = !app.state.kv.get_bool(keys::ANIMATIONS_ENABLED, true);
            app.state.kv.set(keys::ANIMATIONS_ENABLED, json!(next));
            clear_dialog(app);
        }
        "app.toggle.file_context" => {
            let next = !app.state.kv.get_bool(keys::FILE_CONTEXT_ENABLED, true);
            app.state.kv.set(keys::FILE_CONTEXT_ENABLED, json!(next));
            clear_dialog(app);
        }
        "app.toggle.diffwrap" => {
            let next = if app.state.kv.get(keys::DIFF_WRAP_MODE, json!("word")) == json!("word") {
                "none"
            } else {
                "word"
            };
            app.state.kv.set(keys::DIFF_WRAP_MODE, json!(next));
            clear_dialog(app);
        }
        "app.toggle.paste_summary" => {
            let next = !paste_summary_enabled(app);
            app.state.kv.set(keys::PASTE_SUMMARY_ENABLED, json!(next));
            clear_dialog(app);
        }
        "app.toggle.session_directory_filter" => {
            let next = !app
                .state
                .kv
                .get_bool(keys::SESSION_DIRECTORY_FILTER_ENABLED, true);
            app.state
                .kv
                .set(keys::SESSION_DIRECTORY_FILTER_ENABLED, json!(next));
            clear_dialog(app);
            return vec![Effect::SessionRefresh];
        }
        "permission.mode" => {
            app.state.permission_toggle();
            clear_dialog(app);
        }
        "tips.toggle" => {
            let next = !app.state.kv.get_bool("tips_hidden", false);
            app.state.kv.set("tips_hidden", json!(next));
            clear_dialog(app);
        }

        // ---- prompt commands
        "prompt.clear" => {
            crate::state::prompt::clear_prompt(app);
            clear_dialog(app);
        }
        "prompt.submit" | "input.submit" if app.ui.prompt_focused => {
            return crate::state::prompt::submit(app);
        }
        "prompt.paste" => {
            clear_dialog(app);
            return vec![Effect::PromptPaste];
        }
        "prompt.editor" => {
            clear_dialog(app);
            // Seed the editor with the virtual texts expanded inline
            // (`prompt/index.tsx:430-439`).
            let value = crate::state::prompt::expand_pasted_text_placeholders(
                app.ui.prompt.input(),
                &app.ui.prompt.parts,
            );
            return vec![Effect::OpenPromptEditor { value }];
        }
        "prompt.editor_context.clear" => clear_dialog(app),
        "session.interrupt" => return interrupt(app),
        "prompt.skills" => show_dialog(app, PendingDialog::Skill),
        "workspace.set" => show_dialog(app, PendingDialog::WorkspaceSet),
        // Needs the effect returned (project.directories fetch).
        "session.move" => return crate::ui::dialogs::open(app, PendingDialog::MoveSession),
        "prompt.stash" => {
            if app.ui.prompt.input().is_empty() {
                return Vec::new();
            }
            // `stash.push({ input, parts })` — no mode (`prompt/index.tsx:743-748`).
            let entry = crate::state::prompt::PromptEntry {
                input: app.ui.prompt.input().to_string(),
                mode: None,
                parts: app.ui.prompt.parts.clone(),
            };
            app.ui.prompt.stash.push(entry);
            app.ui.prompt.reset();
            clear_dialog(app);
        }
        "prompt.stash.pop" => {
            if let Some(entry) = app.ui.prompt.stash.pop() {
                app.ui.prompt.textarea.set_text(&entry.entry.input);
                app.ui.prompt.parts = entry.entry.parts;
                app.ui.prompt.restore_extmarks_from_parts();
                app.ui.prompt.textarea.buffer_end(false);
            }
            clear_dialog(app);
        }
        "prompt.stash.list" => show_dialog(app, PendingDialog::StashList),

        // ---- session commands
        "session.share" => return session_share(app),
        "session.rename" => {
            if let Some(session_id) = session_id(app) {
                show_dialog(app, PendingDialog::SessionRename { session_id });
            }
        }
        "session.timeline" => show_dialog(app, PendingDialog::Timeline),
        "session.fork" => show_dialog(app, PendingDialog::ForkFromTimeline),
        "session.compact" => return session_compact(app),
        "session.unshare" => {
            let Some(session_id) = session_id(app) else {
                return Vec::new();
            };
            clear_dialog(app);
            return vec![Effect::SessionUnshare { session_id }];
        }
        "session.undo" => return session_undo(app),
        "session.redo" => return session_redo(app),
        "session.sidebar.toggle" => {
            let visible = sidebar_visible(app);
            app.state
                .kv
                .set(keys::SIDEBAR, json!(if visible { "hide" } else { "auto" }));
            app.ui.sidebar_open = !visible;
            clear_dialog(app);
        }
        "session.toggle.conceal" => {
            app.ui.conceal = !app.ui.conceal;
            clear_dialog(app);
        }
        "session.toggle.timestamps" => {
            let next = if app.state.kv.get(keys::TIMESTAMPS, json!("hide")) == json!("show") {
                "hide"
            } else {
                "show"
            };
            app.state.kv.set(keys::TIMESTAMPS, json!(next));
            clear_dialog(app);
        }
        "session.toggle.thinking" => {
            let next = thinking_next(app);
            app.state.kv.set(keys::THINKING_MODE, json!(next));
            clear_dialog(app);
        }
        "session.toggle.actions" => {
            let next = !app.state.kv.get_bool(keys::TOOL_DETAILS_VISIBILITY, true);
            app.state.kv.set(keys::TOOL_DETAILS_VISIBILITY, json!(next));
            clear_dialog(app);
        }
        "session.toggle.scrollbar" => {
            let next = !app.state.kv.get_bool(keys::SCROLLBAR_VISIBLE, false);
            app.state.kv.set(keys::SCROLLBAR_VISIBLE, json!(next));
            clear_dialog(app);
        }
        "session.toggle.generic_tool_output" => {
            let next = !app
                .state
                .kv
                .get_bool(keys::GENERIC_TOOL_OUTPUT_VISIBILITY, false);
            app.state
                .kv
                .set(keys::GENERIC_TOOL_OUTPUT_VISIBILITY, json!(next));
            clear_dialog(app);
        }
        // Transcript scroll commands (`session/index.tsx:752-860`).
        "session.page.up" => {
            scroll_by_half(app, -2);
            clear_dialog(app);
        }
        "session.page.down" => {
            scroll_by_half(app, 2);
            clear_dialog(app);
        }
        "session.line.up" => {
            app.ui.session_scroll.scroll_by(-1);
            clear_dialog(app);
        }
        "session.line.down" => {
            app.ui.session_scroll.scroll_by(1);
            clear_dialog(app);
        }
        "session.half.page.up" => {
            scroll_by_half(app, -1);
            clear_dialog(app);
        }
        "session.half.page.down" => {
            scroll_by_half(app, 1);
            clear_dialog(app);
        }
        "session.first" => {
            app.ui.session_scroll.y = 0;
            app.ui.session_scroll.sticky = false;
            clear_dialog(app);
        }
        "session.last" => {
            app.ui.session_scroll.snap_to_bottom();
            clear_dialog(app);
        }
        "session.messages_last_user" => {
            scroll_to_last_user(app);
            clear_dialog(app);
        }
        "session.message.next" => {
            scroll_to_next_visible_message(app, 1);
            clear_dialog(app);
        }
        "session.message.previous" => {
            scroll_to_next_visible_message(app, -1);
            clear_dialog(app);
        }
        "messages.copy" => return messages_copy(app),
        "session.copy" => {
            clear_dialog(app);
            return vec![Effect::SessionCopyTranscript {
                thinking: thinking_current(app) == "show",
                tool_details: app.state.kv.get_bool(keys::TOOL_DETAILS_VISIBILITY, true),
                assistant_metadata: app
                    .state
                    .kv
                    .get_bool(keys::ASSISTANT_METADATA_VISIBILITY, true),
            }];
        }
        "session.export" => show_dialog(app, PendingDialog::ExportOptions),
        "session.background" => {
            let Some(session_id) = session_id(app) else {
                return Vec::new();
            };
            clear_dialog(app);
            return vec![Effect::SessionBackground { session_id }];
        }
        "session.child.first" => {
            clear_dialog(app);
            let children = children(app);
            if children.len() == 1 {
                return Vec::new();
            }
            if let Some(child) = children
                .iter()
                .find(|session| session.parent_id.is_some())
                .map(|session| session.id.clone())
            {
                enter_child(app, &child);
            }
        }
        "session.parent" => {
            if !child_session_enabled(app) {
                return Vec::new();
            }
            if let Some(parent_id) =
                current_session(app).and_then(|session| session.parent_id.clone())
            {
                // `session.parent` navigates directly — no retry alert
                // (`session/index.tsx:1051-1061`).
                app.state.route.navigate(Route::Session {
                    session_id: parent_id,
                    prompt: None,
                });
                clear_dialog(app);
            }
        }
        "session.child.next" | "session.child.previous" => {
            if !child_session_enabled(app) {
                return Vec::new();
            }
            clear_dialog(app);
            let direction = if name == "session.child.next" { 1 } else { -1 };
            move_child(app, direction);
        }
        _ => {}
    }
    Vec::new()
}

/// `children()` (`session/index.tsx:205-210`).
fn children(app: &App) -> Vec<V1SessionInfo> {
    let Some(current) = current_session(app) else {
        return Vec::new();
    };
    let parent = current
        .parent_id
        .clone()
        .unwrap_or_else(|| current.id.clone());
    let mut children: Vec<V1SessionInfo> = app
        .state
        .sync
        .session
        .iter()
        .filter(|session| {
            session.parent_id.as_deref() == Some(parent.as_str()) || session.id == parent
        })
        .cloned()
        .collect();
    children.sort_by(|a, b| a.id.cmp(&b.id));
    children
}

/// `moveChild(direction)` (`session/index.tsx:434-444`).
fn move_child(app: &mut App, direction: i32) {
    let children = children(app);
    if children.len() == 1 {
        return;
    }
    let sessions: Vec<V1SessionInfo> = children
        .iter()
        .filter(|session| session.parent_id.is_some())
        .cloned()
        .collect();
    let Some(current_id) = current_session(app).map(|session| session.id.clone()) else {
        return;
    };
    let Some(index) = sessions
        .iter()
        .position(|session| session.id == current_id)
        .map(|index| index as i32)
    else {
        return;
    };
    let index = index - direction;
    let index = if index < 0 {
        sessions.len() - 1
    } else if index as usize >= sessions.len() {
        0
    } else {
        index as usize
    };
    if let Some(session) = sessions.get(index) {
        let id = session.id.clone();
        enter_child(app, &id);
    }
}

/// `enterChild` (`session/index.tsx:418-424`): navigate, then the
/// retry-status alert.
fn enter_child(app: &mut App, session_id: &str) {
    app.state.route.navigate(Route::Session {
        session_id: session_id.to_string(),
        prompt: None,
    });
    if let Some(SessionStatusInfo::Retry { message, .. }) =
        app.state.sync.session_status.get(session_id)
    {
        let _ = crate::ui::dialogs::open(
            app,
            PendingDialog::Alert {
                title: "Retry Error".to_string(),
                message: message.clone(),
                exit_on_confirm: false,
            },
        );
    }
}

/// `childSessionHandler` (`session/index.tsx:458-462`): the parent/prev/
/// next handlers only run on child sessions with no dialog open.
fn child_session_enabled(app: &App) -> bool {
    current_session(app)
        .and_then(|session| session.parent_id.as_deref())
        .is_some()
        && app.ui.dialogs.is_empty()
}

fn session_share(app: &mut App) -> Vec<Effect> {
    let Some(session_id) = session_id(app) else {
        return Vec::new();
    };
    if let Some(url) = current_share_url(app) {
        clear_dialog(app);
        return vec![Effect::ClipboardWrite {
            text: url,
            success: Some(clipboard_toast(
                "Share URL copied to clipboard!",
                ToastVariant::Success,
            )),
            failure: Some(clipboard_toast(
                "Failed to copy URL to clipboard",
                ToastVariant::Error,
            )),
        }];
    }
    if !app
        .state
        .kv
        .get(keys::SHARE_CONSENT, Value::Bool(false))
        .as_bool()
        .unwrap_or(false)
    {
        show_dialog(app, PendingDialog::ShareConsent { session_id });
        return Vec::new();
    }
    clear_dialog(app);
    vec![Effect::SessionShare { session_id }]
}

fn session_compact(app: &mut App) -> Vec<Effect> {
    let Some(session_id) = session_id(app) else {
        return Vec::new();
    };
    let Some(model) = app
        .state
        .local
        .model_current(&app.state.sync, &app.state.args)
    else {
        app.show_toast(Toast {
            title: None,
            variant: ToastVariant::Warning,
            message: "Connect a provider to summarize this session".to_string(),
            duration_ms: 3000,
        });
        return Vec::new();
    };
    clear_dialog(app);
    vec![Effect::SessionSummarize {
        session_id,
        provider_id: model.provider_id,
        model_id: model.model_id,
    }]
}

fn session_undo(app: &mut App) -> Vec<Effect> {
    let Some(session_id) = session_id(app) else {
        return Vec::new();
    };
    let mut effects = Vec::new();
    if !matches!(
        app.state.sync.session_status.get(&session_id),
        Some(SessionStatusInfo::Idle)
    ) {
        effects.push(Effect::SessionAbort {
            session_id: session_id.clone(),
        });
    }
    let Some(message) = messages_before_revert(app)
        .into_iter()
        .rev()
        .find(|message| matches!(message, V1Message::User { .. }))
        .map(|message| message_id(message).to_string())
    else {
        return effects;
    };
    set_prompt_from_parts(app, &message);
    clear_dialog(app);
    effects.push(Effect::SessionRevert {
        session_id,
        message_id: message,
    });
    effects
}

fn session_redo(app: &mut App) -> Vec<Effect> {
    let Some(session_id) = session_id(app) else {
        return Vec::new();
    };
    let Some(revert_id) = current_session(app)
        .and_then(|session| session.revert.as_ref())
        .map(|revert| revert.message_id.clone())
    else {
        return Vec::new();
    };
    let messages = app
        .state
        .sync
        .message
        .get(&session_id)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let next = messages
        .iter()
        .find(|message| {
            matches!(message, V1Message::User { .. }) && message_id(message) > revert_id.as_str()
        })
        .map(|message| message_id(message).to_string());
    clear_dialog(app);
    match next {
        Some(next) => vec![Effect::SessionRevert {
            session_id,
            message_id: next,
        }],
        None => {
            set_prompt(app, String::new(), Vec::new());
            vec![Effect::SessionUnrevert { session_id }]
        }
    }
}

fn messages_copy(app: &mut App) -> Vec<Effect> {
    let last = messages_before_revert(app)
        .into_iter()
        .rev()
        .find(|message| matches!(message, V1Message::Assistant { .. }))
        .map(|message| message_id(message).to_string());
    let Some(message_id) = last else {
        toast(
            app,
            ToastVariant::Error,
            "No assistant messages found",
            5000,
        );
        clear_dialog(app);
        return Vec::new();
    };
    let text_parts: Vec<&str> = app
        .state
        .sync
        .part
        .get(&message_id)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| match part {
                    V1Part::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    if text_parts.is_empty() {
        toast(
            app,
            ToastVariant::Error,
            "No text parts found in last assistant message",
            5000,
        );
        clear_dialog(app);
        return Vec::new();
    }
    let text = text_parts.join("\n").trim().to_string();
    if text.is_empty() {
        toast(
            app,
            ToastVariant::Error,
            "No text content found in last assistant message",
            5000,
        );
        clear_dialog(app);
        return Vec::new();
    }
    clear_dialog(app);
    vec![Effect::ClipboardWrite {
        text,
        success: Some(clipboard_toast(
            "Message copied to clipboard!",
            ToastVariant::Success,
        )),
        failure: Some(clipboard_toast(
            "Failed to copy to clipboard",
            ToastVariant::Error,
        )),
    }]
}

/// `session.interrupt` (`prompt/index.tsx:389-421`): double-press
/// within 5 s aborts.
fn interrupt(app: &mut App) -> Vec<Effect> {
    let Some(session_id) = session_id(app) else {
        return Vec::new();
    };
    app.ui.interrupt = app.ui.interrupt.saturating_add(1);
    app.ui.interrupt_reset_at = Some(app.ui.tick_ms.saturating_add(5000));
    if app.ui.interrupt >= 2 {
        app.ui.interrupt = 0;
        app.ui.interrupt_reset_at = None;
        clear_dialog(app);
        return vec![Effect::SessionAbort { session_id }];
    }
    clear_dialog(app);
    Vec::new()
}

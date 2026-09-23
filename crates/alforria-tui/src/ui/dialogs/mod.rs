//! `ui/dialog.tsx` — the dialog stack (M8.7). A stack, not a queue:
//! `replace` closes everything and pushes one; `escape`/`ctrl+c` pop the
//! top; `clear()` closes all. Sizes: medium 60 / large 88 / xlarge 116
//! columns. Opening pushes the keymap `modal` mode.

pub mod model;
pub mod palette;
pub mod primitives;
pub mod sessions;
pub mod system;

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

#[cfg(test)]
mod tests;

use crate::state::kv::keys;
use crate::state::route::Route;
use crate::state::{App, Effect, PendingDialog, Toast, ToastVariant};
use crate::ui::session::sidebar::INSTALLATION_VERSION;
use crate::ui::theme::{Rgba, Theme};

/// `ui/dialog.tsx:22-26` — the medium/large/xlarge widths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DialogSize {
    #[default]
    Medium,
    Large,
    Xlarge,
}

impl DialogSize {
    pub fn width(self) -> u16 {
        match self {
            DialogSize::Medium => 60,
            DialogSize::Large => 88,
            DialogSize::Xlarge => 116,
        }
    }
}

/// The `onMount { dialog.setSize(...) }` calls of the TS dialogs.
fn default_size(kind: &PendingDialog) -> DialogSize {
    match kind {
        PendingDialog::Skill
        | PendingDialog::SessionList
        | PendingDialog::Debug
        | PendingDialog::Timeline
        | PendingDialog::ForkFromTimeline
        | PendingDialog::WorkspaceList => DialogSize::Large,
        PendingDialog::MoveSession => DialogSize::Xlarge,
        _ => DialogSize::Medium,
    }
}

/// One open dialog plus its interaction state.
#[derive(Debug, Clone)]
pub struct DialogFrame {
    pub kind: PendingDialog,
    /// The `DialogSelect` state — `selected`, filter, scrollbox.
    pub select: primitives::SelectState,
    /// The `<textarea>` content of prompt dialogs (`DialogPrompt`) and
    /// the export-options filename.
    pub input: String,
    /// The active index of confirm-style dialogs (cancel/confirm) and
    /// the export-options form.
    pub active: usize,
    /// The `toDelete()` double-press of the session-list/stash delete.
    pub pending_delete: Option<String>,
    /// The focused footer action — `focusedAction` of `DialogSelect`
    /// (`dialog-select.tsx:95-96,358-367`).
    pub focused_action: Option<usize>,
    /// `DialogThemeList.initial` — the theme to revert to when the
    /// dialog closes unconfirmed (`dialog-theme-list.tsx:16-36`).
    pub initial_theme: Option<String>,
    /// `confirmed` — once set, closing keeps the selected theme.
    pub confirmed: bool,
    /// The export-options checkboxes (`DialogExportOptions` store).
    pub checks: Vec<bool>,
    /// Expanded workspace rows (`dialog-workspace-list.tsx`).
    pub expanded: Vec<String>,
}

impl DialogFrame {
    fn new(kind: PendingDialog) -> DialogFrame {
        let frame = DialogFrame {
            kind,
            select: primitives::SelectState::default(),
            input: String::new(),
            // `DialogConfirm` starts on `confirm`;
            // `DialogSessionDeleteFailed` on `delete`;
            // `DialogRetryAction` on `action`.
            active: 1,
            pending_delete: None,
            focused_action: None,
            initial_theme: None,
            confirmed: false,
            checks: Vec::new(),
            expanded: Vec::new(),
        };
        match frame.kind {
            PendingDialog::SessionDeleteFailed { .. } | PendingDialog::ExportOptions => {
                DialogFrame { active: 0, ..frame }
            }
            _ => frame,
        }
    }
}

/// The dialog store (`ui/dialog.tsx:69-76`) — a stack; only the top
/// renders.
#[derive(Debug, Default)]
pub struct DialogStack {
    pub stack: Vec<DialogFrame>,
    pub size: DialogSize,
}

impl DialogStack {
    pub fn is_empty(&self) -> bool {
        self.stack.is_empty()
    }

    pub fn top(&self) -> Option<&DialogFrame> {
        self.stack.last()
    }

    pub fn top_mut(&mut self) -> Option<&mut DialogFrame> {
        self.stack.last_mut()
    }

    pub fn top_kind(&self) -> Option<&PendingDialog> {
        self.stack.last().map(|frame| &frame.kind)
    }
}

/// Close every dialog, running each `onClose` (the theme list reverts an
/// unconfirmed preview) — `dialog.tsx:140-149`.
pub fn clear(app: &mut App) {
    let frames = std::mem::take(&mut app.ui.dialogs.stack);
    for frame in frames {
        revert_theme(app, &frame);
    }
    app.ui.dialogs.size = DialogSize::Medium;
}

fn revert_theme(app: &mut App, frame: &DialogFrame) {
    if let PendingDialog::ThemeList = frame.kind {
        if !frame.confirmed {
            if let Some(initial) = &frame.initial_theme {
                app.ui.theme.set(&mut app.state.kv, initial);
            }
        }
    }
}

/// `dialog.replace(...)` (`ui/dialog.tsx:150-165`) — close everything,
/// then push one.
pub fn open(app: &mut App, kind: PendingDialog) -> Vec<Effect> {
    clear(app);
    let mut frame = DialogFrame::new(kind.clone());
    let mut effects = Vec::new();
    match &kind {
        PendingDialog::ThemeList => {
            frame.initial_theme = Some(app.ui.theme.active.clone());
        }
        PendingDialog::MoveSession => {
            app.ui.move_directories = None;
            if let Some(project_id) = app.state.project.project_id.clone() {
                effects.push(Effect::ProjectDirectories { project_id });
            }
        }
        _ => {}
    }
    app.ui.dialogs.size = default_size(&kind);
    match &kind {
        PendingDialog::SessionRename { session_id } => {
            if let Some(session) = app.state.sync.session(session_id) {
                frame.input = session.title.clone();
            }
        }
        PendingDialog::ExportOptions => {
            // `session-${sessionData.id.slice(0, 8)}.md`
            // (`routes/session/index.tsx:959`).
            if let Some(session_id) = route_session_id(app) {
                let short: String = session_id.chars().take(8).collect();
                frame.input = format!("session-{short}.md");
            }
            // The kv-derived defaults: thinking/tool-details/assistant
            // metadata on, open-without-saving off.
            frame.checks = vec![true, true, true, false];
        }
        PendingDialog::ShareConsent { session_id } => {
            if let Some(session) = app.state.sync.session(session_id) {
                frame.input = format!("{}.md", session.title);
            }
        }
        PendingDialog::ConsoleOrg => {
            // TODO(M8.7): the org list needs `experimental.console.listOrgs`
            // — absent from the M8.1 server seam (recorded gap).
        }
        PendingDialog::SessionList => {
            // Pre-select the current session (`ui/dialog-select.tsx:103-110`).
            if let Some(current) = route_session_id(app) {
                let options = crate::ui::dialogs::sessions::session_list_options(app, &frame);
                if let Some(index) = options
                    .iter()
                    .position(|option| option.value.as_deref() == Some(current))
                {
                    frame.select.move_to(index, max_visible(app));
                }
            }
        }
        _ => {}
    }
    app.ui.dialogs.stack.push(frame);
    effects
}

/// Pops the top dialog (escape/`ctrl+c`/backdrop click) —
/// `ui/dialog.tsx:105-137`.
pub fn pop(app: &mut App) {
    if let Some(frame) = app.ui.dialogs.stack.pop() {
        revert_theme(app, &frame);
    }
}

fn route_session_id(app: &App) -> Option<&str> {
    match &app.state.route.data {
        Route::Session { session_id, .. } => Some(session_id),
        _ => None,
    }
}

// -------------------------------------------------------------- options

/// Compute the option list of a select dialog from live state.
pub fn options(app: &App, frame: &DialogFrame) -> Vec<primitives::SelectOption> {
    use PendingDialog;
    match &frame.kind {
        PendingDialog::CommandPalette => palette::options(app, frame),
        PendingDialog::Model => model::model_options(app),
        PendingDialog::Agent => model::agent_options(app),
        PendingDialog::Variant => model::variant_options(app),
        PendingDialog::ProviderConnect => model::provider_options(
            app,
            &app.ui
                .theme
                .resolve(&app.state.kv)
                .expect("builtin theme resolves"),
        ),
        PendingDialog::ProviderAuthMethod { provider_id } => {
            model::auth_method_options(app, provider_id)
        }
        PendingDialog::Mcp => system::mcp_options(app),
        PendingDialog::ThemeList => system::theme_options(app),
        PendingDialog::SessionList => sessions::session_list_options(app, frame),
        PendingDialog::Skill => system::skill_options(app),
        PendingDialog::StashList => sessions::stash_options(app, frame),
        PendingDialog::Timeline => sessions::timeline_options(app),
        PendingDialog::ForkFromTimeline => sessions::fork_options(app),
        PendingDialog::Message { .. } => sessions::message_options(),
        PendingDialog::MoveSession => sessions::move_options(app, frame),
        PendingDialog::WorkspaceList => system::workspace_options(
            app,
            frame,
            &app.ui
                .theme
                .resolve(&app.state.kv)
                .expect("builtin theme resolves"),
        ),
        PendingDialog::WorkspaceSet => system::workspace_set_options(app),
        PendingDialog::Subagent { .. } => sessions::subagent_options(),
        PendingDialog::ProviderCustomId | PendingDialog::ProviderApiKey { .. } => Vec::new(),
        PendingDialog::Tag | PendingDialog::ConsoleOrg => Vec::new(),
        _ => Vec::new(),
    }
}

/// The footer `actions` — `(title, key label)` pairs.
fn actions(app: &App, frame: &DialogFrame) -> Vec<(String, String)> {
    let hint = |keybind: &str| key_hint(app, keybind).unwrap_or_default();
    match &frame.kind {
        PendingDialog::Model => {
            if crate::state::connected(app) {
                vec![
                    ("Connect provider".to_string(), hint("model_provider_list")),
                    ("Favorite".to_string(), hint("model_favorite_toggle")),
                ]
            } else {
                vec![(
                    "View all providers".to_string(),
                    hint("model_provider_list"),
                )]
            }
        }
        PendingDialog::Mcp => vec![("toggle".to_string(), hint("dialog.mcp.toggle"))],
        PendingDialog::SessionList => {
            let mut actions = vec![
                ("pin/unpin".to_string(), hint("session_pin_toggle")),
                ("delete".to_string(), hint("session_delete")),
                ("rename".to_string(), hint("session_rename")),
            ];
            // `quickSwitchFooterHints` (`dialog-session-list.tsx:197-206`).
            if !app.state.local.session_slots(&app.state.sync).is_empty() {
                let first = hint("session_quick_switch_1");
                let last = hint("session_quick_switch_9");
                if !first.is_empty() && !last.is_empty() {
                    actions.push(("switch".to_string(), quick_switch_range(&first, &last)));
                }
            }
            actions
        }
        PendingDialog::StashList => {
            vec![("delete".to_string(), hint("stash_delete"))]
        }
        PendingDialog::MoveSession => vec![
            ("new".to_string(), hint("dialog.move_session.new")),
            ("delete".to_string(), hint("dialog.move_session.delete")),
            ("refresh".to_string(), hint("dialog.move_session.refresh")),
        ],
        PendingDialog::WorkspaceList => {
            vec![("delete".to_string(), hint("session_delete"))]
        }
        _ => Vec::new(),
    }
}

/// `formatKeyBindings` — the first alternative's first stroke,
/// human-formatted (`keymap.tsx:210-212`).
pub(crate) fn key_hint(app: &App, keybind: &str) -> Option<String> {
    let crate::keymap::bindings::BindingValue::Alternatives(alternatives) =
        app.keymap.bindings.get(keybind)?
    else {
        return None;
    };
    let stroke = alternatives.first()?.first()?;
    let mut parts: Vec<String> = Vec::new();
    if stroke.ctrl {
        parts.push("ctrl".to_string());
    }
    if stroke.meta {
        parts.push("alt".to_string());
    }
    parts.push(stroke.key.clone());
    Some(parts.join("+"))
}

/// `quickSwitchRange` (`dialog-session-list.tsx:360-364`).
fn quick_switch_range(first: &str, last: &str) -> String {
    let mut chars: Vec<char> = first.chars().collect();
    chars.pop();
    let prefix: String = chars.into_iter().collect();
    if first.ends_with('1') && last == format!("{prefix}9") {
        format!("{prefix}1-9")
    } else {
        format!("{first} through {last}")
    }
}

// ----------------------------------------------------------- key input

/// Handle a key while the dialog stack is open.
pub fn handle_key(app: &mut App, key: &crossterm::event::KeyEvent) -> Vec<Effect> {
    let Some(kind) = app.ui.dialogs.top_kind().cloned() else {
        return Vec::new();
    };
    match &kind {
        PendingDialog::SessionRename { .. }
        | PendingDialog::ProviderCustomId
        | PendingDialog::ProviderApiKey { .. } => prompt_key(app, key),
        PendingDialog::ExportOptions => export_key(app, key),
        PendingDialog::Alert {
            exit_on_confirm, ..
        } => {
            if key.code == crossterm::event::KeyCode::Enter
                || key.code == crossterm::event::KeyCode::Esc
            {
                app.ui.dialogs.stack.pop();
                if *exit_on_confirm {
                    app.exit(None);
                }
            }
            Vec::new()
        }
        PendingDialog::Help => {
            if key.code == crossterm::event::KeyCode::Enter
                || key.code == crossterm::event::KeyCode::Esc
            {
                app.ui.dialogs.stack.pop();
            }
            Vec::new()
        }
        PendingDialog::Debug => debug_key(app, key),
        PendingDialog::UpdateAvailable { .. }
        | PendingDialog::ShareConsent { .. }
        | PendingDialog::WorkspaceUnavailable
        | PendingDialog::SessionDeleteFailed { .. }
        | PendingDialog::RetryAction { .. } => confirm_key(app, key),
        PendingDialog::Status | PendingDialog::ConsoleOrg | PendingDialog::Tag => {
            if key.code == crossterm::event::KeyCode::Esc || is_ctrl_c(key) {
                app.ui.dialogs.stack.pop();
            }
            Vec::new()
        }
        _ => select_key(app, key),
    }
}

/// `ctrl+c` — the raw key the dialog layer binds (`dialog.tsx:122-135`).
fn is_ctrl_c(key: &crossterm::event::KeyEvent) -> bool {
    key.code == crossterm::event::KeyCode::Char('c')
        && key
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL)
}

/// A printable char (filter/textarea input).
fn typed_char(key: &crossterm::event::KeyEvent) -> Option<char> {
    if key
        .modifiers
        .intersects(crossterm::event::KeyModifiers::CONTROL)
    {
        return None;
    }
    match key.code {
        crossterm::event::KeyCode::Char(char) => Some(char),
        _ => None,
    }
}

/// The debug dialog — `return` copies, escape closes
/// (`dialog-debug.tsx:48-57`).
fn debug_key(app: &mut App, key: &crossterm::event::KeyEvent) -> Vec<Effect> {
    if key.code == crossterm::event::KeyCode::Esc || is_ctrl_c(key) {
        app.ui.dialogs.stack.pop();
        return Vec::new();
    }
    if key.code != crossterm::event::KeyCode::Enter {
        return Vec::new();
    }
    app.ui.dialogs.stack.pop();
    let text = debug_entries(app)
        .into_iter()
        .map(|(label, value)| format!("{label}. {value}"))
        .collect::<Vec<_>>()
        .join("\n");
    vec![Effect::ClipboardWrite {
        text,
        success: Some(Toast {
            title: None,
            variant: ToastVariant::Info,
            message: "Debug info copied to clipboard".to_string(),
            duration_ms: 5000,
        }),
        failure: None,
    }]
}

/// `DialogPrompt` keys (`ui/dialog-prompt.tsx`): typing into the
/// textarea, `dialog.prompt.submit` confirms, escape closes.
fn prompt_key(app: &mut App, key: &crossterm::event::KeyEvent) -> Vec<Effect> {
    let kind = app.ui.dialogs.top_kind().cloned();
    if app.keymap.matches("dialog.prompt.submit", key) {
        let input = app
            .ui
            .dialogs
            .top()
            .map(|frame| frame.input.clone())
            .unwrap_or_default();
        app.ui.dialogs.stack.pop();
        if let Some(kind) = kind {
            return prompt_confirm(app, kind, input);
        }
        return Vec::new();
    }
    if key.code == crossterm::event::KeyCode::Esc || is_ctrl_c(key) {
        app.ui.dialogs.stack.pop();
        return Vec::new();
    }
    if let Some(frame) = app.ui.dialogs.top_mut() {
        match key.code {
            crossterm::event::KeyCode::Backspace => {
                frame.input.pop();
            }
            _ => {
                if let Some(char) = typed_char(key) {
                    frame.input.push(char);
                }
            }
        }
    }
    Vec::new()
}

/// `DialogPrompt.onConfirm` for the two prompt dialogs.
fn prompt_confirm(app: &mut App, kind: PendingDialog, input: String) -> Vec<Effect> {
    match kind {
        PendingDialog::SessionRename { session_id } => {
            app.ui.dialogs.stack.pop();
            vec![Effect::SessionRename {
                session_id,
                title: input,
            }]
        }
        PendingDialog::ProviderCustomId => {
            let provider_id = input
                .trim()
                .strip_prefix("@ai-sdk/")
                .unwrap_or(input.trim());
            if is_valid_custom_provider_id(provider_id) {
                return open(
                    app,
                    PendingDialog::ProviderAuthMethod {
                        provider_id: provider_id.to_string(),
                    },
                );
            }
            app.show_toast(Toast {
                title: None,
                variant: ToastVariant::Error,
                message: "Provider ids must start with a lowercase letter or number and only use lowercase letters, numbers, hyphens, and underscores".to_string(),
                duration_ms: 5000,
            });
            open(app, PendingDialog::ProviderCustomId)
        }
        PendingDialog::ProviderApiKey { provider_id } => {
            let key = input.trim();
            if key.is_empty() {
                app.show_toast(Toast {
                    title: None,
                    variant: ToastVariant::Error,
                    message: "API key cannot be empty".to_string(),
                    duration_ms: 5000,
                });
                let provider_id = provider_id.clone();
                open(app, PendingDialog::ProviderApiKey { provider_id });
                return Vec::new();
            }
            vec![Effect::AuthSet {
                provider_id,
                key: key.to_string(),
            }]
        }
        _ => Vec::new(),
    }
}

/// `normalizeCustomProviderID` (`dialog-provider.tsx:61-65`):
/// `/^[a-z0-9][a-z0-9-_]*$/`.
fn is_valid_custom_provider_id(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// `DialogExportOptions` keys (`ui/dialog-export-options.tsx`).
fn export_key(app: &mut App, key: &crossterm::event::KeyEvent) -> Vec<Effect> {
    const ORDER: usize = 5;
    if key.code == crossterm::event::KeyCode::Esc || is_ctrl_c(key) {
        app.ui.dialogs.stack.pop();
        return Vec::new();
    }
    if key.code == crossterm::event::KeyCode::Tab {
        if let Some(frame) = app.ui.dialogs.top_mut() {
            frame.active = (frame.active + 1) % ORDER;
        }
        return Vec::new();
    }
    if app.keymap.matches("dialog.prompt.submit", key) {
        let Some(frame) = app.ui.dialogs.top() else {
            return Vec::new();
        };
        let effect = Effect::SessionExport {
            filename: frame.input.clone(),
            thinking: *frame.checks.first().unwrap_or(&true),
            tool_details: *frame.checks.get(1).unwrap_or(&true),
            assistant_metadata: *frame.checks.get(2).unwrap_or(&true),
            open_without_saving: *frame.checks.get(3).unwrap_or(&false),
        };
        app.ui.dialogs.stack.pop();
        return vec![effect];
    }
    let Some(frame) = app.ui.dialogs.top_mut() else {
        return Vec::new();
    };
    if key.code == crossterm::event::KeyCode::Char(' ') && frame.active != 0 {
        let index = frame.active - 1;
        if index < frame.checks.len() {
            frame.checks[index] = !frame.checks[index];
        }
        return Vec::new();
    }
    if frame.active == 0 {
        match key.code {
            crossterm::event::KeyCode::Backspace => {
                frame.input.pop();
            }
            _ => {
                if let Some(char) = typed_char(key) {
                    frame.input.push(char);
                }
            }
        }
    }
    Vec::new()
}

/// The confirm-style dialogs: left/right toggles, return runs
/// (`DialogConfirm`, `DialogSessionDeleteFailed`,
/// `DialogWorkspaceUnavailable`, `DialogRetryAction`).
fn confirm_key(app: &mut App, key: &crossterm::event::KeyEvent) -> Vec<Effect> {
    if key.code == crossterm::event::KeyCode::Esc || is_ctrl_c(key) {
        app.ui.dialogs.stack.pop();
        return Vec::new();
    }
    let left_right = matches!(
        key.code,
        crossterm::event::KeyCode::Left
            | crossterm::event::KeyCode::Right
            | crossterm::event::KeyCode::Up
            | crossterm::event::KeyCode::Down
            | crossterm::event::KeyCode::Tab
    );
    if left_right {
        if let Some(frame) = app.ui.dialogs.top_mut() {
            frame.active = 1 - frame.active;
        }
        return Vec::new();
    }
    if key.code != crossterm::event::KeyCode::Enter {
        return Vec::new();
    }
    let Some(kind) = app.ui.dialogs.top_kind().cloned() else {
        return Vec::new();
    };
    let active = app.ui.dialogs.top().map(|frame| frame.active).unwrap_or(0);
    match kind {
        PendingDialog::UpdateAvailable { version } => {
            app.ui.dialogs.stack.pop();
            if active == 0 {
                // "skip" (`app.tsx:1063-1066`).
                app.state
                    .kv
                    .set(keys::SKIPPED_VERSION, serde_json::json!(version));
                return Vec::new();
            }
            app.show_toast(Toast {
                title: None,
                variant: ToastVariant::Info,
                message: format!("Updating to v{version}…"),
                duration_ms: 30000,
            });
            vec![Effect::GlobalUpgrade { target: version }]
        }
        PendingDialog::ShareConsent { session_id } => {
            app.ui.dialogs.stack.pop();
            if active == 0 {
                return Vec::new();
            }
            app.state
                .kv
                .set(keys::SHARE_CONSENT, serde_json::json!(true));
            vec![Effect::SessionShare { session_id }]
        }
        PendingDialog::WorkspaceUnavailable => {
            app.ui.dialogs.stack.pop();
            if active == 1 {
                // `onRestore` → `openWorkspaceSelect`
                // (`prompt/index.tsx:978-987`).
                return open(app, PendingDialog::WorkspaceSet);
            }
            Vec::new()
        }
        PendingDialog::SessionDeleteFailed { session_id, .. } => {
            app.ui.dialogs.stack.pop();
            if active == 0 {
                // `onDelete` — the delete retry.
                vec![Effect::SessionDelete { session_id }]
            } else {
                open(app, PendingDialog::WorkspaceSet)
            }
        }
        PendingDialog::RetryAction { link, kv_key, .. } => {
            app.ui.dialogs.stack.pop();
            if active == 1 {
                let mut effects = Vec::new();
                if let Some(link) = link {
                    effects.push(Effect::OpenUrl { url: link });
                }
                return effects;
            }
            if let Some(kv_key) = kv_key {
                app.state.kv.set(&kv_key, serde_json::json!(true));
            }
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// The `DialogSelect` dialogs: movement, submit, actions and the filter
/// input (`dialog-select.tsx:369-483`).
fn select_key(app: &mut App, key: &crossterm::event::KeyEvent) -> Vec<Effect> {
    let Some(kind) = app.ui.dialogs.top_kind().cloned() else {
        return Vec::new();
    };
    // Close keys (`dialog.tsx:105-137`): escape/ctrl+c.
    if key.code == crossterm::event::KeyCode::Esc || is_ctrl_c(key) {
        pop(app);
        return Vec::new();
    }
    // ---- `tab`/`shift+tab` — the footer action focus
    // (`dialog-select.tsx:461-476`)
    if key.code == crossterm::event::KeyCode::Tab || key.code == crossterm::event::KeyCode::BackTab
    {
        let direction = if key.code == crossterm::event::KeyCode::Tab {
            1
        } else {
            -1
        };
        move_action(app, &kind, direction);
        return Vec::new();
    }
    // ---- `dialog.select.*` (`dialog-select.tsx:450-459`)
    let movement = if app.keymap.matches("dialog.select.prev", key) {
        Some(-1i64)
    } else if app.keymap.matches("dialog.select.next", key) {
        Some(1)
    } else if app.keymap.matches("dialog.select.page_up", key) {
        Some(-10)
    } else if app.keymap.matches("dialog.select.page_down", key) {
        Some(10)
    } else {
        None
    };
    if let Some(step) = movement {
        let len = filtered_options(app).len();
        let max_visible = max_visible(app);
        if let Some(frame) = app.ui.dialogs.top_mut() {
            frame.focused_action = None;
            frame.select.move_by(step, len, max_visible);
            on_move(app, &kind);
        }
        return Vec::new();
    }
    if app.keymap.matches("dialog.select.home", key) {
        let max_visible = max_visible(app);
        if let Some(frame) = app.ui.dialogs.top_mut() {
            frame.focused_action = None;
            frame.select.move_to(0, max_visible);
        }
        return Vec::new();
    }
    if app.keymap.matches("dialog.select.end", key) {
        let len = filtered_options(app).len();
        let max_visible = max_visible(app);
        if let Some(frame) = app.ui.dialogs.top_mut() {
            frame.focused_action = None;
            frame.select.move_to(len.saturating_sub(1), max_visible);
        }
        return Vec::new();
    }
    // ---- per-dialog action keybinds (checked before the filter so
    // e.g. `space` never types into the MCP filter)
    let (effects, handled) = action_key(app, &kind, key);
    if handled {
        return effects;
    }
    // ---- submit (`dialog.select.submit` = return)
    if app.keymap.matches("dialog.select.submit", key) {
        let focused = app.ui.dialogs.top().and_then(|frame| frame.focused_action);
        if let Some(index) = focused {
            return run_action(app, &kind, index);
        }
        return submit(app, &kind);
    }
    // ---- the filter input
    if let Some(frame) = app.ui.dialogs.top_mut() {
        frame.focused_action = None;
        match key.code {
            crossterm::event::KeyCode::Backspace => {
                frame.select.filter.pop();
            }
            _ => {
                if let Some(char) = typed_char(key) {
                    frame.select.filter.push(char);
                }
            }
        }
        if let PendingDialog::ThemeList = kind {
            // The theme list previews through the filter
            // (`dialog-theme-list.tsx:34-43`).
            let (filter, initial) = {
                let frame = app.ui.dialogs.top().expect("checked above");
                (frame.select.filter.clone(), frame.initial_theme.clone())
            };
            let filtered = {
                let options = options(app, app.ui.dialogs.top().expect("checked above"));
                primitives::filter_options(&filter, options)
            };
            if filter.is_empty() {
                if let Some(initial) = initial {
                    app.ui.theme.set(&mut app.state.kv, &initial);
                }
            } else if let Some(first) = filtered.first() {
                if let Some(value) = &first.value {
                    app.ui.theme.set(&mut app.state.kv, value);
                }
            }
        }
    }
    Vec::new()
}

/// `moveAction` (`dialog-select.tsx:358-367`) — cycle the footer action
/// focus; stepping past either end clears it.
fn move_action(app: &mut App, kind: &PendingDialog, direction: i64) {
    let total = app
        .ui
        .dialogs
        .top()
        .map(|frame| actions(app, frame).len().min(action_keybinds(kind).len()))
        .unwrap_or(0);
    if total == 0 {
        return;
    }
    if let Some(frame) = app.ui.dialogs.top_mut() {
        frame.focused_action = match frame.focused_action {
            None => Some(if direction == 1 { 0 } else { total - 1 }),
            Some(index) => {
                let next = index as i64 + direction;
                if next < 0 || next >= total as i64 {
                    None
                } else {
                    Some(next as usize)
                }
            }
        };
    }
}

/// The currently filtered option list of the top dialog.
/// Mouse-wheel scroll of the topmost dialog's list (`dialog-select.tsx`
/// wraps the options in a `<scrollbox>` — the wheel moves the viewport,
/// not the selection; default speed 3, `util/scroll.ts:24-26`).
pub fn wheel_scroll(app: &mut App, direction: i64) {
    let len = filtered_options(app).len();
    if len == 0 {
        return;
    }
    let max = (len as i64 - max_visible(app) as i64).max(0);
    if let Some(frame) = app.ui.dialogs.top_mut() {
        let next = frame.select.scroll as i64 + direction * 3;
        frame.select.scroll = next.clamp(0, max) as usize;
    }
}

/// The scrollbox window height — `Math.floor(dimensions().height / 2) -
/// 6` (`dialog-select.tsx:213`).
pub fn max_visible(app: &App) -> usize {
    primitives::max_visible_options(app.ui.terminal_height)
}

/// Whether the dialog renders through the `DialogSelect` branch of
/// `content_lines` — the kinds with clickable option rows.
fn is_select_kind(kind: &PendingDialog) -> bool {
    !matches!(
        kind,
        PendingDialog::Status
            | PendingDialog::Help
            | PendingDialog::Debug
            | PendingDialog::Alert { .. }
            | PendingDialog::SessionRename { .. }
            | PendingDialog::ProviderCustomId
            | PendingDialog::ProviderApiKey { .. }
            | PendingDialog::UpdateAvailable { .. }
            | PendingDialog::ShareConsent { .. }
            | PendingDialog::WorkspaceUnavailable
            | PendingDialog::SessionDeleteFailed { .. }
            | PendingDialog::RetryAction { .. }
            | PendingDialog::ExportOptions
            | PendingDialog::ConsoleOrg
    )
}

/// The option row layout of the top dialog — the
/// `(line index, filtered option index)` pairs of the rows the
/// `content_lines` select branch renders. `None` for non-select
/// dialogs.
fn select_layout(app: &App) -> Option<Vec<(usize, usize)>> {
    let frame = app.ui.dialogs.top()?;
    if !is_select_kind(&frame.kind) {
        return None;
    }
    let theme = app
        .ui
        .theme
        .resolve(&app.state.kv)
        .expect("builtin theme resolves");
    let view_options = options(app, frame);
    let view = primitives::SelectView {
        title: select_title(&frame.kind).to_string(),
        filter: true,
        options: view_options,
        actions: actions(app, frame),
        action_focused: frame.focused_action.is_some(),
    };
    // The select branch renders header + filter before the options
    // (content_lines); their text does not affect the row count.
    let width = app.ui.dialogs.size.width();
    let mut lines = vec![
        primitives::header_line(&theme, &view.title, "esc", width),
        primitives::filter_line(&theme, &frame.select, "Search"),
    ];
    Some(primitives::render_options(
        &frame.select,
        &view,
        &theme,
        &mut lines,
        width,
        max_visible(app),
    ))
}

/// The filtered option index rendered at `column`/`row`, if the
/// position lands on an option row of the top dialog
/// (`dialog-select.tsx:640-676`).
pub fn option_row(app: &App, column: u16, row: u16) -> Option<usize> {
    let layout = select_layout(app)?;
    let frame = app.ui.dialogs.top()?;
    let theme = app
        .ui
        .theme
        .resolve(&app.state.kv)
        .expect("builtin theme resolves");
    let lines = content_lines(app, frame, &theme);
    let area = Rect {
        x: 0,
        y: 0,
        width: app.ui.terminal_width.max(1),
        height: app.ui.terminal_height.max(1),
    };
    let rect = frame_rect(app, lines.len(), area);
    if column < rect.x || column >= rect.x + rect.width {
        return None;
    }
    let clicked = row.checked_sub(rect.y)? as usize;
    layout
        .iter()
        .find(|(line, _)| *line == clicked)
        .map(|(_, index)| *index)
}

/// `onMouseDown`/`onMouseOver` (`dialog-select.tsx:664-672`) — hover
/// or press moves the selection to the row under the pointer.
pub fn mouse_move_to(app: &mut App, index: usize) {
    let Some(kind) = app.ui.dialogs.top_kind().cloned() else {
        return;
    };
    let max = max_visible(app);
    if let Some(frame) = app.ui.dialogs.top_mut() {
        frame.select.move_to(index, max);
    }
    on_move(app, &kind);
}

/// `onMouseUp` (`dialog-select.tsx:652-657`) — release activates the
/// row under the pointer.
pub fn mouse_submit(app: &mut App, index: usize) -> Vec<Effect> {
    let Some(kind) = app.ui.dialogs.top_kind().cloned() else {
        return Vec::new();
    };
    let max = max_visible(app);
    if let Some(frame) = app.ui.dialogs.top_mut() {
        frame.select.move_to(index, max);
    }
    on_move(app, &kind);
    submit(app, &kind)
}

fn filtered_options(app: &App) -> Vec<primitives::SelectOption> {
    let Some(frame) = app.ui.dialogs.top() else {
        return Vec::new();
    };
    let filter = frame.select.filter.clone();
    let options = options(app, frame);
    primitives::filter_options(&filter, options)
}

/// `onMove` — the theme list live preview, the timeline jump and the
/// delete-confirm reset (`dialog-theme-list.tsx:29-31`,
/// `dialog-timeline.tsx:44`).
fn on_move(app: &mut App, kind: &PendingDialog) {
    if let Some(frame) = app.ui.dialogs.top_mut() {
        // `moveTo` clears the footer action focus
        // (`dialog-select.tsx:299-309`).
        frame.focused_action = None;
    }
    let Some(frame) = app.ui.dialogs.top() else {
        return;
    };
    let selected = frame.select.selected;
    let filter = frame.select.filter.clone();
    let view_options = options(app, frame);
    let option = primitives::filter_options(&filter, view_options)
        .get(selected)
        .cloned();
    match kind {
        PendingDialog::ThemeList => {
            if let Some(value) = option.and_then(|option| option.value) {
                app.ui.theme.set(&mut app.state.kv, &value);
            }
        }
        PendingDialog::Timeline | PendingDialog::ForkFromTimeline => {
            if let Some(value) = option.and_then(|option| option.value) {
                if let Some((_, y)) = app
                    .ui
                    .session_scroll
                    .children
                    .iter()
                    .find(|(id, _)| *id == value)
                {
                    let top = app.ui.session_scroll.effective_y();
                    app.ui.session_scroll.scroll_by(*y as i64 - top as i64 - 1);
                }
            }
        }
        PendingDialog::SessionList
        | PendingDialog::StashList
        | PendingDialog::MoveSession
        | PendingDialog::WorkspaceList => {
            // `onMove={() => setToDelete(undefined)}`
            if let Some(frame) = app.ui.dialogs.top_mut() {
                frame.pending_delete = None;
            }
        }
        _ => {}
    }
}

/// The footer action keybinds, in `actions()` order — the `tab`-focusable
/// subset (`dialog-select.tsx:143-147,461-476`). The trailing
/// footer hints (the session-list quick switch) are not actions.
fn action_keybinds(kind: &PendingDialog) -> &'static [&'static str] {
    match kind {
        PendingDialog::Model => &["model_provider_list", "model_favorite_toggle"],
        PendingDialog::Mcp => &["dialog.mcp.toggle"],
        PendingDialog::SessionList => &["session_pin_toggle", "session_delete", "session_rename"],
        PendingDialog::StashList => &["stash_delete"],
        PendingDialog::MoveSession => &[
            "dialog.move_session.new",
            "dialog.move_session.delete",
            "dialog.move_session.refresh",
        ],
        PendingDialog::WorkspaceList => &["session_delete"],
        _ => &[],
    }
}

/// The value of the currently selected option.
fn selected_value(app: &App) -> Option<String> {
    app.ui.dialogs.top().and_then(|frame| {
        let filter = frame.select.filter.clone();
        let selected = frame.select.selected;
        let options = options(app, frame);
        primitives::filter_options(&filter, options)
            .get(selected)
            .and_then(|option| option.value.clone())
    })
}

/// Trigger the `index`th footer action on the selected option —
/// `triggerAction` (`dialog-select.tsx:344-356,503-510`).
fn run_action(app: &mut App, kind: &PendingDialog, index: usize) -> Vec<Effect> {
    let value = selected_value(app);
    match kind {
        PendingDialog::Model => match index {
            0 => {
                app.ui.dialogs.stack.pop();
                return open(app, PendingDialog::ProviderConnect);
            }
            1 => {
                if let Some(value) = value {
                    model::toggle_favorite(app, &value);
                }
            }
            _ => {}
        },
        PendingDialog::Mcp => {
            if let (0, Some(name)) = (index, value) {
                return vec![Effect::McpToggle { name }];
            }
        }
        PendingDialog::SessionList => match index {
            0 => {
                if let Some(value) = value {
                    app.state.local.session_toggle_pin(&value);
                }
            }
            1 => {
                if let Some(value) = value {
                    let pending = app
                        .ui
                        .dialogs
                        .top()
                        .and_then(|frame| frame.pending_delete.clone());
                    if pending.as_deref() == Some(value.as_str()) {
                        if let Some(frame) = app.ui.dialogs.top_mut() {
                            frame.pending_delete = None;
                        }
                        return vec![Effect::SessionDelete { session_id: value }];
                    }
                    if let Some(frame) = app.ui.dialogs.top_mut() {
                        frame.pending_delete = Some(value);
                    }
                }
            }
            2 => {
                if let Some(value) = value {
                    app.ui.dialogs.stack.pop();
                    return open(app, PendingDialog::SessionRename { session_id: value });
                }
            }
            _ => {}
        },
        PendingDialog::StashList => {
            if let (0, Some(value)) = (index, value) {
                sessions::stash_delete(app, &value);
            }
        }
        PendingDialog::MoveSession => match index {
            // TODO(M8.7): `dialog.move_session.new` needs the
            // `projectCopy.create` endpoint — absent from the M8.1
            // server seam (recorded gap).
            0 => {}
            1 => {
                if let Some(value) = value {
                    let pending = app
                        .ui
                        .dialogs
                        .top()
                        .and_then(|frame| frame.pending_delete.clone());
                    if pending.as_deref() == Some(value.as_str()) {
                        if let Some(frame) = app.ui.dialogs.top_mut() {
                            frame.pending_delete = None;
                        }
                        // TODO(M8.7): the removal itself needs the
                        // `projectCopy.remove` endpoint — absent from
                        // the server seam (recorded gap).
                        app.show_toast(Toast {
                            title: None,
                            variant: ToastVariant::Warning,
                            message: format!("Failed to delete project copy: {value}"),
                            duration_ms: 5000,
                        });
                    } else if let Some(frame) = app.ui.dialogs.top_mut() {
                        frame.pending_delete = Some(value);
                    }
                }
            }
            2 => {
                if let Some(project_id) = app.state.project.project_id.clone() {
                    return vec![Effect::ProjectDirectories { project_id }];
                }
            }
            _ => {}
        },
        PendingDialog::WorkspaceList => {
            if let (0, Some(value)) = (index, value) {
                let pending = app
                    .ui
                    .dialogs
                    .top()
                    .and_then(|frame| frame.pending_delete.clone());
                if pending.as_deref() == Some(value.as_str()) {
                    if let Some(frame) = app.ui.dialogs.top_mut() {
                        frame.pending_delete = None;
                    }
                    // TODO(M8.7): `experimental.workspace.remove` is
                    // absent from the server seam (recorded gap).
                    app.show_toast(Toast {
                        title: None,
                        variant: ToastVariant::Warning,
                        message: format!("Failed to delete workspace: {value}"),
                        duration_ms: 5000,
                    });
                } else if let Some(frame) = app.ui.dialogs.top_mut() {
                    frame.pending_delete = Some(value);
                }
            }
        }
        _ => {}
    }
    Vec::new()
}

/// The per-dialog action keybinds. Returns the effects plus whether the
/// key was consumed.
fn action_key(
    app: &mut App,
    kind: &PendingDialog,
    key: &crossterm::event::KeyEvent,
) -> (Vec<Effect>, bool) {
    for (index, keybind) in action_keybinds(kind).iter().enumerate() {
        if app.keymap.matches(keybind, key) {
            return (run_action(app, kind, index), true);
        }
    }
    (Vec::new(), false)
}

/// `dialog.select.submit` — the `onSelect` of the selected option.
fn submit(app: &mut App, kind: &PendingDialog) -> Vec<Effect> {
    let Some(option) = filtered_options(app)
        .get(
            app.ui
                .dialogs
                .top()
                .map(|frame| frame.select.selected)
                .unwrap_or(0),
        )
        .cloned()
    else {
        return Vec::new();
    };
    match kind {
        PendingDialog::CommandPalette => {
            crate::ui::dialogs::clear(app);
            if let Some(mut value) = option.value {
                if let Some(stripped) = value.strip_prefix("suggested:") {
                    value = stripped.to_string();
                }
                return crate::command::run(app, &value);
            }
        }
        PendingDialog::Agent => {
            if let Some(name) = option.value {
                crate::ui::dialogs::clear(app);
                let sync = std::mem::take(&mut app.state.sync);
                if let Some(toast) = app.state.local.agent_set(&name, &sync) {
                    app.show_toast(toast);
                }
                app.state.sync = sync;
            }
        }
        PendingDialog::Variant => {
            if let Some(value) = option.value {
                crate::ui::dialogs::clear(app);
                let sync = std::mem::take(&mut app.state.sync);
                if value == "default" {
                    app.state.local.variant_set(&sync, &app.state.args, None);
                } else {
                    app.state
                        .local
                        .variant_set(&sync, &app.state.args, Some(&value));
                }
                app.state.sync = sync;
            }
        }
        PendingDialog::Model => {
            if let Some(value) = option.value {
                return model::select_model(app, &value);
            }
        }
        PendingDialog::ThemeList => {
            if let Some(value) = option.value {
                app.ui.theme.set(&mut app.state.kv, &value);
                if let Some(frame) = app.ui.dialogs.top_mut() {
                    frame.confirmed = true;
                }
                app.ui.dialogs.stack.pop();
            }
        }
        PendingDialog::Mcp => {
            // "Don't close on select, only on escape"
            // (`dialog-mcp.tsx:79-83`).
        }
        PendingDialog::SessionList => {
            if let Some(session_id) = option.value {
                crate::ui::dialogs::clear(app);
                app.state.route.navigate(Route::Session {
                    session_id,
                    prompt: None,
                });
            }
        }
        PendingDialog::Skill => {
            if let Some(skill) = option.value {
                crate::ui::dialogs::clear(app);
                app.ui.prompt.textarea.set_text(&format!("/{skill} "));
            }
        }
        PendingDialog::StashList => {
            sessions::stash_pop(app, &option.value.unwrap_or_default());
        }
        PendingDialog::Timeline => {
            if let Some(message_id) = option.value {
                let Some(session_id) = route_session_id(app).map(str::to_string) else {
                    return Vec::new();
                };
                return open(
                    app,
                    PendingDialog::Message {
                        session_id,
                        message_id,
                    },
                );
            }
        }
        PendingDialog::ForkFromTimeline => {
            let Route::Session { session_id, .. } = &app.state.route.data else {
                return Vec::new();
            };
            let session_id = session_id.clone();
            match option.value.as_deref() {
                Some("full") => {
                    return vec![Effect::SessionForkFromMessage {
                        session_id,
                        message_id: None,
                        seed_prompt: false,
                    }]
                }
                Some(message_id) => {
                    return vec![Effect::SessionForkFromMessage {
                        session_id,
                        message_id: Some(message_id.to_string()),
                        seed_prompt: true,
                    }]
                }
                _ => {}
            }
        }
        PendingDialog::Message {
            session_id,
            message_id,
        } => {
            return sessions::message_action(app, session_id, message_id, &option);
        }
        PendingDialog::MoveSession => {
            if let Some(directory) = option.value {
                let Some(session_id) = route_session_id(app).map(str::to_string) else {
                    return Vec::new();
                };
                crate::ui::dialogs::clear(app);
                return vec![Effect::SessionMove {
                    session_id,
                    directory,
                }];
            }
        }
        PendingDialog::WorkspaceList => {
            if let Some(workspace) = option.value {
                if let Some(frame) = app.ui.dialogs.top_mut() {
                    if let Some(position) = frame.expanded.iter().position(|id| id == &workspace) {
                        frame.expanded.remove(position);
                    } else {
                        frame.expanded.push(workspace);
                    }
                }
            }
        }
        PendingDialog::WorkspaceSet => {
            if let Some(directory) = option.value {
                let Some(session_id) = route_session_id(app).map(str::to_string) else {
                    return Vec::new();
                };
                crate::ui::dialogs::clear(app);
                if directory != "none" {
                    return vec![Effect::SessionMove {
                        session_id,
                        directory,
                    }];
                }
            }
        }
        PendingDialog::ProviderConnect => {
            let value = option.value.unwrap_or_default();
            if value == "__opencode_custom_provider__" {
                return open(app, PendingDialog::ProviderCustomId);
            }
            return open(
                app,
                PendingDialog::ProviderAuthMethod { provider_id: value },
            );
        }
        PendingDialog::ProviderAuthMethod { provider_id } => {
            // `method.type === "api"` (`dialog-provider.tsx:209-217`) — the
            // api-key prompt. OAuth methods need the plugin-hook authorize
            // flow (`provider.oauth.authorize`), still a recorded seam gap.
            let provider_id = provider_id.clone();
            return open(app, PendingDialog::ProviderApiKey { provider_id });
        }
        PendingDialog::Subagent { session_id } => {
            let session_id = session_id.clone();
            crate::ui::dialogs::clear(app);
            app.state.route.navigate(Route::Session {
                session_id,
                prompt: None,
            });
        }
        _ => {}
    }
    Vec::new()
}

// ---------------------------------------------------------------- debug

/// `describeOS` (`util/system.ts:3-13`).
fn describe_os() -> String {
    let name = match std::env::consts::OS {
        "macos" => "macOS",
        "windows" => "Windows",
        "linux" => "Linux",
        other => other,
    };
    let release = std::process::Command::new("uname")
        .arg("-r")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default();
    if release.is_empty() {
        format!("{name} ({})", std::env::consts::ARCH)
    } else {
        format!("{name} {release} ({})", std::env::consts::ARCH)
    }
}

/// `describeTerminal` (`util/system.ts:15-21`).
fn describe_terminal() -> String {
    let env = |key: &str| std::env::var(key).ok().filter(|value| !value.is_empty());
    let program = env("TERM_PROGRAM")
        .or_else(|| env("TERM"))
        .unwrap_or_else(|| "unknown".to_string());
    let version = env("TERM_PROGRAM_VERSION")
        .map(|version| format!(" {version}"))
        .unwrap_or_default();
    let multiplexer = if std::env::var_os("TMUX").is_some() {
        " in tmux"
    } else if std::env::var_os("STY").is_some() {
        " in screen"
    } else {
        ""
    };
    format!("{program}{version}{multiplexer}")
}

/// The debug dialog entries (`dialog-debug.tsx:26-39`).
fn debug_entries(app: &App) -> Vec<(String, String)> {
    let session = match &app.state.route.data {
        Route::Session { session_id, .. } => session_id.clone(),
        _ => "n/a".to_string(),
    };
    let model = app
        .state
        .local
        .model_current(&app.state.sync, &app.state.args)
        .map(|model| model.key());
    vec![
        (
            "Version".to_string(),
            format!("{INSTALLATION_VERSION} (dev)"),
        ),
        (
            "Date".to_string(),
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        ),
        ("OS".to_string(), describe_os()),
        ("Terminal".to_string(), describe_terminal()),
        ("Session ID".to_string(), session),
        (
            "Model".to_string(),
            model.unwrap_or_else(|| "n/a".to_string()),
        ),
    ]
}

// -------------------------------------------------------------- render

/// The dialog frame rectangle (`ui/dialog.tsx:39-64`): centered,
/// `paddingTop = height / 4`, width capped to the terminal.
fn frame_rect(app: &App, lines: usize, area: Rect) -> Rect {
    let width = app
        .ui
        .dialogs
        .size
        .width()
        .min(area.width.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + area.height / 4;
    let height = (lines as u16 + 2).max(1);
    Rect {
        x,
        y,
        width,
        height: height.min(area.bottom().saturating_sub(y).max(1)),
    }
}

/// Backdrop hit test — a click outside the frame closes the dialog
/// (`dialog.tsx:30-38`).
pub fn hit_test(app: &App, column: u16, row: u16) -> bool {
    let Some(frame) = app.ui.dialogs.top() else {
        return false;
    };
    let theme = app
        .ui
        .theme
        .resolve(&app.state.kv)
        .expect("builtin theme resolves");
    let lines = content_lines(app, frame, &theme).len();
    let area = Rect {
        x: 0,
        y: 0,
        width: app.ui.terminal_width.max(1),
        height: app.ui.terminal_height.max(1),
    };
    let rect = frame_rect(app, lines, area);
    column >= rect.x && column < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
}

/// Render the top dialog over a dimmed backdrop (`dialog.tsx:28-66`).
pub fn render(app: &App, frame: &mut ratatui::Frame, theme: &Theme, area: Rect) {
    let Some(dialog) = app.ui.dialogs.top() else {
        return;
    };
    // Backdrop: `RGBA.fromInts(0, 0, 0, 150)` — OpenTUI composites the
    // translucent black over the underlying cells, dimming their text
    // too. ratatui has no alpha channel, so both the fg and bg of every
    // covered cell are tinted toward black by `150/255`.
    let black = Rgba {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: 1.0,
    };
    let alpha = 150.0 / 255.0;
    let buffer = frame.buffer_mut();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let Some(cell) = buffer.cell_mut((x, y)) else {
                continue;
            };
            if let ratatui::style::Color::Rgb(r, g, b) = cell.bg {
                cell.bg = crate::ui::theme::tint(Rgba::from_ints(r, g, b), black, alpha).to_color();
            }
            if let ratatui::style::Color::Rgb(r, g, b) = cell.fg {
                cell.fg = crate::ui::theme::tint(Rgba::from_ints(r, g, b), black, alpha).to_color();
            }
        }
    }

    let lines = content_lines(app, dialog, theme);
    let rect = frame_rect(app, lines.len(), area);
    primitives::paint(&lines, theme, rect, frame);
}

/// The line rows of one dialog.
fn content_lines(app: &App, dialog: &DialogFrame, theme: &Theme) -> Vec<Line<'static>> {
    let width = app.ui.dialogs.size.width();
    let wrap_width = width.saturating_sub(4);
    match &dialog.kind {
        PendingDialog::Status => system::status_lines(app, theme, width),
        PendingDialog::Help => {
            let mut lines = vec![primitives::header_line(theme, "Help", "esc/enter", width)];
            lines.push(Line::styled(
                "  Press ctrl+p to see all available actions and commands in any context.",
                Style::new().fg(theme.text_muted.to_color()),
            ));
            lines.push(ok_button(theme, width));
            lines
        }
        PendingDialog::Debug => {
            let mut lines = vec![primitives::header_line(theme, "Debug", "esc", width)];
            for (label, value) in debug_entries(app) {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  {label:<10} "),
                        Style::new().fg(theme.text_muted.to_color()),
                    ),
                    Span::styled(value, Style::new().fg(theme.text.to_color())),
                ]));
            }
            lines.push(Line::styled(
                "  Share this when reporting an issue.",
                Style::new().fg(theme.text_muted.to_color()),
            ));
            lines
        }
        PendingDialog::Alert {
            title,
            message,
            exit_on_confirm: _,
        } => {
            let mut lines = vec![primitives::header_line(theme, title, "esc", width)];
            primitives::wrap_text(
                message,
                wrap_width,
                &mut lines,
                Style::new().fg(theme.text_muted.to_color()),
            );
            lines.push(ok_button(theme, width));
            lines
        }
        PendingDialog::SessionRename { .. }
        | PendingDialog::ProviderCustomId
        | PendingDialog::ProviderApiKey { .. } => {
            let title = match dialog.kind {
                PendingDialog::SessionRename { .. } => "Rename Session",
                PendingDialog::ProviderApiKey { .. } => "API key",
                _ => "Other",
            };
            vec![
                primitives::header_line(theme, title, "esc", width),
                Line::styled(
                    format!("  {}", dialog.input),
                    Style::new().fg(theme.text.to_color()),
                ),
            ]
        }
        PendingDialog::UpdateAvailable { version } => {
            let mut lines = vec![primitives::header_line(
                theme,
                "Update Available",
                "esc",
                width,
            )];
            primitives::wrap_text(
                &format!("A new release v{version} is available. Would you like to update now?"),
                wrap_width,
                &mut lines,
                Style::new().fg(theme.text_muted.to_color()),
            );
            lines.push(buttons(
                theme,
                width,
                &["Skip", "Confirm"],
                dialog.active,
                1,
                0,
                false,
            ));
            lines
        }
        PendingDialog::ShareConsent { .. } => {
            let mut lines = vec![primitives::header_line(
                theme,
                "Share Session",
                "esc",
                width,
            )];
            primitives::wrap_text(
                "Are you sure you want to share it?",
                wrap_width,
                &mut lines,
                Style::new().fg(theme.text_muted.to_color()),
            );
            lines.push(buttons(
                theme,
                width,
                &["Cancel", "Confirm"],
                dialog.active,
                1,
                0,
                false,
            ));
            lines
        }
        PendingDialog::WorkspaceUnavailable => {
            let mut lines = vec![primitives::header_line(
                theme,
                "Workspace Unavailable",
                "esc",
                width,
            )];
            primitives::wrap_text(
                "This session is attached to a workspace that is no longer available. Would you like to restore this session into a new workspace?",
                wrap_width,
                &mut lines,
                Style::new().fg(theme.text_muted.to_color()),
            );
            lines.push(buttons(
                theme,
                width,
                &["cancel", "restore"],
                dialog.active,
                2,
                1,
                false,
            ));
            lines
        }
        PendingDialog::SessionDeleteFailed {
            session_id,
            workspace,
        } => {
            let mut lines = vec![primitives::header_line(
                theme,
                "Failed to Delete Session",
                "esc",
                width,
            )];
            let session = app
                .state
                .sync
                .session(session_id)
                .map(|session| session.title.clone())
                .unwrap_or_default();
            primitives::wrap_text(
                &format!(
                    "The session \"{session}\" could not be deleted because the workspace \"{workspace}\" is not available."
                ),
                wrap_width,
                &mut lines,
                Style::new().fg(theme.text_muted.to_color()),
            );
            primitives::wrap_text(
                "Choose how you want to recover this broken workspace session.",
                wrap_width,
                &mut lines,
                Style::new().fg(theme.text_muted.to_color()),
            );
            let options = [
                (
                    "Delete workspace",
                    "Delete the workspace and all sessions attached to it.",
                ),
                (
                    "Restore to new workspace",
                    "Try to restore this session into a new workspace.",
                ),
            ];
            for (index, (title, description)) in options.iter().enumerate() {
                if index > 0 {
                    lines.push(Line::raw(""));
                }
                let active = dialog.active == index;
                let box_width = description.chars().count() + 2;
                let bg = |style: Style| {
                    if active {
                        style.bg(theme.primary.to_color())
                    } else {
                        style
                    }
                };
                let (title_fg, description_fg) = if active {
                    (
                        theme.selected_list_item_text.to_color(),
                        theme.selected_list_item_text.to_color(),
                    )
                } else {
                    (theme.text.to_color(), theme.text_muted.to_color())
                };
                lines.push(Line::styled(" ".repeat(box_width), bg(Style::new())));
                lines.push(Line::from(vec![
                    Span::raw(" "),
                    Span::styled(
                        (*title).to_string(),
                        bg(Style::new().fg(title_fg)).add_modifier(ratatui::style::Modifier::BOLD),
                    ),
                    Span::raw(" "),
                ]));
                lines.push(Line::from(vec![
                    Span::raw(" "),
                    Span::styled(
                        (*description).to_string(),
                        bg(Style::new().fg(description_fg)),
                    ),
                    Span::raw(" "),
                ]));
                lines.push(Line::styled(" ".repeat(box_width), bg(Style::new())));
            }
            lines
        }
        PendingDialog::RetryAction {
            title,
            message,
            label,
            ..
        } => {
            let mut lines = vec![primitives::header_line(theme, title, "esc", width)];
            primitives::wrap_text(
                message,
                wrap_width,
                &mut lines,
                Style::new().fg(theme.text_muted.to_color()),
            );
            lines.push(retry_buttons(theme, width, label, dialog.active));
            lines
        }
        PendingDialog::ExportOptions => {
            let labels = [
                "Include thinking",
                "Include tool details",
                "Include assistant metadata",
                "Open without saving",
            ];
            let mut lines = vec![primitives::header_line(
                theme,
                "Export Options",
                "esc",
                width,
            )];
            lines.push(Line::styled(
                format!("  Filename: {}", dialog.input),
                Style::new().fg(theme.text.to_color()),
            ));
            for (index, label) in labels.iter().enumerate() {
                let on = dialog.checks.get(index).copied().unwrap_or(false);
                lines.push(Line::styled(
                    format!("  [{}] {label}", if on { "x" } else { " " }),
                    Style::new().fg(if dialog.active == index + 1 {
                        theme.primary
                    } else {
                        theme.text
                    }
                    .to_color()),
                ));
            }
            lines
        }
        PendingDialog::ConsoleOrg => vec![
            primitives::header_line(theme, "Switch org", "esc", width),
            Line::styled(
                "  No orgs found",
                Style::new().fg(theme.text_muted.to_color()),
            ),
        ],
        _ => {
            // The DialogSelect family.
            let title = select_title(&dialog.kind);
            let view = primitives::SelectView {
                title: title.to_string(),
                filter: true,
                options: options(app, dialog),
                actions: actions(app, dialog),
                action_focused: dialog.focused_action.is_some(),
            };
            let mut lines = vec![primitives::header_line(theme, &view.title, "esc", width)];
            if view.filter {
                lines.push(primitives::filter_line(
                    theme,
                    &dialog.select,
                    if matches!(dialog.kind, PendingDialog::Skill) {
                        "Search skills…"
                    } else {
                        "Search"
                    },
                ));
            }
            primitives::render_options(
                &dialog.select,
                &view,
                theme,
                &mut lines,
                width,
                max_visible(app),
            );
            if !view.actions.is_empty() {
                lines.push(primitives::render_actions(
                    theme,
                    &view.actions,
                    dialog.focused_action,
                ));
            }
            lines
        }
    }
}

/// The `<DialogSelect title={...}>` of each dialog.
fn select_title(kind: &PendingDialog) -> &'static str {
    match kind {
        PendingDialog::CommandPalette => "Commands",
        PendingDialog::Model => "Select model",
        PendingDialog::Agent => "Select agent",
        PendingDialog::Variant => "Select variant",
        PendingDialog::ProviderConnect => "Connect a provider",
        PendingDialog::ProviderCustomId => "Other",
        PendingDialog::ProviderAuthMethod { .. } => "Select auth method",
        PendingDialog::Mcp => "MCPs",
        PendingDialog::ThemeList => "Themes",
        PendingDialog::SessionList => "Sessions",
        PendingDialog::Skill => "Skills",
        PendingDialog::StashList => "Stash",
        PendingDialog::Timeline => "Timeline",
        PendingDialog::ForkFromTimeline => "Fork session",
        PendingDialog::Message { .. } => "Message Actions",
        PendingDialog::MoveSession => "Move session",
        PendingDialog::WorkspaceList => "Workspaces",
        PendingDialog::WorkspaceSet => "Warp",
        PendingDialog::Subagent { .. } => "Subagent Actions",
        PendingDialog::Tag => "Autocomplete",
        _ => "Dialog",
    }
}

/// The right-aligned primary-bg `ok` button
/// (`dialog-alert.tsx:42-54`, `dialog-help.tsx:33-37`).
fn ok_button(theme: &Theme, width: u16) -> Line<'static> {
    let padding = (width as usize).saturating_sub(8);
    Line::from(vec![
        Span::raw(" ".repeat(padding)),
        Span::styled(
            "   ok   ".to_string(),
            Style::new()
                .fg(theme.selected_list_item_text.to_color())
                .bg(theme.primary.to_color()),
        ),
    ])
}

/// The confirm button row — right-aligned
/// (`justifyContent="flex-end"`, `dialog-confirm.tsx:69-88`): the
/// active button gets the primary bg (`dialog-confirm.tsx:75-95`).
#[allow(clippy::too_many_arguments)]
fn buttons(
    theme: &Theme,
    width: u16,
    labels: &[&str],
    active: usize,
    pad: usize,
    gap: usize,
    bold: bool,
) -> Line<'static> {
    let selected_fg = crate::ui::theme::selected_foreground(theme, Some(theme.primary));
    let used = labels
        .iter()
        .map(|label| label.chars().count() + 2 * pad)
        .sum::<usize>()
        + gap * labels.len().saturating_sub(1);
    let mut spans = vec![Span::raw(" ".repeat((width as usize).saturating_sub(used)))];
    for (index, label) in labels.iter().enumerate() {
        if index > 0 && gap > 0 {
            spans.push(Span::raw(" ".repeat(gap)));
        }
        let mut style = Style::new().fg(if index == active {
            selected_fg
        } else {
            theme.text_muted
        }
        .to_color());
        if index == active {
            style = style.bg(theme.primary.to_color());
            if bold {
                style = style.add_modifier(ratatui::style::Modifier::BOLD);
            }
        }
        spans.push(Span::styled(
            format!("{}{label}{}", " ".repeat(pad), " ".repeat(pad)),
            style,
        ));
    }
    Line::from(spans)
}

/// The retry footer — `don't show again` left, the action label right
/// (`justifyContent="space-between"`, `dialog-retry-action.tsx:113-144`).
fn retry_buttons(theme: &Theme, width: u16, label: &str, active: usize) -> Line<'static> {
    let selected_fg = crate::ui::theme::selected_foreground(theme, Some(theme.primary));
    let button = |text: &str, active: bool, inactive_fg: crate::ui::theme::Rgba| -> Span<'static> {
        let mut style = Style::new().fg(if active { selected_fg } else { inactive_fg }.to_color());
        if active {
            style = style
                .bg(theme.primary.to_color())
                .add_modifier(ratatui::style::Modifier::BOLD);
        }
        Span::styled(format!("  {text}  "), style)
    };
    let dismiss = button("don't show again", active == 0, theme.text_muted);
    let action = button(label, active == 1, theme.text);
    let used = dismiss.content.chars().count() + action.content.chars().count();
    Line::from(vec![
        dismiss,
        Span::raw(" ".repeat((width as usize).saturating_sub(used))),
        action,
    ])
}

//! Elm-style model — `App`/`State`/`Msg`/`update()` skeleton (M8.2) plus
//! the terminal runtime surface (M8.3).
//!
//! Solid `createSignal`/`createMemo` become plain fields; Solid
//! `createStore`/`produce` become `&mut` mutations inside [`update`].
//! Async happens only in effects: `update` returns `Vec<Effect>`, the
//! runtime loop turns each into a task and feeds results back.

pub mod kv;
pub mod local;
pub mod prompt;
pub mod route;
pub mod sync;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::keymap::Keymap;
use crate::state::kv::Kv;
use crate::state::local::LocalState;
use crate::state::route::{Route, RouteStore};
use crate::state::sync::SyncState;
use crate::transport::events::{BusEvent, EventMetadata};
use crate::ui::theme::ThemeStore;

/// TUI startup args (`context/args.tsx`).
#[derive(Debug, Clone, Default)]
pub struct Args {
    pub model: Option<String>,
    pub agent: Option<String>,
    pub prompt: Option<String>,
    pub continue_: bool,
    pub session_id: Option<String>,
    pub fork: bool,
    pub auto: bool,
}

/// `permission.mode` (`context/permission.tsx`) — `"auto"` with
/// `--auto`, else `"normal"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionMode {
    Auto,
    Normal,
}

/// `toast.show` input (`ui/toast.tsx`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    pub title: Option<String>,
    pub variant: ToastVariant,
    pub message: String,
    /// `duration ?? 5000` (`ui/toast.tsx:62`).
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastVariant {
    Info,
    Success,
    Warning,
    Error,
}

/// `project.tsx` instance path — `path.get` result
/// (`context/project.tsx:13-18`). Empty strings normalize to `None`
/// (TS truthiness).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InstancePath {
    pub home: Option<String>,
    pub state: Option<String>,
    pub config: Option<String>,
    pub worktree: Option<String>,
    pub directory: Option<String>,
}

/// `project.tsx` workspace state (`context/project.tsx:16-42`).
#[derive(Debug, Clone, Default)]
pub struct WorkspaceState {
    pub current: Option<String>,
    pub list: Vec<Value>,
    pub status: std::collections::BTreeMap<String, String>,
}

/// `project.tsx` store (`context/project.tsx:22-45`) — the parts the
/// sync reducer reads.
#[derive(Debug, Clone, Default)]
pub struct ProjectState {
    pub instance_path: InstancePath,
    pub project_id: Option<String>,
    pub worktree: Option<String>,
    pub main_dir: Option<String>,
    pub workspace: WorkspaceState,
}

/// One state tree — everything `sync.tsx` + `local.tsx` + `kv.tsx` +
/// `route.tsx` own.
pub struct State {
    pub args: Args,
    pub kv: Kv,
    pub project: ProjectState,
    pub sync: SyncState,
    pub local: LocalState,
    pub route: RouteStore,
    pub permission_mode: PermissionMode,
    pub state_dir: Option<PathBuf>,
}

impl State {
    pub fn new(args: Args, state_dir: Option<&Path>) -> State {
        let permission_mode = if args.auto {
            PermissionMode::Auto
        } else {
            PermissionMode::Normal
        };
        let state_dir = state_dir.map(Path::to_path_buf);
        let (kv, local, route) = match &state_dir {
            Some(dir) => (
                Kv::load(dir),
                LocalState::new(Some(dir)),
                RouteStore::new(&args, None),
            ),
            None => (
                Kv::in_memory(),
                LocalState::new(None),
                RouteStore::new(&args, None),
            ),
        };
        State {
            args,
            kv,
            project: ProjectState::default(),
            sync: SyncState::new(),
            local,
            route,
            permission_mode,
            state_dir,
        }
    }

    /// `permission.toggle()` (`context/permission.tsx`).
    pub fn permission_toggle(&mut self) {
        self.permission_mode = match self.permission_mode {
            PermissionMode::Auto => PermissionMode::Normal,
            PermissionMode::Normal => PermissionMode::Auto,
        };
    }
}

/// One dialog on the stack — the `dialog.replace(...)` payloads. The
/// stack itself lives in [`UiState::dialogs`] (M8.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingDialog {
    /// `dialog.replace(DialogProviderList)` (`app.tsx:542-551`).
    ProviderConnect,
    /// The `Other` → custom provider id prompt
    /// (`dialog-provider.tsx:129-141`).
    ProviderCustomId,
    /// `Select auth method` (`dialog-provider.tsx:166-175`); the "api"
    /// method continues into [`PendingDialog::ProviderApiKey`].
    ProviderAuthMethod {
        provider_id: String,
    },
    /// The "api" auth-method prompt (`ApiMethod`,
    /// `dialog-provider.tsx:209-217`) — the entered key is stored through
    /// `auth.set`.
    ProviderApiKey {
        provider_id: String,
    },
    /// `DialogConfirm.show` on `installation.update-available`
    /// (`app.tsx:1033-1079`).
    UpdateAvailable {
        version: String,
    },
    CommandPalette,
    SessionList,
    Model,
    Agent,
    Mcp,
    ThemeList,
    Help,
    Status,
    Debug,
    ConsoleOrg,
    Variant,
    SessionRename {
        session_id: String,
    },
    Timeline,
    ForkFromTimeline,
    Skill,
    StashList,
    WorkspaceList,
    WorkspaceSet,
    ExportOptions,
    MoveSession,
    /// `DialogWorkspaceUnavailable` (`prompt/index.tsx:978-987`).
    WorkspaceUnavailable,
    /// The `Share Session` confirm (`session/index.tsx:489-493`).
    ShareConsent {
        session_id: String,
    },
    /// `DialogAlert.show` (`ui/dialog-alert.tsx`) — the update-complete
    /// alert exits on confirm (`app.tsx:1067-1073`).
    Alert {
        title: String,
        message: String,
        exit_on_confirm: bool,
    },
    /// `DialogSessionDeleteFailed` — session-list delete recovery
    /// (`component/dialog-session-delete-failed.tsx`).
    SessionDeleteFailed {
        session_id: String,
        workspace: String,
    },
    /// `DialogMessage` — per-message actions
    /// (`routes/session/dialog-message.tsx`).
    Message {
        session_id: String,
        message_id: String,
    },
    /// `DialogSubagent` (`routes/session/dialog-subagent.tsx`).
    Subagent {
        session_id: String,
    },
    /// `DialogTag` — the `@`-mention file autocomplete.
    Tag,
    /// `DialogRetryAction` (go-upsell; kv gating
    /// `session/index.tsx:87-113`). `kv_key` is the `dontShow` kv key
    /// written when dismissed.
    RetryAction {
        title: String,
        message: String,
        label: String,
        link: Option<String>,
        kv_key: Option<String>,
    },
}

/// Explicit transcript scroll state (spec §7.2) — the TS `scrollbox`
/// becomes an offset + a stickiness bit. `sticky` is the OpenTUI
/// `stickyScroll` + `stickyStart="bottom"` pair: a view pinned to the
/// bottom stays glued while content grows.
#[derive(Debug, Default)]
pub struct SessionScroll {
    /// `scroll.y` — the top content row rendered.
    pub y: usize,
    /// Pinned to the bottom (scrollbox end).
    pub sticky: bool,
    /// The route session at the last snap — `createEffect(on(() =>
    /// route.sessionID, toBottom))` (`session/index.tsx:1155`).
    pub session: Option<String>,
    /// The last rendered content height (clamps `y`).
    pub content_height: usize,
    pub viewport_height: usize,
    /// `scroll.getChildren()` — (messageID, top row) per message, for
    /// the message-nav commands.
    pub children: Vec<(String, usize)>,
    /// The last-rendered transcript area origin y (mouse hit-testing).
    pub area_y: u16,
    /// The last-rendered clickable part ranges (mouse hit-testing) —
    /// `BlockTool`/`InlineTool`/`ReasoningPart` `onClick`
    /// (`session/index.tsx:1822,1900,1609`).
    pub clicks: Vec<ClickTarget>,
}

/// One clickable transcript part range (`BlockTool`/`InlineTool`/`ReasoningPart`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClickTarget {
    pub id: String,
    /// Inclusive start line (into the full transcript lines).
    pub start: usize,
    /// Exclusive end line.
    pub end: usize,
}

impl SessionScroll {
    /// The effective top row — the bottom edge when sticky.
    pub fn effective_y(&self) -> usize {
        if self.sticky {
            self.content_height.saturating_sub(self.viewport_height)
        } else {
            self.y
        }
    }

    pub fn max_y(&self) -> usize {
        self.content_height.saturating_sub(self.viewport_height)
    }

    /// `scroll.scrollBy(delta)` — down at the bottom stays at the
    /// bottom; reaching the end re-sticks.
    pub fn scroll_by(&mut self, delta: i64) {
        if self.sticky && delta >= 0 {
            return;
        }
        let max = self.max_y() as i64;
        let next = (self.y as i64) + delta;
        self.y = next.clamp(0, max).max(0) as usize;
        self.sticky = self.y as i64 >= max;
    }

    /// `scroll.scrollTo(y)`.
    pub fn scroll_to(&mut self, y: usize) {
        let max = self.max_y();
        self.y = y.min(max);
        self.sticky = self.y >= max;
    }

    /// `scroll.scrollTo(scroll.scrollHeight)` — the `toBottom()`
    /// effect and the post-mount `scrollBy(100_000)`.
    pub fn snap_to_bottom(&mut self) {
        self.sticky = true;
    }
}

/// BlockTool/InlineTool/ReasoningPart `onClick`
/// (`session/index.tsx:1822,1900,1609`): toggle the expansion of the
/// part whose rendered lines contain the clicked screen row. The
/// non-applicable set toggle is a rendering no-op.
fn transcript_click(app: &mut App, row: u16) {
    let scroll = &app.ui.session_scroll;
    if (row as usize) < scroll.area_y as usize {
        return;
    }
    let line = (row - scroll.area_y) as usize + scroll.effective_y();
    let Some(id) = scroll
        .clicks
        .iter()
        .find(|target| target.start <= line && line < target.end)
        .map(|target| target.id.clone())
    else {
        return;
    };
    toggle_set(&mut app.ui.expanded, &id);
    toggle_set(&mut app.ui.expanded_errors, &id);
}

fn toggle_set(set: &mut HashSet<String>, id: &str) {
    if !set.remove(id) {
        set.insert(id.to_string());
    }
}

/// The dedup + bookkeeping sets of the `internal:notifications` plugin
/// (`feature-plugins/system/notifications.ts:44-52`) — session ids
/// that were active since the last idle, ids that errored, and the
/// seen question/permission request ids.
#[derive(Debug, Default)]
pub struct AttentionSets {
    pub active: HashSet<String>,
    pub errored: HashSet<String>,
    pub questions: HashSet<String>,
    pub permissions: HashSet<String>,
}

/// Keymap modes and focus (M8.4), dialog stack (TODO(M8.7)) plus
/// the app-shell bookkeeping of M8.3.
#[derive(Debug, Default)]
pub struct UiState {
    /// One current toast at a time (`ui/toast.tsx:66-69`).
    pub toasts: Vec<Toast>,
    /// `toast.show`'s `setTimeout` deadline (`ui/toast.tsx:70-74`).
    pub toast_deadline_ms: Option<u64>,
    pub attention: AttentionSets,
    pub theme: ThemeStore,
    /// `StartupLoading` state machine (`component/startup-loading.tsx`).
    pub startup_loading: StartupLoading,
    /// The home prompt's placeholder roll (`prompt/index.tsx:289-308`).
    pub home_placeholder: usize,
    /// Spinner clock (ms since launch) — drives the 40 ms redraw + the
    /// spinner frames.
    pub tick_ms: u64,
    /// `continued` (`app.tsx:503-524`).
    pub continued: bool,
    /// `forked` (`app.tsx:529-540`).
    pub forked: bool,
    /// `wasEmpty` of the provider-empty transition (`app.tsx:542-551`).
    pub provider_empty: bool,
    /// The dialog stack (`ui/dialog.tsx` — M8.7).
    pub dialogs: crate::ui::dialogs::DialogStack,
    /// The `modeStack.push("modal")` token while a dialog is open
    /// (`ui/dialog.tsx:81-85`).
    pub modal_token: Option<u64>,
    /// The `modeStack.push("question")` token while a question is
    /// visible (`question.tsx:128-131`).
    pub question_mode_token: Option<u64>,
    /// The permission prompt state machine (M8.7).
    pub permission: crate::ui::session::permission::PermissionState,
    /// The question prompt state machine (M8.7).
    pub question: crate::ui::session::question::QuestionState,
    /// The move-session dialog's `project.directories` fetch
    /// (`dialog-move-session.tsx:60-78`).
    pub move_directories: Option<Vec<serde_json::Value>>,
    /// Exit was requested; `exit_reason` becomes stderr + exit code 1.
    pub exit: bool,
    pub exit_reason: Option<String>,
    /// The prompt textarea's focus — the managed-textarea layer is
    /// enabled while focused (`keymap.tsx:229-232`).
    pub prompt_focused: bool,
    /// The prompt editor (`component/prompt/index.tsx`) — M8.6.
    pub prompt: crate::state::prompt::PromptState,
    /// `conceal` signal (`session/index.tsx:258`) — per-session, not
    /// persisted.
    pub conceal: bool,
    /// `sidebarOpen` (`session/index.tsx:260`).
    pub sidebar_open: bool,
    /// Terminal width — the `>120` sidebar boundary. Updated on
    /// `Msg::Resize`.
    pub terminal_width: u16,
    /// Terminal height — the dialog backdrop geometry
    /// (`paddingTop={height / 4}`, `dialog.tsx:45`).
    pub terminal_height: u16,
    /// `store.interrupt` (`prompt/index.tsx:396-421`).
    pub interrupt: u32,
    pub interrupt_reset_at: Option<u64>,
    /// `docs.open` etc. print their URL instead of opening a browser
    /// (spec §6 N6) — collected by the runtime after the loop.
    pub opened_urls: Vec<String>,
    /// The transcript scrollbox (spec §7.2).
    pub session_scroll: SessionScroll,
    /// `session_mounted` — the sessionID whose mount effect already
    /// fired (`session/index.tsx:286-324`).
    pub session_mounted: Option<String>,
    /// `lastSwitch` of the plan_enter/plan_exit handler
    /// (`session/index.tsx:326-341`).
    pub plan_switch_part: Option<String>,
    /// Expanded tool outputs / reasoning bodies / shell blocks — per
    /// part id (the TS per-component signals collapse into a set).
    pub expanded: HashSet<String>,
    /// Expanded error rows (`errorExpanded` per part).
    pub expanded_errors: HashSet<String>,
    /// The footer's `welcome` state machine (`routes/session/footer.tsx`).
    pub footer_welcome: bool,
    pub footer_flip_at: Option<u64>,
    /// `ready` (`app.tsx:409-421`) — the plugin host finished starting;
    /// the port's analog is the first bootstrap reaching `Complete`.
    pub startup_ready: bool,
    /// `tipOffset` (`feature-plugins/home/tips-view.tsx:132`) — the
    /// `Math.random()` roll picked once at mount.
    pub home_tip: usize,
}

/// `StartupLoading` timers (`component/startup-loading.tsx:26-63`) as a
/// pure clock-driven machine: the 500 ms wait before showing, the 3 s
/// hold after `ready` flips while shown.
#[derive(Debug, Default)]
pub struct StartupLoading {
    show: bool,
    wait: Option<u64>,
    hold: Option<u64>,
    stamp: u64,
    ready: bool,
}

impl StartupLoading {
    /// `ready: true` after 500 ms shows "Finishing startup…"; a shown
    /// overlay holds for 3 s once ready.
    pub fn transition(&mut self, ready: bool, now_ms: u64) {
        self.ready = ready;
        if ready {
            self.wait = None;
            if !self.show || self.hold.is_some() {
                return;
            }
            if now_ms.saturating_sub(self.stamp) >= 3000 {
                self.show = false;
                return;
            }
            self.hold = Some(self.stamp + 3000);
        } else {
            self.hold = None;
            if self.show || self.wait.is_some() {
                return;
            }
            self.wait = Some(now_ms + 500);
        }
    }

    /// Fire the expired `setTimeout` callbacks.
    pub fn poll(&mut self, now_ms: u64) {
        if let Some(deadline) = self.wait {
            if now_ms >= deadline {
                self.wait = None;
                self.stamp = now_ms;
                self.show = true;
            }
        }
        if let Some(deadline) = self.hold {
            if now_ms >= deadline {
                self.hold = None;
                self.show = false;
            }
        }
    }

    pub fn visible(&self) -> bool {
        self.show
    }

    pub fn text(&self) -> &'static str {
        if self.ready {
            "Finishing startup…"
        } else {
            "Loading plugins…"
        }
    }
}

/// The update messages.
pub enum Msg {
    /// One coalesced bus batch — a whole flush applies in a single
    /// `update` pass (the TS `batch()` equivalent, `sdk.tsx:60-66`).
    Bus(Vec<BusEvent>),
    Key(crossterm::event::KeyEvent),
    Mouse(crossterm::event::MouseEvent),
    Resize(u16, u16),
    /// Bracketed paste (`onPaste`, `prompt/index.tsx:1396-1420`).
    Paste(String),
    /// 40 ms frame tick (`app.tsx:196` `targetFps: 60`).
    Tick(std::time::Duration),
}

/// Async work fired by [`update`] — the runtime loop executes each
/// against the server seam.
#[derive(Debug, PartialEq)]
pub enum Effect {
    /// `bootstrap()` (`sync.tsx:451`) — `server.instance.disposed`.
    Bootstrap { fatal: bool },
    /// Auto-reply to a permission under `"auto"` mode
    /// (`sync.tsx:196-207`).
    PermissionAutoReply {
        request_id: String,
        reply: alforria_schema::permission_v1::PermissionV1Reply,
        metadata: EventMetadata,
    },
    /// `lsp.updated` refetches `lsp.status` (`sync.tsx:433-437`).
    LspStatusRefetch { workspace: Option<String> },
    /// `session.fork` + navigate on success (`app.tsx:512-519`).
    SessionFork { session_id: String, navigate: bool },
    /// `terminal.suspend` — leave raw mode, `SIGTSTP`, resume on
    /// `SIGCONT` (`app.tsx:870-879`).
    SuspendTerminal,
    /// `session.share` (`session/index.tsx:488-512`).
    SessionShare { session_id: String },
    /// `session.unshare` (`session/index.tsx:559-585`).
    SessionUnshare { session_id: String },
    /// `session.summarize` (`session/index.tsx:561-572`).
    SessionSummarize {
        session_id: String,
        provider_id: String,
        model_id: String,
    },
    /// `session.abort` (undo + interrupt paths).
    SessionAbort { session_id: String },
    /// `session.revert` (`session/index.tsx:610-644`).
    SessionRevert {
        session_id: String,
        message_id: String,
    },
    /// `session.unrevert` (`session/index.tsx:646-671`).
    SessionUnrevert { session_id: String },
    /// `sync.session.refresh()` — re-list sessions (`local.tsx`).
    SessionRefresh,
    /// `experimental.session.background` (`session/index.tsx:1024-1030`).
    SessionBackground { session_id: String },
    /// `session.copy` — clipboard write of `formatTranscript(...)`
    /// (TODO(M8.8): the transcript formatter).
    SessionCopyTranscript {
        thinking: bool,
        tool_details: bool,
        assistant_metadata: bool,
    },
    /// Clipboard write with optional toasts (`util/clipboard.ts`).
    ClipboardWrite {
        text: String,
        success: Option<Toast>,
        failure: Option<Toast>,
    },
    /// §6 N6: `docs.open` prints the URL instead of opening a browser.
    OpenUrl { url: String },
    /// `session.get` on mount + 404 toast/home + workspace re-bootstrap
    /// + hydrate + snap-to-bottom (`session/index.tsx:286-324`).
    SessionMount {
        session_id: String,
        previous_workspace: Option<String>,
    },
    /// `sync.session.sync(sessionID)` — the `task` tool hydrates its
    /// child session on mount (`session/index.tsx:2221-2224`).
    SessionHydrate { session_id: String },
    /// `prompt.paste` — read the clipboard, run the paste pipeline
    /// (`prompt/index.tsx:374-391`).
    PromptPaste,
    /// `prompt.editor` — suspend the terminal, open `$EDITOR` seeded
    /// with `value`, apply the edited content
    /// (`prompt/index.tsx:424-514`).
    OpenPromptEditor { value: String },
    /// The submit pipeline dispatch (`prompt/index.tsx:947-1147`).
    PromptSubmit {
        payload: Box<crate::state::prompt::SubmitPayload>,
    },
    /// `permission.reply` from the UI prompt (`permission.tsx:165-174`).
    PermissionReply {
        request_id: String,
        reply: alforria_schema::permission_v1::PermissionV1Reply,
        message: Option<String>,
    },
    /// `question.reply` (`question.tsx:48-55`).
    QuestionReply {
        request_id: String,
        answers: Vec<alforria_schema::question_v1::QuestionV1Answer>,
    },
    /// `question.reject` (`question.tsx:57-62`).
    QuestionReject { request_id: String },
    /// `session.update` title (`DialogSessionRename`).
    SessionRename { session_id: String, title: String },
    /// `auth.set` (`dialog-provider.tsx:405-412`) — store the entered API
    /// key for the provider, then re-bootstrap.
    AuthSet { provider_id: String, key: String },
    /// `session.delete` (`dialog-session-list.tsx:248-262`).
    SessionDelete { session_id: String },
    /// `local.mcp.toggle(name)` + `mcp.status` refresh
    /// (`dialog-mcp.tsx:49-66`).
    McpToggle { name: String },
    /// `global.upgrade` (`app.tsx:1051`).
    GlobalUpgrade { target: String },
    /// `experimental.moveSession` (`component/prompt/move.tsx`).
    SessionMove {
        session_id: String,
        directory: String,
    },
    /// `project.directories` fetch for the move-session dialog
    /// (`dialog-move-session.tsx:60-78`).
    ProjectDirectories { project_id: String },
    /// `session.fork` (+ optional `messageID`) with a prompt seeded
    /// from the forked message parts (`dialog-fork-from-timeline.tsx`).
    SessionForkFromMessage {
        session_id: String,
        message_id: Option<String>,
        seed_prompt: bool,
    },
    /// The export-options confirm — the file write + `$EDITOR` open
    /// (`session/index.tsx:946-1020`).
    SessionExport {
        filename: String,
        thinking: bool,
        tool_details: bool,
        assistant_metadata: bool,
        open_without_saving: bool,
    },
    /// `attention.notify(...)` (`feature-plugins/system/notifications.ts`).
    Attention {
        title: Option<String>,
        message: String,
        /// `notification: false` for subagent sessions — bell only.
        notification: bool,
        bell: bool,
    },
}

pub struct App {
    pub state: State,
    pub ui: UiState,
    pub config: crate::config::TuiConfig,
    /// The resolved keymap (`config/index.tsx:95-111`) — M8.4.
    pub keymap: Keymap,
    /// The attention seam (spec §2.3) — terminal BEL/OSC 9 in
    /// production, a recorder in tests.
    pub attention: std::sync::Arc<dyn crate::attention::Attention>,
    /// Bumped on every update — the redraw signal (§2.1).
    pub version: u64,
}

impl App {
    /// The shared braille spinner (`component/spinner.tsx:10-24`),
    /// driven by the render tick; `⋯` when animations are off.
    pub fn session_spinner(&self) -> &'static str {
        let animations = self
            .state
            .kv
            .get_bool(crate::state::kv::keys::ANIMATIONS_ENABLED, true);
        if animations {
            let index = (self.ui.tick_ms / 80) as usize % crate::ui::SPINNER_FRAMES.len();
            crate::ui::SPINNER_FRAMES[index]
        } else {
            "⋯"
        }
    }

    /// The `creatingDots()` cycle (`move.tsx:190-196`): 1..3 dots,
    /// one per second.
    pub fn submitting_dots(&self) -> String {
        ".".repeat((self.ui.tick_ms / 1000) as usize % 3 + 1)
    }

    pub fn new(config: crate::config::TuiConfig, args: Args, state_dir: Option<&Path>) -> App {
        let keymap = Keymap::resolve(&config);
        let mut state = State::new(args, state_dir);
        let mut ui = UiState {
            prompt: crate::state::prompt::PromptState::new(state_dir),
            theme: ThemeStore::init(&mut state.kv, config.theme.as_deref()),
            conceal: true,
            prompt_focused: true,
            terminal_width: 80,
            ..UiState::default()
        };
        ui.home_tip = rand::random::<usize>();
        App {
            state,
            ui,
            config,
            keymap,
            attention: crate::attention::terminal_attention(),
            version: 0,
        }
    }

    /// `toast.show` (`ui/toast.tsx:60-74`): one current toast — new shows
    /// replace; the duration is the dismissal `setTimeout`.
    pub fn show_toast(&mut self, toast: Toast) {
        let deadline = self.ui.tick_ms.saturating_add(toast.duration_ms);
        self.ui.toasts.clear();
        self.ui.toasts.push(toast);
        self.ui.toast_deadline_ms = Some(deadline);
    }

    /// `exit(reason?)` (`app.tsx:248-252`).
    pub fn exit(&mut self, reason: Option<String>) {
        self.ui.exit = true;
        self.ui.exit_reason = reason;
    }
}

/// The update function — pure state transitions; effects carry all
/// async work.
pub fn update(app: &mut App, msg: Msg) -> Vec<Effect> {
    let mut effects = Vec::new();
    match msg {
        Msg::Bus(events) => {
            for event in events {
                local::on_bus_event(&mut app.state, &event.event);
                effects.extend(sync::apply_event(&mut app.state, event.clone()));
                effects.extend(crate::app::on_bus_event(app, event.clone()));
                effects.extend(crate::attention::on_bus_event(app, &event.event));
            }
        }
        Msg::Key(key) => {
            // Dialogs take the keyboard while the stack is open (the
            // pushed `modal` mode, `ui/dialog.tsx:105-137`).
            if !app.ui.dialogs.is_empty() {
                effects.extend(crate::ui::dialogs::handle_key(app, &key));
            } else if let Some(handled) = crate::ui::session::permission::handle_key(app, &key) {
                effects.extend(handled);
            } else if let Some(handled) = crate::ui::session::question::handle_key(app, &key) {
                effects.extend(handled);
            } else {
                let context = crate::keymap::DispatchContext {
                    route_is_session: matches!(app.state.route.data, Route::Session { .. }),
                    prompt_focused: app.ui.prompt_focused,
                    foreground_tasks: crate::command::foreground_tasks(app) > 0,
                };
                let commands = app.keymap.dispatch(&context, &key, app.ui.tick_ms);
                // While the autocomplete is open it takes the keyboard
                // (§5.2 escape priority).
                let (mut handled, autocomplete_effects) = prompt::autocomplete_key(app, &key);
                effects.extend(autocomplete_effects);
                // TS dispatch evaluates every layer's `enabled()` gate before
                // running handlers, then fires ALL enabled bindings
                // (`keymap.tsx:229-232` + `app.tsx:975-985`) — snapshot the
                // gates first so a handler's side effects can't flip a later
                // gate (ctrl+c clears AND `app.exit` stays disabled).
                let enabled: Vec<bool> = commands
                    .iter()
                    .map(|name| {
                        if name.starts_with("input.") {
                            app.ui.prompt_focused && app.ui.dialogs.is_empty()
                        } else {
                            crate::command::is_enabled(app, name)
                        }
                    })
                    .collect();
                for (name, enabled) in commands.iter().zip(enabled) {
                    if !enabled {
                        continue;
                    }
                    if prompt::handle_command(app, name) {
                        handled = true;
                        continue;
                    }
                    effects.extend(crate::command::run(app, name));
                    handled = true;
                }
                if !handled && commands.is_empty() {
                    prompt::text_input(app, &key);
                }
            }
        }
        Msg::Mouse(mouse) => match mouse.kind {
            // TODO(M8.5), hover handling; the transcript wheel uses the
            // config `scroll_speed`. Dialogs get the wheel first —
            // the topmost dialog's scrollbox scrolls instead of the
            // transcript (`dialog-select.tsx:610-616`).
            crossterm::event::MouseEventKind::ScrollUp if !app.ui.dialogs.is_empty() => {
                crate::ui::dialogs::wheel_scroll(app, -1);
            }
            crossterm::event::MouseEventKind::ScrollDown if !app.ui.dialogs.is_empty() => {
                crate::ui::dialogs::wheel_scroll(app, 1);
            }
            crossterm::event::MouseEventKind::ScrollUp => {
                app.ui.session_scroll.scroll_by(-(scroll_speed(app) as i64));
            }
            crossterm::event::MouseEventKind::ScrollDown => {
                app.ui.session_scroll.scroll_by(scroll_speed(app) as i64);
            }
            // Dialog option rows are mouse-interactive
            // (`dialog-select.tsx:640-676`): hover and press move the
            // selection to the row under the pointer; release activates.
            crossterm::event::MouseEventKind::Moved if !app.ui.dialogs.is_empty() => {
                if let Some(index) = crate::ui::dialogs::option_row(app, mouse.column, mouse.row) {
                    crate::ui::dialogs::mouse_move_to(app, index);
                }
            }
            // The bottom bars are mouse-interactive too
            // (`permission.tsx:676-693`, `question.tsx:296-408`):
            // hover/press moves the selection to the row under the
            // pointer.
            crossterm::event::MouseEventKind::Moved if app.ui.dialogs.is_empty() => {
                crate::ui::session::permission::mouse_over(app, mouse.column, mouse.row);
                crate::ui::session::question::mouse_over(app, mouse.column, mouse.row);
            }
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left)
                if app.ui.dialogs.is_empty() =>
            {
                crate::ui::session::question::mouse_over(app, mouse.column, mouse.row);
            }
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left)
                if !app.ui.dialogs.is_empty() =>
            {
                if let Some(index) = crate::ui::dialogs::option_row(app, mouse.column, mouse.row) {
                    crate::ui::dialogs::mouse_move_to(app, index);
                }
            }
            crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left)
                if !app.ui.dialogs.is_empty()
                    && crate::ui::dialogs::option_row(app, mouse.column, mouse.row).is_some() =>
            {
                let index = crate::ui::dialogs::option_row(app, mouse.column, mouse.row)
                    .expect("checked above");
                effects.extend(crate::ui::dialogs::mouse_submit(app, index));
            }
            // Backdrop click-through (`dialog.tsx:30-38`): a release
            // outside the frame pops the top dialog. The port has no
            // mouse text selection (recorded divergence §7.8), so a
            // selection never blocks the close.
            crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left)
                if !app.ui.dialogs.is_empty()
                    && !crate::ui::dialogs::hit_test(app, mouse.column, mouse.row) =>
            {
                crate::ui::dialogs::pop(app);
            }
            // BlockTool/InlineTool/ReasoningPart onClick
            // (`session/index.tsx:1822,1900,1609`): toggle the expansion
            // of the part whose rendered lines contain the click.
            crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left)
                if app.ui.dialogs.is_empty() =>
            {
                if let Some(mut permission) =
                    crate::ui::session::permission::mouse_select(app, mouse.column, mouse.row)
                {
                    effects.append(&mut permission);
                } else if let Some(mut question) =
                    crate::ui::session::question::mouse_select(app, mouse.column, mouse.row)
                {
                    effects.append(&mut question);
                } else {
                    transcript_click(app, mouse.row);
                }
            }
            _ => {}
        },
        Msg::Resize(columns, rows) => {
            // Layout is recomputed on every draw; the sidebar boundary
            // needs the width, the dialog frames the height.
            app.ui.terminal_width = columns;
            app.ui.terminal_height = rows;
        }
        Msg::Paste(text) => {
            // Bracketed-paste normalization happens at the boundary
            // (`prompt/index.tsx:1402-1405`); an empty paste falls back
            // to the clipboard-paste command (the win32 image quirk).
            let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
            if normalized.trim().is_empty() {
                effects.extend(crate::command::run(app, "prompt.paste"));
            } else {
                prompt::paste_input_text(app, &normalized);
            }
        }
        Msg::Tick(elapsed) => {
            let now_ms = elapsed.as_millis() as u64;
            app.ui.tick_ms = now_ms;
            // The toast dismissal `setTimeout` (`ui/toast.tsx:70-74`).
            if let Some(deadline) = app.ui.toast_deadline_ms {
                if now_ms >= deadline {
                    app.ui.toasts.clear();
                    app.ui.toast_deadline_ms = None;
                }
            }
            app.ui.startup_loading.poll(now_ms);
            if app.state.sync.status == Some(crate::state::sync::SyncStatus::Complete) {
                app.ui.startup_ready = true;
            }
            app.ui
                .startup_loading
                .transition(app.ui.startup_ready, now_ms);
            // The timed leader's `setTimeout` (`registerTimedLeader`).
            app.keymap.poll(now_ms);
            // `setTimeout(() => setStore("interrupt", 0), 5000)`.
            if let Some(reset_at) = app.ui.interrupt_reset_at {
                if now_ms >= reset_at {
                    app.ui.interrupt = 0;
                    app.ui.interrupt_reset_at = None;
                }
            }
            // The footer's `welcome` rotation (`routes/session/footer.tsx:31-45`).
            if !connected(app) {
                if let Some(flip_at) = app.ui.footer_flip_at {
                    if now_ms >= flip_at {
                        app.ui.footer_welcome = !app.ui.footer_welcome;
                        app.ui.footer_flip_at =
                            Some(now_ms + if app.ui.footer_welcome { 5000 } else { 10000 });
                    }
                } else {
                    app.ui.footer_flip_at = Some(now_ms + 10000);
                }
            } else {
                app.ui.footer_welcome = false;
                app.ui.footer_flip_at = None;
            }
        }
    }
    app.version += 1;
    effects.extend(crate::app::post_update(app));
    effects
}

/// `useConnected()` (`component/use-connected.tsx`): some provider
/// other than `opencode`, or an `opencode` model with a nonzero input
/// cost.
pub fn connected(app: &App) -> bool {
    app.state.sync.provider.iter().any(|provider| {
        provider.get("id").and_then(Value::as_str) != Some("opencode")
            || provider
                .get("models")
                .and_then(Value::as_object)
                .map(|models| {
                    models.values().any(|model| {
                        model
                            .get("cost")
                            .and_then(|cost| cost.get("input"))
                            .and_then(Value::as_f64)
                            .map(|input| input != 0.0)
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false)
    })
}

/// `getScrollAcceleration(tuiConfig)` (`util/scroll.ts`): the default
/// is a fixed `scroll_speed` of 3. The macOS acceleration curve lives
/// in `@opentui/core` and cannot be ported verbatim — when enabled the
/// port keeps the fixed speed (recorded as a documented divergence).
fn scroll_speed(app: &App) -> u32 {
    app.config.scroll_speed.round().max(1.0) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_mode_from_args() {
        let state = State::new(
            Args {
                auto: true,
                ..Args::default()
            },
            None,
        );
        assert_eq!(state.permission_mode, PermissionMode::Auto);
        let state = State::new(Args::default(), None);
        assert_eq!(state.permission_mode, PermissionMode::Normal);
    }

    #[test]
    fn update_bumps_version_and_routes_local_prunes() {
        use alforria_schema::event_manifest::Event;
        use alforria_schema::session_v1::SessionDeletedData;

        let mut app = App::new(crate::config::TuiConfig::default(), Args::default(), None);
        let deleted = BusEvent {
            event: Event::SessionDeleted(SessionDeletedData {
                session_id: "ses_1".into(),
                info: session_info("ses_1"),
            }),
            metadata: Default::default(),
        };
        app.state.local.session_toggle_pin("ses_1");
        let version = app.version;
        let effects = update(&mut app, Msg::Bus(vec![deleted]));
        assert!(effects.is_empty());
        assert_eq!(app.version, version + 1);
        assert!(!app.state.local.session_is_pinned("ses_1"));
    }

    #[test]
    fn exit_keys_request_exit() {
        // The prompt starts focused but empty — the `app.exit` gate
        // (`app.tsx:977-985`) keeps ctrl+c/ctrl+d exiting.
        let mut app = App::new(crate::config::TuiConfig::default(), Args::default(), None);
        update(
            &mut app,
            Msg::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('c'),
                crossterm::event::KeyModifiers::CONTROL,
            )),
        );
        assert!(app.ui.exit);

        let mut app = App::new(crate::config::TuiConfig::default(), Args::default(), None);
        update(
            &mut app,
            Msg::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('d'),
                crossterm::event::KeyModifiers::CONTROL,
            )),
        );
        assert!(app.ui.exit);

        // A focused non-empty prompt: ctrl+c clears the input instead
        // (§5.2 — the app.exit gate).
        let mut app = App::new(crate::config::TuiConfig::default(), Args::default(), None);
        app.ui.prompt.textarea.set_text("typing");
        update(
            &mut app,
            Msg::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('c'),
                crossterm::event::KeyModifiers::CONTROL,
            )),
        );
        assert!(!app.ui.exit);
        assert_eq!(app.ui.prompt.input(), "");

        let mut app = App::new(crate::config::TuiConfig::default(), Args::default(), None);
        update(
            &mut app,
            Msg::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('c'),
                crossterm::event::KeyModifiers::NONE,
            )),
        );
        assert!(!app.ui.exit);
    }

    #[test]
    fn transcript_click_toggles_part_expansion() {
        let mut app = App::new(crate::config::TuiConfig::default(), Args::default(), None);
        app.ui.session_scroll.area_y = 0;
        app.ui.session_scroll.y = 0;
        app.ui.session_scroll.sticky = false;
        app.ui.session_scroll.clicks = vec![ClickTarget {
            id: "prt_1".to_string(),
            start: 2,
            end: 5,
        }];
        let up = || {
            Msg::Mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
                column: 0,
                row: 3,
                modifiers: crossterm::event::KeyModifiers::NONE,
            })
        };
        update(&mut app, up());
        assert!(app.ui.expanded.contains("prt_1"));
        assert!(app.ui.expanded_errors.contains("prt_1"));
        update(&mut app, up());
        assert!(!app.ui.expanded.contains("prt_1"));
        // A miss (outside every recorded part range) toggles nothing.
        update(
            &mut app,
            Msg::Mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
                column: 0,
                row: 10,
                modifiers: crossterm::event::KeyModifiers::NONE,
            }),
        );
        assert!(!app.ui.expanded.contains("prt_1"));
    }

    #[test]
    fn startup_loading_takes_its_timers() {
        let mut startup = StartupLoading::default();
        startup.transition(false, 0);
        assert!(!startup.visible());
        startup.poll(500);
        assert!(startup.visible());
        startup.transition(true, 600);
        assert!(startup.visible());
        startup.poll(3600);
        assert!(!startup.visible());
    }

    #[test]
    fn startup_loading_waits_for_ready_hold() {
        let mut startup = StartupLoading::default();
        startup.transition(false, 0);
        startup.poll(500);
        assert!(startup.visible());
        // ready within 3 s of the show — held.
        startup.transition(true, 2000);
        assert!(startup.visible());
        // no double-hold
        startup.transition(true, 2100);
        startup.poll(6000);
        assert!(!startup.visible());

        // shown just before ready: hide immediately after 3 s window
        let mut startup = StartupLoading::default();
        startup.transition(false, 0);
        startup.poll(4000);
        assert!(startup.visible());
        startup.transition(true, 4600);
        assert!(startup.visible());
        startup.poll(8000);
        assert!(!startup.visible());
    }

    #[test]
    fn tick_wires_startup_ready_to_first_bootstrap() {
        let mut app = App::new(crate::config::TuiConfig::default(), Args::default(), None);
        update(&mut app, Msg::Tick(std::time::Duration::from_millis(0)));
        update(&mut app, Msg::Tick(std::time::Duration::from_millis(600)));
        assert!(!app.ui.startup_ready);
        assert!(app.ui.startup_loading.visible(), "still loading");

        app.state.sync.status = Some(crate::state::sync::SyncStatus::Complete);
        update(&mut app, Msg::Tick(std::time::Duration::from_millis(700)));
        assert!(app.ui.startup_ready);
    }

    #[test]
    fn connected_checks_every_model() {
        let mut app = App::new(crate::config::TuiConfig::default(), Args::default(), None);
        app.state.sync.provider = vec![serde_json::json!({
            "id": "opencode",
            "models": {
                "a": { "cost": { "input": 0 } },
                "b": { "cost": { "input": 3 } },
            },
        })];
        assert!(connected(&app), "any nonzero model counts");
        app.state.sync.provider = vec![serde_json::json!({
            "id": "opencode",
            "models": {
                "a": { "cost": { "input": 0 } },
                "b": { "cost": { "input": 0 } },
            },
        })];
        assert!(!connected(&app));
    }

    fn session_info(id: &str) -> alforria_schema::session_v1::V1SessionInfo {
        alforria_schema::session_v1::V1SessionInfo {
            id: id.into(),
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
}

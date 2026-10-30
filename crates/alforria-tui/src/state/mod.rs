//! Elm-style model — `App`/`State`/`Msg`/`update()` skeleton (M8.2) plus
//! the terminal runtime surface (M8.3).
//!
//! Solid `createSignal`/`createMemo` become plain fields; Solid
//! `createStore`/`produce` become `&mut` mutations inside [`update`].
//! Async happens only in effects: `update` returns `Vec<Effect>`, the
//! runtime loop turns each into a task and feeds results back.

pub mod frecency;
pub mod kv;
pub mod local;
pub mod prompt;
pub mod route;
#[cfg(test)]
mod routing_tests;
pub mod selection;
pub mod session_search;
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
    /// method continues into [`PendingDialog::ProviderApiKey`], an
    /// "oauth" method into [`PendingDialog::ProviderOauth`].
    ProviderAuthMethod {
        provider_id: String,
    },
    /// An OAuth sign-in in progress — `AutoMethod` / `CodeMethod`
    /// (`dialog-provider.tsx:227-349`). `method: "auto"` shows the URL
    /// while the no-code callback waits, and the input takes a pasted
    /// redirect (a browser on another machine can't reach the server's
    /// loopback); `method: "code"` is the paste input alone.
    ProviderOauth {
        provider_id: String,
        /// The auth-method index (`callback({ method })`).
        method: usize,
        /// This sign-in's id — results of an older flow are dropped.
        flow: u64,
        title: String,
        url: String,
        instructions: String,
        /// `authorization.method === "auto"`.
        auto: bool,
        /// The last pasted code was rejected (`Invalid code`).
        rejected: bool,
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
    /// A workflow's arguments, before it starts. `name` is the saved workflow,
    /// or the name for `script` when running one that is no longer saved.
    WorkflowRun {
        name: Option<String>,
        script: Option<String>,
        title: Option<String>,
        args: String,
    },
    /// A new workflow's name; the editor opens next.
    WorkflowNew,
    WorkflowStop {
        run_id: String,
        name: String,
    },
    WorkflowDelete {
        name: String,
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
    /// A `task` part opens its child session (`session/index.tsx:2215-2328`).
    pub child_session: Option<String>,
    /// A `workflow` part opens its run on the workflows screen.
    pub workflow_run: Option<String>,
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
fn transcript_click(app: &mut App, row: u16, effects: &mut Vec<Effect>) {
    let scroll = &app.ui.session_scroll;
    if (row as usize) < scroll.area_y as usize {
        return;
    }
    let line = (row - scroll.area_y) as usize + scroll.effective_y();
    let Some(target) = scroll
        .clicks
        .iter()
        .find(|target| target.start <= line && line < target.end)
        .cloned()
    else {
        return;
    };
    if let Some(child) = target.child_session {
        crate::command::enter_child(app, &child);
        return;
    }
    if let Some(run) = target.workflow_run {
        effects.extend(crate::ui::workflows::open(app, Some(&run)));
        return;
    }
    let id = target.id;
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
    /// Mouse text selection (copy-on-select).
    pub selection: selection::Selection,
    /// `OPENCODE_EXPERIMENTAL_DISABLE_COPY_ON_SELECT`: a selection stays
    /// until a key copies or dismisses it.
    pub copy_on_select_disabled: bool,
    /// The session list's server search (debounce and results).
    pub session_search: session_search::SessionSearch,
    /// The wheel's acceleration state (`scroll_acceleration.enabled`).
    pub scroll_accel: ScrollAccel,
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
    pub workflows: crate::ui::workflows::WorkflowsState,
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
    /// The OAuth sign-in still waiting for its result — the
    /// [`PendingDialog::ProviderOauth`] `flow`; `None` once it settled.
    pub oauth_flow: Option<u64>,
    /// The last allocated OAuth flow id.
    pub oauth_seq: u64,
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

    /// The next show/hide deadline, if one is armed.
    pub fn deadline(&self) -> Option<u64> {
        match (self.wait, self.hold) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
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
    /// SIGHUP: the terminal is gone — exit (`app.tsx:231-235`).
    Hangup,
}

/// Async work fired by [`update`] — the runtime loop executes each
/// against the server seam.
#[derive(Debug, PartialEq)]
pub enum Effect {
    /// `bootstrap()` (`sync.tsx:451`) — `server.instance.disposed`.
    Bootstrap {
        fatal: bool,
    },
    /// List the pending permissions and questions (`GET /permission`,
    /// `GET /question`) — at startup and on every stream (re)connect — and
    /// reconcile them with the live events
    /// ([`sync::prompts_fetched`](crate::state::sync::prompts_fetched)).
    PromptsRefresh,
    /// Auto-reply to a permission under `"auto"` mode
    /// (`sync.tsx:196-207`).
    PermissionAutoReply {
        request_id: String,
        reply: alforria_schema::permission_v1::PermissionV1Reply,
        metadata: EventMetadata,
    },
    /// `lsp.updated` refetches `lsp.status` (`sync.tsx:433-437`).
    LspStatusRefetch {
        workspace: Option<String>,
    },
    /// `session.fork` + navigate on success (`app.tsx:512-519`).
    SessionFork {
        session_id: String,
        navigate: bool,
    },
    /// `terminal.suspend` — leave raw mode, `SIGTSTP`, resume on
    /// `SIGCONT` (`app.tsx:870-879`).
    SuspendTerminal,
    /// `session.share` (`session/index.tsx:488-512`).
    SessionShare {
        session_id: String,
    },
    /// `session.unshare` (`session/index.tsx:559-585`).
    SessionUnshare {
        session_id: String,
    },
    /// `session.summarize` (`session/index.tsx:561-572`).
    SessionSummarize {
        session_id: String,
        provider_id: String,
        model_id: String,
    },
    /// `session.abort` (undo + interrupt paths).
    SessionAbort {
        session_id: String,
    },
    /// `session.revert` (`session/index.tsx:610-644`).
    SessionRevert {
        session_id: String,
        message_id: String,
    },
    /// `session.unrevert` (`session/index.tsx:646-671`).
    SessionUnrevert {
        session_id: String,
    },
    /// `sync.session.refresh()` — re-list sessions (`local.tsx`).
    SessionRefresh,
    /// `experimental.session.background` (`session/index.tsx:1024-1030`).
    SessionBackground {
        session_id: String,
    },
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
    OpenUrl {
        url: String,
    },
    /// `session.get` on mount + 404 toast/home + workspace re-bootstrap
    /// + hydrate + snap-to-bottom (`session/index.tsx:286-324`).
    SessionMount {
        session_id: String,
        previous_workspace: Option<String>,
    },
    /// Read the workflow runs, and the saved workflows if asked.
    WorkflowsRefresh {
        definitions: bool,
    },
    WorkflowStart {
        name: Option<String>,
        script: Option<String>,
        args: serde_json::Value,
    },
    WorkflowAbort {
        run_id: String,
    },
    WorkflowDelete {
        name: String,
    },
    /// Open `$EDITOR` on a script and save what comes back.
    WorkflowEdit {
        name: String,
        original: String,
        scope: String,
        overwrite: bool,
    },
    /// `sync.session.sync(sessionID)` — the `task` tool hydrates its
    /// child session on mount (`session/index.tsx:2221-2224`).
    SessionHydrate {
        session_id: String,
    },
    /// The session list's server search (`GET /session?search=…`).
    SessionSearch {
        query: String,
    },
    /// `GET /api/fs/find?query=…&limit=20` for the `@` popup.
    FindFiles {
        query: String,
        workspace: Option<String>,
    },
    /// After an SSE reconnect: `GET /session/status` replaces the statuses.
    SessionStatusRefresh,
    /// After an SSE reconnect: hydrate the session again, even if it was
    /// fully synced (the events in between are lost).
    SessionResync {
        session_id: String,
    },
    /// `prompt.paste` — read the clipboard, run the paste pipeline
    /// (`prompt/index.tsx:374-391`).
    PromptPaste,
    /// `prompt.editor` — suspend the terminal, open `$EDITOR` seeded
    /// with `value`, apply the edited content
    /// (`prompt/index.tsx:424-514`).
    OpenPromptEditor {
        value: String,
    },
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
    QuestionReject {
        request_id: String,
    },
    /// `session.update` title (`DialogSessionRename`).
    SessionRename {
        session_id: String,
        title: String,
    },
    /// `auth.set` (`dialog-provider.tsx:405-412`) — store the entered API
    /// key for the provider, then re-bootstrap.
    AuthSet {
        provider_id: String,
        key: String,
    },
    /// `auth.remove` — sign out of / drop the auth.json credential (the
    /// server also revokes a LibertAI session), then re-bootstrap.
    AuthRemove {
        provider_id: String,
    },
    /// `provider.oauth.authorize` (`dialog-provider.tsx:184-205`), then
    /// for `method: "auto"` the no-code `provider.oauth.callback` wait
    /// (`AutoMethod` onMount, `dialog-provider.tsx:263-282`).
    ProviderOauthAuthorize {
        provider_id: String,
        method: usize,
        title: String,
    },
    /// `provider.oauth.callback` with a pasted code (`CodeMethod`
    /// onConfirm, and the remote-browser fallback of an auto flow).
    ProviderOauthCallback {
        provider_id: String,
        method: usize,
        flow: u64,
        code: String,
    },
    /// `session.delete` (`dialog-session-list.tsx:248-262`).
    SessionDelete {
        session_id: String,
    },
    /// `local.mcp.toggle(name)` + `mcp.status` refresh
    /// (`dialog-mcp.tsx:49-66`).
    McpToggle {
        name: String,
    },
    /// `global.upgrade` (`app.tsx:1051`).
    GlobalUpgrade {
        target: String,
    },
    /// `experimental.moveSession` (`component/prompt/move.tsx`).
    SessionMove {
        session_id: String,
        directory: String,
    },
    /// `project.directories` fetch for the move-session dialog
    /// (`dialog-move-session.tsx:60-78`).
    ProjectDirectories {
        project_id: String,
    },
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
    /// The browser seam — provider sign-in URLs; a recorder in tests.
    pub browser: std::sync::Arc<dyn crate::browser::Browser>,
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
        ui.copy_on_select_disabled = selection::copy_on_select_disabled_from_env();
        App {
            state,
            ui,
            config,
            keymap,
            attention: crate::attention::terminal_attention(),
            // Unit tests never launch a real browser.
            browser: if cfg!(test) {
                std::sync::Arc::new(crate::browser::RecordingBrowser::default())
            } else {
                crate::browser::system_browser()
            },
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
                crate::ui::workflows::on_bus_event(app, &event.event);
            }
        }
        // Only presses act. A release (reported where the terminal sends
        // them, e.g. Windows consoles) must not land on the screen behind a
        // dialog its press just closed — an `escape` release would reject
        // the pending permission.
        Msg::Key(key) if key.kind == crossterm::event::KeyEventKind::Release => {}
        Msg::Key(key) if selection_key(app, &key, &mut effects) => {}
        Msg::Key(key) => {
            // Dialogs take the keyboard while the stack is open (the
            // pushed `modal` mode, `ui/dialog.tsx:105-137`).
            if !app.ui.dialogs.is_empty() {
                effects.extend(dialog_key(app, &key));
            } else if let Some(handled) = crate::ui::session::permission::handle_key(app, &key) {
                effects.extend(handled);
            } else if let Some(handled) = crate::ui::session::question::handle_key(app, &key) {
                effects.extend(handled);
            } else if let Some(handled) = crate::ui::workflows::handle_key(app, &key) {
                effects.extend(handled);
            } else {
                // Keys a pending permission/question does not own reach
                // the keymap like any other (`ctrl+p`, leader sequences).
                let context = crate::keymap::DispatchContext {
                    route_is_session: matches!(app.state.route.data, Route::Session { .. }),
                    prompt_focused: app.ui.prompt_focused,
                    foreground_tasks: crate::command::foreground_tasks(app) > 0,
                    prompt_mounted: !crate::ui::session::prompt_replaced(app),
                };
                // While the autocomplete is open it takes the keys it
                // handles (§5.2 escape priority): Enter selects instead of
                // submitting, ctrl+p moves instead of opening the palette,
                // Esc hides instead of interrupting.
                let (autocompleted, autocomplete_effects) = prompt::autocomplete_key(app, &key);
                effects.extend(autocomplete_effects);
                let commands = if autocompleted {
                    Vec::new()
                } else {
                    app.keymap.dispatch(&context, &key, app.ui.tick_ms)
                };
                let mut handled = false;
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
                    // A pending permission/question overrides `app.exit`
                    // (`permission.tsx:451-460`, `question.tsx:225-233`):
                    // `<leader>q` rejects instead of quitting.
                    if *name == "app.exit" {
                        if crate::ui::workflows::hold_exit(app) {
                            handled = true;
                            continue;
                        }
                        if let Some(reply) = crate::ui::session::permission::app_exit(app)
                            .or_else(|| crate::ui::session::question::app_exit(app))
                        {
                            effects.extend(reply);
                            handled = true;
                            continue;
                        }
                    }
                    if prompt::handle_command(app, name) {
                        handled = true;
                        continue;
                    }
                    effects.extend(crate::command::run(app, name));
                    handled = true;
                }
                if !autocompleted && !handled && commands.is_empty() {
                    prompt::text_input(app, &key);
                }
            }
        }
        Msg::Mouse(mouse) if selection_mouse(app, &mouse, &mut effects) => {}
        Msg::Mouse(mouse)
            if app.ui.dialogs.is_empty() && crate::ui::workflows::handle_mouse(app, &mouse) => {}
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
                let speed = scroll_speed(app) as i64;
                app.ui.session_scroll.scroll_by(-speed);
            }
            crossterm::event::MouseEventKind::ScrollDown => {
                let speed = scroll_speed(app) as i64;
                app.ui.session_scroll.scroll_by(speed);
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
            // The header's `esc` hint closes (`dialog-select.tsx:562-564`).
            crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left)
                if !app.ui.dialogs.is_empty()
                    && crate::ui::dialogs::close_hint(app, mouse.column, mouse.row) =>
            {
                crate::ui::dialogs::clear(app);
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
            // outside the frame pops the top dialog. The release that ends
            // a selection drag never gets here (`selection_mouse`), so a
            // selection never closes the dialog.
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
                    transcript_click(app, mouse.row, &mut effects);
                }
            }
            _ => {}
        },
        Msg::Resize(columns, rows) => {
            // Layout is recomputed on every draw; the sidebar boundary
            // needs the width, the dialog frames the height.
            app.ui.terminal_width = columns;
            app.ui.terminal_height = rows;
            crate::ui::dialogs::on_resize(app);
        }
        Msg::Paste(text) => {
            // Bracketed-paste normalization happens at the boundary
            // (`prompt/index.tsx:1402-1405`); an empty paste falls back
            // to the clipboard-paste command (the win32 image quirk).
            let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
            if crate::ui::dialogs::paste(app, &normalized) {
                // A text-input dialog on top took it.
            } else if paste_into_bottom_bar(app, &normalized) {
                // The permission reject message or the question's "Other"
                // answer has the focus.
            } else if crate::ui::session::prompt_replaced(app) {
                // No prompt on screen to paste into.
            } else if normalized.trim().is_empty() {
                effects.extend(crate::command::run(app, "prompt.paste"));
            } else {
                prompt::paste_input_text(app, &normalized);
            }
        }
        Msg::Hangup => app.exit(None),
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
            effects.extend(crate::ui::workflows::on_tick(app, now_ms));
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
    // The `@` popup's file search (`sdk.client.v2.fs.find`) runs as an
    // effect; its answer lands through `prompt::files_found`.
    if let Some(search) = session_search::observe(app) {
        effects.push(search);
    }
    if let Some(query) = app.ui.prompt.autocomplete.file_request.take() {
        effects.push(Effect::FindFiles {
            query,
            workspace: app.state.project.workspace.current.clone(),
        });
    }
    app.version += 1;
    effects.extend(crate::app::post_update(app));
    effects
}

/// `Selection.copy` (`util/selection.ts`): the selected text to the
/// clipboard with the "Copied to clipboard" toast, then clear.
fn copy_selection(app: &mut App) -> Option<Effect> {
    let text = app.ui.selection.text();
    app.ui.selection.clear();
    Some(Effect::ClipboardWrite {
        text: text?,
        success: Some(Toast {
            title: None,
            variant: ToastVariant::Info,
            message: "Copied to clipboard".to_string(),
            duration_ms: 3000,
        }),
        failure: Some(Toast {
            title: None,
            variant: ToastVariant::Error,
            message: "Failed to copy to clipboard".to_string(),
            duration_ms: 5000,
        }),
    })
}

/// The mouse half of the selection. Returns whether it took the event.
fn selection_mouse(
    app: &mut App,
    mouse: &crossterm::event::MouseEvent,
    effects: &mut Vec<Effect>,
) -> bool {
    use crossterm::event::{MouseButton, MouseEventKind};
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            // A press may start a selection; the press itself still acts.
            app.ui.selection.begin((mouse.column, mouse.row));
            false
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            app.ui.selection.extend((mouse.column, mouse.row));
            true
        }
        MouseEventKind::Up(MouseButton::Left) if app.ui.selection.active() => {
            // The end of a drag is not a click.
            if !app.ui.copy_on_select_disabled {
                effects.extend(copy_selection(app));
            }
            true
        }
        MouseEventKind::Up(MouseButton::Left) => {
            app.ui.selection.clear();
            false
        }
        // Right click copies a kept selection (`app.tsx` onMouseDown).
        MouseEventKind::Down(MouseButton::Right) if app.ui.selection.active() => {
            effects.extend(copy_selection(app));
            true
        }
        _ => false,
    }
}

/// A key while a selection is kept (`handleSelectionKey`): ctrl+c and
/// ctrl+y copy, Esc dismisses (and is consumed), any other key dismisses
/// and goes on. Returns whether the key was consumed.
fn selection_key(
    app: &mut App,
    key: &crossterm::event::KeyEvent,
    effects: &mut Vec<Effect>,
) -> bool {
    use crossterm::event::{KeyCode, KeyModifiers};
    if !app.ui.selection.active() {
        return false;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('c' | 'y') if ctrl => {
            effects.extend(copy_selection(app));
            true
        }
        KeyCode::Esc => {
            app.ui.selection.clear();
            true
        }
        _ => {
            app.ui.selection.clear();
            false
        }
    }
}

/// A key while the dialog stack is open: the `app.global` bindings
/// (session list, new session, quick switch) still fire, as in TS; every
/// other key belongs to the dialog.
fn dialog_key(app: &mut App, key: &crossterm::event::KeyEvent) -> Vec<Effect> {
    let context = crate::keymap::DispatchContext {
        route_is_session: matches!(app.state.route.data, Route::Session { .. }),
        prompt_focused: false,
        foreground_tasks: crate::command::foreground_tasks(app) > 0,
        prompt_mounted: !crate::ui::session::prompt_replaced(app),
    };
    let was_pending = app.keymap.leader_active();
    let commands = app.keymap.dispatch_in(
        &context,
        key,
        app.ui.tick_ms,
        &[crate::keymap::Scope::AppGlobal],
    );
    if !commands.is_empty() {
        let mut effects = Vec::new();
        for name in commands {
            if crate::command::is_enabled(app, name) {
                effects.extend(crate::command::run(app, name));
            }
        }
        return effects;
    }
    // A key that armed or extended a leader sequence, or cancelled one
    // (Esc/Backspace), was the keymap's.
    let cancelled = was_pending
        && matches!(
            key.code,
            crossterm::event::KeyCode::Esc | crossterm::event::KeyCode::Backspace
        );
    if app.keymap.leader_active() || cancelled {
        return Vec::new();
    }
    crate::ui::dialogs::handle_key(app, key)
}

/// Bracketed paste into a focused bottom-bar field.
fn paste_into_bottom_bar(app: &mut App, text: &str) -> bool {
    if crate::ui::session::permission::visible(app).is_some()
        && app.ui.permission.stage == crate::ui::session::permission::PermissionStage::Reject
    {
        app.ui.permission.reject_input.push_str(text);
        return true;
    }
    if crate::ui::session::question::visible(app).is_some() && app.ui.question.editing {
        app.ui.question.input.push_str(text);
        return true;
    }
    false
}

/// The animation frame interval (the TS spinner `interval={40}`).
pub const FRAME_INTERVAL_MS: u64 = 40;
/// The prompt cursor blink half-period (`ui/session/prompt.rs`).
const BLINK_MS: u64 = 530;

/// Whether something on screen moves by itself: a spinner (a busy or
/// retrying session, a prompt being submitted) or the startup overlay.
pub fn animating(app: &App) -> bool {
    !app.ui.startup_ready
        || app.ui.startup_loading.visible()
        || app.ui.prompt.submitting
        || crate::ui::workflows::animating(app)
        || app.state.sync.session_status.values().any(|status| {
            matches!(
                status,
                alforria_schema::session_status::SessionStatusInfo::Busy
                    | alforria_schema::session_status::SessionStatusInfo::Retry { .. }
            )
        })
}

/// How long the loop may sleep before the next [`Msg::Tick`]: one frame
/// while [`animating`], otherwise until the nearest timer (toast, leader,
/// interrupt reset, footer rotation, startup overlay, cursor blink).
/// `None`: nothing moves until the next message.
pub fn next_tick_in(app: &App, now_ms: u64) -> Option<u64> {
    if animating(app) {
        return Some(FRAME_INTERVAL_MS);
    }
    let mut deadlines = vec![
        app.ui.toast_deadline_ms,
        app.ui.interrupt_reset_at,
        app.ui.footer_flip_at,
        app.ui.startup_loading.deadline(),
        app.keymap.pending_deadline(),
        app.ui.session_search.due_at,
        crate::ui::workflows::next_poll_at(app, now_ms),
    ];
    if app.ui.prompt_focused && app.config.cursor_blinks() {
        deadlines.push(Some((now_ms / BLINK_MS + 1) * BLINK_MS));
    }
    deadlines
        .into_iter()
        .flatten()
        .map(|deadline| deadline.saturating_sub(now_ms))
        .min()
        .map(|delay| delay.max(1))
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

/// `getScrollAcceleration(tuiConfig)` (`util/scroll.ts`): a fixed
/// `scroll_speed` (default 3), or with `scroll_acceleration.enabled` the
/// `MacOSScrollAccel` curve.
fn scroll_speed(app: &mut App) -> u32 {
    if app.config.scroll_acceleration_enabled {
        let now = app.ui.tick_ms;
        return app.ui.scroll_accel.tick(now).round().max(1.0) as u32;
    }
    app.config.scroll_speed.round().max(1.0) as u32
}

/// `MacOSScrollAccel` (`@opentui/core`): wheel ticks in a quick streak
/// scroll further. The multiplier grows exponentially with the tick rate
/// over the last three intervals (`1 + A·(e^(v/τ) − 1)`, `v` = 100 ms ÷
/// average interval), capped at 6; a pause over 150 ms resets it.
#[derive(Debug, Default)]
pub struct ScrollAccel {
    last_tick_ms: Option<u64>,
    intervals: Vec<u64>,
}

impl ScrollAccel {
    const A: f64 = 0.8;
    const TAU: f64 = 3.0;
    const MAX_MULTIPLIER: f64 = 6.0;
    const STREAK_TIMEOUT_MS: u64 = 150;
    const MIN_TICK_INTERVAL_MS: u64 = 6;
    const HISTORY: usize = 3;

    pub fn tick(&mut self, now_ms: u64) -> f64 {
        let Some(last) = self.last_tick_ms else {
            self.last_tick_ms = Some(now_ms);
            self.intervals.clear();
            return 1.0;
        };
        let interval = now_ms.saturating_sub(last);
        if interval > Self::STREAK_TIMEOUT_MS {
            self.last_tick_ms = Some(now_ms);
            self.intervals.clear();
            return 1.0;
        }
        if interval < Self::MIN_TICK_INTERVAL_MS {
            return 1.0;
        }
        self.last_tick_ms = Some(now_ms);
        self.intervals.push(interval);
        if self.intervals.len() > Self::HISTORY {
            self.intervals.remove(0);
        }
        let average = self.intervals.iter().sum::<u64>() as f64 / self.intervals.len() as f64;
        let velocity = 100.0 / average;
        let multiplier = 1.0 + Self::A * ((velocity / Self::TAU).exp() - 1.0);
        multiplier.min(Self::MAX_MULTIPLIER)
    }
}

impl App {
    /// The prompt cursor cell shows now: focused, and in the visible half
    /// of the blink unless `cursor.blinking` is off.
    pub fn prompt_cursor_shown(&self) -> bool {
        self.ui.prompt_focused
            && (!self.config.cursor_blinks() || (self.ui.tick_ms / BLINK_MS).is_multiple_of(2))
    }

    /// The prompt cursor cell's style for `cursor.style`: `block` (and
    /// `default`) is the given block style; `underline` underlines the
    /// cell. A one-column bar cannot be drawn in a cell, so `line`
    /// underlines too.
    pub fn prompt_cursor_style(&self, block: ratatui::style::Style) -> ratatui::style::Style {
        use crate::config::CursorStyle;
        match self.config.cursor.map(|cursor| cursor.style) {
            Some(CursorStyle::Underline | CursorStyle::Line) => {
                ratatui::style::Style::new().add_modifier(ratatui::style::Modifier::UNDERLINED)
            }
            _ => block,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_app_sleeps_until_a_timer() {
        let mut app = App::new(crate::config::TuiConfig::default(), Args::default(), None);
        app.ui.startup_ready = true;
        app.ui.prompt_focused = false;
        assert_eq!(next_tick_in(&app, 10_000), None, "nothing moves");
        app.ui.prompt_focused = true;
        assert_eq!(
            next_tick_in(&app, 10_000),
            Some(70),
            "next blink edge (19 × 530)"
        );
        app.ui.toast_deadline_ms = Some(10_050);
        assert_eq!(next_tick_in(&app, 10_000), Some(50), "toast dismissal");
        app.state.sync.session_status.insert(
            "ses_a".to_string(),
            alforria_schema::session_status::SessionStatusInfo::Busy,
        );
        assert_eq!(
            next_tick_in(&app, 10_000),
            Some(FRAME_INTERVAL_MS),
            "a spinner animates"
        );
    }

    #[test]
    fn scroll_acceleration_grows_in_a_streak_and_resets_after_a_pause() {
        let mut accel = ScrollAccel::default();
        assert_eq!(accel.tick(1000), 1.0, "first tick");
        let mut last = 1.0;
        for step in 1..=4 {
            let next = accel.tick(1000 + step * 20);
            assert!(next >= last, "{next} < {last}");
            last = next;
        }
        assert!(last > 1.0 && last <= 6.0, "{last}");
        assert_eq!(accel.tick(2000), 1.0, "a pause resets");

        let config = crate::config::TuiConfig {
            scroll_acceleration_enabled: true,
            ..crate::config::TuiConfig::default()
        };
        let mut app = App::new(config, Args::default(), None);
        app.ui.tick_ms = 5000;
        assert_eq!(
            scroll_speed(&mut app),
            1,
            "acceleration replaces scroll_speed"
        );
    }

    #[test]
    fn cursor_config_controls_the_blink() {
        let config = crate::config::TuiConfig {
            cursor: Some(crate::config::CursorConfig {
                style: crate::config::CursorStyle::Underline,
                blinking: false,
            }),
            ..crate::config::TuiConfig::default()
        };
        let mut app = App::new(config, Args::default(), None);
        app.ui.startup_ready = true;
        app.ui.prompt_focused = true;
        for tick in [0, 530, 1060, 1590] {
            app.ui.tick_ms = tick;
            assert!(app.prompt_cursor_shown(), "steady cursor at {tick}");
        }
        assert_eq!(next_tick_in(&app, 10_000), None, "no blink to wake for");
        let style = app.prompt_cursor_style(ratatui::style::Style::new());
        assert!(style
            .add_modifier
            .contains(ratatui::style::Modifier::UNDERLINED));
    }

    #[test]
    fn hangup_exits() {
        let mut app = App::new(crate::config::TuiConfig::default(), Args::default(), None);
        update(&mut app, Msg::Hangup);
        assert!(app.ui.exit);
    }

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
            child_session: None,
            workflow_run: None,
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

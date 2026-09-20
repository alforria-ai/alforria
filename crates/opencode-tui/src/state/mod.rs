//! Elm-style model — `App`/`State`/`Msg`/`update()` skeleton (M8.2) plus
//! the terminal runtime surface (M8.3).
//!
//! Solid `createSignal`/`createMemo` become plain fields; Solid
//! `createStore`/`produce` become `&mut` mutations inside [`update`].
//! Async happens only in effects: `update` returns `Vec<Effect>`, the
//! runtime loop turns each into a task and feeds results back.

pub mod kv;
pub mod local;
pub mod route;
pub mod sync;

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::state::kv::Kv;
use crate::state::local::LocalState;
use crate::state::route::RouteStore;
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

/// A dialog awaiting the dialog stack (TODO(M8.7) renders/interacts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingDialog {
    /// `dialog.replace(DialogProviderList)` (`app.tsx:542-551`).
    ProviderConnect,
    /// `DialogConfirm.show` on `installation.update-available`
    /// (`app.tsx:1033-1079`).
    UpdateAvailable { version: String },
}

/// Keymap modes and focus (TODO(M8.4)), dialog stack (TODO(M8.7)) plus
/// the app-shell bookkeeping of M8.3.
#[derive(Debug, Default)]
pub struct UiState {
    /// One current toast at a time (`ui/toast.tsx:66-69`).
    pub toasts: Vec<Toast>,
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
    pub dialog: Option<PendingDialog>,
    /// Exit was requested; `exit_reason` becomes stderr + exit code 1.
    pub exit: bool,
    pub exit_reason: Option<String>,
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
    /// 40 ms frame tick (`app.tsx:196` `targetFps: 60`).
    Tick(std::time::Duration),
}

/// Async work fired by [`update`] — the runtime loop executes each
/// against the server seam.
#[derive(Debug)]
pub enum Effect {
    /// `bootstrap()` (`sync.tsx:451`) — `server.instance.disposed`.
    Bootstrap { fatal: bool },
    /// Auto-reply to a permission under `"auto"` mode
    /// (`sync.tsx:196-207`).
    PermissionAutoReply {
        request_id: String,
        reply: opencode_schema::permission_v1::PermissionV1Reply,
        metadata: EventMetadata,
    },
    /// `lsp.updated` refetches `lsp.status` (`sync.tsx:433-437`).
    LspStatusRefetch { workspace: Option<String> },
    /// `session.fork` + navigate on success (`app.tsx:512-519`).
    SessionFork { session_id: String, navigate: bool },
    /// `terminal.suspend` — leave raw mode, `SIGTSTP`, resume on
    /// `SIGCONT` (`app.tsx:870-879`).
    SuspendTerminal,
}

pub struct App {
    pub state: State,
    pub ui: UiState,
    pub config: crate::config::TuiConfig,
    /// Bumped on every update — the redraw signal (§2.1).
    pub version: u64,
}

impl App {
    pub fn new(config: crate::config::TuiConfig, args: Args, state_dir: Option<&Path>) -> App {
        let mut state = State::new(args, state_dir);
        let ui = UiState {
            theme: ThemeStore::init(&mut state.kv, config.theme.as_deref()),
            ..UiState::default()
        };
        App {
            state,
            ui,
            config,
            version: 0,
        }
    }

    /// `toast.show` (`ui/toast.tsx:60-74`): one current toast — new shows
    /// replace.
    pub fn show_toast(&mut self, toast: Toast) {
        self.ui.toasts.clear();
        self.ui.toasts.push(toast);
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
                effects.extend(crate::app::on_bus_event(app, event));
            }
        }
        Msg::Key(key) => {
            // TODO(M8.4): keymap dispatch (leader, modes, bindings).
            // TODO(M8.6): a focused non-empty prompt clears the input
            // on ctrl+c instead of exiting (app.tsx:977-985).
            if key.kind != crossterm::event::KeyEventKind::Release
                && key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL)
            {
                match key.code {
                    crossterm::event::KeyCode::Char('c') | crossterm::event::KeyCode::Char('d') => {
                        app.exit(None);
                    }
                    _ => {}
                }
            }
        }
        Msg::Mouse(_) => {
            // TODO(M8.5): scroll + click handling (util/scroll.ts).
        }
        Msg::Resize(_, _) => {
            // Layout is recomputed on every draw.
        }
        Msg::Tick(elapsed) => {
            let now_ms = elapsed.as_millis() as u64;
            app.ui.tick_ms = now_ms;
            app.ui.startup_loading.poll(now_ms);
            // The Rust port has no plugin host (spec §6 N2) — `ready` is
            // always true, so the overlay only ever shows the
            // "Finishing startup…" hold.
            app.ui.startup_loading.transition(true, now_ms);
        }
    }
    app.version += 1;
    effects.extend(crate::app::post_update(app));
    effects
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
        use opencode_schema::event_manifest::Event;
        use opencode_schema::session_v1::SessionDeletedData;

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

    fn session_info(id: &str) -> opencode_schema::session_v1::V1SessionInfo {
        opencode_schema::session_v1::V1SessionInfo {
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
}

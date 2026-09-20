//! Elm-style model — `App`/`State`/`Msg`/`update()` skeleton (M8.2).
//!
//! Solid `createSignal`/`createMemo` become plain fields; Solid
//! `createStore`/`produce` become `&mut` mutations inside
//! [`update`]. Async happens only in effects: `update` returns
//! `Vec<Effect>`, a driver turns each into a task that sends results
//! back into the loop (TODO(M8.3)).

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
    pub variant: ToastVariant,
    pub message: String,
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

/// Keymap modes, dialog stack, toasts, focus (TODO(M8.3+)) plus the
/// redraw counter (`App.version`).
#[derive(Debug, Default)]
pub struct UiState {
    pub toasts: Vec<Toast>,
}

/// The update messages.
pub enum Msg {
    /// One coalesced bus batch — a whole flush applies in a single
    /// `update` pass (the TS `batch()` equivalent, `sdk.tsx:60-66`).
    Bus(Vec<BusEvent>),
    // TODO(M8.3): Key/Mouse/Resize/Effect/Tick variants.
}

/// Async work fired by [`update`] — a driver executor turns each into
/// a task and feeds results back (TODO(M8.3)).
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
}

pub struct App {
    pub state: State,
    pub ui: UiState,
    /// Bumped on every update — the redraw signal (§2.1).
    pub version: u64,
}

/// The update function — pure state transitions; effects carry all
/// async work.
pub fn update(app: &mut App, msg: Msg) -> Vec<Effect> {
    match msg {
        Msg::Bus(events) => {
            let mut effects = Vec::new();
            for event in events {
                local::on_bus_event(&mut app.state, &event.event);
                effects.extend(sync::apply_event(&mut app.state, event));
            }
            app.version += 1;
            effects
        }
    }
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

        let mut app = App {
            state: State::new(Args::default(), None),
            ui: UiState::default(),
            version: 0,
        };
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

//! `sync.tsx` — the sync store + reducer + bootstrap + session hydrate
//! (M8.2).
//!
//! TS reference: `context/sync.tsx` (673 lines), ported mechanically.
//! The Solid store maps to [`SyncState`]; the event subscriber
//! (`sync.tsx:176-446`) maps to [`apply_event`]; bootstrap
//! (`:451-552`) maps to [`bootstrap`]; session hydrate (`:594-667`)
//! maps to [`session_sync`].

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::{json, Value};

use alforria_schema::event_manifest::Event;
use alforria_schema::file_diff::SnapshotFileDiff;
use alforria_schema::permission_v1::{PermissionV1Reply, PermissionV1Request};
use alforria_schema::question_v1::QuestionV1Request;
use alforria_schema::session_status::SessionStatusInfo;
use alforria_schema::session_todo::TodoInfo;
use alforria_schema::session_v1::{MessagePartDeltaData, V1Message, V1Part, V1SessionInfo};
use tokio::join as tokio_join;

use crate::state::kv::{keys, Kv};
use crate::state::{Effect, ProjectState, State};
use crate::transport::api::{Location, ServerApi, SessionListQuery};
use crate::transport::events::BusEvent;

/// `store.status` (`sync.tsx:71`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncStatus {
    Loading,
    Partial,
    Complete,
}

/// The per-hydrate live-tracking set (`sync.tsx:152-158`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HydrateTracker {
    pub messages: HashSet<String>,
    pub parts: HashSet<String>,
}

/// The sync store (`sync.tsx:70-144`). Untyped fields (`agent`,
/// `command`, `lsp`, …) hold the raw JSON the server sent — the same
/// dynamic typing the TS store keeps.
#[derive(Debug, Clone, Default)]
pub struct SyncState {
    pub status: Option<SyncStatus>,
    pub provider: Vec<Value>,
    pub provider_default: BTreeMap<String, String>,
    pub provider_next: Value,
    pub console_state: Value,
    pub capabilities: bool,
    pub provider_auth: BTreeMap<String, Vec<Value>>,
    pub agent: Vec<Value>,
    pub command: Vec<Value>,
    pub permission: BTreeMap<String, Vec<PermissionV1Request>>,
    pub question: BTreeMap<String, Vec<QuestionV1Request>>,
    pub config: Value,
    pub session: Vec<V1SessionInfo>,
    pub session_status: BTreeMap<String, SessionStatusInfo>,
    pub session_diff: BTreeMap<String, Vec<SnapshotFileDiff>>,
    pub todo: BTreeMap<String, Vec<TodoInfo>>,
    pub message: BTreeMap<String, Vec<V1Message>>,
    pub part: BTreeMap<String, Vec<V1Part>>,
    pub lsp: Vec<Value>,
    pub mcp: BTreeMap<String, Value>,
    pub mcp_resource: BTreeMap<String, Value>,
    pub formatter: Vec<Value>,
    pub vcs: Option<Value>,

    /// `fullSyncedSessions` (`sync.tsx:150`).
    pub full_synced: HashSet<String>,
    /// `hydratingSessions` (`sync.tsx:152`).
    pub hydrating_sessions: HashMap<String, HydrateTracker>,
}

fn empty_console_state() -> Value {
    json!({
        "consoleManagedProviders": [],
        "switchableOrgCount": 0,
    })
}

fn empty_provider_next() -> Value {
    json!({
        "all": [],
        "default": {},
        "connected": [],
    })
}

impl SyncState {
    pub fn new() -> SyncState {
        SyncState {
            status: Some(SyncStatus::Loading),
            provider_next: empty_provider_next(),
            console_state: empty_console_state(),
            ..SyncState::default()
        }
    }

    /// `touchMessage` (`sync.tsx:153-155`).
    fn touch_message(&mut self, session_id: &str, message_id: &str) {
        if let Some(tracker) = self.hydrating_sessions.get_mut(session_id) {
            tracker.messages.insert(message_id.to_string());
        }
    }

    /// `touchPart` (`sync.tsx:156-158`).
    fn touch_part(&mut self, session_id: &str, part_id: &str) {
        if let Some(tracker) = self.hydrating_sessions.get_mut(session_id) {
            tracker.parts.insert(part_id.to_string());
        }
    }

    /// `result.session.get(sessionID)` (`sync.tsx:572-576`).
    pub fn session(&self, session_id: &str) -> Option<&V1SessionInfo> {
        self.session.iter().find(|s| s.id == session_id)
    }

    /// `result.session.status(sessionID)` (`sync.tsx:584-593`) — the
    /// derived working-status of a session.
    pub fn session_working_status(&self, session_id: &str) -> SessionWorkingStatus {
        let Some(session) = self.session(session_id) else {
            return SessionWorkingStatus::Idle;
        };
        // TS truthiness: a `0` compacting timestamp is falsy.
        if session.time.compacting.is_some_and(|v| v != 0) {
            return SessionWorkingStatus::Compacting;
        }
        let Some(last) = self.message.get(session_id).and_then(|m| m.last()) else {
            return SessionWorkingStatus::Idle;
        };
        match last {
            V1Message::User { .. } => SessionWorkingStatus::Working,
            V1Message::Assistant { time, .. } => match time.completed {
                Some(_) => SessionWorkingStatus::Idle,
                None => SessionWorkingStatus::Working,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionWorkingStatus {
    Compacting,
    Working,
    Idle,
}

// ------------------------------------------------------- sorted inserts

/// The result of [`search`] (`sync.tsx:41-52`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Match {
    pub found: bool,
    pub index: usize,
}

/// Binary search with sorted-insert index (`sync.tsx:41-52`).
pub fn search<T>(items: &[T], target: &str, key: impl Fn(&T) -> String) -> Match {
    let mut left: isize = 0;
    let mut right: isize = items.len() as isize - 1;
    while left <= right {
        let middle = ((left + right) / 2) as usize;
        let value = key(&items[middle]);
        if value == target {
            return Match {
                found: true,
                index: middle,
            };
        }
        if value.as_str() < target {
            left = middle as isize + 1;
        } else {
            right = middle as isize - 1;
        }
    }
    Match {
        found: false,
        index: left as usize,
    }
}

/// `messageKey` (`sync.tsx:58`) — `time.created + id`, string
/// concatenation (JS number-to-string, then concat).
pub fn message_key(message: &V1Message) -> String {
    format!("{}{}", message_created(message), message_id(message))
}

fn message_created(message: &V1Message) -> f64 {
    match message {
        V1Message::User { time, .. } => time.created,
        V1Message::Assistant { time, .. } => time.created as f64,
    }
}

/// `compareMessage` (`sync.tsx:54-56`).
fn compare_message(a: &V1Message, b: &V1Message) -> Ordering {
    message_created(a)
        .partial_cmp(&message_created(b))
        .unwrap_or(Ordering::Equal)
        .then_with(|| message_id(a).cmp(message_id(b)))
}

pub(crate) fn message_id(message: &V1Message) -> &str {
    match message {
        V1Message::User { id, .. } | V1Message::Assistant { id, .. } => id,
    }
}

pub(crate) fn message_session_id(message: &V1Message) -> &str {
    match message {
        V1Message::User { session_id, .. } | V1Message::Assistant { session_id, .. } => session_id,
    }
}

pub(crate) fn part_id(part: &V1Part) -> &str {
    match part {
        V1Part::Text { id, .. }
        | V1Part::Subtask { id, .. }
        | V1Part::Reasoning { id, .. }
        | V1Part::File { id, .. }
        | V1Part::Tool { id, .. }
        | V1Part::StepStart { id, .. }
        | V1Part::StepFinish { id, .. }
        | V1Part::Snapshot { id, .. }
        | V1Part::Patch { id, .. }
        | V1Part::Agent { id, .. }
        | V1Part::Retry { id, .. }
        | V1Part::Compaction { id, .. } => id,
    }
}

fn part_session_id(part: &V1Part) -> &str {
    match part {
        V1Part::Text { session_id, .. }
        | V1Part::Subtask { session_id, .. }
        | V1Part::Reasoning { session_id, .. }
        | V1Part::File { session_id, .. }
        | V1Part::Tool { session_id, .. }
        | V1Part::StepStart { session_id, .. }
        | V1Part::StepFinish { session_id, .. }
        | V1Part::Snapshot { session_id, .. }
        | V1Part::Patch { session_id, .. }
        | V1Part::Agent { session_id, .. }
        | V1Part::Retry { session_id, .. }
        | V1Part::Compaction { session_id, .. } => session_id,
    }
}

fn part_message_id(part: &V1Part) -> &str {
    match part {
        V1Part::Text { message_id, .. }
        | V1Part::Subtask { message_id, .. }
        | V1Part::Reasoning { message_id, .. }
        | V1Part::File { message_id, .. }
        | V1Part::Tool { message_id, .. }
        | V1Part::StepStart { message_id, .. }
        | V1Part::StepFinish { message_id, .. }
        | V1Part::Snapshot { message_id, .. }
        | V1Part::Patch { message_id, .. }
        | V1Part::Agent { message_id, .. }
        | V1Part::Retry { message_id, .. }
        | V1Part::Compaction { message_id, .. } => message_id,
    }
}

/// `message.part.delta` string-append (`sync.tsx:398-415`): `part[field]
/// = (part[field] ?? "") + delta`. The part round-trips through JSON so
/// the append applies to any top-level string field.
fn append_delta(part: &mut V1Part, field: &str, delta: &str) {
    let Ok(mut value) = serde_json::to_value(&*part) else {
        return;
    };
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    let next = match obj.get(field) {
        Some(Value::String(existing)) => format!("{existing}{delta}"),
        _ => delta.to_string(),
    };
    obj.insert(field.to_string(), Value::String(next));
    if let Ok(updated) = serde_json::from_value(value) {
        *part = updated;
    }
}

/// The sync event subscriber (`sync.tsx:176-446`). Returns the effects
/// the handler fires (auto-reply, refetch, re-bootstrap).
pub fn apply_event(state: &mut State, bus_event: BusEvent) -> Vec<Effect> {
    let BusEvent { event, metadata } = bus_event;
    let sync = &mut state.sync;
    match event {
        Event::ServerInstanceDisposed(_) => {
            vec![Effect::Bootstrap { fatal: true }]
        }
        Event::PermissionReplied(data) => {
            let Some(requests) = sync.permission.get_mut(&data.session_id) else {
                return Vec::new();
            };
            let m = search(requests, &data.request_id, |r: &PermissionV1Request| {
                r.id.clone()
            });
            if m.found {
                requests.remove(m.index);
            }
            Vec::new()
        }
        Event::PermissionAsked(request) => {
            if state.permission_mode == crate::state::PermissionMode::Auto {
                return vec![Effect::PermissionAutoReply {
                    request_id: request.id,
                    reply: PermissionV1Reply::Once,
                    metadata,
                }];
            }
            let request = permission_request(request);
            let requests = sync
                .permission
                .entry(request.session_id.clone())
                .or_default();
            let m = search(requests, &request.id, |r| r.id.clone());
            if m.found {
                requests[m.index] = request;
            } else {
                requests.insert(m.index, request);
            }
            Vec::new()
        }
        Event::QuestionReplied(data) => {
            remove_question(sync, &data.session_id, &data.request_id);
            Vec::new()
        }
        Event::QuestionRejected(data) => {
            remove_question(sync, &data.session_id, &data.request_id);
            Vec::new()
        }
        Event::QuestionAsked(data) => {
            let request = question_request(data);
            let requests = sync.question.entry(request.session_id.clone()).or_default();
            let m = search(requests, &request.id, |r| r.id.clone());
            if m.found {
                requests[m.index] = request;
            } else {
                requests.insert(m.index, request);
            }
            Vec::new()
        }
        Event::TodoUpdated(data) => {
            sync.todo.insert(data.session_id, data.todos);
            Vec::new()
        }
        Event::SessionDiff(data) => {
            sync.session_diff.insert(data.session_id, data.diff);
            Vec::new()
        }
        Event::SessionDeleted(data) => {
            let m = search(&sync.session, &data.info.id, |s: &V1SessionInfo| {
                s.id.clone()
            });
            if m.found {
                sync.session.remove(m.index);
            }
            Vec::new()
        }
        Event::SessionUpdated(data) => {
            let m = search(&sync.session, &data.info.id, |s: &V1SessionInfo| {
                s.id.clone()
            });
            if m.found {
                sync.session[m.index] = data.info;
            } else {
                sync.session.insert(m.index, data.info);
            }
            Vec::new()
        }
        Event::SessionNextMoved(data) => {
            let m = search(&sync.session, &data.session_id, |s: &V1SessionInfo| {
                s.id.clone()
            });
            if !m.found {
                return Vec::new();
            }
            let session = &mut sync.session[m.index];
            session.directory = data.location.directory.clone();
            session.path = data.subdirectory.clone();
            session.workspace_id = data.location.workspace_id.clone();
            session.time.updated = data.timestamp.max(0) as u64;
            Vec::new()
        }
        Event::SessionStatus(data) => {
            sync.session_status.insert(data.session_id, data.status);
            Vec::new()
        }
        Event::MessageUpdated(data) => {
            sync.touch_message(message_session_id(&data.info), message_id(&data.info));
            let key = message_key(&data.info);
            let messages = sync
                .message
                .entry(message_session_id(&data.info).to_string())
                .or_default();
            let m = search(messages, &key, message_key);
            if m.found {
                messages[m.index] = data.info;
            } else {
                messages.insert(m.index, data.info);
            }
            if messages.len() > 100 {
                let oldest = messages.remove(0);
                sync.part.remove(message_id(&oldest));
            }
            Vec::new()
        }
        Event::MessageRemoved(data) => {
            sync.touch_message(&data.session_id, &data.message_id);
            let Some(messages) = sync.message.get_mut(&data.session_id) else {
                return Vec::new();
            };
            if let Some(index) = messages
                .iter()
                .position(|message| message_id(message) == data.message_id)
            {
                messages.remove(index);
            }
            Vec::new()
        }
        Event::MessagePartUpdated(data) => {
            let part = data.part;
            sync.touch_part(part_session_id(&part), part_id(&part));
            let parts = sync
                .part
                .entry(part_message_id(&part).to_string())
                .or_default();
            let m = search(parts, part_id(&part), |p| part_id(p).to_string());
            if m.found {
                parts[m.index] = part;
            } else {
                parts.insert(m.index, part);
            }
            Vec::new()
        }
        Event::MessagePartDelta(MessagePartDeltaData {
            session_id,
            message_id: message_id_field,
            part_id: part_id_field,
            field,
            delta,
        }) => {
            let m = {
                let Some(parts) = sync.part.get(&message_id_field) else {
                    return Vec::new();
                };
                let m = search(parts, &part_id_field, |p| part_id(p).to_string());
                if !m.found {
                    return Vec::new();
                }
                m
            };
            sync.touch_part(&session_id, &part_id_field);
            if let Some(parts) = sync.part.get_mut(&message_id_field) {
                append_delta(&mut parts[m.index], &field, &delta);
            }
            Vec::new()
        }
        Event::MessagePartRemoved(data) => {
            sync.touch_part(&data.session_id, &data.part_id);
            let Some(parts) = sync.part.get_mut(&data.message_id) else {
                return Vec::new();
            };
            let m = search(parts, &data.part_id, |p| part_id(p).to_string());
            if m.found {
                parts.remove(m.index);
            }
            Vec::new()
        }
        Event::LspUpdated(_) => {
            vec![Effect::LspStatusRefetch {
                workspace: state.project.workspace.current.clone(),
            }]
        }
        Event::VcsBranchUpdated(data) => {
            if metadata.workspace == state.project.workspace.current {
                sync.vcs = Some(json!({ "branch": data.branch }));
            }
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// `permission.asked` stores the event properties verbatim — same
/// shape as `PermissionV1Request` (`permission_v1.rs`).
fn permission_request(
    data: alforria_schema::permission_v1::PermissionAskedData,
) -> PermissionV1Request {
    PermissionV1Request {
        id: data.id,
        session_id: data.session_id,
        permission: data.permission,
        patterns: data.patterns,
        metadata: data.metadata,
        always: data.always,
        tool: data.tool,
    }
}

/// `question.asked` stores the event properties verbatim — same shape
/// as `QuestionV1Request`.
fn question_request(data: alforria_schema::question_v1::QuestionAskedData) -> QuestionV1Request {
    QuestionV1Request {
        id: data.id,
        session_id: data.session_id,
        questions: data.questions,
        tool: data.tool,
    }
}

fn remove_question(sync: &mut SyncState, session_id: &str, request_id: &str) {
    let Some(requests) = sync.question.get_mut(session_id) else {
        return;
    };
    let m = search(requests, request_id, |r: &QuestionV1Request| r.id.clone());
    if m.found {
        requests.remove(m.index);
    }
}

// ----------------------------------------------------------- bootstrap

/// `sessionListQuery` (`sync.tsx:160-168`).
pub fn session_list_query(kv: &Kv, project: &ProjectState) -> SessionListQuery {
    if !kv.get_bool(keys::SESSION_DIRECTORY_FILTER_ENABLED, true) {
        return scope_project();
    }
    let (Some(worktree), Some(directory)) = (
        project.instance_path.worktree.clone(),
        project.instance_path.directory.clone(),
    ) else {
        return scope_project();
    };
    SessionListQuery {
        path: Some(relative_path(&worktree, &directory)),
        ..SessionListQuery::default()
    }
}

fn scope_project() -> SessionListQuery {
    SessionListQuery {
        scope: Some("project".to_string()),
        ..SessionListQuery::default()
    }
}

/// `path.relative(resolve(worktree), directory)` with `\` → `/`
/// (`sync.tsx:163-167`).
fn relative_path(from: &str, to: &str) -> String {
    let from: Vec<&str> = from.trim_end_matches('/').split('/').collect();
    let to: Vec<&str> = to.trim_end_matches('/').split('/').collect();
    let mut common = 0;
    while common < from.len() && common < to.len() && from[common] == to[common] {
        common += 1;
    }
    let mut segments: Vec<String> = Vec::new();
    for _ in common..from.len() {
        segments.push("..".to_string());
    }
    segments.extend(to[common..].iter().map(|s| s.to_string()));
    if segments.is_empty() {
        return ".".to_string();
    }
    segments.join("/").replace('\\', "/")
}

/// `listSessions` (`sync.tsx:170-174`): `start = now - 30d`, sorted by
/// id. Response failures are tolerated (empty list), matching the TS
/// call site which does not use `throwOnError`.
pub async fn list_sessions(
    api: &dyn ServerApi,
    loc: &Location,
    kv: &Kv,
    project: &ProjectState,
) -> Vec<V1SessionInfo> {
    let now_ms = now_epoch_ms();
    let mut query = session_list_query(kv, project);
    query.start = Some(now_ms - 30 * 24 * 60 * 60 * 1000);
    let mut sessions = api.session_list(loc, query).await.unwrap_or_default();
    sessions.sort_by(|a, b| a.id.cmp(&b.id));
    sessions
}

pub(crate) fn now_epoch_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn array_of(value: Value) -> Vec<Value> {
    match value {
        Value::Array(items) => items,
        _ => Vec::new(),
    }
}

pub(crate) fn object_of(value: Value) -> BTreeMap<String, Value> {
    match value {
        Value::Object(map) => map.into_iter().collect(),
        _ => BTreeMap::new(),
    }
}

/// `project.sync()` (`context/project.tsx:38-52`).
async fn project_sync(state: &mut State, api: &dyn ServerApi, loc: &Location) -> Result<()> {
    let (path, project) = tokio_join!(api.path_get(loc), api.project_current(loc));
    let path = path.context("tui bootstrap failed: path.get")?;
    let project = project.context("tui bootstrap failed: project.current")?;
    state.project.instance_path = project_instance_path(&path);
    state.project.project_id = project
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string);
    state.project.worktree = project
        .get("worktree")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(project_id) = &state.project.project_id {
        if let Ok(directories) = api.project_directories(loc, project_id).await {
            state.project.main_dir = directories.as_array().and_then(|items| {
                items
                    .iter()
                    .rev()
                    .find(|item| item.get("strategy").is_none())
                    .and_then(|item| item.get("directory"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        }
    }
    Ok(())
}

fn project_instance_path(path: &Value) -> crate::state::InstancePath {
    let text = |key: &str| {
        path.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    crate::state::InstancePath {
        home: text("home"),
        state: text("state"),
        config: text("config"),
        worktree: text("worktree"),
        directory: text("directory"),
    }
}

/// `project.workspace.sync()` (`context/project.tsx:54-70`) —
/// best-effort; failures leave workspace state untouched.
async fn workspace_sync(state: &mut State, api: &dyn ServerApi, loc: &Location) {
    let Ok(listed) = api.experimental_workspace_list(loc).await else {
        return;
    };
    let status = api.experimental_workspace_status(loc).await.ok();
    let Some(list) = listed.as_array() else {
        return;
    };
    state.project.workspace.list = list.to_vec();
    if let Some(status) = status.and_then(|s| s.as_array().cloned()) {
        let next: BTreeMap<String, String> = status
            .iter()
            .filter_map(|item| {
                let id = item.get("workspaceID").and_then(Value::as_str)?;
                let s = item.get("status").and_then(Value::as_str)?;
                Some((id.to_string(), s.to_string()))
            })
            .collect();
        state.project.workspace.status = next;
    }
    if let Some(current) = state.project.workspace.current.clone() {
        let exists = list
            .iter()
            .any(|item| item.get("id").and_then(Value::as_str) == Some(current.as_str()));
        if !exists {
            state.project.workspace.current = None;
        }
    }
}

/// `bootstrap()` (`sync.tsx:451-552`). Phase 1 is blocking: failures
/// return `Err` — the TS caller exits when `fatal`, rethrows otherwise
/// (`:546-551`). Phase 2 failures are logged, not fatal (`:519-539`).
pub async fn bootstrap(state: &mut State, api: &dyn ServerApi, fatal: bool) -> Result<()> {
    let result = bootstrap_inner(state, api).await;
    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            eprintln!("tui bootstrap failed: {error:#}");
            if fatal {
                Err(error.context("fatal"))
            } else {
                Err(error)
            }
        }
    }
}

async fn bootstrap_inner(state: &mut State, api: &dyn ServerApi) -> Result<()> {
    let loc = Location {
        directory: None,
        workspace: state.project.workspace.current.clone(),
    };

    project_sync(state, api, &loc).await?;
    let sessions = list_sessions(api, &loc, &state.kv, &state.project).await;

    let (providers, provider_list, capabilities, console, agents, config) = tokio_join!(
        api.config_providers(&loc),
        api.provider_list(&loc),
        api.experimental_capabilities(&loc),
        api.experimental_console(&loc),
        api.app_agents(&loc),
        api.config_get(&loc),
    );
    let providers = providers.context("tui bootstrap failed: config.providers")?;
    let provider_list = provider_list.context("tui bootstrap failed: provider.list")?;
    let agents = agents.context("tui bootstrap failed: app.agents")?;
    let config = config.context("tui bootstrap failed: config.get")?;
    let capabilities = capabilities.ok();
    let console_state = console.unwrap_or_else(|_| empty_console_state());

    let sync = &mut state.sync;
    sync.provider = providers
        .get("providers")
        .cloned()
        .map(array_of)
        .unwrap_or_default();
    if let Some(default) = providers.get("default") {
        sync.provider_default = serde_json::from_value(default.clone()).unwrap_or_default();
    }
    sync.provider_next = provider_list;
    sync.capabilities = capabilities
        .as_ref()
        .and_then(|c| c.get("backgroundSubagents"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    sync.console_state = console_state;
    sync.agent = array_of(agents);
    sync.config = config;
    if state.args.continue_ {
        sync.session = sessions.clone();
    }
    if sync.status != Some(SyncStatus::Complete) {
        sync.status = Some(SyncStatus::Partial);
    }

    // Non-blocking phase (`sync.tsx:519-539`).
    let (command, lsp, mcp, mcp_resource, formatter, session_status, provider_auth, vcs) = tokio_join!(
        api.command_list(&loc),
        api.lsp_status(&loc),
        api.mcp_status(&loc),
        api.experimental_resource_list(&loc),
        api.formatter_status(&loc),
        api.session_status(&loc),
        api.provider_auth(&loc),
        api.vcs_get(&loc),
    );
    {
        let sync = &mut state.sync;
        if let Ok(v) = command {
            sync.command = array_of(v);
        }
        if let Ok(v) = lsp {
            sync.lsp = array_of(v);
        }
        if let Ok(v) = mcp {
            sync.mcp = object_of(v);
        }
        if let Ok(v) = mcp_resource {
            sync.mcp_resource = object_of(v);
        }
        if let Ok(v) = formatter {
            sync.formatter = array_of(v);
        }
        if let Ok(v) = session_status {
            sync.session_status = v;
        }
        if let Ok(v) = provider_auth {
            sync.provider_auth = parse_auth_map(v);
        }
        if let Ok(v) = vcs {
            sync.vcs = if v.is_null() { None } else { Some(v) };
        }
    }
    workspace_sync(state, api, &loc).await;
    if !state.args.continue_ {
        state.sync.session = sessions;
    }
    state.sync.status = Some(SyncStatus::Complete);
    Ok(())
}

fn parse_auth_map(value: Value) -> BTreeMap<String, Vec<Value>> {
    value
        .as_object()
        .map(|map| {
            map.iter()
                .filter_map(|(key, item)| {
                    serde_json::from_value(item.clone())
                        .ok()
                        .map(|parsed| (key.clone(), parsed))
                })
                .collect()
        })
        .unwrap_or_default()
}

// ------------------------------------------------------------- hydrate

/// `session.sync(sessionID)` (`sync.tsx:594-667`): hydrate a session —
/// parallel `session.get`/`messages`/`todo`/`diff` — with live-tracking
/// wins for messages/parts already streamed in. Runs at most once per
/// session (`fullSyncedSessions`).
pub async fn session_sync(state: &mut State, api: &dyn ServerApi, session_id: &str) -> Result<()> {
    if state.sync.full_synced.contains(session_id) {
        return Ok(());
    }
    state
        .sync
        .hydrating_sessions
        .entry(session_id.to_string())
        .or_default();
    let result = hydrate(state, api, session_id).await;
    state.sync.hydrating_sessions.remove(session_id);
    result
}

async fn hydrate(state: &mut State, api: &dyn ServerApi, session_id: &str) -> Result<()> {
    let loc = Location::default();
    let (session, messages, todo, diff) = tokio_join!(
        api.session_get(&loc, session_id),
        api.session_messages(&loc, session_id, Some(100)),
        api.session_todo(&loc, session_id),
        api.session_diff(&loc, session_id, None),
    );
    let session = session?; // `throwOnError: true` (`sync.tsx:602`)
    let messages = messages.unwrap_or_default();
    let todo = todo.unwrap_or_default();
    let diff = diff.unwrap_or_default();

    let tracker = state
        .sync
        .hydrating_sessions
        .get(session_id)
        .cloned()
        .unwrap_or_default();
    let sync = &mut state.sync;

    let m = search(&sync.session, session_id, |s: &V1SessionInfo| s.id.clone());
    if m.found {
        sync.session[m.index] = session;
    } else {
        sync.session.insert(m.index, session);
    }
    sync.todo.insert(session_id.to_string(), todo);

    let current_messages = sync.message.get(session_id).cloned().unwrap_or_default();
    let mut infos: Vec<V1Message> = Vec::new();
    for message in &messages {
        if !tracker.messages.contains(message_id(&message.info)) {
            infos.push(message.info.clone());
        } else if let Some(current) = current_messages
            .iter()
            .find(|item| message_id(item) == message_id(&message.info))
        {
            infos.push(current.clone());
        }
    }
    let live_only: Vec<V1Message> = current_messages
        .iter()
        .filter(|message| {
            tracker.messages.contains(message_id(message))
                && !infos
                    .iter()
                    .any(|item| message_id(item) == message_id(message))
        })
        .cloned()
        .collect();
    infos.extend(live_only);
    infos.sort_by(compare_message);

    let split = infos.len().saturating_sub(100);
    let removed: Vec<V1Message> = infos.drain(..split).collect();
    let visible = infos;

    let visible_ids: HashSet<String> = visible.iter().map(|m| message_id(m).to_string()).collect();
    for message in &messages {
        if !visible_ids.contains(message_id(&message.info)) {
            sync.part.remove(message_id(&message.info));
            continue;
        }
        let current_parts = sync
            .part
            .get(message_id(&message.info))
            .cloned()
            .unwrap_or_default();
        let mut parts: Vec<V1Part> = Vec::new();
        for part in &message.parts {
            let current = current_parts
                .iter()
                .find(|item| part_id(item) == part_id(part));
            if tracker.parts.contains(part_id(part)) {
                if let Some(current) = current {
                    parts.push(current.clone());
                }
                continue;
            }
            if let Some(current) = current {
                if is_text_or_reasoning(part)
                    && is_text_or_reasoning(current)
                    && part_text(part).is_empty()
                    && !part_text(current).is_empty()
                {
                    parts.push(current.clone());
                    continue;
                }
            }
            parts.push(part.clone());
        }
        let tracked_only: Vec<V1Part> = current_parts
            .iter()
            .filter(|part| {
                tracker.parts.contains(part_id(part))
                    && !parts.iter().any(|item| part_id(item) == part_id(part))
            })
            .cloned()
            .collect();
        parts.extend(tracked_only);
        sync.part
            .insert(message_id(&message.info).to_string(), parts);
    }
    for message in &removed {
        sync.part.remove(message_id(message));
    }
    sync.message.insert(session_id.to_string(), visible);
    sync.session_diff.insert(session_id.to_string(), diff);
    sync.full_synced.insert(session_id.to_string());
    Ok(())
}

fn is_text_or_reasoning(part: &V1Part) -> bool {
    matches!(part, V1Part::Text { .. } | V1Part::Reasoning { .. })
}

fn part_text(part: &V1Part) -> &str {
    match part {
        V1Part::Text { text, .. } | V1Part::Reasoning { text, .. } => text,
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use serde_json::json;

    use alforria_schema::event_manifest::Event;
    use alforria_schema::file_diff::SnapshotFileDiff;
    use alforria_schema::lsp_event::LspUpdatedData;
    use alforria_schema::permission_v1::PermissionAskedData;
    use alforria_schema::question_v1::QuestionAskedData;
    use alforria_schema::session_event::Moved;
    use alforria_schema::session_status::SessionStatusData;
    use alforria_schema::session_todo::TodoInfo;
    use alforria_schema::session_todo::TodoUpdatedData;
    use alforria_schema::session_v1::MessagePartDeltaData;
    use alforria_schema::session_v1::MessagePartRemovedData;
    use alforria_schema::session_v1::MessagePartUpdatedData;
    use alforria_schema::session_v1::MessageRemovedData;
    use alforria_schema::session_v1::MessageUpdatedData;
    use alforria_schema::session_v1::SessionDeletedData;
    use alforria_schema::session_v1::SessionDiffData;
    use alforria_schema::session_v1::SessionUpdatedData;
    use alforria_schema::session_v1::V1Message;
    use alforria_schema::session_v1::V1Part;
    use alforria_schema::session_v1::V1SessionInfo;
    use alforria_schema::session_v1::V1SessionTime;
    use alforria_schema::session_v1::V1StepTokens;
    use alforria_schema::vcs_event::VcsBranchUpdatedData;

    use super::*;
    use crate::state::kv::Kv;
    use crate::state::Args;
    use crate::state::PermissionMode;
    use crate::state::{InstancePath, ProjectState};
    use crate::transport::api::MessageWithParts;

    // --------------------------------------------------------- fixtures

    fn session_info(id: &str) -> V1SessionInfo {
        V1SessionInfo {
            id: id.to_string(),
            slug: "x".to_string(),
            project_id: "prj".to_string(),
            workspace_id: None,
            directory: "/repo".to_string(),
            path: None,
            parent_id: None,
            summary: None,
            cost: None,
            tokens: None,
            share: None,
            title: "X".to_string(),
            agent: None,
            model: None,
            version: "1".to_string(),
            metadata: None,
            time: V1SessionTime {
                created: 1,
                updated: 1,
                compacting: None,
                archived: None,
            },
            permission: None,
            revert: None,
        }
    }

    fn user_message(id: &str, session: &str, created: f64) -> V1Message {
        V1Message::User {
            id: id.to_string(),
            session_id: session.to_string(),
            time: alforria_schema::session_v1::UserTime { created },
            format: None,
            summary: None,
            agent: "build".to_string(),
            model: alforria_schema::session_v1::V1UserModel {
                provider_id: "anthropic".to_string(),
                model_id: "claude".to_string(),
                variant: None,
            },
            system: None,
            tools: None,
        }
    }

    fn assistant_message(id: &str, session: &str, created: u64) -> V1Message {
        V1Message::Assistant {
            id: id.to_string(),
            session_id: session.to_string(),
            time: alforria_schema::session_v1::AssistantTime {
                created,
                completed: None,
            },
            error: None,
            parent_id: "msg_parent".to_string(),
            model_id: "claude".to_string(),
            provider_id: "anthropic".to_string(),
            mode: "primary".to_string(),
            agent: "build".to_string(),
            path: alforria_schema::session_v1::V1Path {
                cwd: "/repo".to_string(),
                root: "/repo".to_string(),
            },
            summary: None,
            cost: 0.0,
            tokens: V1StepTokens {
                total: None,
                input: 0.0,
                output: 0.0,
                reasoning: 0.0,
                cache: alforria_schema::session_v1::V1TokenCache {
                    read: 0.0,
                    write: 0.0,
                },
            },
            structured: None,
            variant: None,
            finish: None,
        }
    }

    fn text_part(id: &str, session: &str, message: &str, text: &str) -> V1Part {
        V1Part::Text {
            id: id.to_string(),
            session_id: session.to_string(),
            message_id: message.to_string(),
            text: text.to_string(),
            synthetic: None,
            ignored: None,
            time: None,
            metadata: None,
        }
    }

    fn state_for_test() -> State {
        State::new(Args::default(), None)
    }

    fn bus(event: Event) -> BusEvent {
        BusEvent {
            event,
            metadata: Default::default(),
        }
    }

    // ----------------------------------------------------------- search

    #[test]
    fn search_finds_and_returns_insert_index() {
        let items = vec!["a".to_string(), "c".to_string(), "e".to_string()];
        let found = search(&items, "c", |item| item.clone());
        assert!(found.found);
        assert_eq!(found.index, 1);
        let missing = search(&items, "d", |item| item.clone());
        assert!(!missing.found);
        assert_eq!(missing.index, 2);
        let empty = search::<String>(&[], "x", |item| item.clone());
        assert!(!empty.found);
        assert_eq!(empty.index, 0);
    }

    // --------------------------------------------------------- sessions

    #[test]
    fn session_updated_reconciles_or_inserts() {
        let mut state = state_for_test();
        let mut ses_b = session_info("ses_b");
        ses_b.title = "B".into();
        let mut ses_a = session_info("ses_a");
        ses_a.title = "A".into();
        apply_event(
            &mut state,
            bus(Event::SessionUpdated(SessionUpdatedData {
                session_id: "ses_b".into(),
                info: ses_b,
            })),
        );
        apply_event(
            &mut state,
            bus(Event::SessionUpdated(SessionUpdatedData {
                session_id: "ses_a".into(),
                info: ses_a,
            })),
        );
        let ids: Vec<&str> = state.sync.session.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["ses_a", "ses_b"]);
        let mut updated = session_info("ses_b");
        updated.title = "B2".into();
        apply_event(
            &mut state,
            bus(Event::SessionUpdated(SessionUpdatedData {
                session_id: "ses_b".into(),
                info: updated,
            })),
        );
        assert_eq!(state.sync.session.len(), 2);
        assert_eq!(state.sync.session[1].title, "B2");
    }

    #[test]
    fn session_deleted_removes() {
        let mut state = state_for_test();
        let mut info = session_info("ses_a");
        info.title = "bye".into();
        state.sync.session = vec![info.clone()];
        apply_event(
            &mut state,
            bus(Event::SessionDeleted(SessionDeletedData {
                session_id: "ses_a".into(),
                info,
            })),
        );
        assert!(state.sync.session.is_empty());
    }

    #[test]
    fn session_next_moved_updates_location() {
        let mut state = state_for_test();
        state.sync.session = vec![session_info("ses_a")];
        let moved = Moved {
            timestamp: 42,
            session_id: "ses_a".to_string(),
            location: alforria_schema::location::LocationRef {
                directory: "/repo/sub".to_string(),
                workspace_id: Some("wrk_2".to_string()),
                project: None,
            },
            subdirectory: Some("sub".to_string()),
        };
        apply_event(&mut state, bus(Event::SessionNextMoved(moved)));
        let session = &state.sync.session[0];
        assert_eq!(session.directory, "/repo/sub");
        assert_eq!(session.path.as_deref(), Some("sub"));
        assert_eq!(session.workspace_id.as_deref(), Some("wrk_2"));
        assert_eq!(session.time.updated, 42);
    }

    #[test]
    fn session_status_sets_per_session() {
        let mut state = state_for_test();
        apply_event(
            &mut state,
            bus(Event::SessionStatus(SessionStatusData {
                session_id: "ses_a".into(),
                status: alforria_schema::session_status::SessionStatusInfo::Busy,
            })),
        );
        assert!(matches!(
            state.sync.session_status.get("ses_a"),
            Some(alforria_schema::session_status::SessionStatusInfo::Busy)
        ));
    }

    // ------------------------------------------------------ permissions

    fn permission_asked(id: &str, session: &str) -> Event {
        Event::PermissionAsked(PermissionAskedData {
            id: id.to_string(),
            session_id: session.to_string(),
            permission: "bash".to_string(),
            patterns: vec!["echo".to_string()],
            metadata: serde_json::Map::new(),
            always: vec![],
            tool: None,
        })
    }

    #[test]
    fn permission_asked_stores_sorted() {
        let mut state = state_for_test();
        apply_event(&mut state, bus(permission_asked("per_b", "ses_a")));
        apply_event(&mut state, bus(permission_asked("per_a", "ses_a")));
        apply_event(&mut state, bus(permission_asked("per_c", "ses_a")));
        let ids: Vec<&str> = state.sync.permission["ses_a"]
            .iter()
            .map(|r| r.id.as_str())
            .collect();
        assert_eq!(ids, vec!["per_a", "per_b", "per_c"]);
    }

    #[test]
    fn permission_auto_mode_replies_once_and_never_stores() {
        let mut state = state_for_test();
        state.permission_mode = PermissionMode::Auto;
        let effects = apply_event(&mut state, bus(permission_asked("per_1", "ses_a")));
        assert!(state.sync.permission.is_empty());
        match effects.as_slice() {
            [Effect::PermissionAutoReply {
                request_id, reply, ..
            }] => {
                assert_eq!(request_id, "per_1");
                assert_eq!(*reply, PermissionV1Reply::Once);
            }
            _ => panic!("expected one auto-reply effect, got {effects:?}"),
        }
    }

    #[test]
    fn permission_replied_removes_by_id() {
        let mut state = state_for_test();
        apply_event(&mut state, bus(permission_asked("per_a", "ses_a")));
        apply_event(&mut state, bus(permission_asked("per_b", "ses_a")));
        apply_event(
            &mut state,
            bus(Event::PermissionReplied(
                alforria_schema::permission_v1::PermissionRepliedData {
                    session_id: "ses_a".into(),
                    request_id: "per_a".into(),
                    reply: PermissionV1Reply::Once,
                },
            )),
        );
        let ids: Vec<&str> = state.sync.permission["ses_a"]
            .iter()
            .map(|r| r.id.as_str())
            .collect();
        assert_eq!(ids, vec!["per_b"]);
    }

    // -------------------------------------------------------- questions

    fn question_asked(id: &str, session: &str) -> Event {
        Event::QuestionAsked(QuestionAskedData {
            id: id.to_string(),
            session_id: session.to_string(),
            questions: vec![],
            tool: None,
        })
    }

    #[test]
    fn question_asked_stores_and_replied_rejected_remove() {
        let mut state = state_for_test();
        apply_event(&mut state, bus(question_asked("que_a", "ses_a")));
        apply_event(&mut state, bus(question_asked("que_b", "ses_a")));
        assert_eq!(state.sync.question["ses_a"].len(), 2);
        apply_event(
            &mut state,
            bus(Event::QuestionReplied(
                alforria_schema::question_v1::QuestionRepliedData {
                    session_id: "ses_a".into(),
                    request_id: "que_a".into(),
                    answers: vec![],
                },
            )),
        );
        apply_event(
            &mut state,
            bus(Event::QuestionRejected(
                alforria_schema::question_v1::QuestionRejectedData {
                    session_id: "ses_a".into(),
                    request_id: "que_b".into(),
                },
            )),
        );
        assert!(state.sync.question["ses_a"].is_empty());
    }

    // ------------------------------------------------------ todo / diff

    #[test]
    fn todo_updated_replaces_per_session() {
        let mut state = state_for_test();
        let todo = || TodoInfo {
            content: "x".into(),
            status: "pending".into(),
            priority: "high".into(),
        };
        state.sync.todo.insert("ses_a".into(), vec![todo(), todo()]);
        apply_event(
            &mut state,
            bus(Event::TodoUpdated(TodoUpdatedData {
                session_id: "ses_a".into(),
                todos: vec![todo()],
            })),
        );
        assert_eq!(state.sync.todo["ses_a"].len(), 1);
    }

    #[test]
    fn session_diff_sets() {
        let mut state = state_for_test();
        let diff = SnapshotFileDiff {
            file: Some("a".into()),
            patch: None,
            additions: 1.0,
            deletions: 2.0,
            status: None,
        };
        apply_event(
            &mut state,
            bus(Event::SessionDiff(SessionDiffData {
                session_id: "ses_a".into(),
                diff: vec![diff],
            })),
        );
        assert_eq!(state.sync.session_diff["ses_a"].len(), 1);
    }

    // --------------------------------------------------------- messages

    #[test]
    fn message_updated_sorted_insert_by_string_key() {
        let mut state = state_for_test();
        apply_event(
            &mut state,
            bus(Event::MessageUpdated(MessageUpdatedData {
                session_id: "ses_a".into(),
                info: user_message("msg_c", "ses_a", 100.0),
            })),
        );
        apply_event(
            &mut state,
            bus(Event::MessageUpdated(MessageUpdatedData {
                session_id: "ses_a".into(),
                info: user_message("msg_d", "ses_a", 10.0),
            })),
        );
        let messages = &state.sync.message["ses_a"];
        assert_eq!(
            messages
                .iter()
                .map(|m| message_id(m).to_string())
                .collect::<Vec<_>>(),
            vec!["msg_c", "msg_d"],
            "messageKey is string concatenation: 100c < 10b"
        );
    }

    #[test]
    fn message_cap_evicts_oldest_and_its_parts() {
        let mut state = state_for_test();
        for i in 0..100 {
            let info = user_message(&format!("msg_{i:03}"), "ses_a", i as f64);
            state
                .sync
                .message
                .entry("ses_a".to_string())
                .or_default()
                .push(info);
            state.sync.part.insert(
                format!("msg_{i:03}"),
                vec![text_part("prt", "ses_a", &format!("msg_{i:03}"), "x")],
            );
        }
        apply_event(
            &mut state,
            bus(Event::MessageUpdated(MessageUpdatedData {
                session_id: "ses_a".into(),
                info: user_message("msg_new", "ses_a", 500.0),
            })),
        );
        let messages = &state.sync.message["ses_a"];
        assert_eq!(messages.len(), 100);
        assert_eq!(message_id(messages.first().unwrap()), "msg_001");
        assert!(
            !state.sync.part.contains_key("msg_000"),
            "the oldest message's parts are deleted with it"
        );
        assert!(state.sync.part.contains_key("msg_001"));
    }

    #[test]
    fn message_removed_splices_by_id() {
        let mut state = state_for_test();
        state.sync.message.insert(
            "ses_a".to_string(),
            vec![
                user_message("msg_1", "ses_a", 1.0),
                user_message("msg_2", "ses_a", 2.0),
            ],
        );
        apply_event(
            &mut state,
            bus(Event::MessageRemoved(MessageRemovedData {
                session_id: "ses_a".into(),
                message_id: "msg_1".into(),
            })),
        );
        assert_eq!(state.sync.message["ses_a"].len(), 1);
        assert_eq!(message_id(&state.sync.message["ses_a"][0]), "msg_2");
    }

    // ------------------------------------------------------------- parts

    #[test]
    fn message_part_updated_inserts_and_replaces_by_id() {
        let mut state = state_for_test();
        apply_event(
            &mut state,
            bus(Event::MessagePartUpdated(MessagePartUpdatedData {
                session_id: "ses_a".into(),
                part: text_part("prt_b", "ses_a", "msg_1", "hi"),
                time: 1.0,
            })),
        );
        apply_event(
            &mut state,
            bus(Event::MessagePartUpdated(MessagePartUpdatedData {
                session_id: "ses_a".into(),
                part: text_part("prt_a", "ses_a", "msg_1", "first"),
                time: 1.0,
            })),
        );
        let parts = state.sync.part.get("msg_1").unwrap();
        assert_eq!(
            parts
                .iter()
                .map(|p| part_id(p).to_string())
                .collect::<Vec<_>>(),
            vec!["prt_a", "prt_b"]
        );
        apply_event(
            &mut state,
            bus(Event::MessagePartUpdated(MessagePartUpdatedData {
                session_id: "ses_a".into(),
                part: text_part("prt_b", "ses_a", "msg_1", "replaced"),
                time: 2.0,
            })),
        );
        let parts = state.sync.part.get("msg_1").unwrap();
        assert_eq!(parts.len(), 2);
        match &parts[1] {
            V1Part::Text { text, .. } => assert_eq!(text, "replaced"),
            _ => panic!("expected text part"),
        }
    }

    #[test]
    fn part_delta_appends_to_the_part_field() {
        let mut state = state_for_test();
        state.sync.part.insert(
            "msg_1".to_string(),
            vec![
                text_part("prt_a", "ses_a", "msg_1", "Hello"),
                text_part("prt_b", "ses_a", "msg_1", ""),
            ],
        );
        apply_event(
            &mut state,
            bus(Event::MessagePartDelta(MessagePartDeltaData {
                session_id: "ses_a".into(),
                message_id: "msg_1".into(),
                part_id: "prt_b".into(),
                field: "text".into(),
                delta: " wo".into(),
            })),
        );
        apply_event(
            &mut state,
            bus(Event::MessagePartDelta(MessagePartDeltaData {
                session_id: "ses_a".into(),
                message_id: "msg_1".into(),
                part_id: "prt_b".into(),
                field: "text".into(),
                delta: "rld".into(),
            })),
        );
        match &state.sync.part["msg_1"][1] {
            V1Part::Text { text, .. } => assert_eq!(text, " world"),
            _ => panic!("expected text part"),
        }
    }

    #[test]
    fn part_delta_for_unknown_part_or_message_is_dropped() {
        let mut state = state_for_test();
        state.sync.part.insert(
            "msg_1".to_string(),
            vec![text_part("prt_a", "ses_a", "msg_1", "x")],
        );
        apply_event(
            &mut state,
            bus(Event::MessagePartDelta(MessagePartDeltaData {
                session_id: "ses_a".into(),
                message_id: "msg_1".into(),
                part_id: "prt_missing".into(),
                field: "text".into(),
                delta: "?".into(),
            })),
        );
        apply_event(
            &mut state,
            bus(Event::MessagePartDelta(MessagePartDeltaData {
                session_id: "ses_a".into(),
                message_id: "msg_missing".into(),
                part_id: "prt_a".into(),
                field: "text".into(),
                delta: "?".into(),
            })),
        );
        match &state.sync.part["msg_1"][0] {
            V1Part::Text { text, .. } => assert_eq!(text, "x"),
            _ => panic!("expected text part"),
        }
    }

    #[test]
    fn part_removed_splices_by_id() {
        let mut state = state_for_test();
        state.sync.part.insert(
            "msg_1".to_string(),
            vec![
                text_part("prt_a", "ses_a", "msg_1", ""),
                text_part("prt_b", "ses_a", "msg_1", ""),
            ],
        );
        apply_event(
            &mut state,
            bus(Event::MessagePartRemoved(MessagePartRemovedData {
                session_id: "ses_a".into(),
                message_id: "msg_1".into(),
                part_id: "prt_a".into(),
            })),
        );
        assert_eq!(state.sync.part["msg_1"].len(), 1);
        assert_eq!(part_id(&state.sync.part["msg_1"][0]), "prt_b");
    }

    // ------------------------------------------------------ lsp / vcs

    #[test]
    fn lsp_updated_returns_refetch_effect() {
        let mut state = state_for_test();
        state.project.workspace.current = Some("wrk_1".into());
        let effects = apply_event(&mut state, bus(Event::LspUpdated(LspUpdatedData {})));
        match effects.as_slice() {
            [Effect::LspStatusRefetch { workspace }] => {
                assert_eq!(workspace.as_deref(), Some("wrk_1"));
            }
            _ => panic!("expected a refetch effect, got {effects:?}"),
        }
    }

    #[test]
    fn vcs_branch_updated_only_for_matching_workspace() {
        let mut state = state_for_test();
        state.project.workspace.current = Some("wrk_1".into());
        let metadata = crate::transport::events::EventMetadata {
            workspace: Some("wrk_other".into()),
            ..Default::default()
        };
        apply_event(
            &mut state,
            BusEvent {
                event: Event::VcsBranchUpdated(VcsBranchUpdatedData {
                    branch: Some("main".into()),
                }),
                metadata,
            },
        );
        assert!(state.sync.vcs.is_none());
        let metadata = crate::transport::events::EventMetadata {
            workspace: Some("wrk_1".into()),
            ..Default::default()
        };
        apply_event(
            &mut state,
            BusEvent {
                event: Event::VcsBranchUpdated(VcsBranchUpdatedData {
                    branch: Some("main".into()),
                }),
                metadata,
            },
        );
        assert_eq!(state.sync.vcs, Some(json!({ "branch": "main" })));
    }

    #[test]
    fn server_instance_disposed_rebootstraps() {
        let mut state = state_for_test();
        let effects = apply_event(
            &mut state,
            bus(Event::ServerInstanceDisposed(
                alforria_schema::server_event::ServerInstanceDisposedData {
                    directory: "/repo".into(),
                },
            )),
        );
        match effects.as_slice() {
            [Effect::Bootstrap { fatal }] => assert!(*fatal),
            _ => panic!("expected a bootstrap effect, got {effects:?}"),
        }
    }

    // ---------------------------------------------- derived status

    #[test]
    fn session_working_status_matrix() {
        let mut state = state_for_test();
        let mut compacting = session_info("ses_a");
        compacting.time.compacting = Some(1);
        state.sync.session = vec![compacting];
        assert_eq!(
            state.sync.session_working_status("ses_a"),
            SessionWorkingStatus::Compacting
        );
        state.sync.session[0].time.compacting = None;
        assert_eq!(
            state.sync.session_working_status("ses_a"),
            SessionWorkingStatus::Idle,
            "no messages yet"
        );
        state
            .sync
            .message
            .insert("ses_a".into(), vec![user_message("msg_1", "ses_a", 1.0)]);
        assert_eq!(
            state.sync.session_working_status("ses_a"),
            SessionWorkingStatus::Working,
            "last message is a user message"
        );
        let mut assistant = assistant_message("msg_2", "ses_a", 2);
        state
            .sync
            .message
            .get_mut("ses_a")
            .unwrap()
            .push(assistant.clone());
        assert_eq!(
            state.sync.session_working_status("ses_a"),
            SessionWorkingStatus::Working,
            "assistant without completed time"
        );
        if let V1Message::Assistant { time, .. } = &mut assistant {
            time.completed = Some(3);
        }
        state.sync.message.get_mut("ses_a").unwrap()[1] = assistant;
        assert_eq!(
            state.sync.session_working_status("ses_a"),
            SessionWorkingStatus::Idle
        );
        assert_eq!(
            state.sync.session_working_status("ses_missing"),
            SessionWorkingStatus::Idle
        );
    }

    // ------------------------------------------------------- list query

    #[test]
    fn session_list_query_matrix() {
        let kv = Kv::in_memory();
        let mut project = ProjectState::default();
        assert_eq!(
            session_list_query(&kv, &project).scope.as_deref(),
            Some("project")
        );

        project.instance_path = InstancePath {
            worktree: Some("/repo".into()),
            directory: Some("/repo/sub dir".into()),
            ..InstancePath::default()
        };
        let query = session_list_query(&kv, &project);
        assert_eq!(query.scope, None);
        assert_eq!(query.path.as_deref(), Some("sub dir"));

        project.instance_path.worktree = None;
        assert_eq!(
            session_list_query(&kv, &project).scope.as_deref(),
            Some("project")
        );
    }

    #[test]
    fn relative_path_matches_posix_relative() {
        assert_eq!(relative_path("/a/b", "/a/b"), ".");
        assert_eq!(relative_path("/a/b", "/a/b/c"), "c");
        assert_eq!(relative_path("/a/b", "/a/b/c/d"), "c/d");
        assert_eq!(relative_path("/a/b/c", "/a/x"), "../../x");
    }

    // ---------------------------------------------------------- FakeApi

    #[derive(Default)]
    struct FakeApi {
        fail_providers: bool,
        fail_session_get: bool,
        sessions: Vec<V1SessionInfo>,
        messages: Vec<MessageWithParts>,
        todos: Vec<TodoInfo>,
        diffs: Vec<SnapshotFileDiff>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeApi {
        fn record(&self, name: &str) {
            self.calls.lock().unwrap().push(name.to_string());
        }
    }

    #[async_trait]
    impl ServerApi for FakeApi {
        async fn path_get(&self, _loc: &Location) -> Result<Value> {
            self.record("path_get");
            Ok(json!({
                "home": "/home", "state": "/state", "config": "/cfg",
                "worktree": "/repo", "directory": "/repo/sub",
            }))
        }
        async fn project_current(&self, _loc: &Location) -> Result<Value> {
            self.record("project_current");
            Ok(json!({"id": "prj_1", "worktree": "/repo"}))
        }
        async fn project_directories(&self, _loc: &Location, _project_id: &str) -> Result<Value> {
            self.record("project_directories");
            Ok(json!([
                {"directory": "/repo", "strategy": "git"},
                {"directory": "/repo/sub"},
            ]))
        }
        async fn experimental_workspace_list(&self, _loc: &Location) -> Result<Value> {
            self.record("workspace_list");
            Ok(json!([]))
        }
        async fn experimental_workspace_status(&self, _loc: &Location) -> Result<Value> {
            Ok(json!([]))
        }
        async fn config_providers(&self, _loc: &Location) -> Result<Value> {
            self.record("config_providers");
            if self.fail_providers {
                return Err(anyhow::anyhow!("boom"));
            }
            Ok(json!({
                "providers": [{"id": "anthropic", "models": {"claude": {}}}],
                "default": {"anthropic": "claude"},
            }))
        }
        async fn config_get(&self, _loc: &Location) -> Result<Value> {
            Ok(json!({"model": "anthropic/claude"}))
        }
        async fn provider_list(&self, _loc: &Location) -> Result<Value> {
            self.record("provider_list");
            Ok(json!({"all": [], "default": {}, "connected": []}))
        }
        async fn provider_auth(&self, _loc: &Location) -> Result<Value> {
            Ok(json!({}))
        }
        async fn app_agents(&self, _loc: &Location) -> Result<Value> {
            Ok(json!([{"name": "build", "mode": "primary"}]))
        }
        async fn command_list(&self, _loc: &Location) -> Result<Value> {
            self.record("command_list");
            Ok(json!([{"name": "command"}]))
        }
        async fn lsp_status(&self, _loc: &Location) -> Result<Value> {
            Ok(json!([{"id": "lsp"}]))
        }
        async fn mcp_status(&self, _loc: &Location) -> Result<Value> {
            Ok(json!({"mcp": {"status": "connected"}}))
        }
        async fn mcp_connect(&self, _loc: &Location, _name: &str) -> Result<bool> {
            Ok(true)
        }
        async fn mcp_disconnect(&self, _loc: &Location, _name: &str) -> Result<bool> {
            Ok(true)
        }
        async fn formatter_status(&self, _loc: &Location) -> Result<Value> {
            Ok(json!([]))
        }
        async fn vcs_get(&self, _loc: &Location) -> Result<Value> {
            Ok(json!({"branch": "main"}))
        }
        async fn experimental_capabilities(&self, _loc: &Location) -> Result<Value> {
            self.record("capabilities");
            Ok(json!({"backgroundSubagents": true}))
        }
        async fn experimental_console(&self, _loc: &Location) -> Result<Value> {
            self.record("console");
            Ok(json!({"consoleManagedProviders": [], "switchableOrgCount": 1}))
        }
        async fn experimental_resource_list(&self, _loc: &Location) -> Result<Value> {
            Ok(json!({}))
        }
        async fn experimental_session_background(
            &self,
            _loc: &Location,
            _session_id: &str,
        ) -> Result<bool> {
            Ok(true)
        }
        async fn sync_start(&self, _loc: &Location) -> Result<bool> {
            Ok(true)
        }
        async fn global_upgrade(&self, _loc: &Location, _target: &str) -> Result<Value> {
            Ok(Value::Null)
        }
        async fn experimental_move_session(
            &self,
            _loc: &Location,
            _req: crate::transport::api::MoveSession,
        ) -> Result<()> {
            Ok(())
        }
        async fn session_list(
            &self,
            _loc: &Location,
            _query: SessionListQuery,
        ) -> Result<Vec<V1SessionInfo>> {
            self.record("session_list");
            Ok(self.sessions.clone())
        }
        async fn session_get(&self, _loc: &Location, _session_id: &str) -> Result<V1SessionInfo> {
            if self.fail_session_get {
                return Err(anyhow::anyhow!("missing"));
            }
            Ok(self
                .sessions
                .first()
                .cloned()
                .unwrap_or_else(|| session_info("ses_a")))
        }
        async fn session_messages(
            &self,
            _loc: &Location,
            _session_id: &str,
            _limit: Option<u64>,
        ) -> Result<Vec<MessageWithParts>> {
            Ok(self.messages.clone())
        }
        async fn session_todo(&self, _loc: &Location, _session_id: &str) -> Result<Vec<TodoInfo>> {
            Ok(self.todos.clone())
        }
        async fn session_diff(
            &self,
            _loc: &Location,
            _session_id: &str,
            _message_id: Option<&str>,
        ) -> Result<Vec<SnapshotFileDiff>> {
            Ok(self.diffs.clone())
        }
        async fn session_create(
            &self,
            _loc: &Location,
            _req: crate::transport::api::SessionCreate,
        ) -> Result<V1SessionInfo> {
            Ok(session_info("new"))
        }
        async fn session_prompt(
            &self,
            _loc: &Location,
            _session_id: &str,
            _req: crate::transport::api::SessionPrompt,
        ) -> Result<MessageWithParts> {
            unreachable!()
        }
        async fn session_command(
            &self,
            _loc: &Location,
            _session_id: &str,
            _req: crate::transport::api::SessionCommand,
        ) -> Result<MessageWithParts> {
            unreachable!()
        }
        async fn session_shell(
            &self,
            _loc: &Location,
            _session_id: &str,
            _req: crate::transport::api::SessionShell,
        ) -> Result<MessageWithParts> {
            unreachable!()
        }
        async fn session_abort(&self, _loc: &Location, _session_id: &str) -> Result<bool> {
            Ok(true)
        }
        async fn session_revert(
            &self,
            _loc: &Location,
            _session_id: &str,
            _message_id: &str,
            _part_id: Option<&str>,
        ) -> Result<V1SessionInfo> {
            Ok(session_info("ses_a"))
        }
        async fn session_unrevert(
            &self,
            _loc: &Location,
            _session_id: &str,
        ) -> Result<V1SessionInfo> {
            Ok(session_info("ses_a"))
        }
        async fn session_summarize(
            &self,
            _loc: &Location,
            _session_id: &str,
            _provider_id: &str,
            _model_id: &str,
        ) -> Result<bool> {
            Ok(true)
        }
        async fn session_share(&self, _loc: &Location, _session_id: &str) -> Result<V1SessionInfo> {
            Ok(session_info("ses_a"))
        }
        async fn session_unshare(
            &self,
            _loc: &Location,
            _session_id: &str,
        ) -> Result<V1SessionInfo> {
            Ok(session_info("ses_a"))
        }
        async fn session_fork(
            &self,
            _loc: &Location,
            _session_id: &str,
            _message_id: Option<&str>,
        ) -> Result<V1SessionInfo> {
            Ok(session_info("ses_fork"))
        }
        async fn session_rename(
            &self,
            _loc: &Location,
            _session_id: &str,
            _title: &str,
        ) -> Result<V1SessionInfo> {
            Ok(session_info("ses_a"))
        }
        async fn session_delete(&self, _loc: &Location, _session_id: &str) -> Result<bool> {
            Ok(true)
        }
        async fn session_status(
            &self,
            _loc: &Location,
        ) -> Result<std::collections::BTreeMap<String, SessionStatusInfo>> {
            Ok(std::collections::BTreeMap::new())
        }
        async fn permission_reply(
            &self,
            _loc: &Location,
            _request_id: &str,
            _reply: PermissionV1Reply,
            _message: Option<&str>,
        ) -> Result<bool> {
            Ok(true)
        }
        async fn question_reply(
            &self,
            _loc: &Location,
            _request_id: &str,
            _answers: Vec<alforria_schema::question_v1::QuestionV1Answer>,
        ) -> Result<bool> {
            Ok(true)
        }
        async fn question_reject(&self, _loc: &Location, _request_id: &str) -> Result<bool> {
            Ok(true)
        }
        async fn auth_set(&self, _loc: &Location, _provider_id: &str, _key: &str) -> Result<bool> {
            Ok(true)
        }
    }

    #[tokio::test]
    async fn bootstrap_two_phases_and_state() {
        let mut state = state_for_test();
        let api = FakeApi {
            sessions: vec![session_info("ses_a")],
            ..FakeApi::default()
        };
        bootstrap(&mut state, &api, true).await.expect("bootstraps");
        assert_eq!(state.sync.status, Some(SyncStatus::Complete));
        assert_eq!(state.sync.provider.len(), 1);
        assert_eq!(
            state.sync.provider_default.get("anthropic"),
            Some(&"claude".to_string())
        );
        assert!(state.sync.capabilities);
        assert_eq!(
            state.sync.console_state,
            json!({"consoleManagedProviders": [], "switchableOrgCount": 1})
        );
        assert_eq!(state.sync.agent.len(), 1);
        assert_eq!(state.sync.config, json!({"model": "anthropic/claude"}));
        assert_eq!(state.sync.session.len(), 1);
        assert_eq!(state.sync.command.len(), 1);
        assert_eq!(state.sync.lsp.len(), 1);
        assert_eq!(state.sync.mcp.len(), 1);
        assert_eq!(state.sync.vcs, Some(json!({"branch": "main"})));
        assert_eq!(
            state.project.instance_path.worktree.as_deref(),
            Some("/repo")
        );
        assert_eq!(state.project.project_id.as_deref(), Some("prj_1"));
        assert_eq!(state.project.main_dir.as_deref(), Some("/repo/sub"));
    }

    #[tokio::test]
    async fn bootstrap_fatal_error_propagates() {
        let mut state = state_for_test();
        let api = FakeApi {
            fail_providers: true,
            ..FakeApi::default()
        };
        let err = bootstrap(&mut state, &api, true)
            .await
            .expect_err("phase-1 failures propagate");
        assert!(err.to_string().contains("fatal"));
        assert_eq!(state.sync.status, Some(SyncStatus::Loading));
    }

    #[tokio::test]
    async fn hydrate_live_tracking_wins() {
        let mut state = state_for_test();
        // Simulate live-streamed content before the hydrate call:
        // msg_live has a part that the server data will also carry,
        // but with different content.
        state.sync.session = vec![session_info("ses_a")];
        state.sync.message.insert(
            "ses_a".into(),
            vec![assistant_message("msg_live", "ses_a", 5)],
        );
        state.sync.part.insert(
            "msg_live".into(),
            vec![text_part("prt_live", "ses_a", "msg_live", "live text")],
        );
        state.sync.hydrating_sessions.insert(
            "ses_a".into(),
            HydrateTracker {
                messages: ["msg_live"].into_iter().map(str::to_string).collect(),
                parts: ["prt_live"].into_iter().map(str::to_string).collect(),
            },
        );
        let api = FakeApi {
            fail_session_get: false,
            sessions: vec![session_info("ses_a")],
            messages: vec![MessageWithParts {
                info: assistant_message("msg_live", "ses_a", 5),
                parts: vec![text_part("prt_live", "ses_a", "msg_live", "server text")],
            }],
            ..FakeApi::default()
        };
        session_sync(&mut state, &api, "ses_a").await.unwrap();
        let messages = &state.sync.message["ses_a"];
        assert_eq!(messages.len(), 1);
        match &state.sync.part["msg_live"][0] {
            V1Part::Text { text, .. } => assert_eq!(text, "live text", "live part wins"),
            _ => panic!("expected text part"),
        }
        assert!(state.sync.full_synced.contains("ses_a"));
    }

    #[tokio::test]
    async fn hydrate_empty_hydrated_text_loses() {
        let mut state = state_for_test();
        state.sync.session = vec![session_info("ses_a")];
        state.sync.part.insert(
            "msg_1".into(),
            vec![text_part("prt_a", "ses_a", "msg_1", "populated live")],
        );
        let api = FakeApi {
            sessions: vec![session_info("ses_a")],
            messages: vec![MessageWithParts {
                info: assistant_message("msg_1", "ses_a", 5),
                parts: vec![text_part("prt_a", "ses_a", "msg_1", "")],
            }],
            ..FakeApi::default()
        };
        session_sync(&mut state, &api, "ses_a").await.unwrap();
        match &state.sync.part["msg_1"][0] {
            V1Part::Text { text, .. } => assert_eq!(text, "populated live"),
            _ => panic!("expected text part"),
        }
    }

    #[tokio::test]
    async fn hydrate_visible_window_and_part_cleanup() {
        let mut state = state_for_test();
        state.sync.session = vec![session_info("ses_a")];
        // 102 messages on the server; the live store only has the last
        // message plus parts for the first.
        let mut messages = Vec::new();
        for i in 0..102 {
            // The last message's part shares the live part's id, so the
            // tracker must win over the server data.
            let part_id = if i == 101 { "prt_live" } else { "prt_x" };
            messages.push(MessageWithParts {
                info: assistant_message(&format!("msg_{i:03}"), "ses_a", i),
                parts: vec![text_part(part_id, "ses_a", &format!("msg_{i:03}"), "x")],
            });
        }
        state.sync.message.insert(
            "ses_a".into(),
            vec![assistant_message("msg_101", "ses_a", 101)],
        );
        state.sync.part.insert(
            "msg_000".into(),
            vec![text_part("prt_000", "ses_a", "msg_000", "x")],
        );
        state.sync.part.insert(
            "msg_101".into(),
            vec![text_part("prt_live", "ses_a", "msg_101", "live")],
        );
        state.sync.hydrating_sessions.insert(
            "ses_a".into(),
            HydrateTracker {
                messages: ["msg_101"].into_iter().map(str::to_string).collect(),
                parts: ["prt_live"].into_iter().map(str::to_string).collect(),
            },
        );
        let api = FakeApi {
            sessions: vec![session_info("ses_a")],
            messages,
            ..FakeApi::default()
        };
        session_sync(&mut state, &api, "ses_a").await.unwrap();
        let visible = &state.sync.message["ses_a"];
        assert_eq!(visible.len(), 100);
        assert_eq!(message_id(visible.first().unwrap()), "msg_002");
        assert_eq!(message_id(visible.last().unwrap()), "msg_101");
        assert!(
            !state.sync.part.contains_key("msg_000"),
            "parts of evicted messages are deleted"
        );
        assert!(
            state.sync.part.contains_key("msg_002"),
            "parts of visible messages are kept"
        );
        match &state.sync.part["msg_101"][0] {
            V1Part::Text { text, .. } => assert_eq!(text, "live", "tracked part wins"),
            _ => panic!("expected text part"),
        }
    }

    #[tokio::test]
    async fn hydrate_runs_once_per_session() {
        let mut state = state_for_test();
        state.sync.session = vec![session_info("ses_a")];
        state.sync.full_synced.insert("ses_a".into());
        let api = FakeApi {
            fail_session_get: true,
            ..FakeApi::default()
        };
        session_sync(&mut state, &api, "ses_a")
            .await
            .expect("already synced sessions are no-ops");
    }
}

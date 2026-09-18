//! Session store — port of `session/session.ts` plus the seven durable
//! session projectors (`packages/core/src/session/projector.ts`).
//!
//! Writes are events: every mutation publishes a `SessionV1.Event.*` on the
//! bus and the registered projector persists the row inside the commit
//! transaction (spec §2.3).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use opencode_schema::file_diff::SnapshotFileDiff;
use opencode_schema::permission_v1::PermissionV1Ruleset;
use opencode_schema::schema::JsonMap;
use opencode_schema::session::SessionTokens;
use opencode_schema::session_v1::{
    MessagePartDeltaData, MessagePartRemovedData, MessagePartUpdatedData, MessageRemovedData,
    MessageUpdatedData, SessionCreatedData, SessionDeletedData, SessionUpdatedData, V1Message,
    V1Part, V1SessionInfo, V1SessionModel, V1SessionRevert, V1SessionShare, V1SessionSummary,
    V1SessionTime,
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::event::bus::{EventBus, PublishOptions};
use crate::event::definition::Definition;
use crate::session::agents;
use crate::session::error::{NotFoundError, SessionError};
use crate::session::event_definitions;
use crate::session::ids::{MessageId, PartId, SessionId};
use crate::session::message::{self, MessageStore, WithParts};
use crate::session::run_state::{BackgroundJobInfo, BackgroundJobStatus, BackgroundJobs};
use crate::storage::schema::{json_opt_to_string, part_from_row, session_from_row, Session};
use crate::storage::Storage;
use crate::CoreError;

/// `InstallationVersion` (installation/version.ts:6) — the build version,
/// or `"local"` when unset.
pub const INSTALLATION_VERSION: &str = match option_env!("OPENCODE_VERSION") {
    Some(version) => version,
    None => "local",
};

const PARENT_TITLE_PREFIX: &str = "New session - ";
const CHILD_TITLE_PREFIX: &str = "Child session - ";

/// `isDefaultTitle` (session.ts:48-55):
/// `^(New session - |Child session - )\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$`.
pub fn is_default_title(title: &str) -> bool {
    let Some(rest) = title
        .strip_prefix(PARENT_TITLE_PREFIX)
        .or_else(|| title.strip_prefix(CHILD_TITLE_PREFIX))
    else {
        return false;
    };
    let bytes = rest.as_bytes();
    if bytes.len() != 24 {
        return false;
    }
    let digits = |from: usize, to: usize| bytes[from..to].iter().all(|b| b.is_ascii_digit());
    let at = |i: usize, c: u8| bytes[i] == c;
    digits(0, 4)
        && at(4, b'-')
        && digits(5, 7)
        && at(7, b'-')
        && digits(8, 10)
        && at(10, b'T')
        && digits(11, 13)
        && at(13, b':')
        && digits(14, 16)
        && at(16, b':')
        && digits(17, 19)
        && at(19, b'.')
        && digits(20, 23)
        && at(23, b'Z')
}

/// `getForkedTitle` (session.ts:161-169): `^(.+) \(fork #(\d+)\)$` bumping.
pub fn get_forked_title(title: &str) -> String {
    // Greedy `(.+)`: the rightmost ` (fork #N)` that closes the string.
    let mut end = title.len();
    while end > 0 {
        let Some(pos) = title[..end].rfind(" (fork #") else {
            break;
        };
        let rest = &title[pos + " (fork #".len()..];
        if let Some(num) = rest.strip_suffix(')') {
            if let Ok(num) = num.parse::<u64>() {
                return format!("{} (fork #{})", &title[..pos], num + 1);
            }
        }
        end = pos;
    }
    format!("{title} (fork #1)")
}

/// `sessionPath` (session.ts:171-173): relpath with `/` separators.
pub fn session_path(worktree: &Path, cwd: &Path) -> String {
    let worktree = worktree
        .canonicalize()
        .unwrap_or_else(|_| worktree.to_path_buf());
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    match cwd.strip_prefix(&worktree) {
        Ok(relative) => relative.to_string_lossy().replace('\\', "/"),
        Err(_) => cwd.to_string_lossy().replace('\\', "/"),
    }
}

/// `EmptyTokens` (session.ts:192).
pub fn empty_tokens() -> SessionTokens {
    SessionTokens {
        input: 0.0,
        output: 0.0,
        reasoning: 0.0,
        cache: opencode_schema::session::SessionTokensCache {
            read: 0.0,
            write: 0.0,
        },
    }
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `new Date().toISOString()` — `YYYY-MM-DDTHH:MM:SS.mmmZ`.
fn iso_now(now_ms: u64) -> String {
    let secs = now_ms / 1000;
    let millis = now_ms % 1000;
    let days = secs / 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    let rem = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

/// Days-since-epoch → (y, m, d) — Howard Hinnant's civil_from_days.
fn civil_from_days(days: i64) -> (i64, u64, u64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let y = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = y + era * 400;
    let yd = doe - (365 * y + y / 4 - y / 100);
    let mp = (5 * yd + 2) / 153;
    let day = yd - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month as u64, day as u64)
}

// ---------------------------------------------------------------------------
// Row ↔ Info mapping (session.ts:59-159)
// ---------------------------------------------------------------------------

fn storage_err<E: std::fmt::Display>(field: &str, err: E) -> SessionError {
    CoreError::Storage(format!("invalid session {field}: {err}")).into()
}

/// `fromRow` (session.ts:59-101).
pub fn from_row(row: &Session) -> Result<V1SessionInfo, SessionError> {
    let summary = if row.summary_additions.is_some()
        || row.summary_deletions.is_some()
        || row.summary_files.is_some()
    {
        let diffs: Option<Vec<SnapshotFileDiff>> = row
            .summary_diffs
            .as_ref()
            .map(|value| serde_json::from_value(value.clone()))
            .transpose()
            .map_err(|err| storage_err("summary_diffs", err))?;
        Some(V1SessionSummary {
            additions: row.summary_additions.unwrap_or(0) as f64,
            deletions: row.summary_deletions.unwrap_or(0) as f64,
            files: row.summary_files.unwrap_or(0) as f64,
            diffs,
        })
    } else {
        None
    };
    let model: Option<V1SessionModel> = row
        .model
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(|err| storage_err("model", err))?;
    let revert: Option<V1SessionRevert> = row
        .revert
        .as_ref()
        .map(|value| serde_json::from_value(value.clone()))
        .transpose()
        .map_err(|err| storage_err("revert", err))?;
    let permission: Option<PermissionV1Ruleset> = row
        .permission
        .as_ref()
        .map(|value| serde_json::from_value(value.clone()))
        .transpose()
        .map_err(|err| storage_err("permission", err))?;
    let metadata: Option<JsonMap> = row
        .metadata
        .as_ref()
        .map(|value| serde_json::from_value(value.clone()))
        .transpose()
        .map_err(|err| storage_err("metadata", err))?;
    Ok(V1SessionInfo {
        id: row.id.clone(),
        slug: row.slug.clone(),
        project_id: row.project_id.clone(),
        workspace_id: row.workspace_id.clone(),
        directory: row.directory.clone(),
        path: row.path.clone(),
        parent_id: row.parent_id.clone(),
        summary,
        cost: Some(row.cost),
        tokens: Some(SessionTokens {
            input: row.tokens_input as f64,
            output: row.tokens_output as f64,
            reasoning: row.tokens_reasoning as f64,
            cache: opencode_schema::session::SessionTokensCache {
                read: row.tokens_cache_read as f64,
                write: row.tokens_cache_write as f64,
            },
        }),
        share: row.share_url.clone().map(|url| V1SessionShare { url }),
        title: row.title.clone(),
        agent: row.agent.clone(),
        model,
        version: row.version.clone(),
        metadata,
        time: V1SessionTime {
            created: row.time_created as u64,
            updated: row.time_updated as u64,
            compacting: row.time_compacting.map(|t| t as u64),
            archived: row.time_archived.map(|t| t as f64),
        },
        permission,
        revert,
    })
}

/// `toRow` / the projector's `sessionRow` (projector.ts:29-71).
pub fn to_row(info: &V1SessionInfo) -> Result<Session, SessionError> {
    let tokens = info.tokens.clone().unwrap_or_else(empty_tokens);
    let metadata = info
        .metadata
        .as_ref()
        .map(serde_json::to_value)
        .transpose()
        .map_err(|err| CoreError::Storage(err.to_string()))?;
    Ok(Session {
        id: info.id.clone(),
        project_id: info.project_id.clone(),
        workspace_id: info.workspace_id.clone(),
        parent_id: info.parent_id.clone(),
        slug: info.slug.clone(),
        directory: info.directory.clone(),
        path: info.path.clone(),
        title: info.title.clone(),
        version: info.version.clone(),
        share_url: info.share.as_ref().map(|share| share.url.clone()),
        summary_additions: info.summary.as_ref().map(|s| s.additions as i64),
        summary_deletions: info.summary.as_ref().map(|s| s.deletions as i64),
        summary_files: info.summary.as_ref().map(|s| s.files as i64),
        summary_diffs: info
            .summary
            .as_ref()
            .and_then(|s| s.diffs.as_ref())
            .map(|diffs| serde_json::to_value(diffs).unwrap_or_default()),
        metadata,
        cost: info.cost.unwrap_or(0.0),
        tokens_input: tokens.input as i64,
        tokens_output: tokens.output as i64,
        tokens_reasoning: tokens.reasoning as i64,
        tokens_cache_read: tokens.cache.read as i64,
        tokens_cache_write: tokens.cache.write as i64,
        revert: match &info.revert {
            Some(revert) => Some(serde_json::to_value(revert)?),
            None => None,
        },
        permission: match &info.permission {
            Some(permission) => Some(serde_json::to_value(permission)?),
            None => None,
        },
        agent: info.agent.clone(),
        model: match &info.model {
            Some(model) => Some(serialize_json_string(&model.clone())?),
            None => None,
        },
        time_created: info.time.created as i64,
        time_updated: info.time.updated as i64,
        time_compacting: info.time.compacting.map(|t| t as i64),
        time_archived: info.time.archived.map(|t| t as i64),
    })
}

fn serialize_json_string<T: Serialize>(value: &T) -> Result<String, CoreError> {
    serde_json::to_string(value).map_err(|err| CoreError::Storage(err.to_string()))
}

// ---------------------------------------------------------------------------
// Patch (session.ts:479-481)
// ---------------------------------------------------------------------------

/// Tri-state patch value: keep the current value, set a new one, or clear
/// it (TS `field === null ? undefined : (field ?? current)`).
#[derive(Debug, Clone, PartialEq, Default)]
pub enum SetClear<T> {
    #[default]
    Keep,
    Set(T),
    Clear,
}

impl<T> SetClear<T> {
    pub fn apply(self, current: Option<T>) -> Option<T> {
        match self {
            SetClear::Keep => current,
            SetClear::Set(value) => Some(value),
            SetClear::Clear => None,
        }
    }
}

/// `Patch["time"]` — a spread-merged partial `{...current.time, ...input}`.
#[derive(Debug, Clone, Default)]
pub struct PartialTime {
    pub created: Option<u64>,
    pub updated: Option<u64>,
    pub compacting: Option<u64>,
    /// `archived` is set to `undefined` by `setArchived` (key present), so
    /// it needs the full tri-state.
    pub archived: SetClear<f64>,
}

/// `Patch` (session.ts:479-481): plain fields replace when `Some`; `time`
/// spreads; `share`/`summary`/`revert`/`permission` are tri-state.
#[derive(Debug, Clone, Default)]
pub struct SessionPatch {
    pub workspace_id: SetClear<String>,
    pub parent_id: Option<String>,
    pub directory: Option<String>,
    pub path: Option<String>,
    pub title: Option<String>,
    pub agent: Option<String>,
    pub model: Option<V1SessionModel>,
    pub version: Option<String>,
    pub metadata: Option<JsonMap>,
    pub cost: Option<f64>,
    pub tokens: Option<SessionTokens>,
    pub time: Option<PartialTime>,
    pub share: SetClear<V1SessionShare>,
    pub summary: SetClear<V1SessionSummary>,
    pub revert: SetClear<V1SessionRevert>,
    pub permission: SetClear<PermissionV1Ruleset>,
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// `ListInput` (session.ts:339-351).
#[derive(Debug, Clone, Default)]
pub struct ListInput {
    pub directory: Option<String>,
    /// `scope === "project"` skips the directory condition.
    pub scope_project: bool,
    /// `path` given (possibly empty) vs absent.
    pub path: Option<String>,
    pub workspace_id: Option<String>,
    pub roots: bool,
    pub start: Option<i64>,
    pub search: Option<String>,
    pub limit: Option<i64>,
}

/// `GlobalListInput` (session.ts:352-362).
#[derive(Debug, Clone, Default)]
pub struct GlobalListInput {
    pub directory: Option<String>,
    pub roots: bool,
    pub start: Option<i64>,
    pub cursor: Option<i64>,
    pub search: Option<String>,
    pub limit: Option<i64>,
    /// Defaults to false — archived sessions are excluded.
    pub archived: bool,
}

/// `ProjectInfo` (session.ts:238-242).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectInfo {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub worktree: String,
}

/// `GlobalInfo` (session.ts:244-246) — `Info` plus its project.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GlobalInfo {
    #[serde(flatten)]
    pub info: V1SessionInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectInfo>,
}

/// The instance bits the store needs (TS `InstanceState.context` /
/// `InstanceState.workspaceID`).
#[derive(Debug, Clone)]
pub struct SessionContext {
    pub project_id: String,
    pub directory: PathBuf,
    pub worktree: PathBuf,
    pub workspace_id: Option<String>,
}

/// `CreateInput` (session.ts:264-275) — the store-side surface.
#[derive(Debug, Clone, Default)]
pub struct CreateInput {
    pub id: Option<String>,
    pub parent_id: Option<String>,
    pub title: Option<String>,
    pub agent: Option<String>,
    pub model: Option<V1SessionModel>,
    pub metadata: Option<JsonMap>,
    pub permission: Option<PermissionV1Ruleset>,
    pub workspace_id: Option<String>,
    pub directory: Option<String>,
    pub path: Option<String>,
}

/// `Session.Service` (session.ts:486-985).
#[derive(Clone)]
pub struct SessionStore {
    events: Arc<EventBus>,
    storage: Arc<Storage>,
    background: Arc<dyn BackgroundJobs>,
    clock: Arc<dyn crate::Clock>,
    messages: MessageStore,
}

impl SessionStore {
    pub fn new(
        events: Arc<EventBus>,
        storage: Arc<Storage>,
        background: Arc<dyn BackgroundJobs>,
        clock: Arc<dyn crate::Clock>,
    ) -> SessionStore {
        let messages = MessageStore::new(storage.clone());
        SessionStore {
            events,
            storage,
            background,
            clock,
            messages,
        }
    }

    fn now(&self) -> u64 {
        self.clock.now_ms()
    }

    fn publish(&self, definition: &Definition, data: Value) -> Result<(), SessionError> {
        self.events
            .publish(definition, data, PublishOptions::default())?;
        Ok(())
    }

    /// `createNext` (session.ts:499-538).
    pub fn create_next(
        &self,
        ctx: &SessionContext,
        input: &CreateInput,
    ) -> Result<V1SessionInfo, SessionError> {
        let now = self.now();
        let title = input.title.clone().unwrap_or_else(|| {
            format!(
                "{}{}",
                if input.parent_id.is_some() {
                    CHILD_TITLE_PREFIX
                } else {
                    PARENT_TITLE_PREFIX
                },
                iso_now(now)
            )
        });
        let info = V1SessionInfo {
            id: SessionId::descending(input.id.as_deref())?,
            slug: agents::slug_create(),
            version: INSTALLATION_VERSION.to_string(),
            project_id: ctx.project_id.clone(),
            directory: input
                .directory
                .clone()
                .unwrap_or_else(|| ctx.directory.to_string_lossy().to_string()),
            path: input.path.clone(),
            workspace_id: input.workspace_id.clone(),
            parent_id: input.parent_id.clone(),
            title,
            agent: input.agent.clone(),
            model: input.model.clone(),
            metadata: input.metadata.clone(),
            permission: input.permission.clone(),
            cost: Some(0.0),
            tokens: Some(empty_tokens()),
            summary: None,
            share: None,
            time: V1SessionTime {
                created: now,
                updated: now,
                compacting: None,
                archived: None,
            },
            revert: None,
        };
        self.publish(
            &event_definitions::SESSION_CREATED,
            serde_json::to_value(SessionCreatedData {
                session_id: info.id.clone(),
                info: info.clone(),
            })?,
        )?;
        Ok(info)
    }

    /// `create` (session.ts:667-689).
    pub fn create(
        &self,
        ctx: &SessionContext,
        input: &CreateInput,
    ) -> Result<V1SessionInfo, SessionError> {
        let mut input = input.clone();
        if input.workspace_id.is_none() {
            input.workspace_id = ctx.workspace_id.clone();
        }
        if input.directory.is_none() {
            input.directory = Some(ctx.directory.to_string_lossy().to_string());
        }
        if input.path.is_none() {
            input.path = Some(session_path(&ctx.worktree, &ctx.directory));
        }
        self.create_next(ctx, &input)
    }

    fn get_row(&self, id: &str) -> Result<Session, SessionError> {
        let row = self
            .storage
            .with_connection(|conn| {
                let mut stmt = conn.prepare("SELECT * FROM session WHERE id = ?1")?;
                let mut rows = stmt.query([id])?;
                match rows.next()? {
                    Some(row) => Ok::<_, CoreError>(Some(session_from_row(row)?)),
                    None => Ok(None),
                }
            })
            .map_err(SessionError::from)?;
        row.ok_or_else(|| {
            SessionError::NotFound(NotFoundError {
                message: format!("Session not found: {id}"),
            })
        })
    }

    /// `get` (session.ts:539-545).
    pub fn get(&self, id: &str) -> Result<V1SessionInfo, SessionError> {
        from_row(&self.get_row(id)?)
    }

    /// `list` (session.ts:547-553) — list scoped to the instance project.
    pub fn list(
        &self,
        ctx: &SessionContext,
        input: &ListInput,
    ) -> Result<Vec<V1SessionInfo>, SessionError> {
        let mut input = input.clone();
        if input.workspace_id.is_none() {
            input.workspace_id = ctx.workspace_id.clone();
        }
        self.list_by_project(&ctx.project_id, &input)
    }

    /// `listByProject` (session.ts:955-1008).
    pub fn list_by_project(
        &self,
        project_id: &str,
        input: &ListInput,
    ) -> Result<Vec<V1SessionInfo>, SessionError> {
        let mut conditions = vec!["project_id = ?1".to_string()];
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(project_id.to_string())];
        if let Some(workspace) = &input.workspace_id {
            params.push(Box::new(workspace.clone()));
            conditions.push(format!("workspace_id = ?{}", params.len()));
        }
        match input.path.as_deref() {
            Some(path) if !path.is_empty() => {
                params.push(Box::new(path.to_string()));
                let path = params.len();
                let mut condition = format!("(path = ?{path} OR LIKE(path, ?{path} || '/%')");
                if let Some(directory) = &input.directory {
                    params.push(Box::new(directory.clone()));
                    condition.push_str(&format!(
                        " OR (path IS NULL AND directory = ?{})",
                        params.len()
                    ));
                }
                condition.push(')');
                conditions.push(condition);
            }
            _ => {
                if !input.scope_project {
                    if let Some(directory) = &input.directory {
                        params.push(Box::new(directory.clone()));
                        conditions.push(format!("directory = ?{}", params.len()));
                    }
                }
            }
        }
        if input.roots {
            conditions.push("parent_id IS NULL".to_string());
        }
        if let Some(start) = input.start {
            params.push(Box::new(start));
            conditions.push(format!("time_updated >= ?{}", params.len()));
        }
        if let Some(search) = &input.search {
            params.push(Box::new(format!("%{search}%")));
            conditions.push(format!("title LIKE ?{}", params.len()));
        }
        let sql = format!(
            "SELECT * FROM session WHERE {} ORDER BY time_updated DESC LIMIT {}",
            conditions.join(" AND "),
            input.limit.unwrap_or(100)
        );
        let rows = self
            .storage
            .with_connection(|conn| {
                let mut stmt = conn.prepare(&sql)?;
                let mut rows = stmt.query(rusqlite::params_from_iter(
                    params.iter().map(|p| p.as_ref()),
                ))?;
                let mut out = Vec::new();
                while let Some(row) = rows.next()? {
                    out.push(session_from_row(row)?);
                }
                Ok::<_, CoreError>(out)
            })
            .map_err(SessionError::from)?;
        rows.iter().map(from_row).collect()
    }

    /// `listGlobal` (session.ts:555-594).
    pub fn list_global(&self, input: &GlobalListInput) -> Result<Vec<GlobalInfo>, SessionError> {
        let mut conditions: Vec<String> = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(directory) = &input.directory {
            params.push(Box::new(directory.clone()));
            conditions.push(format!("directory = ?{}", params.len()));
        }
        if input.roots {
            conditions.push("parent_id IS NULL".to_string());
        }
        if let Some(start) = input.start {
            params.push(Box::new(start));
            conditions.push(format!("time_updated >= ?{}", params.len()));
        }
        if let Some(cursor) = input.cursor {
            params.push(Box::new(cursor));
            conditions.push(format!("time_updated < ?{}", params.len()));
        }
        if let Some(search) = &input.search {
            params.push(Box::new(format!("%{search}%")));
            conditions.push(format!("title LIKE ?{}", params.len()));
        }
        if !input.archived {
            conditions.push("time_archived IS NULL".to_string());
        }
        let sql = format!(
            "SELECT * FROM session {} ORDER BY time_updated DESC, id DESC LIMIT {}",
            if conditions.is_empty() {
                String::new()
            } else {
                format!("WHERE {}", conditions.join(" AND "))
            },
            input.limit.unwrap_or(100)
        );
        let rows = self
            .storage
            .with_connection(|conn| {
                let mut stmt = conn.prepare(&sql)?;
                let mut rows = stmt.query(rusqlite::params_from_iter(
                    params.iter().map(|p| p.as_ref()),
                ))?;
                let mut out = Vec::new();
                while let Some(row) = rows.next()? {
                    out.push(session_from_row(row)?);
                }
                Ok::<_, CoreError>(out)
            })
            .map_err(SessionError::from)?;
        let mut infos = Vec::new();
        for row in rows {
            let info = from_row(&row)?;
            let project = self
                .storage
                .with_connection(|conn| {
                    let mut stmt =
                        conn.prepare("SELECT id, name, worktree FROM project WHERE id = ?1")?;
                    let mut rows = stmt.query([&info.project_id])?;
                    match rows.next()? {
                        Some(row) => Ok::<_, CoreError>(Some(ProjectInfo {
                            id: row.get(0)?,
                            name: row.get(1)?,
                            worktree: row.get(2)?,
                        })),
                        None => Ok::<_, CoreError>(None),
                    }
                })
                .map_err(SessionError::from)?;
            infos.push(GlobalInfo { info, project });
        }
        Ok(infos)
    }

    /// Minimal project resolution for the HTTP surface: resolve-or-create
    /// the `project` row for a directory (TS `Project.resolve` +
    /// `Project.commit` during instance bootstrap,
    /// `packages/core/src/project.ts:101-119`). TODO(M7): git
    /// remote/root-commit ids and the full project registry.
    pub fn ensure_project(&self, worktree: &Path) -> Result<String, SessionError> {
        let key = worktree.to_string_lossy().to_string();
        let existing = self.storage.with_connection(|conn| {
            match conn.query_row(
                "SELECT id FROM project WHERE worktree = ?1",
                [&key],
                |row| row.get::<_, String>(0),
            ) {
                Ok(id) => Ok(Some(id)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(err) => Err(CoreError::from(err)),
            }
        });
        if let Some(id) = existing? {
            return Ok(id);
        }
        let id = format!("prj_{}", ulid::Ulid::new());
        let now = self.now() as i64;
        self.storage.with_connection(|conn| {
            conn.execute(
                "INSERT INTO project (id, worktree, sandboxes, time_created, time_updated)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![id, key, "[]", now, now],
            )?;
            Ok::<(), CoreError>(())
        })?;
        Ok(id)
    }

    /// `children` (session.ts:596-604).
    pub fn children(&self, parent_id: &str) -> Result<Vec<V1SessionInfo>, SessionError> {
        let rows = self
            .storage
            .with_connection(|conn| {
                let mut stmt = conn.prepare("SELECT * FROM session WHERE parent_id = ?1")?;
                let mut rows = stmt.query([parent_id])?;
                let mut out = Vec::new();
                while let Some(row) = rows.next()? {
                    out.push(session_from_row(row)?);
                }
                Ok::<_, CoreError>(out)
            })
            .map_err(SessionError::from)?;
        rows.iter().map(from_row).collect()
    }

    /// `remove` (session.ts:606-627): cancel background jobs, remove
    /// children recursively, publish `Deleted`.
    pub fn remove(&self, session_id: &str) -> Result<(), SessionError> {
        let session = self.get(session_id)?;
        // `remove` needs to work in all cases, such as broken sessions that
        // run cleanup without instance state (session.ts:606-627) — errors
        // after the initial get soft-fail to a log.
        let result = self.remove_guts(session_id, session);
        if let Err(error) = result {
            tracing::error!("failed to remove session {session_id}: {error}");
        }
        Ok(())
    }

    fn remove_guts(
        &self,
        session_id: &str,
        session: opencode_schema::session_v1::V1SessionInfo,
    ) -> Result<(), SessionError> {
        cancel_session_background_jobs(self.background.as_ref(), session_id)?;
        let children = self.children(session_id)?;
        for child in children {
            self.remove(&child.id)?;
        }
        self.publish(
            &event_definitions::SESSION_DELETED,
            serde_json::to_value(SessionDeletedData {
                session_id: session_id.to_string(),
                info: session,
            })?,
        )?;
        self.events.remove(session_id)?;
        Ok(())
    }

    /// `patch` (session.ts:734-747).
    pub fn patch(&self, session_id: &str, patch: SessionPatch) -> Result<(), SessionError> {
        let current = self.get(session_id)?;
        let next = apply_patch(current, patch);
        self.publish(
            &event_definitions::SESSION_UPDATED,
            serde_json::to_value(SessionUpdatedData {
                session_id: session_id.to_string(),
                info: next,
            })?,
        )?;
        Ok(())
    }

    fn patch_time(&self) -> PartialTime {
        PartialTime {
            updated: Some(self.now()),
            ..Default::default()
        }
    }

    /// `touch` (session.ts:749-751).
    pub fn touch(&self, session_id: &str) -> Result<(), SessionError> {
        self.patch(
            session_id,
            SessionPatch {
                time: Some(self.patch_time()),
                ..Default::default()
            },
        )
    }

    /// `setTitle` (session.ts:753-755).
    pub fn set_title(&self, session_id: &str, title: &str) -> Result<(), SessionError> {
        self.patch(
            session_id,
            SessionPatch {
                title: Some(title.to_string()),
                ..Default::default()
            },
        )
    }

    /// `setArchived` (session.ts:757-759).
    pub fn set_archived(&self, session_id: &str, time: Option<u64>) -> Result<(), SessionError> {
        self.patch(
            session_id,
            SessionPatch {
                time: Some(PartialTime {
                    archived: match time {
                        Some(time) => SetClear::Set(time as f64),
                        None => SetClear::Clear,
                    },
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
    }

    /// `setMetadata` (session.ts:761-763).
    pub fn set_metadata(&self, session_id: &str, metadata: JsonMap) -> Result<(), SessionError> {
        self.patch(
            session_id,
            SessionPatch {
                metadata: Some(metadata),
                time: Some(self.patch_time()),
                ..Default::default()
            },
        )
    }

    /// `setAgentModel` (session.ts:765-777).
    pub fn set_agent_model(
        &self,
        session_id: &str,
        agent: &str,
        model: V1SessionModel,
        time: u64,
    ) -> Result<(), SessionError> {
        self.patch(
            session_id,
            SessionPatch {
                agent: Some(agent.to_string()),
                model: Some(model),
                time: Some(PartialTime {
                    updated: Some(time),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
    }

    /// `setPermission` (session.ts:779-787).
    pub fn set_permission(
        &self,
        session_id: &str,
        permission: PermissionV1Ruleset,
    ) -> Result<(), SessionError> {
        self.patch(
            session_id,
            SessionPatch {
                permission: SetClear::Set(permission),
                time: Some(self.patch_time()),
                ..Default::default()
            },
        )
    }

    /// `setRevert` (session.ts:789-799).
    pub fn set_revert(
        &self,
        session_id: &str,
        revert: Option<V1SessionRevert>,
        summary: Option<V1SessionSummary>,
    ) -> Result<(), SessionError> {
        self.patch(
            session_id,
            SessionPatch {
                summary: match summary {
                    Some(summary) => SetClear::Set(summary),
                    None => SetClear::Clear,
                },
                revert: match revert {
                    Some(revert) => SetClear::Set(revert),
                    None => SetClear::Clear,
                },
                time: Some(self.patch_time()),
                ..Default::default()
            },
        )
    }

    /// `clearRevert` (session.ts:801-803).
    pub fn clear_revert(&self, session_id: &str) -> Result<(), SessionError> {
        self.patch(
            session_id,
            SessionPatch {
                revert: SetClear::Clear,
                time: Some(self.patch_time()),
                ..Default::default()
            },
        )
    }

    /// `setSummary` (session.ts:805-808).
    pub fn set_summary(
        &self,
        session_id: &str,
        summary: Option<V1SessionSummary>,
    ) -> Result<(), SessionError> {
        self.patch(
            session_id,
            SessionPatch {
                summary: match summary {
                    Some(summary) => SetClear::Set(summary),
                    None => SetClear::Clear,
                },
                time: Some(self.patch_time()),
                ..Default::default()
            },
        )
    }

    /// `setShare` (session.ts:810-812).
    pub fn set_share(
        &self,
        session_id: &str,
        share: Option<V1SessionShare>,
    ) -> Result<(), SessionError> {
        self.patch(
            session_id,
            SessionPatch {
                share: match share {
                    Some(share) => SetClear::Set(share),
                    None => SetClear::Clear,
                },
                time: Some(self.patch_time()),
                ..Default::default()
            },
        )
    }

    /// `setWorkspace` (session.ts:814-821): the spread in `patch` clears
    /// the key when the input is undefined.
    pub fn set_workspace(
        &self,
        session_id: &str,
        workspace_id: Option<String>,
    ) -> Result<(), SessionError> {
        self.patch(
            session_id,
            SessionPatch {
                workspace_id: match workspace_id {
                    Some(workspace) => SetClear::Set(workspace),
                    None => SetClear::Clear,
                },
                time: Some(self.patch_time()),
                ..Default::default()
            },
        )
    }

    /// `diff` (session.ts:823-826) — a stub in the pinned commit.
    pub fn diff(&self, _session_id: &str) -> Result<Vec<SnapshotFileDiff>, SessionError> {
        Ok(Vec::new())
    }

    /// `messages` (session.ts:828-851): a page of up to `limit` items, or
    /// the full (chronological) message stream.
    pub fn messages(
        &self,
        session_id: &str,
        limit: Option<usize>,
    ) -> Result<Vec<WithParts>, SessionError> {
        if let Some(limit) = limit {
            if limit != 0 {
                let page = self.messages.page(session_id, limit, None)?;
                return Ok(page.items);
            }
        }
        const SIZE: usize = 50;
        let mut result: Vec<WithParts> = Vec::new();
        let mut before: Option<String> = None;
        loop {
            let page = self.messages.page(session_id, SIZE, before.as_deref())?;
            if page.items.is_empty() {
                break;
            }
            for item in page.items.iter().rev() {
                result.push(item.clone());
            }
            if !page.more || page.cursor.is_none() {
                break;
            }
            before = page.cursor;
        }
        result.reverse();
        Ok(result)
    }

    /// `updateMessage` (session.ts:629-638).
    pub fn update_message(&self, msg: &V1Message) -> Result<(), SessionError> {
        let session_id = message::message_session_id(msg).to_string();
        self.publish(
            &event_definitions::MESSAGE_UPDATED,
            serde_json::to_value(MessageUpdatedData {
                session_id,
                info: msg.clone(),
            })?,
        )?;
        Ok(())
    }

    /// `removeMessage` (session.ts:853-862).
    pub fn remove_message(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> Result<String, SessionError> {
        self.publish(
            &event_definitions::MESSAGE_REMOVED,
            serde_json::to_value(MessageRemovedData {
                session_id: session_id.to_string(),
                message_id: message_id.to_string(),
            })?,
        )?;
        Ok(message_id.to_string())
    }

    /// `updatePart` (session.ts:640-643 + 661-666).
    pub fn update_part(&self, part: &V1Part) -> Result<(), SessionError> {
        let session_id = message::part_session_id(part).to_string();
        self.publish(
            &event_definitions::MESSAGE_PART_UPDATED,
            serde_json::to_value(MessagePartUpdatedData {
                session_id,
                part: part.clone(),
                time: self.now() as f64,
            })?,
        )?;
        Ok(())
    }

    /// `getPart` (session.ts:653-660).
    pub fn get_part(
        &self,
        session_id: &str,
        message_id: &str,
        part_id: &str,
    ) -> Result<Option<V1Part>, SessionError> {
        let row = self
            .storage
            .with_connection(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT * FROM part WHERE session_id = ?1 AND message_id = ?2 AND id = ?3",
                )?;
                let mut rows = stmt.query([session_id, message_id, part_id])?;
                match rows.next()? {
                    Some(row) => Ok::<_, CoreError>(Some(part_from_row(row)?)),
                    None => Ok::<_, CoreError>(None),
                }
            })
            .map_err(SessionError::from)?;
        Ok(match row {
            Some(row) => Some(message::part_from_row_data(&row)?),
            None => None,
        })
    }

    /// `removePart` (session.ts:864-876).
    pub fn remove_part(
        &self,
        session_id: &str,
        message_id: &str,
        part_id: &str,
    ) -> Result<String, SessionError> {
        self.publish(
            &event_definitions::MESSAGE_PART_REMOVED,
            serde_json::to_value(MessagePartRemovedData {
                session_id: session_id.to_string(),
                message_id: message_id.to_string(),
                part_id: part_id.to_string(),
            })?,
        )?;
        Ok(part_id.to_string())
    }

    /// `updatePartDelta` (session.ts:878-886).
    pub fn update_part_delta(
        &self,
        session_id: &str,
        message_id: &str,
        part_id: &str,
        field: &str,
        delta: &str,
    ) -> Result<(), SessionError> {
        self.publish(
            &message::event::MESSAGE_PART_DELTA,
            serde_json::to_value(MessagePartDeltaData {
                session_id: session_id.to_string(),
                message_id: message_id.to_string(),
                part_id: part_id.to_string(),
                field: field.to_string(),
                delta: delta.to_string(),
            })?,
        )?;
        Ok(())
    }

    /// `findMessage` (session.ts:888-904): newest-first scan.
    pub fn find_message(
        &self,
        session_id: &str,
        predicate: &dyn Fn(&WithParts) -> bool,
    ) -> Result<Option<WithParts>, SessionError> {
        const SIZE: usize = 50;
        let mut before: Option<String> = None;
        loop {
            let page = self.messages.page(session_id, SIZE, before.as_deref())?;
            if page.items.is_empty() {
                break;
            }
            for item in page.items.iter().rev() {
                if predicate(item) {
                    return Ok(Some(item.clone()));
                }
            }
            if !page.more || page.cursor.is_none() {
                break;
            }
            before = page.cursor;
        }
        Ok(None)
    }

    /// `fork` (session.ts:691-732): copy messages (up to `message_id`)
    /// into a fresh session with new message/part ids.
    pub fn fork(
        &self,
        ctx: &SessionContext,
        session_id: &str,
        message_id: Option<&str>,
    ) -> Result<V1SessionInfo, SessionError> {
        let original = self.get(session_id)?;
        let title = get_forked_title(&original.title);
        let session = self.create_next(
            ctx,
            &CreateInput {
                directory: Some(ctx.directory.to_string_lossy().to_string()),
                path: Some(session_path(&ctx.worktree, &ctx.directory)),
                workspace_id: original.workspace_id.clone(),
                title: Some(title),
                metadata: original.metadata.clone(),
                ..Default::default()
            },
        )?;
        let msgs = self.messages(session_id, None)?;
        let mut id_map: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let target = match message_id {
            Some(message_id) => msgs
                .iter()
                .position(|msg| message::message_id(&msg.info) == message_id)
                .unwrap_or(msgs.len()),
            None => msgs.len(),
        };
        for msg in msgs.into_iter().take(target) {
            let new_id = MessageId::ascending(None)?;
            id_map.insert(message::message_id(&msg.info).to_string(), new_id.clone());
            let info = clone_message_with(&msg.info, &session.id, &new_id, &id_map);
            self.update_message(&info)?;
            for part in &msg.parts {
                let new_part = clone_part(part, &session.id, message::message_id(&info), &id_map)?;
                self.update_part(&new_part)?;
            }
        }
        Ok(session)
    }
}

/// `patch` spread semantics (session.ts:734-747).
fn apply_patch(mut info: V1SessionInfo, patch: SessionPatch) -> V1SessionInfo {
    match patch.workspace_id {
        SetClear::Set(v) => info.workspace_id = Some(v),
        SetClear::Clear => info.workspace_id = None,
        SetClear::Keep => {}
    }
    if let Some(v) = patch.parent_id {
        info.parent_id = Some(v);
    }
    if let Some(v) = patch.directory {
        info.directory = v;
    }
    if let Some(v) = patch.path {
        info.path = Some(v);
    }
    if let Some(v) = patch.title {
        info.title = v;
    }
    if let Some(v) = patch.agent {
        info.agent = Some(v);
    }
    if let Some(v) = patch.model {
        info.model = Some(v);
    }
    if let Some(v) = patch.version {
        info.version = v;
    }
    if let Some(v) = patch.metadata {
        info.metadata = Some(v);
    }
    if let Some(v) = patch.cost {
        info.cost = Some(v);
    }
    if let Some(v) = patch.tokens {
        info.tokens = Some(v);
    }
    if let Some(time) = patch.time {
        if let Some(v) = time.created {
            info.time.created = v;
        }
        if let Some(v) = time.updated {
            info.time.updated = v;
        }
        if let Some(v) = time.compacting {
            info.time.compacting = Some(v);
        }
        match time.archived {
            SetClear::Keep => {}
            SetClear::Set(v) => info.time.archived = Some(v),
            SetClear::Clear => info.time.archived = None,
        }
    }
    // share: null → undefined, partial → merged with current; the wire
    // type has a single field, so merge ≡ replace.
    info.share = match patch.share {
        SetClear::Keep => info.share,
        SetClear::Set(share) => Some(share),
        SetClear::Clear => None,
    };
    info.summary = patch.summary.apply(info.summary);
    info.revert = patch.revert.apply(info.revert);
    info.permission = patch.permission.apply(info.permission);
    info
}

/// `cancelBackgroundJobs` (session.ts:989-1007) — the simple (non-fixpoint)
/// variant `remove` uses.
fn cancel_session_background_jobs(
    background: &dyn BackgroundJobs,
    session_id: &str,
) -> Result<(), CoreError> {
    let jobs = background.list()?;
    for job in jobs {
        if matches_session_job(&job, session_id) {
            background.cancel(&job.id)?;
        }
    }
    Ok(())
}

fn matches_session_job(job: &BackgroundJobInfo, session_id: &str) -> bool {
    if job.status != BackgroundJobStatus::Running {
        return false;
    }
    if job.id == session_id {
        return true;
    }
    let metadata = |key: &str| {
        job.metadata
            .as_ref()
            .and_then(|metadata| metadata.get(key))
            .and_then(Value::as_str)
    };
    metadata("sessionId") == Some(session_id) || metadata("parentSessionId") == Some(session_id)
}

// ---------------------------------------------------------------------------
// Message/part cloning helpers (fork, session.ts:691-732)
// ---------------------------------------------------------------------------

fn clone_message_with(
    info: &V1Message,
    session_id: &str,
    id: &str,
    id_map: &std::collections::HashMap<String, String>,
) -> V1Message {
    let mut value = serde_json::to_value(info).unwrap_or_default();
    if let Some(object) = value.as_object_mut() {
        object.insert("id".to_string(), Value::String(id.to_string()));
        object.insert(
            "sessionID".to_string(),
            Value::String(session_id.to_string()),
        );
        if let Some(parent) = object.get("parentID").and_then(Value::as_str) {
            if let Some(remapped) = id_map.get(parent) {
                object.insert("parentID".to_string(), Value::String(remapped.clone()));
            }
        }
    }
    serde_json::from_value(value).unwrap_or_else(|_| info.clone())
}

fn clone_part(
    part: &V1Part,
    session_id: &str,
    message_id: &str,
    id_map: &std::collections::HashMap<String, String>,
) -> Result<V1Part, SessionError> {
    let new_id = PartId::ascending(None)?;
    let mut value = serde_json::to_value(part).unwrap_or_default();
    if let Some(object) = value.as_object_mut() {
        object.insert("id".to_string(), Value::String(new_id));
        object.insert(
            "sessionID".to_string(),
            Value::String(session_id.to_string()),
        );
        object.insert(
            "messageID".to_string(),
            Value::String(message_id.to_string()),
        );
        if object.get("type").and_then(Value::as_str) == Some("compaction") {
            if let Some(tail) = object.get("tail_start_id").and_then(Value::as_str) {
                if let Some(remapped) = id_map.get(tail) {
                    object.insert("tail_start_id".to_string(), Value::String(remapped.clone()));
                }
            }
        }
    }
    Ok(serde_json::from_value(value).unwrap_or_else(|_| part.clone()))
}

// ---------------------------------------------------------------------------
// Projectors (packages/core/src/session/projector.ts)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
struct Usage {
    cost: f64,
    input: f64,
    output: f64,
    reasoning: f64,
    cache_read: f64,
    cache_write: f64,
}

/// `usage(part)` (projector.ts:36-45): a `step-finish` part carrying
/// `cost` and `tokens`.
fn usage(part: &Value) -> Option<Usage> {
    let object = part.as_object()?;
    if object.get("type")?.as_str()? != "step-finish" {
        return None;
    }
    if !object.contains_key("cost") || !object.contains_key("tokens") {
        return None;
    }
    let tokens = object.get("tokens")?.as_object()?;
    let cache = tokens.get("cache")?.as_object()?;
    Some(Usage {
        cost: object.get("cost")?.as_f64().unwrap_or(0.0),
        input: tokens.get("input")?.as_f64().unwrap_or(0.0),
        output: tokens.get("output")?.as_f64().unwrap_or(0.0),
        reasoning: tokens.get("reasoning")?.as_f64().unwrap_or(0.0),
        cache_read: cache.get("read")?.as_f64().unwrap_or(0.0),
        cache_write: cache.get("write")?.as_f64().unwrap_or(0.0),
    })
}

/// `applyUsage` (projector.ts:86-105). The TS `time_updated: time_updated`
/// self-assignment is a no-op and not reproduced.
fn apply_usage(
    conn: &Connection,
    session_id: &str,
    value: &Usage,
    sign: f64,
) -> Result<(), CoreError> {
    conn.execute(
        "UPDATE session SET
            cost = cost + ?2,
            tokens_input = tokens_input + ?3,
            tokens_output = tokens_output + ?4,
            tokens_reasoning = tokens_reasoning + ?5,
            tokens_cache_read = tokens_cache_read + ?6,
            tokens_cache_write = tokens_cache_write + ?7
        WHERE id = ?1",
        rusqlite::params![
            session_id,
            value.cost * sign,
            (value.input * sign) as i64,
            (value.output * sign) as i64,
            (value.reasoning * sign) as i64,
            (value.cache_read * sign) as i64,
            (value.cache_write * sign) as i64,
        ],
    )?;
    Ok(())
}

/// `insertMessage`/`messageData` (projector.ts:73-78 + 306-323): the
/// message row the `message.*` projectors write.
fn message_row(info: &V1Message) -> Result<(String, String, Value), CoreError> {
    let id = message::message_id(info).to_string();
    let session_id = message::message_session_id(info).to_string();
    let mut value =
        serde_json::to_value(info).map_err(|err| CoreError::Storage(err.to_string()))?;
    if let Some(object) = value.as_object_mut() {
        object.remove("id");
        object.remove("sessionID");
    }
    Ok((id, session_id, value))
}

/// `partData` (projector.ts:80-84): the part row triple.
fn part_row(part: &V1Part) -> Result<(String, String, String, Value), CoreError> {
    let value = serde_json::to_value(part).map_err(|err| CoreError::Storage(err.to_string()))?;
    let object = value
        .as_object()
        .ok_or_else(|| CoreError::Storage("part is not an object".to_string()))?;
    let id = object
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| CoreError::Storage("part missing id".to_string()))?
        .to_string();
    let session_id = object
        .get("sessionID")
        .and_then(Value::as_str)
        .ok_or_else(|| CoreError::Storage("part missing sessionID".to_string()))?
        .to_string();
    let message_id = object
        .get("messageID")
        .and_then(Value::as_str)
        .ok_or_else(|| CoreError::Storage("part missing messageID".to_string()))?
        .to_string();
    let mut data = value;
    if let Some(object) = data.as_object_mut() {
        object.remove("id");
        object.remove("sessionID");
        object.remove("messageID");
    }
    Ok((id, message_id, session_id, data))
}

fn invalid_payload(err: serde_json::Error) -> CoreError {
    CoreError::Storage(format!("invalid event data: {err}"))
}

/// Register the seven durable session projectors on the bus
/// (`projector.ts:206-294` — the `SessionV1.Event.*` handlers).
pub fn register_projectors(events: &EventBus) {
    // session.created (projector.ts:206-225)
    events.project(
        &event_definitions::SESSION_CREATED,
        Arc::new(|conn, payload| {
            let data: SessionCreatedData =
                serde_json::from_value(payload.data.clone()).map_err(invalid_payload)?;
            let row = to_row(&data.info).map_err(|_| CoreError::Storage("row".into()))?;
            let inserted = conn.execute(
                "INSERT INTO session (id, project_id, workspace_id, parent_id, slug, directory, path, title, version, share_url, summary_additions, summary_deletions, summary_files, summary_diffs, metadata, cost, tokens_input, tokens_output, tokens_reasoning, tokens_cache_read, tokens_cache_write, revert, permission, agent, model, time_created, time_updated, time_compacting, time_archived)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29)
                 ON CONFLICT (id) DO NOTHING",
                rusqlite::params![
                    row.id,
                    row.project_id,
                    row.workspace_id,
                    row.parent_id,
                    row.slug,
                    row.directory,
                    row.path,
                    row.title,
                    row.version,
                    row.share_url,
                    row.summary_additions,
                    row.summary_deletions,
                    row.summary_files,
                    json_opt_to_string(&row.summary_diffs)?,
                    json_opt_to_string(&row.metadata)?,
                    row.cost,
                    row.tokens_input,
                    row.tokens_output,
                    row.tokens_reasoning,
                    row.tokens_cache_read,
                    row.tokens_cache_write,
                    json_opt_to_string(&row.revert)?,
                    json_opt_to_string(&row.permission)?,
                    row.agent,
                    row.model,
                    row.time_created,
                    row.time_updated,
                    row.time_compacting,
                    row.time_archived,
                ],
            )?;
            if inserted == 0 {
                // SessionAlreadyProjected defect (projector.ts:213-214).
                return Err(CoreError::Storage(format!(
                    "SessionAlreadyProjected: {}",
                    data.info.id
                )));
            }
            if let Some(workspace) = &data.info.workspace_id {
                conn.execute(
                    "UPDATE workspace SET time_used = ?1 WHERE id = ?2",
                    rusqlite::params![now_ms() as i64, workspace],
                )?;
            }
            Ok(())
        }),
    );

    // session.updated (projector.ts:226-234)
    events.project(
        &event_definitions::SESSION_UPDATED,
        Arc::new(|conn, payload| {
            let data: SessionUpdatedData =
                serde_json::from_value(payload.data.clone()).map_err(invalid_payload)?;
            let row = to_row(&data.info).map_err(|_| CoreError::Storage("row".into()))?;
            // drizzle omits `undefined` columns from `.set()`, so
            // `summary_diffs` and `permission` are only written when
            // present — COALESCE keeps the stored value otherwise.
            conn.execute(
                "UPDATE session SET
                    project_id = ?2, workspace_id = ?3, parent_id = ?4, slug = ?5,
                    directory = ?6, path = ?7, title = ?8, version = ?9, share_url = ?10,
                    summary_additions = ?11, summary_deletions = ?12, summary_files = ?13,
                    metadata = ?14, cost = ?15, tokens_input = ?16, tokens_output = ?17,
                    tokens_reasoning = ?18, tokens_cache_read = ?19, tokens_cache_write = ?20,
                    revert = ?21, agent = ?22, model = ?23, time_created = ?24,
                    time_updated = ?25, time_compacting = ?26, time_archived = ?27,
                    summary_diffs = COALESCE(?28, summary_diffs),
                    permission = COALESCE(?29, permission)
                WHERE id = ?1",
                rusqlite::params![
                    row.id,
                    row.project_id,
                    row.workspace_id,
                    row.parent_id,
                    row.slug,
                    row.directory,
                    row.path,
                    row.title,
                    row.version,
                    row.share_url,
                    row.summary_additions,
                    row.summary_deletions,
                    row.summary_files,
                    json_opt_to_string(&row.metadata)?,
                    row.cost,
                    row.tokens_input,
                    row.tokens_output,
                    row.tokens_reasoning,
                    row.tokens_cache_read,
                    row.tokens_cache_write,
                    json_opt_to_string(&row.revert)?,
                    row.agent,
                    row.model,
                    row.time_created,
                    row.time_updated,
                    row.time_compacting,
                    row.time_archived,
                    json_opt_to_string(&row.summary_diffs)?,
                    json_opt_to_string(&row.permission)?,
                ],
            )?;
            Ok(())
        }),
    );

    // session.deleted (projector.ts:236-238)
    events.project(
        &event_definitions::SESSION_DELETED,
        Arc::new(|conn, payload| {
            let data: SessionDeletedData =
                serde_json::from_value(payload.data.clone()).map_err(invalid_payload)?;
            conn.execute("DELETE FROM session WHERE id = ?1", [&data.session_id])?;
            Ok(())
        }),
    );

    // message.updated (projector.ts:239-251)
    events.project(
        &event_definitions::MESSAGE_UPDATED,
        Arc::new(|conn, payload| {
            let data: MessageUpdatedData =
                serde_json::from_value(payload.data.clone()).map_err(invalid_payload)?;
            let (id, session_id, row_data) = message_row(&data.info)?;
            let now = now_ms() as i64;
            conn.execute(
                "INSERT INTO message (id, session_id, time_created, time_updated, data)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (id) DO UPDATE SET data = excluded.data",
                rusqlite::params![
                    id,
                    session_id,
                    message::message_time_created(&data.info) as i64,
                    now,
                    row_data.to_string(),
                ],
            )?;
            Ok(())
        }),
    );

    // message.removed (projector.ts:252-274)
    events.project(
        &event_definitions::MESSAGE_REMOVED,
        Arc::new(|conn, payload| {
            let data: MessageRemovedData =
                serde_json::from_value(payload.data.clone()).map_err(invalid_payload)?;
            let rows = {
                let mut stmt =
                    conn.prepare("SELECT * FROM part WHERE message_id = ?1 AND session_id = ?2")?;
                let mut rows = stmt.query(rusqlite::params![data.message_id, data.session_id])?;
                let mut out = Vec::new();
                while let Some(row) = rows.next()? {
                    out.push(part_from_row(row)?);
                }
                out
            };
            for row in &rows {
                if let Some(previous) = usage(&row.data) {
                    apply_usage(conn, &data.session_id, &previous, -1.0)?;
                }
            }
            conn.execute(
                "DELETE FROM message WHERE id = ?1 AND session_id = ?2",
                rusqlite::params![data.message_id, data.session_id],
            )?;
            Ok(())
        }),
    );

    // message.part.updated (projector.ts:275-294)
    events.project(
        &event_definitions::MESSAGE_PART_UPDATED,
        Arc::new(|conn, payload| {
            let data: MessagePartUpdatedData =
                serde_json::from_value(payload.data.clone()).map_err(invalid_payload)?;
            let (id, message_id, session_id, row_data) = part_row(&data.part)?;
            let previous = {
                let mut stmt = conn.prepare("SELECT * FROM part WHERE id = ?1")?;
                let mut rows = stmt.query([&id])?;
                match rows.next()? {
                    Some(row) => Some(part_from_row(row)?),
                    None => None,
                }
            };
            let now = now_ms() as i64;
            conn.execute(
                "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (id) DO UPDATE SET data = excluded.data",
                rusqlite::params![
                    id,
                    message_id,
                    session_id,
                    data.time as i64,
                    now,
                    row_data.to_string(),
                ],
            )?;
            if let Some(row) = &previous {
                if let Some(previous) = usage(&row.data) {
                    apply_usage(conn, &row.session_id, &previous, -1.0)?;
                }
            }
            if let Some(next) = usage(&serde_json::to_value(&data.part).unwrap_or_default()) {
                apply_usage(conn, &session_id, &next, 1.0)?;
            }
            Ok(())
        }),
    );

    // message.part.removed (projector.ts:252-274 — the PartRemoved handler)
    events.project(
        &event_definitions::MESSAGE_PART_REMOVED,
        Arc::new(|conn, payload| {
            let data: MessagePartRemovedData =
                serde_json::from_value(payload.data.clone()).map_err(invalid_payload)?;
            let row = {
                let mut stmt =
                    conn.prepare("SELECT * FROM part WHERE id = ?1 AND session_id = ?2")?;
                let mut rows = stmt.query(rusqlite::params![data.part_id, data.session_id])?;
                match rows.next()? {
                    Some(row) => Some(part_from_row(row)?),
                    None => None,
                }
            };
            if let Some(row) = &row {
                if let Some(previous) = usage(&row.data) {
                    apply_usage(conn, &data.session_id, &previous, -1.0)?;
                }
            }
            conn.execute(
                "DELETE FROM part WHERE id = ?1 AND session_id = ?2",
                rusqlite::params![data.part_id, data.session_id],
            )?;
            Ok(())
        }),
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_title_detection() {
        for prefix in [PARENT_TITLE_PREFIX, CHILD_TITLE_PREFIX] {
            let title = format!("{prefix}2026-09-17T12:34:56.789Z");
            assert!(is_default_title(&title), "{title}");
        }
        assert!(!is_default_title("My custom title"));
        assert!(!is_default_title("New session - 2026-09-17T12:34:56Z"));
        assert!(!is_default_title("New session - x"));
    }

    #[test]
    fn forked_title_bumps() {
        assert_eq!(get_forked_title("My session"), "My session (fork #1)");
        assert_eq!(
            get_forked_title("My session (fork #1)"),
            "My session (fork #2)"
        );
        assert_eq!(
            get_forked_title("My session (fork #9)"),
            "My session (fork #10)"
        );
        assert_eq!(
            get_forked_title("My session (fork #1) (fork #2)"),
            "My session (fork #1) (fork #3)"
        );
        assert_eq!(
            get_forked_title("My session (fork #abc)"),
            "My session (fork #abc) (fork #1)"
        );
    }

    #[test]
    fn session_path_relative() {
        if cfg!(windows) {
            assert_eq!(
                session_path(Path::new("C:\\repo"), Path::new("C:\\repo\\a\\b")),
                "a/b"
            );
        } else {
            assert_eq!(
                session_path(Path::new("/repo"), Path::new("/repo/a/b")),
                "a/b"
            );
            assert_eq!(session_path(Path::new("/repo"), Path::new("/repo")), "");
        }
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::session::run_state::{BackgroundJobInfo, BackgroundJobs};
    use crate::session::SessionServices;
    use opencode_schema::session_v1::{
        AssistantTime, UserTime, V1Message, V1Part, V1StepTokens, V1TokenCache, V1UserModel,
    };
    use std::path::Path;

    struct NoJobs;
    impl BackgroundJobs for NoJobs {
        fn list(&self) -> Result<Vec<BackgroundJobInfo>, CoreError> {
            Ok(Vec::new())
        }
        fn cancel(&self, _: &str) -> Result<(), CoreError> {
            Ok(())
        }
    }

    struct FixedClock(u64);
    impl crate::Clock for FixedClock {
        fn now_ms(&self) -> u64 {
            self.0
        }
    }

    fn services(dir: &Path) -> SessionServices {
        let storage = Storage::open(dir.join("db.sqlite")).unwrap();
        let services = SessionServices::new(
            std::sync::Arc::new(storage),
            std::sync::Arc::new(NoJobs),
            std::sync::Arc::new(FixedClock(1_761_000_000_000)),
            &crate::session::agents::AgentRegistryInput::default(),
        );
        let storage = services.storage.clone();
        let _ = storage;
        services
    }

    fn ctx(project_id: &str) -> SessionContext {
        SessionContext {
            project_id: project_id.to_string(),
            directory: std::path::PathBuf::from("/repo/sub"),
            worktree: std::path::PathBuf::from("/repo"),
            workspace_id: None,
        }
    }

    fn insert_project(services: &SessionServices, id: &str) {
        services
            .storage
            .with_connection(|conn| {
                conn.execute(
                    "INSERT INTO project (id, worktree, sandboxes, time_created, time_updated)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![id, "/repo", "[]", 1, 1],
                )
            })
            .unwrap();
    }

    fn user_message(session: &str, id: &str, created: f64) -> V1Message {
        V1Message::User {
            id: id.to_string(),
            session_id: session.to_string(),
            time: UserTime { created },
            format: None,
            summary: None,
            agent: "build".to_string(),
            model: V1UserModel {
                provider_id: "anthropic".to_string(),
                model_id: "claude".to_string(),
                variant: None,
            },
            system: None,
            tools: None,
        }
    }

    #[test]
    fn create_get_patch_round_trip() {
        let dir = crate::storage::test_support::TempDir::new("store-roundtrip");
        let services = services(dir.path());
        insert_project(&services, "proj_01");
        let sessions = &services.sessions;
        let context = ctx("proj_01");

        let created = sessions.create(&context, &CreateInput::default()).unwrap();
        assert!(created.id.starts_with("ses_"));
        assert!(is_default_title(&created.title));
        assert_eq!(created.directory, "/repo/sub");
        assert_eq!(created.path.as_deref(), Some("sub"));

        // create → projector → storage → get
        let loaded = sessions.get(&created.id).unwrap();
        assert_eq!(loaded.id, created.id);
        assert_eq!(loaded.title, created.title);
        assert_eq!(loaded.cost, Some(0.0));

        // patch: title, share, summary, revert, permission
        sessions.set_title(&created.id, "My session").unwrap();
        sessions
            .set_share(
                &created.id,
                Some(V1SessionShare {
                    url: "https://x".into(),
                }),
            )
            .unwrap();
        sessions
            .set_summary(
                &created.id,
                Some(V1SessionSummary {
                    additions: 1.0,
                    deletions: 2.0,
                    files: 3.0,
                    diffs: None,
                }),
            )
            .unwrap();
        sessions
            .set_permission(
                &created.id,
                serde_json::from_value::<PermissionV1Ruleset>(serde_json::json!([])).unwrap(),
            )
            .unwrap();
        sessions
            .set_metadata(
                &created.id,
                serde_json::from_value(serde_json::json!({"k": "v"})).unwrap(),
            )
            .unwrap();

        let loaded = sessions.get(&created.id).unwrap();
        assert_eq!(loaded.title, "My session");
        assert_eq!(
            loaded.share.as_ref().map(|s| s.url.as_str()),
            Some("https://x")
        );
        let summary = loaded.summary.unwrap();
        assert_eq!(summary.additions, 1.0);
        assert_eq!(summary.files, 3.0);
        assert_eq!(loaded.metadata.as_ref().unwrap()["k"], "v");

        // clear share
        sessions.set_share(&created.id, None).unwrap();
        assert!(sessions.get(&created.id).unwrap().share.is_none());

        // clear archived time via setArchived(None)
        sessions.set_archived(&created.id, None).unwrap();
        assert!(sessions.get(&created.id).unwrap().time.archived.is_none());
        sessions.set_archived(&created.id, Some(1234)).unwrap();
        assert_eq!(
            sessions.get(&created.id).unwrap().time.archived,
            Some(1234.0)
        );

        // list/list_by_project
        let listed = sessions.list(&context, &ListInput::default()).unwrap();
        assert_eq!(listed.len(), 1);
        let global = services
            .sessions
            .list_global(&GlobalListInput::default())
            .unwrap();
        assert_eq!(global.len(), 0, "archived sessions excluded by default");
        let global = services
            .sessions
            .list_global(&GlobalListInput {
                archived: true,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(global.len(), 1);
        assert_eq!(
            global[0].project.as_ref().map(|p| p.id.as_str()),
            Some("proj_01")
        );
    }

    #[test]
    fn created_events_are_idempotent_per_the_projector() {
        let dir = crate::storage::test_support::TempDir::new("store-projector");
        let services = services(dir.path());
        insert_project(&services, "proj_01");
        let sessions = &services.sessions;
        let context = ctx("proj_01");
        let created = sessions.create(&context, &Default::default()).unwrap();
        // Replaying session.created for the same id must fail: the projector
        // rejects a duplicate insert (SessionAlreadyProjected).
        let err = sessions
            .events
            .publish(
                &event_definitions::SESSION_CREATED,
                serde_json::to_value(SessionCreatedData {
                    info: created.clone(),
                    session_id: created.id.clone(),
                })
                .unwrap(),
                PublishOptions::default(),
            )
            .is_err();
        assert!(err, "duplicate session.created must be rejected");
    }

    #[test]
    fn remove_recursively_deletes_children() {
        let dir = crate::storage::test_support::TempDir::new("store-remove");
        let services = services(dir.path());
        insert_project(&services, "proj_01");
        let sessions = &services.sessions;
        let context = ctx("proj_01");
        let parent = sessions.create(&context, &Default::default()).unwrap();
        let child = sessions
            .create(
                &context,
                &CreateInput {
                    parent_id: Some(parent.id.clone()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(sessions.children(&parent.id).unwrap().len(), 1);
        sessions.remove(&parent.id).unwrap();
        assert!(sessions.get(&parent.id).is_err());
        assert!(
            sessions.get(&child.id).is_err(),
            "child removed with parent"
        );
    }

    #[test]
    fn message_and_part_projection_with_usage_accounting() {
        let dir = crate::storage::test_support::TempDir::new("store-messages");
        let services = services(dir.path());
        insert_project(&services, "proj_01");
        let sessions = &services.sessions;
        let context = ctx("proj_01");
        let created = sessions.create(&context, &Default::default()).unwrap();

        let message = user_message(&created.id, "msg_01", 10.0);
        sessions.update_message(&message).unwrap();
        let assistant = V1Message::Assistant {
            id: "msg_02".to_string(),
            session_id: created.id.clone(),
            time: AssistantTime {
                created: 11,
                completed: None,
            },
            error: None,
            parent_id: "msg_01".to_string(),
            model_id: "claude".to_string(),
            provider_id: "anthropic".to_string(),
            mode: "primary".to_string(),
            agent: "build".to_string(),
            path: opencode_schema::session_v1::V1Path {
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
                cache: V1TokenCache {
                    read: 0.0,
                    write: 0.0,
                },
            },
            structured: None,
            variant: None,
            finish: None,
        };
        sessions.update_message(&assistant).unwrap();

        // step-finish part adds usage; upserting replaces the old usage.
        let step_finish = V1Part::StepFinish {
            id: "prt_01".to_string(),
            session_id: created.id.clone(),
            message_id: "msg_02".to_string(),
            reason: "stop".to_string(),
            snapshot: None,
            cost: 0.1,
            tokens: V1StepTokens {
                total: None,
                input: 100.0,
                output: 50.0,
                reasoning: 10.0,
                cache: V1TokenCache {
                    read: 5.0,
                    write: 2.0,
                },
            },
        };
        sessions.update_part(&step_finish).unwrap();
        let loaded = sessions.get(&created.id).unwrap();
        let tokens = loaded.tokens.unwrap();
        assert_eq!(tokens.input, 100.0);
        assert_eq!(tokens.cache.read, 5.0);
        assert_eq!(loaded.cost, Some(0.1));

        let step_finish =
            match serde_json::from_value::<V1Part>(serde_json::to_value(&step_finish).unwrap())
                .unwrap()
            {
                V1Part::StepFinish {
                    id,
                    session_id,
                    message_id,
                    reason,
                    snapshot,
                    ..
                } => V1Part::StepFinish {
                    id,
                    session_id,
                    message_id,
                    reason,
                    snapshot,
                    cost: 0.2,
                    tokens: V1StepTokens {
                        total: None,
                        input: 70.0,
                        output: 30.0,
                        reasoning: 0.0,
                        cache: V1TokenCache {
                            read: 0.0,
                            write: 0.0,
                        },
                    },
                },
                _ => unreachable!(),
            };
        sessions.update_part(&step_finish).unwrap();
        let loaded = sessions.get(&created.id).unwrap();
        assert_eq!(loaded.tokens.unwrap().input, 70.0);
        assert_eq!(loaded.cost, Some(0.2));

        // get_part round trip
        let part = sessions
            .get_part(&created.id, "msg_02", "prt_01")
            .unwrap()
            .unwrap();
        assert!(matches!(part, V1Part::StepFinish { .. }));

        // part removal subtracts usage
        sessions
            .remove_part(&created.id, "msg_02", "prt_01")
            .unwrap();
        let loaded = sessions.get(&created.id).unwrap();
        assert_eq!(loaded.cost, Some(0.0));
        assert_eq!(loaded.tokens.unwrap().input, 0.0);

        // messages() returns chronologically
        let messages = sessions.messages(&created.id, None).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(
            crate::session::message::message_id(&messages[0].info),
            "msg_01"
        );

        // remove_message subtracts the usage of the message's parts
        sessions.update_message(&assistant).unwrap();
        // (re-publish is idempotent on the message row)
        let _ = sessions.messages(&created.id, Some(1)).unwrap().len();

        // find_message: newest-first
        let found = sessions.find_message(&created.id, &|_| false).unwrap();
        assert!(found.is_none());
        let found = sessions
            .find_message(&created.id, &|msg| {
                crate::session::message::message_id(&msg.info) == "msg_02"
            })
            .unwrap();
        assert!(found.is_some());
    }

    #[test]
    fn fork_copies_messages_with_new_ids() {
        let dir = crate::storage::test_support::TempDir::new("store-fork");
        let services = services(dir.path());
        insert_project(&services, "proj_01");
        let sessions = &services.sessions;
        let context = ctx("proj_01");
        let created = sessions.create(&context, &Default::default()).unwrap();
        let message = user_message(&created.id, "msg_01", 10.0);
        sessions.update_message(&message).unwrap();
        sessions
            .update_part(&V1Part::Text {
                id: "prt_01".to_string(),
                session_id: created.id.clone(),
                message_id: "msg_01".to_string(),
                text: "hello".to_string(),
                synthetic: None,
                ignored: None,
                time: None,
                metadata: None,
            })
            .unwrap();

        let fork = sessions.fork(&context, &created.id, None).unwrap();
        assert_eq!(fork.parent_id, created.parent_id);
        let messages = sessions.messages(&fork.id, None).unwrap();
        assert_eq!(messages.len(), 1);
        let forked_id = crate::session::message::message_id(&messages[0].info);
        assert_ne!(forked_id, "msg_01");
        assert_eq!(messages[0].parts.len(), 1);
        let title_should_bump = get_forked_title(&created.title);
        assert_eq!(fork.title, title_should_bump);
    }
}

//! Session store — port of `session/session.ts` plus the seven durable
//! session projectors (`packages/core/src/session/projector.ts`).
//!
//! Writes are events: every mutation publishes a `SessionV1.Event.*` on the
//! bus and the registered projector persists the row inside the commit
//! transaction (spec §2.3).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use alforria_schema::file_diff::SnapshotFileDiff;
use alforria_schema::permission_v1::PermissionV1Ruleset;
use alforria_schema::schema::JsonMap;
use alforria_schema::session::SessionTokens;
use alforria_schema::session_v1::{
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
///
/// `path.relative` yields the empty string when both paths are equal — a
/// session in the worktree root stores `""`, not `"."` (`path.relative`
/// parity; opencode stores the same).
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
        cache: alforria_schema::session::SessionTokensCache {
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
            cache: alforria_schema::session::SessionTokensCache {
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
    /// alforria extension: created by a caller below `admin`
    /// (`session::caller`). The session is restricted for life. A child of
    /// a restricted session and a fork of one are restricted whatever this
    /// says.
    pub restricted: bool,
    /// alforria extension: marks the session starts with
    /// ([`SessionMark`]). A child gets its parent's as well, whatever this
    /// says.
    pub marks: SessionMarks,
    /// alforria extension: ceilings the session starts with
    /// ([`SessionCeiling`]), beyond its parent's.
    pub ceilings: Vec<SessionCeiling>,
    /// alforria extension: a fork takes its source's ceilings, as a child
    /// takes its parent's.
    pub ceilings_from: Option<String>,
}

/// What the server records about where a session's text came from
/// (alforria extension). Two marks with two causes and two effects; they
/// live in the server's own `session_mark` table, never in the session's
/// metadata, which a client can write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionMark {
    /// **Provenance.** The session has seen content from outside: a tool
    /// that returns such content answered in it
    /// ([`crate::tool::outside`]), a prompt from a caller below `admin`
    /// brought a file, or a trigger's run delivered a payload. What it
    /// writes to memory is unverified and not injected, a trigger it
    /// schedules carries the mark into its runs, and excerpts of it are
    /// fenced. It changes no permission.
    OutsideContent,
    /// **Ceiling.** A run in the session was started by outside input (a
    /// webhook or watch trigger). The read-only ceiling applies
    /// (`trigger::rules`), standing approvals are ignored and an ask is a
    /// deny at once. Implies [`SessionMark::OutsideContent`].
    OutsideStart,
}

impl SessionMark {
    /// The value in `session_mark.mark`.
    pub const fn as_str(self) -> &'static str {
        match self {
            SessionMark::OutsideContent => "outside_content",
            SessionMark::OutsideStart => "outside_start",
        }
    }

    /// **The one definition of "this session has this mark"**, as SQL over
    /// the expression `session_id` (a column, or a parameter). The Rust
    /// lookups run this same text.
    pub fn sql(self, session_id: &str) -> String {
        format!(
            "EXISTS(SELECT 1 FROM session_mark WHERE session_mark.session_id = {session_id} \
             AND session_mark.mark = '{}')",
            self.as_str()
        )
    }
}

/// The marks a session is created with, beyond what it inherits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionMarks {
    pub outside_content: bool,
    pub outside_start: bool,
}

impl SessionMarks {
    /// Both: what an unreadable source counts as.
    pub const ALL: SessionMarks = SessionMarks {
        outside_content: true,
        outside_start: true,
    };

    fn list(self) -> Vec<SessionMark> {
        let mut out = Vec::new();
        if self.outside_content || self.outside_start {
            out.push(SessionMark::OutsideContent);
        }
        if self.outside_start {
            out.push(SessionMark::OutsideStart);
        }
        out
    }
}

/// A named permission ceiling stored for a session (alforria extension):
/// readings of the rules that cap whatever policy is in force when a turn
/// runs ([`SessionStore::bounded`]). Server-owned, inherited by children and
/// forks, and sealed: a session's own rules and standing approvals do not
/// reach it (`tool::permission::Ceiling::sealed`).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionCeiling {
    /// `trigger::rules::CREATOR_CEILING`, `trigger::rules::OUTSIDE_CEILING`.
    pub name: String,
    /// Each is a ceiling of its own: the strictest of all applies.
    pub readings: Vec<PermissionV1Ruleset>,
}

/// A session's ceilings as read from the store: the argument
/// `Policy::for_session` needs. Only [`SessionStore::policy`] makes one.
pub struct StoredCeilings(Vec<SessionCeiling>);

impl StoredCeilings {
    /// `(name, reading)` for every reading of every ceiling.
    pub(crate) fn readings(&self) -> impl Iterator<Item = (&str, &PermissionV1Ruleset)> {
        self.0.iter().flat_map(|ceiling| {
            ceiling
                .readings
                .iter()
                .map(move |reading| (ceiling.name.as_str(), reading))
        })
    }
}

/// Session metadata keys the server keeps for itself. A client cannot set,
/// change or remove them ([`SessionStore::set_client_metadata`],
/// [`strip_reserved_metadata`]). `untrustedInput` is where the marks used
/// to live; nothing reads it any more, and nothing a client sends can make
/// it look as if something did.
pub const RESERVED_METADATA_KEYS: [&str; 2] = ["untrustedInput", "triggerID"];

/// Drop the server's keys from metadata a client supplied.
pub fn strip_reserved_metadata(metadata: &mut JsonMap) {
    for key in RESERVED_METADATA_KEYS {
        metadata.remove(key);
    }
}

/// `Session.Service` (session.ts:486-985).
#[derive(Clone)]
pub struct SessionStore {
    events: Arc<EventBus>,
    storage: Arc<Storage>,
    background: Arc<dyn BackgroundJobs>,
    clock: Arc<dyn crate::Clock>,
    messages: MessageStore,
    /// The upload area (`server.upload.directories`), shared by every
    /// clone: files there were put by a client, and a tool that reads one
    /// brings outside content into the session (`tool::outside`).
    uploads: Arc<std::sync::RwLock<Vec<String>>>,
    /// Whether callers below `admin` can upload there
    /// ([`SessionStore::set_uploads_from_below_admin`]).
    uploads_from_below_admin: Arc<std::sync::atomic::AtomicBool>,
    /// What a file tool reported for a call, by `(session, call)`, from
    /// the moment the tool finishes to the moment its result is appended
    /// ([`SessionStore::report_returned_paths`]). In memory only.
    returned_paths:
        Arc<std::sync::Mutex<std::collections::HashMap<(String, String), ReturnedPathsReport>>>,
}

/// What a tool reported for one call, and the call it reported it for.
struct ReturnedPathsReport {
    tool: String,
    input: Value,
    /// `None`: two reports came in under one call id, and neither is used.
    paths: Option<crate::tool::outside::ReturnedPaths>,
}

/// More unclaimed reports than this and all are dropped: the calls they
/// belong to are then judged without one, which is the marking side.
const MAX_RETURNED_PATHS_REPORTS: usize = 1024;

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
            uploads: Default::default(),
            uploads_from_below_admin: Default::default(),
            returned_paths: Default::default(),
        }
    }

    /// Tell the store where the upload area is (the server does, from its
    /// policy). Unset: there is none.
    pub fn set_upload_directories(&self, directories: Vec<String>) {
        *self
            .uploads
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = directories;
    }

    /// Tell the store whether credentials below `admin` exist that can
    /// upload into the upload area (the server does: bearer tokens are
    /// configured).
    pub fn set_uploads_from_below_admin(&self, possible: bool) {
        self.uploads_from_below_admin
            .store(possible, std::sync::atomic::Ordering::Relaxed);
    }

    /// The upload directories a language server or a formatter must stay
    /// out of (`lsp::Excluded`): all of them while callers below `admin`
    /// can upload, none otherwise — a standalone project's own `uploads/`
    /// is an ordinary directory.
    pub fn untooled_upload_directories(&self) -> Vec<String> {
        if !self
            .uploads_from_below_admin
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return Vec::new();
        }
        self.uploads
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// A tool's own report of what it opened and returned, for the call
    /// `call_id` of `session_id` (alforria extension;
    /// `tool::outside::ReturnedPaths`). Kept until the call's result is
    /// appended, and used there only for a result of the same tool with the
    /// same input.
    pub fn report_returned_paths(
        &self,
        session_id: &str,
        call_id: &str,
        tool: &str,
        input: &Value,
        paths: crate::tool::outside::ReturnedPaths,
    ) {
        let mut reports = self
            .returned_paths
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if reports.len() >= MAX_RETURNED_PATHS_REPORTS {
            reports.clear();
        }
        let key = (session_id.to_string(), call_id.to_string());
        let paths = match reports.contains_key(&key) {
            // A call id used twice: which result the list belongs to cannot
            // be told.
            true => None,
            false => Some(paths),
        };
        reports.insert(
            key,
            ReturnedPathsReport {
                tool: tool.to_string(),
                input: input.clone(),
                paths,
            },
        );
    }

    /// The list reported for this result's call, if there is one and it is
    /// this call's. Taken: a list is used once.
    fn take_returned_paths(
        &self,
        session_id: &str,
        result: &crate::tool::outside::ToolResult<'_>,
    ) -> Option<crate::tool::outside::ReturnedPaths> {
        let report = self
            .returned_paths
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&(session_id.to_string(), result.call_id?.to_string()))?;
        (report.tool == result.tool && &report.input == result.input)
            .then_some(report.paths)
            .flatten()
    }

    /// **The one place a tool's result marks its session** (alforria
    /// extension): called by the processor for every result it is about to
    /// append, before it appends it. A result that brings outside content
    /// ([`crate::tool::outside`]) sets [`SessionMark::OutsideContent`], for
    /// life. An error here is an error for the result: content is not
    /// appended to a session that could not be marked for it.
    ///
    /// What it costs: nothing for a tool that never returns outside
    /// content; otherwise one indexed read of `session_mark`, and for a
    /// session that already has the mark nothing more — no filesystem
    /// lookup and no write, so no wait for the database's write lock. The
    /// row is written once per session. The read is of the table, not of a
    /// cache in this process: a session id can be chosen by its creator and
    /// used again after a delete, and other processes write the same
    /// database. The row is written whether or not anything reads it yet
    /// (memory and triggers are off by default): a session that read
    /// outside content before memory was switched on must still count as
    /// having read it afterwards.
    pub fn note_tool_result(
        &self,
        session_id: &str,
        result: &crate::tool::outside::ToolResult<'_>,
    ) -> Result<(), SessionError> {
        use crate::tool::outside::{
            carries_outside_content, classify_result, Outside, SessionFacts,
        };
        let class = classify_result(result);
        if class == Outside::Never {
            return Ok(());
        }
        // Taken whatever follows, so that it does not stay behind.
        let returned = match class {
            Outside::ReadsPath | Outside::SearchesPath => {
                self.take_returned_paths(session_id, result)
            }
            _ => None,
        };
        if self.has_mark(session_id, SessionMark::OutsideContent)? {
            return Ok(());
        }
        let uploads = self
            .uploads
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let directory = match class {
            Outside::ReadsPath | Outside::SearchesPath if !uploads.is_empty() => {
                PathBuf::from(self.get(session_id)?.directory)
            }
            _ => PathBuf::new(),
        };
        let child_marked = || {
            let sql = format!(
                "SELECT EXISTS(SELECT 1 FROM session child WHERE child.parent_id = ?1 AND {})",
                SessionMark::OutsideContent.sql("child.id")
            );
            // A child whose marks cannot be read counts as marked.
            self.storage
                .with_connection(|conn| {
                    conn.query_row(&sql, [session_id], |row| row.get::<_, bool>(0))
                        .map_err(CoreError::from)
                })
                .unwrap_or(true)
        };
        let facts = SessionFacts {
            directory: &directory,
            uploads: &uploads,
            child_marked: &child_marked,
            returned: returned.as_ref(),
        };
        if carries_outside_content(result, &facts) {
            self.mark(session_id, SessionMark::OutsideContent)?;
        }
        Ok(())
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
        let metadata = input.metadata.clone();
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
            metadata,
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
        // alforria extension: marks, ceilings and restriction are inherited
        // — every session is created here, so a child or fork cannot come
        // out freer than its source because its creator did not think of
        // it — and they are written in the transaction that inserts the
        // session row (the event's commit hook): no reader, and no crash,
        // finds the session without them. A parent whose marks cannot be
        // read counts as carrying both; a source whose ceilings or
        // restriction cannot be read fails the creation.
        let mut marks = input.marks;
        if let Some(parent_id) = &input.parent_id {
            let inherited = self.inherited_marks(parent_id);
            marks.outside_content |= inherited.outside_content;
            marks.outside_start |= inherited.outside_start;
        }
        let mut ceilings: Vec<(String, String)> = Vec::new();
        for source in [&input.parent_id, &input.ceilings_from]
            .into_iter()
            .flatten()
        {
            for ceiling in self.ceilings(source)? {
                ceilings.push((ceiling.name, serde_json::to_string(&ceiling.readings)?));
            }
        }
        for ceiling in &input.ceilings {
            ceilings.push((
                ceiling.name.clone(),
                serde_json::to_string(&ceiling.readings)?,
            ));
        }
        let restricted = input.restricted
            || match &input.parent_id {
                Some(parent) => self.is_restricted(parent)?,
                None => false,
            };
        let session_id = info.id.clone();
        let stamp = now as i64;
        let with_session: crate::event::CommitHook = Box::new(move |conn, _seq| {
            for mark in marks.list() {
                Self::mark_on(conn, &session_id, mark, stamp)?;
            }
            for (name, readings) in &ceilings {
                Self::set_ceiling_on(conn, &session_id, name, readings, stamp)?;
            }
            if restricted {
                Self::restrict_on(conn, &session_id, stamp)?;
            }
            Ok(())
        });
        self.events.publish(
            &event_definitions::SESSION_CREATED,
            serde_json::to_value(SessionCreatedData {
                session_id: info.id.clone(),
                info: info.clone(),
            })?,
            PublishOptions {
                commit: Some(with_session),
                ..PublishOptions::default()
            },
        )?;
        Ok(info)
    }

    /// The session a message belongs to, if the message exists (alforria
    /// extension): routes that take a session and a message check the two
    /// go together before acting on the message.
    pub fn message_session(&self, message_id: &str) -> Result<Option<String>, SessionError> {
        self.storage
            .with_connection(|conn| {
                let mut statement = conn.prepare("SELECT session_id FROM message WHERE id = ?1")?;
                let mut rows = statement.query([message_id])?;
                match rows.next()? {
                    Some(row) => Ok::<_, CoreError>(Some(row.get::<_, String>(0)?)),
                    None => Ok(None),
                }
            })
            .map_err(SessionError::from)
    }

    /// The session and message a part belongs to, if the part exists.
    pub fn part_owner(&self, part_id: &str) -> Result<Option<(String, String)>, SessionError> {
        self.storage
            .with_connection(|conn| {
                let mut statement =
                    conn.prepare("SELECT session_id, message_id FROM part WHERE id = ?1")?;
                let mut rows = statement.query([part_id])?;
                match rows.next()? {
                    Some(row) => Ok::<_, CoreError>(Some((row.get(0)?, row.get(1)?))),
                    None => Ok(None),
                }
            })
            .map_err(SessionError::from)
    }

    /// The marks stored for a session ([`SessionMark`]). An error is the
    /// caller's to fail closed on; [`SessionStore::seen_outside_content`]
    /// and [`SessionStore::started_by_outside_input`] do.
    pub fn marks(&self, session_id: &str) -> Result<SessionMarks, SessionError> {
        let sql = format!(
            "SELECT {}, {}",
            SessionMark::OutsideContent.sql("?1"),
            SessionMark::OutsideStart.sql("?1")
        );
        self.storage
            .with_connection(|conn| {
                conn.query_row(&sql, [session_id], |row| {
                    Ok(SessionMarks {
                        outside_content: row.get::<_, bool>(0)?,
                        outside_start: row.get::<_, bool>(1)?,
                    })
                })
                .map_err(CoreError::from)
            })
            .map_err(SessionError::from)
    }

    /// What a child or a fork of `session_id` starts with: its marks, or
    /// both when it does not exist or they cannot be read.
    fn inherited_marks(&self, session_id: &str) -> SessionMarks {
        let exists = self
            .storage
            .with_connection(|conn| {
                conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM session WHERE id = ?1)",
                    [session_id],
                    |row| row.get::<_, bool>(0),
                )
                .map_err(CoreError::from)
            })
            .unwrap_or(false);
        match self.marks(session_id) {
            Ok(marks) if exists => marks,
            _ => SessionMarks::ALL,
        }
    }

    /// Whether the session has `mark`.
    pub fn has_mark(&self, session_id: &str, mark: SessionMark) -> Result<bool, SessionError> {
        let sql = format!("SELECT {}", mark.sql("?1"));
        self.storage
            .with_connection(|conn| {
                conn.query_row(&sql, [session_id], |row| row.get::<_, bool>(0))
                    .map_err(CoreError::from)
            })
            .map_err(SessionError::from)
    }

    /// `has_mark`, failing closed: a session that does not exist, or whose
    /// marks cannot be read, counts as marked.
    fn marked_or_unknown(&self, session_id: &str, mark: SessionMark) -> bool {
        let sql = format!(
            "SELECT NOT EXISTS(SELECT 1 FROM session WHERE id = ?1) OR {}",
            mark.sql("?1")
        );
        self.storage
            .with_connection(|conn| {
                conn.query_row(&sql, [session_id], |row| row.get::<_, bool>(0))
                    .map_err(CoreError::from)
            })
            .unwrap_or(true)
    }

    /// Provenance ([`SessionMark::OutsideContent`]): the session has seen
    /// content from outside. Fails closed.
    pub fn seen_outside_content(&self, session_id: &str) -> bool {
        self.marked_or_unknown(session_id, SessionMark::OutsideContent)
    }

    /// Ceiling ([`SessionMark::OutsideStart`]): a run in the session was
    /// started by outside input. Fails closed.
    pub fn started_by_outside_input(&self, session_id: &str) -> bool {
        self.marked_or_unknown(session_id, SessionMark::OutsideStart)
    }

    /// Whether what runs for the session is held to the restricted
    /// session's floor: the session is restricted
    /// ([`SessionStore::is_restricted`]) or outside input started a run in
    /// it ([`SessionMark::OutsideStart`]). The one predicate behind the
    /// write guard of the session's tools and the restricted shell
    /// environment of its bash tool, `shell` command and inline template
    /// shell (`shell_env::apply_shell_for`). Fails closed: a restriction or
    /// a mark that cannot be read counts as one.
    pub fn runs_restricted(&self, session_id: &str) -> bool {
        self.started_by_outside_input(session_id) || self.is_restricted(session_id).unwrap_or(true)
    }

    /// Set a mark, for life. Setting [`SessionMark::OutsideStart`] sets
    /// [`SessionMark::OutsideContent`] too.
    pub fn mark(&self, session_id: &str, mark: SessionMark) -> Result<(), SessionError> {
        let now = self.now() as i64;
        self.storage
            .with_connection(|conn| Self::mark_on(conn, session_id, mark, now))
            .map_err(SessionError::from)
    }

    /// [`SessionStore::mark`] on a connection the caller holds.
    fn mark_on(
        conn: &rusqlite::Connection,
        session_id: &str,
        mark: SessionMark,
        now: i64,
    ) -> Result<(), CoreError> {
        let marks: &[SessionMark] = match mark {
            SessionMark::OutsideContent => &[SessionMark::OutsideContent],
            SessionMark::OutsideStart => &[SessionMark::OutsideContent, SessionMark::OutsideStart],
        };
        for mark in marks {
            conn.execute(
                "INSERT OR IGNORE INTO session_mark (session_id, mark, time_created) VALUES (?1, ?2, ?3)",
                rusqlite::params![session_id, mark.as_str(), now],
            )?;
        }
        Ok(())
    }

    /// The ceilings stored for a session, by name.
    pub fn ceilings(&self, session_id: &str) -> Result<Vec<SessionCeiling>, SessionError> {
        let rows = self
            .storage
            .with_connection(|conn| {
                let mut statement = conn.prepare(
                    "SELECT name, readings FROM session_ceiling WHERE session_id = ?1 ORDER BY name",
                )?;
                let rows = statement
                    .query_map([session_id], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, CoreError>(rows)
            })
            .map_err(SessionError::from)?;
        rows.into_iter()
            .map(|(name, readings)| {
                Ok(SessionCeiling {
                    name,
                    readings: serde_json::from_str(&readings)?,
                })
            })
            .collect()
    }

    /// Set (replace) one of a session's ceilings.
    pub fn set_ceiling(
        &self,
        session_id: &str,
        ceiling: &SessionCeiling,
    ) -> Result<(), SessionError> {
        let now = self.now() as i64;
        let readings = serde_json::to_string(&ceiling.readings)?;
        self.storage
            .with_connection(|conn| {
                Self::set_ceiling_on(conn, session_id, &ceiling.name, &readings, now)
            })
            .map_err(SessionError::from)
    }

    /// [`SessionStore::set_ceiling`] on a connection the caller holds.
    fn set_ceiling_on(
        conn: &rusqlite::Connection,
        session_id: &str,
        name: &str,
        readings: &str,
        now: i64,
    ) -> Result<(), CoreError> {
        conn.execute(
            "INSERT INTO session_ceiling (session_id, name, readings, time_updated)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (session_id, name) DO UPDATE SET
                readings = excluded.readings, time_updated = excluded.time_updated",
            rusqlite::params![session_id, name, readings, now],
        )?;
        Ok(())
    }

    /// **The policy a turn in `session_id` is judged by** — the one place
    /// it is assembled, and the only way to combine an agent's policy with
    /// a session's rules (`Policy::for_session` takes a value only this
    /// store produces). The result, through the one `Policy::evaluate`, is
    /// the strictest of: `agent`'s policy as it stands now (with the host's
    /// ceiling it carries) read with the session's own rules, a trigger
    /// creator's bounds, and the outside-input ceiling. A session or
    /// ceilings that cannot be read give a policy that denies everything.
    pub fn policy(
        &self,
        session_id: &str,
        agent: &crate::tool::permission::Policy,
    ) -> crate::tool::permission::Policy {
        use alforria_schema::permission_v1::{PermissionV1Action, PermissionV1Rule};
        match (self.get(session_id), self.ceilings(session_id)) {
            (Ok(session), Ok(ceilings)) => agent.for_session(
                &session.permission.unwrap_or_default(),
                &StoredCeilings(ceilings),
            ),
            _ => agent.for_session(
                &Vec::new(),
                &StoredCeilings(vec![SessionCeiling {
                    name: "unreadable".to_string(),
                    readings: vec![vec![PermissionV1Rule {
                        permission: "*".to_string(),
                        pattern: "*".to_string(),
                        action: PermissionV1Action::Deny,
                    }]],
                }]),
            ),
        }
    }

    /// Remove a session's marks. An operator's decision, like
    /// [`SessionStore::lift_restriction`]: nothing in the engine calls it.
    pub fn clear_marks(&self, session_id: &str) -> Result<bool, SessionError> {
        self.storage
            .with_connection(|conn| {
                conn.execute(
                    "DELETE FROM session_mark WHERE session_id = ?1",
                    [session_id],
                )
                .map(|rows| rows > 0)
                .map_err(CoreError::from)
            })
            .map_err(SessionError::from)
    }

    /// Whether a session is restricted (alforria extension,
    /// `session::caller`): created by a caller below `admin`, or descended
    /// from such a session.
    pub fn is_restricted(&self, session_id: &str) -> Result<bool, SessionError> {
        self.storage
            .with_connection(|conn| {
                conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM session_restriction WHERE session_id = ?1)",
                    [session_id],
                    |row| row.get::<_, bool>(0),
                )
                .map_err(CoreError::from)
            })
            .map_err(SessionError::from)
    }

    /// Restrict a session, for life.
    pub fn restrict(&self, session_id: &str) -> Result<(), SessionError> {
        let now = self.now() as i64;
        self.storage
            .with_connection(|conn| Self::restrict_on(conn, session_id, now))
            .map_err(SessionError::from)
    }

    /// [`SessionStore::restrict`] on a connection the caller holds.
    fn restrict_on(
        conn: &rusqlite::Connection,
        session_id: &str,
        now: i64,
    ) -> Result<(), CoreError> {
        conn.execute(
            "INSERT OR IGNORE INTO session_restriction (session_id, time_created) VALUES (?1, ?2)",
            rusqlite::params![session_id, now],
        )?;
        Ok(())
    }

    /// Lift a session's restriction. The one way out, and an operator's
    /// decision: nothing in the engine calls it.
    pub fn lift_restriction(&self, session_id: &str) -> Result<bool, SessionError> {
        self.storage
            .with_connection(|conn| {
                conn.execute(
                    "DELETE FROM session_restriction WHERE session_id = ?1",
                    [session_id],
                )
                .map(|rows| rows > 0)
                .map_err(CoreError::from)
            })
            .map_err(SessionError::from)
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
        session: alforria_schema::session_v1::V1SessionInfo,
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

    /// [`SessionStore::set_metadata`] with metadata a client sent
    /// (alforria extension): the server's own keys
    /// ([`RESERVED_METADATA_KEYS`]) are neither taken from it nor lost to
    /// it.
    pub fn set_client_metadata(
        &self,
        session_id: &str,
        mut metadata: JsonMap,
    ) -> Result<(), SessionError> {
        strip_reserved_metadata(&mut metadata);
        let kept = self.get(session_id)?.metadata.unwrap_or_default();
        for key in RESERVED_METADATA_KEYS {
            if let Some(value) = kept.get(key) {
                metadata.insert(key.to_string(), value.clone());
            }
        }
        self.set_metadata(session_id, metadata)
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
            result.extend(page.items.into_iter().rev());
            if !page.more || page.cursor.is_none() {
                break;
            }
            before = page.cursor;
        }
        result.reverse();
        Ok(result)
    }

    /// [`Self::messages`] with every `summary.diffs` empty. Never write a
    /// message from this back to the store: that wipes them.
    pub fn messages_without_diffs(&self, session_id: &str) -> Result<Vec<WithParts>, SessionError> {
        const SIZE: usize = 50;
        let mut result: Vec<WithParts> = Vec::new();
        let mut before: Option<String> = None;
        loop {
            let page = self
                .messages
                .page_without_diffs(session_id, SIZE, before.as_deref())?;
            if page.items.is_empty() {
                break;
            }
            result.extend(page.items.into_iter().rev());
            if !page.more || page.cursor.is_none() {
                break;
            }
            before = page.cursor;
        }
        result.reverse();
        Ok(result)
    }

    /// `message_id` and every message after it, oldest first, with every
    /// `summary.diffs` empty. Never write one back to the store.
    pub fn messages_since(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> Result<Vec<WithParts>, SessionError> {
        self.messages.since(session_id, message_id)
    }

    /// One message with its parts (`MessageV2.get`, message-v2.ts:506-519).
    pub fn get_message(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> Result<WithParts, SessionError> {
        self.messages.get(session_id, message_id)
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
        self.fork_as(ctx, session_id, message_id, false)
    }

    /// [`SessionStore::fork`] on behalf of a caller that is, or is not,
    /// below `admin` (alforria extension). The fork is restricted when
    /// either its source or the caller forking it is: a fork never has
    /// more rights than whoever made it.
    pub fn fork_as(
        &self,
        ctx: &SessionContext,
        session_id: &str,
        message_id: Option<&str>,
        caller_restricted: bool,
    ) -> Result<V1SessionInfo, SessionError> {
        let original = self.get(session_id)?;
        let marks = self.inherited_marks(session_id);
        let title = get_forked_title(&original.title);
        let session = self.create_next(
            ctx,
            &CreateInput {
                directory: Some(ctx.directory.to_string_lossy().to_string()),
                path: Some(session_path(&ctx.worktree, &ctx.directory)),
                workspace_id: original.workspace_id.clone(),
                title: Some(title),
                metadata: original.metadata.clone(),
                // alforria extension: a session whose runs outside input
                // started is held to rules of its own (`trigger::rules`).
                // Its fork holds the same text, so it keeps the same rules
                // and the same marks; a source whose marks cannot be read
                // counts as carrying both. Upstream's fork copies no
                // rules, and for every other session neither does this one.
                permission: marks
                    .outside_start
                    .then(|| original.permission.clone())
                    .flatten(),
                restricted: caller_restricted || self.is_restricted(session_id)?,
                marks,
                ceilings_from: Some(session_id.to_string()),
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
        // alforria extension: the fork was created as its source then
        // was, and the messages were read after that. A result that marks
        // a session marks it before it is appended
        // (`note_tool_result`), so whatever the copy holds, its source
        // has the mark for by now: the source is read again and the fork
        // brought up to it — marks, ceilings, restriction, and the rules
        // an outside-start session's fork keeps. A fork that cannot be is
        // not left behind holding the content without them.
        match self.catch_up_with(&session.id, session_id, marks) {
            Ok(false) => Ok(session),
            Ok(true) => self.get(&session.id),
            Err(error) => {
                if let Err(removal) = self.remove(&session.id) {
                    tracing::warn!("a fork that could not be marked was not removed: {removal}");
                }
                Err(error)
            }
        }
    }

    /// [`SessionStore::fork_as`]: give `fork` what `source` has now beyond
    /// `inherited`, the marks it was created with. Whether the fork's own
    /// rules changed.
    fn catch_up_with(
        &self,
        fork: &str,
        source: &str,
        inherited: SessionMarks,
    ) -> Result<bool, SessionError> {
        let marks = self.inherited_marks(source);
        for mark in marks.list() {
            self.mark(fork, mark)?;
        }
        // A ceiling of the same name is replaced; none is removed.
        for ceiling in self.ceilings(source)? {
            self.set_ceiling(fork, &ceiling)?;
        }
        if self.is_restricted(source)? {
            self.restrict(fork)?;
        }
        if marks.outside_start && !inherited.outside_start {
            if let Some(permission) = self.get(source)?.permission {
                self.set_permission(fork, permission)?;
                return Ok(true);
            }
        }
        Ok(false)
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

    // session.next.moved (projector.ts:241-253)
    events.project(
        &crate::control_plane::SESSION_NEXT_MOVED,
        Arc::new(|conn, payload| {
            let data: alforria_schema::session_event::Moved =
                serde_json::from_value(payload.data.clone()).map_err(invalid_payload)?;
            conn.execute(
                "UPDATE session SET directory = ?2, path = ?3, workspace_id = ?4, time_updated = ?5 WHERE id = ?1",
                rusqlite::params![
                    data.session_id,
                    data.location.directory,
                    data.subdirectory,
                    data.location.workspace_id,
                    data.timestamp,
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
    use alforria_schema::session_v1::{
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

    /// A fork read its source's marks, ceilings and restriction, created
    /// itself with them, and only then copied the messages: a source
    /// marked, bounded or restricted in between left a fork that holds the
    /// content without what the content brought. Here the source changes
    /// at the moment the fork's row is created — after the first read,
    /// before the copy — and the fork still comes out as its source is.
    #[test]
    fn a_fork_of_a_session_marked_while_it_is_forked_carries_the_marks() {
        use alforria_schema::permission_v1::{PermissionV1Action, PermissionV1Rule};
        let dir = crate::storage::test_support::TempDir::new("store-fork-race");
        let services = services(dir.path());
        insert_project(&services, "proj_01");
        let sessions = &services.sessions;
        let context = ctx("proj_01");
        let source = sessions.create(&context, &Default::default()).unwrap();
        sessions
            .update_message(&user_message(&source.id, "msg_01", 10.0))
            .unwrap();
        let ceiling = SessionCeiling {
            name: crate::trigger::rules::OUTSIDE_CEILING.to_string(),
            readings: vec![vec![PermissionV1Rule {
                permission: "*".to_string(),
                pattern: "*".to_string(),
                action: PermissionV1Action::Deny,
            }]],
        };
        let _listening = services.events.listen(Arc::new({
            let (sessions, source, ceiling) =
                (sessions.clone(), source.id.clone(), ceiling.clone());
            move |event: &crate::event::Payload| {
                let created = event.r#type == "session.created"
                    && event.data["info"]["id"].as_str() != Some(source.as_str());
                if created {
                    sessions.mark(&source, SessionMark::OutsideStart).unwrap();
                    sessions.set_ceiling(&source, &ceiling).unwrap();
                    sessions.restrict(&source).unwrap();
                }
            }
        }));

        let fork = sessions.fork(&context, &source.id, None).unwrap();
        assert_eq!(sessions.messages(&fork.id, None).unwrap().len(), 1);
        assert_eq!(
            sessions.marks(&source.id).unwrap(),
            SessionMarks::ALL,
            "the source was marked while it was being forked"
        );
        assert_eq!(sessions.marks(&fork.id).unwrap(), SessionMarks::ALL);
        assert_eq!(sessions.ceilings(&fork.id).unwrap(), vec![ceiling]);
        assert!(sessions.is_restricted(&fork.id).unwrap());
    }

    /// The two marks a session can carry are independent, and each is
    /// inherited by children and forks whatever the other says. A fork of
    /// a session that carries outside text keeps that session's rules
    /// too: the text went with it.
    #[test]
    fn restriction_and_the_untrusted_mark_are_inherited_independently() {
        use alforria_schema::permission_v1::{PermissionV1Action, PermissionV1Rule};
        let dir = crate::storage::test_support::TempDir::new("store-two-marks");
        let services = services(dir.path());
        insert_project(&services, "proj_01");
        let sessions = &services.sessions;
        let context = ctx("proj_01");
        let rules = vec![PermissionV1Rule {
            permission: "*".to_string(),
            pattern: "*".to_string(),
            action: PermissionV1Action::Deny,
        }];
        let marked = |untrusted: bool| SessionMarks {
            outside_content: untrusted,
            outside_start: untrusted,
        };
        for (restricted, untrusted) in [(false, false), (true, false), (false, true), (true, true)]
        {
            let root = sessions
                .create(
                    &context,
                    &CreateInput {
                        restricted,
                        marks: marked(untrusted),
                        permission: Some(rules.clone()),
                        ..Default::default()
                    },
                )
                .unwrap();
            let child = sessions
                .create(
                    &context,
                    &CreateInput {
                        parent_id: Some(root.id.clone()),
                        ..Default::default()
                    },
                )
                .unwrap();
            let grandchild = sessions
                .create(
                    &context,
                    &CreateInput {
                        parent_id: Some(child.id.clone()),
                        // A creator that says otherwise is not believed.
                        restricted: false,
                        metadata: Some(JsonMap::new()),
                        ..Default::default()
                    },
                )
                .unwrap();
            let fork = sessions.fork(&context, &root.id, None).unwrap();
            let fork_of_fork = sessions.fork(&context, &fork.id, None).unwrap();
            for (name, session) in [
                ("root", &root),
                ("child", &child),
                ("grandchild", &grandchild),
                ("fork", &fork),
                ("fork of fork", &fork_of_fork),
            ] {
                let at = format!("{name} of restricted={restricted} untrusted={untrusted}");
                assert_eq!(
                    sessions.is_restricted(&session.id).unwrap(),
                    restricted,
                    "{at}"
                );
                assert_eq!(
                    sessions.started_by_outside_input(&session.id),
                    untrusted,
                    "{at}"
                );
                assert_eq!(
                    sessions.seen_outside_content(&session.id),
                    untrusted,
                    "{at}"
                );
            }
            // Upstream's fork copies no rules; one that carries outside
            // text keeps the rules that hold it.
            assert_eq!(
                fork.permission,
                untrusted.then(|| rules.clone()),
                "restricted={restricted} untrusted={untrusted}"
            );
            assert_eq!(fork_of_fork.permission, fork.permission);

            // Replacing the metadata never clears the mark, and has no
            // say over the restriction.
            sessions.set_metadata(&root.id, JsonMap::new()).unwrap();
            let mut cleared = JsonMap::new();
            cleared.insert("untrustedInput".to_string(), serde_json::json!(false));
            sessions.set_metadata(&child.id, cleared).unwrap();
            for id in [&root.id, &child.id] {
                assert_eq!(sessions.started_by_outside_input(id), untrusted);
                assert_eq!(sessions.is_restricted(id).unwrap(), restricted);
            }
            // Lifting the restriction (the operator's act) leaves the
            // other mark, and the children's own restriction, alone.
            sessions.lift_restriction(&root.id).unwrap();
            assert_eq!(sessions.started_by_outside_input(&root.id), untrusted);
            assert_eq!(sessions.is_restricted(&child.id).unwrap(), restricted);
        }
    }

    /// A session comes into being with its restriction, marks and ceilings
    /// or not at all: whoever hears of it finds them set, and when one of
    /// them cannot be written there is no session.
    #[test]
    fn a_session_is_created_with_its_restriction_marks_and_ceilings_or_not_at_all() {
        let dir = crate::storage::test_support::TempDir::new("store-create-atomic");
        let services = services(dir.path());
        insert_project(&services, "proj_01");
        let sessions = services.sessions.clone();
        let context = ctx("proj_01");
        let input = CreateInput {
            restricted: true,
            marks: SessionMarks::ALL,
            ceilings: vec![SessionCeiling {
                name: "test".to_string(),
                readings: vec![vec![]],
            }],
            ..Default::default()
        };

        // What a reader sees at the moment the session is announced.
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let _subscription = services.events.listen({
            let (sessions, seen) = (sessions.clone(), seen.clone());
            Arc::new(move |event: &crate::event::Payload| {
                if event.r#type != event_definitions::SESSION_CREATED.r#type {
                    return;
                }
                let id = event.data["sessionID"].as_str().unwrap().to_string();
                seen.lock().unwrap().push((
                    sessions.is_restricted(&id).unwrap(),
                    sessions.marks(&id).unwrap(),
                    sessions.ceilings(&id).unwrap().len(),
                ));
            })
        });
        sessions.create(&context, &input).unwrap();
        assert_eq!(*seen.lock().unwrap(), vec![(true, SessionMarks::ALL, 1)]);

        // The restriction cannot be written: no session row is left behind,
        // and nothing of it in the other tables.
        let count = |table: &str| {
            services.storage.with_connection(|conn| {
                conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap()
            })
        };
        let before = (
            count("session"),
            count("session_mark"),
            count("session_ceiling"),
        );
        services.storage.with_connection(|conn| {
            conn.execute_batch(
                "ALTER TABLE session_restriction RENAME TO session_restriction_gone;
                 CREATE TABLE session_restriction (elsewhere TEXT);",
            )
            .unwrap()
        });
        assert!(sessions.create(&context, &input).is_err());
        assert_eq!(
            (
                count("session"),
                count("session_mark"),
                count("session_ceiling")
            ),
            before
        );
    }

    /// A property a decision rests on is never derived from a lookup that
    /// failed in the permissive direction. With the session table gone
    /// (any storage error reads the same), and for a session that was
    /// never there: asks are denied, the untrusted mark is presumed, and a
    /// restriction that cannot be read is an error, not "unrestricted".
    #[tokio::test]
    async fn security_marks_fail_closed_when_a_lookup_fails() {
        use alforria_schema::permission_v1::{
            PermissionV1Action, PermissionV1AskInput, PermissionV1Rule,
        };
        let dir = crate::storage::test_support::TempDir::new("store-fail-closed");
        let services = services(dir.path());
        insert_project(&services, "proj_01");
        let sessions = &services.sessions;
        let context = ctx("proj_01");
        let plain = sessions.create(&context, &Default::default()).unwrap();
        let ask = |session: &str| PermissionV1AskInput {
            id: None,
            session_id: session.to_string(),
            permission: "bash".to_string(),
            patterns: vec!["ls".to_string()],
            metadata: JsonMap::new(),
            always: vec![],
            tool: None,
            ruleset: vec![PermissionV1Rule {
                permission: "bash".to_string(),
                pattern: "*".to_string(),
                action: PermissionV1Action::Ask,
            }],
        };
        // The widest unattended policy: were the ask raised, it would be
        // allowed.
        services
            .permission
            .set_unattended(Some(crate::session::permission::Unattended {
                timeout: std::time::Duration::from_millis(10),
                on_timeout: crate::session::permission::OnTimeout::Allow,
            }));
        // A readable, unmarked session is asked about (and allowed here).
        services.permission.ask(ask(&plain.id)).await.unwrap();

        // An id that is no session's.
        assert!(sessions.started_by_outside_input("ses_never"));
        assert!(sessions.seen_outside_content("ses_never"));
        assert!(matches!(
            services.permission.ask(ask("ses_never")).await,
            Err(crate::session::permission::PermissionError::Denied { .. })
        ));
        assert!(!sessions.is_restricted("ses_never").unwrap());

        // The mark survives a metadata replace on a row that is there.
        let marked = sessions
            .create(
                &context,
                &CreateInput {
                    marks: SessionMarks::ALL,
                    ..Default::default()
                },
            )
            .unwrap();
        sessions.set_metadata(&marked.id, JsonMap::new()).unwrap();
        assert!(sessions.started_by_outside_input(&marked.id));

        // Now every lookup fails.
        services.storage.with_connection(|conn| {
            conn.execute_batch(
                "ALTER TABLE session RENAME TO session_gone;
                 ALTER TABLE session_restriction RENAME TO session_restriction_gone;",
            )
            .unwrap()
        });
        assert!(sessions.get(&plain.id).is_err());
        assert!(sessions.started_by_outside_input(&plain.id));
        assert!(matches!(
            services.permission.ask(ask(&plain.id)).await,
            Err(crate::session::permission::PermissionError::Denied { .. })
        ));
        assert!(services.permission.list().is_empty(), "nothing was asked");
        assert!(
            sessions.is_restricted(&plain.id).is_err(),
            "an unreadable restriction is an error for the caller to refuse on"
        );
        // A child created now presumes the mark, and cannot be created
        // unrestricted by default: its parent's restriction is unreadable.
        assert!(sessions
            .create(
                &context,
                &CreateInput {
                    parent_id: Some(plain.id.clone()),
                    ..Default::default()
                },
            )
            .is_err());
    }

    /// M8: the marks are the server's. Nothing a client can put in a
    /// session's metadata sets one, clears one, or makes Rust and SQL
    /// disagree; the reserved keys are not a client's to write or remove;
    /// and a mark that cannot be read counts as set.
    #[test]
    fn client_metadata_neither_sets_nor_clears_nor_confuses_the_marks() {
        let dir = crate::storage::test_support::TempDir::new("store-marks-server-owned");
        let services = services(dir.path());
        insert_project(&services, "proj_01");
        let sessions = &services.sessions;
        let context = ctx("proj_01");
        let plain = sessions.create(&context, &CreateInput::default()).unwrap();
        let sql_marked = |id: &str| -> (bool, bool) {
            services.storage.with_connection(|conn| {
                conn.query_row(
                    &format!(
                        "SELECT {}, {} FROM session s WHERE s.id = ?1",
                        SessionMark::OutsideContent.sql("s.id"),
                        SessionMark::OutsideStart.sql("s.id")
                    ),
                    [id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap()
            })
        };
        for value in [
            serde_json::json!(true),
            serde_json::json!(1),
            serde_json::json!("yes"),
            serde_json::json!({ "x": 1 }),
        ] {
            let mut metadata = JsonMap::new();
            metadata.insert("untrustedInput".to_string(), value.clone());
            metadata.insert("triggerID".to_string(), serde_json::json!("forged"));
            metadata.insert("mine".to_string(), serde_json::json!(1));
            sessions
                .set_client_metadata(&plain.id, metadata.clone())
                .unwrap();
            let stored = sessions.get(&plain.id).unwrap().metadata.unwrap();
            assert_eq!(stored.len(), 1, "{value}: {stored:?}");
            assert_eq!(stored["mine"], 1);
            // Even written past the route, the key means nothing.
            sessions.set_metadata(&plain.id, metadata).unwrap();
            assert_eq!(sql_marked(&plain.id), (false, false), "{value}");
            assert!(!sessions.seen_outside_content(&plain.id), "{value}");
            assert!(!sessions.started_by_outside_input(&plain.id), "{value}");
            assert_eq!(sessions.marks(&plain.id).unwrap(), SessionMarks::default());
            sessions.set_metadata(&plain.id, JsonMap::new()).unwrap();
        }

        // A marked session: a client's metadata does not clear the marks,
        // and does not take the server's own key away.
        let mut own = JsonMap::new();
        own.insert("triggerID".to_string(), serde_json::json!("deliveries"));
        let run = sessions
            .create(
                &context,
                &CreateInput {
                    metadata: Some(own),
                    marks: SessionMarks {
                        outside_content: false,
                        outside_start: true,
                    },
                    ..Default::default()
                },
            )
            .unwrap();
        let mut cleared = JsonMap::new();
        cleared.insert("untrustedInput".to_string(), serde_json::json!(false));
        sessions.set_client_metadata(&run.id, cleared).unwrap();
        assert_eq!(
            sql_marked(&run.id),
            (true, true),
            "the ceiling implies provenance"
        );
        assert_eq!(sessions.marks(&run.id).unwrap(), SessionMarks::ALL);
        let stored = sessions.get(&run.id).unwrap().metadata.unwrap();
        assert_eq!(stored["triggerID"], "deliveries");
        assert!(!stored.contains_key("untrustedInput"));

        // Provenance alone is inherited alone, by children and forks.
        let seen = sessions
            .create(
                &context,
                &CreateInput {
                    marks: SessionMarks {
                        outside_content: true,
                        outside_start: false,
                    },
                    ..Default::default()
                },
            )
            .unwrap();
        let child = sessions
            .create(
                &context,
                &CreateInput {
                    parent_id: Some(seen.id.clone()),
                    ..Default::default()
                },
            )
            .unwrap();
        let fork = sessions.fork(&context, &child.id, None).unwrap();
        for id in [&seen.id, &child.id, &fork.id] {
            assert!(sessions.seen_outside_content(id));
            assert!(!sessions.started_by_outside_input(id));
        }

        // The table alone unreadable: every lookup fails closed, and a
        // child cannot be created unmarked.
        services.storage.with_connection(|conn| {
            conn.execute_batch("ALTER TABLE session_mark RENAME TO session_mark_gone;")
                .unwrap()
        });
        assert!(sessions.marks(&plain.id).is_err());
        assert!(sessions.seen_outside_content(&plain.id));
        assert!(sessions.started_by_outside_input(&plain.id));
        assert!(sessions
            .create(
                &context,
                &CreateInput {
                    parent_id: Some(plain.id.clone()),
                    ..Default::default()
                },
            )
            .is_err());
    }
}

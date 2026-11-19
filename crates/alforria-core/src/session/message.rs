//! Message store — port of the store side of `session/message-v2.ts`
//! (message-v2.ts:46-604): cursor paging, hydrate, `filterCompacted`,
//! `latest`.
//!
//! Writes do not exist on this side: `SessionStore::update_message` /
//! `update_part` publish the `SessionV1.Event.*` events and the projectors
//! persist the rows (spec §2.3).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rusqlite::Connection;
use serde_json::Value;

use alforria_schema::session_v1::{V1Message, V1Part};

use crate::session::error::SessionError;
use crate::storage::schema::{message_from_row, part_from_row, Message, Part};
use crate::storage::Storage;
use crate::CoreError;

/// `SYNTHETIC_ATTACHMENT_PROMPT` (message-v2.ts:46).
pub const SYNTHETIC_ATTACHMENT_PROMPT: &str = "Attached media from tool result:";

/// `MessageV2.Event` (message-v2.ts:55-61) — the definitions the message
/// store round-trips through.
pub mod event {
    use crate::event::definition::Definition;
    use crate::session::event_definitions;

    pub use crate::session::event_definitions::{
        MESSAGE_PART_DELTA, MESSAGE_PART_REMOVED, MESSAGE_PART_UPDATED, MESSAGE_REMOVED,
        MESSAGE_UPDATED,
    };

    /// The `Event` map keys in TS order (Updated, Removed, PartUpdated,
    /// PartDelta, PartRemoved).
    pub const MAP: [Definition; 5] = [
        event_definitions::MESSAGE_UPDATED,
        event_definitions::MESSAGE_REMOVED,
        event_definitions::MESSAGE_PART_UPDATED,
        event_definitions::MESSAGE_PART_DELTA,
        event_definitions::MESSAGE_PART_REMOVED,
    ];
}

/// `SessionV1.WithParts` — a message plus its parts.
#[derive(Debug, Clone, PartialEq)]
pub struct WithParts {
    pub info: V1Message,
    pub parts: Vec<V1Part>,
}

pub fn message_id(info: &V1Message) -> &str {
    match info {
        V1Message::User { id, .. } | V1Message::Assistant { id, .. } => id,
    }
}

pub fn message_role(info: &V1Message) -> &'static str {
    match info {
        V1Message::User { .. } => "user",
        V1Message::Assistant { .. } => "assistant",
    }
}

fn message_id_of(info: &V1Message) -> String {
    message_id(info).to_string()
}

/// `info.sessionID` for any message.
pub(crate) fn message_session_id(info: &V1Message) -> &str {
    match info {
        V1Message::User { session_id, .. } | V1Message::Assistant { session_id, .. } => session_id,
    }
}

/// `part.sessionID` for any part.
pub(crate) fn part_session_id(part: &V1Part) -> &str {
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

pub fn message_time_created(info: &V1Message) -> f64 {
    match info {
        V1Message::User { time, .. } => time.created,
        V1Message::Assistant { time, .. } => time.created as f64,
    }
}

/// Rebuild a [`V1Message`] from a stored row (`info(row)`, message-v2.ts:80-85):
/// `data` carries everything but `id`/`sessionID`.
pub(crate) fn info_from_row(row: &Message) -> Result<V1Message, CoreError> {
    let mut data = row.data.clone();
    let object = data
        .as_object_mut()
        .ok_or_else(|| CoreError::Storage("message data is not an object".to_string()))?;
    object.insert("id".to_string(), Value::String(row.id.clone()));
    object.insert(
        "sessionID".to_string(),
        Value::String(row.session_id.clone()),
    );
    serde_json::from_value(data)
        .map_err(|err| CoreError::Storage(format!("invalid message row: {err}")))
}

/// Rebuild a [`V1Part`] from a stored row (`part(row)`, message-v2.ts:87-93).
pub(crate) fn part_from_row_data(row: &Part) -> Result<V1Part, CoreError> {
    let mut data = row.data.clone();
    let object = data
        .as_object_mut()
        .ok_or_else(|| CoreError::Storage("part data is not an object".to_string()))?;
    object.insert("id".to_string(), Value::String(row.id.clone()));
    object.insert(
        "sessionID".to_string(),
        Value::String(row.session_id.clone()),
    );
    object.insert(
        "messageID".to_string(),
        Value::String(row.message_id.clone()),
    );
    serde_json::from_value(data)
        .map_err(|err| CoreError::Storage(format!("invalid part row: {err}")))
}

// ---------------------------------------------------------------------------
// Cursor (message-v2.ts:63-78)
// ---------------------------------------------------------------------------

/// The paging cursor: `{id, time}`.
#[derive(Debug, Clone, PartialEq)]
pub struct Cursor {
    pub id: String,
    pub time: f64,
}

pub mod cursor {
    use super::*;

    /// `cursor.encode` — base64url of `{id, time}`.
    pub fn encode(input: &Cursor) -> Result<String, CoreError> {
        let json = serde_json::json!({ "id": input.id, "time": input.time });
        let text =
            serde_json::to_string(&json).map_err(|err| CoreError::Storage(err.to_string()))?;
        Ok(URL_SAFE_NO_PAD.encode(text))
    }

    /// `cursor.decode` (validation: `time` is a finite number `>= 0`).
    pub fn decode(input: &str) -> Result<Cursor, CoreError> {
        let text = URL_SAFE_NO_PAD
            .decode(input)
            .map_err(|err| CoreError::Storage(format!("invalid cursor: {err}")))?;
        let json: Value = serde_json::from_slice(&text)
            .map_err(|err| CoreError::Storage(format!("invalid cursor: {err}")))?;
        let id = json
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| CoreError::Storage("cursor missing id".to_string()))?;
        let time = json
            .get("time")
            .and_then(Value::as_f64)
            .ok_or_else(|| CoreError::Storage("cursor missing time".to_string()))?;
        if !time.is_finite() || time < 0.0 {
            return Err(CoreError::Storage("cursor time must be >= 0".to_string()));
        }
        Ok(Cursor {
            id: id.to_string(),
            time,
        })
    }
}

/// A `page()` result (message-v2.ts:443-467).
#[derive(Debug, Clone, Default)]
pub struct MessagePage {
    pub items: Vec<WithParts>,
    pub more: bool,
    pub cursor: Option<String>,
}

/// A [`MessageStore::window`] result.
#[derive(Debug, Clone, Default)]
pub struct Window {
    /// What [`filter_compacted`] returns, as far back as was read.
    pub messages: Vec<WithParts>,
    /// Whether that reaches the last compaction, or the start of the
    /// session. When not, older messages were left in the database.
    pub complete: bool,
}

/// A [`MessageStore::dangling`] result.
#[derive(Debug, Clone, Default)]
pub struct Dangling {
    /// Ids of assistant messages with no completion time, oldest first.
    pub assistants: Vec<String>,
    /// Tool parts still pending or running.
    pub tools: Vec<V1Part>,
}

/// A [`MessageStore::last_compaction`] result.
#[derive(Debug, Clone)]
pub struct Compacted {
    /// The compaction marker: the user message the summary answers.
    pub user_id: String,
    pub summary: WithParts,
    /// Where the history the summary does not cover starts, for
    /// [`MessageStore::page_after`].
    pub from: Cursor,
    /// Whether the message at `from` is itself part of that history.
    pub inclusive: bool,
}

/// The message store (`MessageV2` store side).
#[derive(Debug, Clone)]
pub struct MessageStore {
    storage: Arc<Storage>,
}

impl MessageStore {
    pub fn new(storage: Arc<Storage>) -> MessageStore {
        MessageStore { storage }
    }

    /// `MessageV2.page` (message-v2.ts:425-467): order
    /// `time_created desc, id desc`, fetch `limit + 1` to compute `more`,
    /// items reversed to chronological, cursor = last row.
    pub fn page(
        &self,
        session_id: &str,
        limit: usize,
        before: Option<&str>,
    ) -> Result<MessagePage, SessionError> {
        self.page_of(ALL_COLUMNS, session_id, limit, before)
    }

    /// [`Self::page`] with every `summary.diffs` empty.
    pub fn page_without_diffs(
        &self,
        session_id: &str,
        limit: usize,
        before: Option<&str>,
    ) -> Result<MessagePage, SessionError> {
        self.page_of(COLUMNS_WITHOUT_DIFFS, session_id, limit, before)
    }

    fn page_of(
        &self,
        columns: &str,
        session_id: &str,
        limit: usize,
        before: Option<&str>,
    ) -> Result<MessagePage, SessionError> {
        let before = match before {
            Some(encoded) => Some(cursor::decode(encoded)?),
            None => None,
        };
        let mut rows = self.query_page(columns, session_id, limit + 1, before.as_ref())?;
        if rows.is_empty() {
            let exists = self.storage.with_connection(|conn| {
                conn.query_row("SELECT id FROM session WHERE id = ?1", [session_id], |_| {
                    Ok(())
                })
                .is_ok()
            });
            if !exists {
                return Err(SessionError::not_found(format!(
                    "Session not found: {session_id}"
                )));
            }
            return Ok(MessagePage::default());
        }

        let more = rows.len() > limit;
        if more {
            rows.truncate(limit);
        }
        let items = self.hydrate(&rows)?;
        let items = items.into_iter().rev().collect();
        let cursor = if more {
            rows.last()
                .map(|row| {
                    cursor::encode(&Cursor {
                        id: row.id.clone(),
                        time: row.time_created as f64,
                    })
                })
                .transpose()?
        } else {
            None
        };
        Ok(MessagePage {
            items,
            more,
            cursor,
        })
    }

    /// `older(row)` (message-v2.ts:95-96): `time_created < row.time ||
    /// (== && id < row.id)`.
    fn query_page(
        &self,
        columns: &str,
        session_id: &str,
        limit: usize,
        before: Option<&Cursor>,
    ) -> Result<Vec<Message>, CoreError> {
        let sql = if before.is_some() {
            format!("SELECT {columns} FROM message WHERE session_id = ?1 AND (time_created < ?2 OR (time_created = ?2 AND id < ?3)) ORDER BY time_created DESC, id DESC LIMIT ?4")
        } else {
            format!("SELECT {columns} FROM message WHERE session_id = ?1 ORDER BY time_created DESC, id DESC LIMIT ?4")
        };
        let empty = String::new();
        let (time, id) = match before {
            Some(cursor) => (cursor.time as i64, cursor.id.clone()),
            None => (0, empty),
        };
        self.storage.with_connection(|conn| {
            query_messages(
                conn,
                &sql,
                rusqlite::params![session_id, time, id, limit as i64],
            )
        })
    }

    /// `hydrate` (message-v2.ts:98-122): batch part lookup per message,
    /// ordered by (message_id, id).
    fn hydrate(&self, rows: &[Message]) -> Result<Vec<WithParts>, CoreError> {
        self.hydrate_parts(rows, false)
    }

    fn hydrate_parts(
        &self,
        rows: &[Message],
        snapshots_only: bool,
    ) -> Result<Vec<WithParts>, CoreError> {
        self.storage.count_message_rows(rows.len());
        let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
        let mut part_lists: HashMap<String, Vec<V1Part>> = HashMap::new();
        if !ids.is_empty() {
            let placeholders = vec!["?"; ids.len()].join(", ");
            let filter = if snapshots_only {
                " AND json_extract(data, '$.type') IN ('step-start', 'step-finish')"
            } else {
                ""
            };
            let sql = format!(
                "SELECT * FROM part WHERE message_id IN ({placeholders}){filter} ORDER BY message_id, id"
            );
            let part_rows = self.storage.with_connection(|conn| {
                let mut stmt = conn.prepare(&sql)?;
                let mut rows = stmt.query(rusqlite::params_from_iter(ids.iter()))?;
                let mut out = Vec::new();
                while let Some(row) = rows.next()? {
                    out.push(part_from_row(row)?);
                }
                Ok::<_, CoreError>(out)
            })?;
            for row in part_rows {
                let part = part_from_row_data(&row)?;
                part_lists
                    .entry(row.message_id.clone())
                    .or_default()
                    .push(part);
            }
        }
        rows.iter()
            .map(|row| {
                Ok(WithParts {
                    info: info_from_row(row)?,
                    parts: part_lists.remove(&row.id).unwrap_or_default(),
                })
            })
            .collect()
    }

    /// Load the requested turn regardless of compaction. The user and its
    /// assistant replies are found by indexed lookups, in chronological order.
    /// Only snapshot step parts are hydrated; summary diffs are omitted.
    pub(crate) fn turn_snapshots(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> Result<Vec<WithParts>, SessionError> {
        let rows = self.storage.with_connection(|conn| {
            query_messages(conn, &turn_query(), [session_id, message_id])
        })?;
        if rows.is_empty() {
            let exists: bool = self.storage.with_connection(|conn| {
                conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM session WHERE id = ?1)",
                    [session_id],
                    |row| row.get(0),
                )
            })?;
            if !exists {
                return Err(SessionError::not_found(format!(
                    "Session not found: {session_id}"
                )));
            }
        }
        Ok(self.hydrate_parts(&rows, true)?)
    }

    /// `MessageV2.stream` (message-v2.ts:469-490): page-until-exhausted in
    /// batches of 50, newest-first. A missing session resolves to `[]`.
    pub fn stream(&self, session_id: &str) -> Result<Vec<WithParts>, SessionError> {
        self.stream_of(ALL_COLUMNS, session_id)
    }

    /// [`Self::stream`] with every `summary.diffs` empty, for code that reads
    /// a session's history on each step and has no use for its file diffs.
    /// Never write a message from this back to the store: that wipes them.
    pub fn stream_without_diffs(&self, session_id: &str) -> Result<Vec<WithParts>, SessionError> {
        self.stream_of(COLUMNS_WITHOUT_DIFFS, session_id)
    }

    /// Load only the context retained by the latest completed compaction.
    /// Page hydration stops at that boundary, including a retained tail that
    /// crosses pages; archived history costs no additional queries or memory.
    pub fn active_context(&self, session_id: &str) -> Result<Vec<WithParts>, SessionError> {
        let mut pending = Vec::new().into_iter();
        let mut before = None;
        let mut done = false;
        let mut failure = None;
        let context = filter_compacted(std::iter::from_fn(|| loop {
            if let Some(message) = pending.next() {
                return Some(message);
            }
            if done {
                return None;
            }
            match self.page_without_diffs(session_id, 50, before.as_deref()) {
                Ok(page) => {
                    done = !page.more || page.cursor.is_none();
                    before = page.cursor;
                    pending = page.items.into_iter().rev().collect::<Vec<_>>().into_iter();
                }
                Err(SessionError::NotFound(_)) => {
                    done = true;
                }
                Err(error) => {
                    failure = Some(error);
                    done = true;
                }
            }
        }));
        match failure {
            Some(error) => Err(error),
            None => Ok(context),
        }
    }

    fn stream_of(&self, columns: &str, session_id: &str) -> Result<Vec<WithParts>, SessionError> {
        let size = 50usize;
        let mut result = Vec::new();
        let mut before: Option<String> = None;
        loop {
            let page = match self.page_of(columns, session_id, size, before.as_deref()) {
                Ok(page) => page,
                // TS: catchIf(NotFoundError, () => empty page)
                Err(SessionError::NotFound(_)) => return Ok(Vec::new()),
                Err(err) => return Err(err),
            };
            if page.items.is_empty() {
                break;
            }
            result.extend(page.items.into_iter().rev());
            if !page.more || page.cursor.is_none() {
                break;
            }
            before = page.cursor;
        }
        Ok(result)
    }

    /// The history a turn works from: [`filter_compacted`] over
    /// [`Self::stream_without_diffs`], read page by page and no further
    /// than the last compaction. A history with more than `max_bytes` of
    /// parts since then is cut off there (`complete: false`): it cannot be
    /// sent, only compacted, and holding all of it would cost memory in
    /// proportion to the session.
    pub fn window(&self, session_id: &str, max_bytes: usize) -> Result<Window, SessionError> {
        // Most windows end within a few messages of the newest: start with
        // a small page and grow it, so a short window is not paid for at
        // the full page size on every step.
        let mut size: usize = 16;
        let mut walk = CompactedWalk::default();
        let mut before: Option<String> = None;
        let mut bytes = 0usize;
        let mut complete = true;
        'pages: loop {
            let page =
                match self.page_of(COLUMNS_WITHOUT_DIFFS, session_id, size, before.as_deref()) {
                    Ok(page) => page,
                    Err(SessionError::NotFound(_)) => break,
                    Err(err) => return Err(err),
                };
            if page.items.is_empty() {
                break;
            }
            let ids: Vec<&str> = page.items.iter().map(|msg| message_id(&msg.info)).collect();
            bytes += self.part_bytes(&ids)?;
            for msg in page.items.into_iter().rev() {
                if walk.push(msg) {
                    break 'pages;
                }
            }
            if !page.more || page.cursor.is_none() {
                break;
            }
            if bytes > max_bytes {
                complete = false;
                break;
            }
            before = page.cursor;
            size = (size * 2).min(64);
        }
        Ok(Window {
            messages: walk.finish(),
            complete,
        })
    }

    /// The newest message, if the session has any.
    pub fn newest(&self, session_id: &str) -> Result<Option<WithParts>, SessionError> {
        match self.page_of(COLUMNS_WITHOUT_DIFFS, session_id, 1, None) {
            Ok(page) => Ok(page.items.into_iter().next()),
            Err(SessionError::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// `message_id` and every message after it, oldest first, without
    /// reading anything older. Empty when the session has no such message.
    /// `summary.diffs` come back empty.
    pub fn since(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> Result<Vec<WithParts>, SessionError> {
        let row = self.storage.with_connection(|conn| {
            conn.query_row(
                "SELECT time_created FROM message WHERE id = ?1 AND session_id = ?2",
                [message_id, session_id],
                |row| row.get::<_, i64>(0),
            )
            .ok()
        });
        let Some(time) = row else {
            return Ok(Vec::new());
        };
        const SIZE: usize = 50;
        let mut result = Vec::new();
        let mut after = Cursor {
            id: message_id.to_string(),
            time: time as f64,
        };
        let mut inclusive = true;
        loop {
            let page = self.page_after(session_id, Some(&after), inclusive, SIZE)?;
            let full = page.len() == SIZE;
            if let Some(last) = page.last() {
                after = Cursor {
                    id: message_id_of(&last.info),
                    time: message_time_created(&last.info),
                };
                inclusive = false;
            }
            result.extend(page);
            if !full {
                break;
            }
        }
        Ok(result)
    }

    /// What a process that died mid-turn left unfinished in a session:
    /// assistant messages never completed, and tool calls still pending or
    /// running.
    pub fn dangling(&self, session_id: &str) -> Result<Dangling, SessionError> {
        let (assistants, tools) = self.storage.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id FROM message
                 WHERE session_id = ?1
                   AND json_extract(data, '$.role') = 'assistant'
                   AND json_extract(data, '$.time.completed') IS NULL
                 ORDER BY time_created, id",
            )?;
            let assistants = stmt
                .query_map([session_id], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            let mut stmt = conn.prepare(
                "SELECT * FROM part
                 WHERE session_id = ?1
                   AND json_extract(data, '$.type') = 'tool'
                   AND json_extract(data, '$.state.status') IN ('pending', 'running')
                 ORDER BY message_id, id",
            )?;
            let mut rows = stmt.query([session_id])?;
            let mut tools = Vec::new();
            while let Some(row) = rows.next()? {
                tools.push(part_from_row(row)?);
            }
            Ok::<_, CoreError>((assistants, tools))
        })?;
        Ok(Dangling {
            assistants,
            tools: tools
                .iter()
                .map(part_from_row_data)
                .collect::<Result<Vec<_>, CoreError>>()?,
        })
    }

    /// The stored size of these messages' parts.
    fn part_bytes(&self, message_ids: &[&str]) -> Result<usize, CoreError> {
        if message_ids.is_empty() {
            return Ok(0);
        }
        let placeholders = vec!["?"; message_ids.len()].join(", ");
        let sql = format!(
            "SELECT COALESCE(SUM(LENGTH(data)), 0) FROM part WHERE message_id IN ({placeholders})"
        );
        self.storage.with_connection(|conn| {
            let bytes: i64 = conn.query_row(
                &sql,
                rusqlite::params_from_iter(message_ids.iter()),
                |row| row.get(0),
            )?;
            Ok(bytes.max(0) as usize)
        })
    }

    /// Up to `limit` messages after `after` (or from it, when `inclusive`),
    /// oldest first: the forward counterpart of [`Self::page`], for reading
    /// a long history in order without holding it. `summary.diffs` come back
    /// empty.
    pub fn page_after(
        &self,
        session_id: &str,
        after: Option<&Cursor>,
        inclusive: bool,
        limit: usize,
    ) -> Result<Vec<WithParts>, SessionError> {
        self.page_after_of(COLUMNS_WITHOUT_DIFFS, session_id, after, inclusive, limit)
    }

    /// [`Self::page_after`] with `summary.diffs` as stored.
    pub fn page_after_with_diffs(
        &self,
        session_id: &str,
        after: Option<&Cursor>,
        limit: usize,
    ) -> Result<Vec<WithParts>, SessionError> {
        self.page_after_of(ALL_COLUMNS, session_id, after, false, limit)
    }

    fn page_after_of(
        &self,
        columns: &str,
        session_id: &str,
        after: Option<&Cursor>,
        inclusive: bool,
        limit: usize,
    ) -> Result<Vec<WithParts>, SessionError> {
        let sql = if after.is_some() {
            let id = if inclusive { ">=" } else { ">" };
            format!("SELECT {columns} FROM message WHERE session_id = ?1 AND (time_created > ?2 OR (time_created = ?2 AND id {id} ?3)) ORDER BY time_created ASC, id ASC LIMIT ?4")
        } else {
            format!("SELECT {columns} FROM message WHERE session_id = ?1 ORDER BY time_created ASC, id ASC LIMIT ?4")
        };
        let (time, id) = match after {
            Some(cursor) => (cursor.time as i64, cursor.id.clone()),
            None => (0, String::new()),
        };
        let rows = self.storage.with_connection(|conn| {
            query_messages(
                conn,
                &sql,
                rusqlite::params![session_id, time, id, limit as i64],
            )
        })?;
        Ok(self.hydrate(&rows)?)
    }

    /// Where the newest completed compaction left the history: its summary
    /// and the first message still standing in full (the tail it kept, or
    /// the message after the summary). `None` for a session never
    /// compacted.
    pub fn last_compaction(&self, session_id: &str) -> Result<Option<Compacted>, SessionError> {
        // Summaries are few; their rows are found without reading parts.
        let rows = self.storage.with_connection(|conn| {
            query_messages(
                conn,
                "SELECT id, session_id, time_created, time_updated, data FROM message
                 WHERE session_id = ?1
                   AND json_extract(data, '$.role') = 'assistant'
                   AND json_extract(data, '$.summary') = 1
                   AND json_extract(data, '$.finish') IS NOT NULL
                   AND json_extract(data, '$.error') IS NULL
                 ORDER BY time_created DESC, id DESC",
                rusqlite::params![session_id],
            )
        })?;
        for row in rows {
            let V1Message::Assistant { parent_id, .. } = info_from_row(&row)? else {
                continue;
            };
            let Ok(parent) = self.get(session_id, &parent_id) else {
                continue;
            };
            let Some(part) = parent.parts.iter().find(|part| is_compaction(part)) else {
                continue;
            };
            let summary = self.get(session_id, &row.id)?;
            let tail = match compaction_tail(part).and_then(|tail| tail.clone()) {
                Some(tail) => self.get(session_id, &tail).ok(),
                None => None,
            };
            let from = match &tail {
                Some(tail) => Cursor {
                    id: message_id(&tail.info).to_string(),
                    time: message_time_created(&tail.info),
                },
                None => Cursor {
                    id: row.id.clone(),
                    time: row.time_created as f64,
                },
            };
            return Ok(Some(Compacted {
                user_id: parent_id,
                summary,
                from,
                inclusive: tail.is_some(),
            }));
        }
        Ok(None)
    }

    /// `MessageV2.parts` (message-v2.ts:492-504) — parts ordered by id.
    pub fn parts(&self, message_id: &str) -> Result<Vec<V1Part>, SessionError> {
        let rows = self.storage.with_connection(|conn| {
            let mut stmt = conn.prepare("SELECT * FROM part WHERE message_id = ?1 ORDER BY id")?;
            let mut rows = stmt.query([message_id])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                out.push(part_from_row(row)?);
            }
            Ok::<_, CoreError>(out)
        })?;
        rows.iter()
            .map(part_from_row_data)
            .collect::<Result<Vec<_>, CoreError>>()
            .map_err(SessionError::from)
    }

    /// `MessageV2.get` (message-v2.ts:506-519).
    pub fn get(&self, session_id: &str, message_id: &str) -> Result<WithParts, SessionError> {
        Ok(WithParts {
            info: self.get_info(session_id, message_id)?,
            parts: self.parts(message_id)?,
        })
    }

    /// Read metadata without hydrating the message's parts.
    pub(crate) fn get_info(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> Result<V1Message, SessionError> {
        let row = self.storage.with_connection(|conn| {
            let mut stmt =
                conn.prepare("SELECT * FROM message WHERE id = ?1 AND session_id = ?2")?;
            let mut rows = stmt.query([message_id, session_id])?;
            match rows.next()? {
                Some(row) => Ok::<_, CoreError>(Some(message_from_row(row)?)),
                None => Ok::<_, CoreError>(None),
            }
        })?;
        let Some(row) = row else {
            return Err(SessionError::not_found(format!(
                "Message not found: {message_id}"
            )));
        };
        Ok(info_from_row(&row)?)
    }
}

/// Every column of a message row.
const ALL_COLUMNS: &str = "*";

/// A message row with `summary.diffs` emptied by SQLite, so the patches are
/// never copied out of the database.
const COLUMNS_WITHOUT_DIFFS: &str = "id, session_id, time_created, time_updated, \
     CASE WHEN json_type(data, '$.summary.diffs') IS NULL THEN data \
     ELSE json_set(data, '$.summary.diffs', json('[]')) END AS data";

fn turn_query() -> String {
    use crate::storage::connection::{TURN_ASSISTANT, TURN_PARENT};

    // Separate lookups avoid an OR predicate that can scan the session's
    // entire history. The second arm uses the local parent-ID index.
    format!(
        "SELECT {COLUMNS_WITHOUT_DIFFS} FROM message WHERE session_id = ?1 AND id = ?2
         UNION ALL
         SELECT {COLUMNS_WITHOUT_DIFFS} FROM message
         WHERE session_id = ?1 AND {TURN_ASSISTANT}
           AND {TURN_PARENT} = ?2 AND id != ?2
         ORDER BY time_created, id"
    )
}

fn query_messages(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<Message>, CoreError> {
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query(params)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(message_from_row(row)?);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// filterCompacted (message-v2.ts:521-572)
// ---------------------------------------------------------------------------

fn compaction_tail(part: &V1Part) -> Option<&Option<String>> {
    match part {
        V1Part::Compaction { tail_start_id, .. } => Some(tail_start_id),
        _ => None,
    }
}

fn is_compaction(part: &V1Part) -> bool {
    matches!(part, V1Part::Compaction { .. })
}

/// The retain-walk of `filterCompacted` (message-v2.ts:521-553), fed one
/// message at a time, newest first, so a caller reading pages from SQLite
/// can stop reading as soon as the walk does.
#[derive(Default)]
struct CompactedWalk {
    result: Vec<WithParts>,
    completed: HashSet<String>,
    retain: Option<String>,
}

impl CompactedWalk {
    /// Take the next older message. Returns `true` once nothing older is
    /// needed.
    fn push(&mut self, msg: WithParts) -> bool {
        let id = message_id(&msg.info).to_string();
        self.result.push(msg);
        let msg = self.result.last().expect("just pushed");
        if let Some(retained) = self.retain.as_deref() {
            return id == retained;
        }
        if message_role(&msg.info) == "user" && self.completed.contains(&id) {
            let Some(part) = msg.parts.iter().find(|part| is_compaction(part)) else {
                return false;
            };
            let Some(tail) = compaction_tail(part).and_then(|tail| tail.clone()) else {
                return true;
            };
            let done = id == tail;
            self.retain = Some(tail);
            return done;
        }
        if let V1Message::Assistant {
            summary: Some(true),
            finish: Some(_),
            error: None,
            parent_id,
            ..
        } = &msg.info
        {
            self.completed.insert(parent_id.clone());
        }
        false
    }

    /// Chronological order, reordered around the last compaction.
    fn finish(self) -> Vec<WithParts> {
        reorder_compacted(self.result)
    }
}

/// `filterCompacted` (message-v2.ts:521-572 — binding): retain-walk over the
/// newest-first stream, reverse, then reorder around the last compaction
/// into `[compaction-user, summary, tail, remainder]`.
pub fn filter_compacted(msgs: impl IntoIterator<Item = WithParts>) -> Vec<WithParts> {
    let mut walk = CompactedWalk::default();
    for msg in msgs {
        if walk.push(msg) {
            break;
        }
    }
    walk.finish()
}

/// The second half of `filterCompacted` (message-v2.ts:554-572).
fn reorder_compacted(mut result: Vec<WithParts>) -> Vec<WithParts> {
    result.reverse();

    let compaction_index = result.iter().rposition(|msg| {
        message_role(&msg.info) == "user"
            && msg
                .parts
                .iter()
                .any(|part| is_compaction(part) && part_tail(part).is_some())
    });
    let Some(compaction_index) = compaction_index else {
        return result;
    };
    let compaction = &result[compaction_index];
    let compaction_id = message_id(&compaction.info);
    let tail_id = compaction
        .parts
        .iter()
        .find_map(|part| part_tail(part).cloned().flatten());
    let summary_index = result
        .iter()
        .enumerate()
        .find(|(index, msg)| {
            if index <= &compaction_index {
                return false;
            }
            if let V1Message::Assistant {
                summary: Some(true),
                parent_id,
                ..
            } = &msg.info
            {
                parent_id == compaction_id
            } else {
                false
            }
        })
        .map(|(index, _)| index);
    let tail_index = tail_id
        .as_deref()
        .and_then(|tail| result.iter().position(|msg| message_id(&msg.info) == tail));
    if let (Some(tail_index), Some(summary_index)) = (tail_index, summary_index) {
        if tail_index < compaction_index && summary_index > compaction_index {
            let mut out = Vec::with_capacity(result.len());
            out.extend(result[compaction_index..=summary_index].iter().cloned());
            out.extend(result[tail_index..compaction_index].iter().cloned());
            out.extend(result[summary_index + 1..].iter().cloned());
            return out;
        }
    }
    result
}

fn part_tail(part: &V1Part) -> Option<&Option<String>> {
    match part {
        V1Part::Compaction { tail_start_id, .. } => Some(tail_start_id),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// latest (message-v2.ts:578-604)
// ---------------------------------------------------------------------------

/// `MessageV2.latest` (message-v2.ts:582-604).
#[derive(Debug, Clone, Default)]
pub struct Latest {
    pub user: Option<V1Message>,
    pub assistant: Option<V1Message>,
    pub finished: Option<V1Message>,
    pub tasks: Vec<V1Part>,
}

/// `isAfter` (message-v2.ts:600-604): `(time.created, id)` ordering — IDs
/// are only a tie-breaker.
fn is_after(info: &V1Message, other: Option<&V1Message>) -> bool {
    let Some(other) = other else {
        return true;
    };
    let (created, id) = (message_time_created(info), message_id(info));
    let other_created = message_time_created(other);
    if created != other_created {
        return created > other_created;
    }
    id > message_id(other)
}

pub fn latest(msgs: &[WithParts]) -> Latest {
    let mut out = Latest::default();
    for msg in msgs {
        let info = &msg.info;
        match info {
            V1Message::User { .. } => {
                if is_after(info, out.user.as_ref()) {
                    out.user = Some(info.clone());
                }
            }
            V1Message::Assistant { .. } => {
                if is_after(info, out.assistant.as_ref()) {
                    out.assistant = Some(info.clone());
                }
                if matches!(
                    info,
                    V1Message::Assistant {
                        finish: Some(_),
                        ..
                    }
                ) && is_after(info, out.finished.as_ref())
                {
                    out.finished = Some(info.clone());
                }
            }
        }
    }
    for msg in msgs {
        if out.finished.is_some() && !is_after(&msg.info, out.finished.as_ref()) {
            continue;
        }
        out.tasks.extend(
            msg.parts
                .iter()
                .filter(|part| matches!(part, V1Part::Compaction { .. } | V1Part::Subtask { .. }))
                .cloned(),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_support::TempDir;
    use crate::storage::Storage;
    use alforria_schema::session_v1::{
        AssistantTime, UserTime, V1Path, V1StepTokens, V1TokenCache, V1UserModel,
    };

    fn storage() -> (Arc<Storage>, TempDir) {
        let dir = TempDir::new("message-store");
        (
            Arc::new(Storage::open(dir.path().join("db.sqlite")).unwrap()),
            dir,
        )
    }

    fn user_message(id: &str, session: &str, created: f64) -> V1Message {
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

    fn assistant_message(
        id: &str,
        session: &str,
        created: u64,
        parent: &str,
        summary: Option<bool>,
        finish: Option<String>,
    ) -> V1Message {
        V1Message::Assistant {
            id: id.to_string(),
            session_id: session.to_string(),
            time: AssistantTime {
                created,
                completed: None,
            },
            error: None,
            parent_id: parent.to_string(),
            model_id: "claude".to_string(),
            provider_id: "anthropic".to_string(),
            mode: "primary".to_string(),
            agent: "build".to_string(),
            path: V1Path {
                cwd: "/repo".to_string(),
                root: "/repo".to_string(),
            },
            summary,
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
            finish,
        }
    }

    fn compaction_part(id: &str, session: &str, msg: &str, tail: Option<&str>) -> V1Part {
        V1Part::Compaction {
            id: id.to_string(),
            session_id: session.to_string(),
            message_id: msg.to_string(),
            auto: true,
            overflow: None,
            tail_start_id: tail.map(|t| t.to_string()),
        }
    }

    fn with_parts(info: V1Message, parts: Vec<V1Part>) -> WithParts {
        WithParts { info, parts }
    }

    #[test]
    #[ignore = "manual benchmark: cargo test -p alforria-core benchmark_active_context -- --ignored --nocapture"]
    fn benchmark_active_context_with_archived_history() {
        use crate::session::test_support::{create_session, harness};
        for archived in [100, 1000, 10_000] {
            let h = harness("context-benchmark", vec![]);
            let session = create_session(&h.services.sessions, &h.worktree);
            for index in 0..archived {
                h.services
                    .sessions
                    .update_message(&user_message(
                        &format!("msg_{index:05}"),
                        &session.id,
                        index as f64,
                    ))
                    .unwrap();
            }
            h.services
                .sessions
                .update_message(&user_message("msg_compact", &session.id, archived as f64))
                .unwrap();
            h.services
                .sessions
                .update_part(&compaction_part(
                    "prt_compact",
                    &session.id,
                    "msg_compact",
                    None,
                ))
                .unwrap();
            h.services
                .sessions
                .update_message(&assistant_message(
                    "msg_summary",
                    &session.id,
                    archived + 1,
                    "msg_compact",
                    Some(true),
                    Some("stop".into()),
                ))
                .unwrap();
            let start = std::time::Instant::now();
            for _ in 0..100 {
                let context = h.services.messages.active_context(&session.id).unwrap();
                assert_eq!(context.len(), 2);
                std::hint::black_box(context);
            }
            let active = start.elapsed().as_micros() / 100;
            let start = std::time::Instant::now();
            for _ in 0..10 {
                std::hint::black_box(filter_compacted(
                    h.services
                        .messages
                        .stream_without_diffs(&session.id)
                        .unwrap(),
                ));
            }
            println!(
                "archived={archived}, active_messages=2, lazy_us={active}, eager_us={}",
                start.elapsed().as_micros() / 10
            );
        }
    }

    #[test]
    fn turn_snapshot_query_uses_indexed_parent_lookups_and_only_snapshot_parts() {
        use crate::session::test_support::{create_session, harness, text_part};
        let h = harness("turn-snapshot-query", vec![]);
        let session = create_session(&h.services.sessions, &h.worktree);
        for (id, time) in [("msg_u", 1.0), ("msg_other", 3.0)] {
            h.services
                .sessions
                .update_message(&user_message(id, &session.id, time))
                .unwrap();
        }
        for (id, parent, time) in [
            ("msg_reply_1", "msg_u", 2),
            ("msg_other_reply", "msg_other", 4),
            ("msg_reply_2", "msg_u", 5),
        ] {
            h.services
                .sessions
                .update_message(&assistant_message(
                    id,
                    &session.id,
                    time,
                    parent,
                    None,
                    None,
                ))
                .unwrap();
            for part in [
                V1Part::StepStart {
                    id: format!("prt_{id}_start"),
                    session_id: session.id.clone(),
                    message_id: id.into(),
                    snapshot: Some(format!("snapshot_{id}")),
                },
                text_part(
                    &session.id,
                    id,
                    &format!("prt_{id}_text"),
                    "not needed for diff",
                ),
            ] {
                h.services.sessions.update_part(&part).unwrap();
            }
        }
        let messages = h
            .services
            .messages
            .turn_snapshots(&session.id, "msg_u")
            .unwrap();
        assert_eq!(
            messages
                .iter()
                .map(|msg| message_id(&msg.info))
                .collect::<Vec<_>>(),
            ["msg_u", "msg_reply_1", "msg_reply_2"]
        );
        assert!(messages[0].parts.is_empty());
        for reply in &messages[1..] {
            assert!(matches!(reply.parts.as_slice(), [V1Part::StepStart { .. }]));
        }
        let plan = h.services.storage.with_connection(|conn| {
            conn.prepare(&format!("EXPLAIN QUERY PLAN {}", turn_query()))
                .unwrap()
                .query_map([&session.id, "msg_u"], |row| row.get::<_, String>(3))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        });
        assert!(
            plan.iter()
                .any(|line| line.contains("sqlite_autoindex_message_1")),
            "{plan:?}"
        );
        assert!(
            plan.iter()
                .any(|line| line.contains(crate::storage::connection::TURN_INDEX)
                    && line.contains("<expr>=?")),
            "{plan:?}"
        );
        assert!(
            !plan.iter().any(|line| line.starts_with("SCAN message")),
            "{plan:?}"
        );
        assert!(matches!(
            h.services.messages.turn_snapshots("other_session", "msg_u"),
            Err(SessionError::NotFound(_))
        ));
    }

    #[test]
    fn active_context_stops_before_archived_history_and_retains_a_cross_page_tail() {
        use crate::session::test_support::{create_session, harness};
        for archived in [100, 1000] {
            for tail in [None, Some(archived - 80)] {
                let h = harness("context-boundary", vec![]);
                let session = create_session(&h.services.sessions, &h.worktree);
                for index in 0..archived {
                    h.services
                        .sessions
                        .update_message(&user_message(
                            &format!("msg_{index:05}"),
                            &session.id,
                            index as f64,
                        ))
                        .unwrap();
                }
                let compact = "msg_compact";
                let summary = "msg_summary";
                h.services
                    .sessions
                    .update_message(&user_message(compact, &session.id, archived as f64))
                    .unwrap();
                let tail_id = tail.map(|index| format!("msg_{index:05}"));
                h.services
                    .sessions
                    .update_part(&compaction_part(
                        "prt_compact",
                        &session.id,
                        compact,
                        tail_id.as_deref(),
                    ))
                    .unwrap();
                h.services
                    .sessions
                    .update_message(&assistant_message(
                        summary,
                        &session.id,
                        archived + 1,
                        compact,
                        Some(true),
                        Some("stop".into()),
                    ))
                    .unwrap();
                let expected = filter_compacted(
                    h.services
                        .messages
                        .stream_without_diffs(&session.id)
                        .unwrap(),
                );
                // This row is older than all pages needed for the active
                // context. Eager hydration fails if it ever reads the row.
                h.services
                    .storage
                    .with_connection(|conn| {
                        conn.execute("UPDATE message SET data = '{}' WHERE id = 'msg_00000'", [])
                    })
                    .unwrap();
                assert_eq!(
                    h.services.messages.active_context(&session.id).unwrap(),
                    expected
                );
                assert!(h
                    .services
                    .messages
                    .stream_without_diffs(&session.id)
                    .is_err());
            }
        }
    }

    #[test]
    fn cursor_round_trip() {
        let encoded = cursor::encode(&Cursor {
            id: "msg_01J".to_string(),
            time: 1234.0,
        })
        .unwrap();
        let decoded = cursor::decode(&encoded).unwrap();
        assert_eq!(decoded.id, "msg_01J");
        assert_eq!(decoded.time, 1234.0);
        // A TS-produced cursor (`JSON.stringify({id, time})`) decodes too.
        let ts_encoded = URL_SAFE_NO_PAD.encode(r#"{"id":"msg_01J","time":1234}"#.as_bytes());
        let decoded = cursor::decode(&ts_encoded).unwrap();
        assert_eq!(decoded.time, 1234.0);
    }

    #[test]
    fn cursor_rejects_negative_time() {
        let encoded = URL_SAFE_NO_PAD.encode(r#"{"id":"msg_01J","time":-1}"#);
        assert!(cursor::decode(&encoded).is_err());
        assert!(cursor::decode("not-base64!").is_err());
        assert!(cursor::decode(&URL_SAFE_NO_PAD.encode("{}")).is_err());
    }

    #[test]
    fn page_missing_session_not_found() {
        let (storage, _dir) = storage();
        let store = MessageStore::new(storage);
        let err = store.page("ses_missing", 50, None).unwrap_err();
        assert!(
            err.to_string().contains("Session not found: ses_missing"),
            "{err}"
        );
    }

    #[test]
    fn filter_compacted_passthrough_without_compaction() {
        // Newest-first input, as produced by stream().
        let msgs = vec![
            with_parts(user_message("msg_3", "ses", 3.0), vec![]),
            with_parts(
                assistant_message("msg_2", "ses", 2, "msg_1", None, None),
                vec![],
            ),
            with_parts(user_message("msg_1", "ses", 1.0), vec![]),
        ];
        let out = filter_compacted(msgs);
        let ids: Vec<&str> = out.iter().map(|m| message_id(&m.info)).collect();
        assert_eq!(ids, vec!["msg_1", "msg_2", "msg_3"]);
    }

    #[test]
    fn filter_compacted_reorders_around_compaction() {
        // Newest-first input, as produced by stream().
        let msgs = vec![
            with_parts(user_message("msg_5", "ses", 5.0), vec![]),
            with_parts(
                assistant_message("msg_4", "ses", 4, "msg_3", Some(true), Some("stop".into())),
                vec![],
            ),
            with_parts(
                user_message("msg_3", "ses", 3.0),
                vec![compaction_part("prt_3", "ses", "msg_3", Some("msg_1"))],
            ),
            with_parts(
                assistant_message("msg_2", "ses", 2, "msg_1", None, None),
                vec![],
            ),
            with_parts(user_message("msg_1", "ses", 1.0), vec![]),
        ];
        let out = filter_compacted(msgs);
        let ids: Vec<&str> = out.iter().map(|m| message_id(&m.info)).collect();
        // [compaction-user, summary, tail (msg_1..msg_2), remainder (msg_5)]
        assert_eq!(ids, vec!["msg_3", "msg_4", "msg_1", "msg_2", "msg_5"]);
    }

    #[test]
    fn filter_compacted_retains_tail_only() {
        // Compaction at msg_3 with tail msg_1: everything older than msg_1
        // is dropped by the retain walk.
        let msgs = vec![
            with_parts(user_message("msg_4", "ses", 4.0), vec![]),
            with_parts(
                assistant_message("msg_3a", "ses", 3, "msg_3", Some(true), Some("stop".into())),
                vec![],
            ),
            with_parts(
                user_message("msg_3", "ses", 3.0),
                vec![compaction_part("prt_3", "ses", "msg_3", Some("msg_1"))],
            ),
            with_parts(
                assistant_message("msg_2", "ses", 2, "msg_1", None, None),
                vec![],
            ),
            with_parts(user_message("msg_1", "ses", 1.0), vec![]),
            with_parts(user_message("msg_0", "ses", 0.0), vec![]),
        ];
        let out = filter_compacted(msgs);
        let ids: Vec<&str> = out.iter().map(|m| message_id(&m.info)).collect();
        assert_eq!(ids, vec!["msg_3", "msg_3a", "msg_1", "msg_2", "msg_4"]);
    }

    #[test]
    fn latest_tie_breaks_on_id() {
        let msgs = vec![
            with_parts(user_message("msg_1", "ses", 100.0), vec![]),
            with_parts(user_message("msg_2", "ses", 100.0), vec![]),
        ];
        let latest = super::latest(&msgs);
        assert_eq!(message_id(latest.user.as_ref().unwrap()), "msg_2");

        let msgs = vec![
            with_parts(user_message("msg_2", "ses", 100.0), vec![]),
            with_parts(user_message("msg_1", "ses", 101.0), vec![]),
        ];
        let latest = super::latest(&msgs);
        assert_eq!(message_id(latest.user.as_ref().unwrap()), "msg_1");
    }

    #[test]
    fn latest_tasks_after_finished() {
        let msgs = vec![
            with_parts(
                user_message("msg_3", "ses", 3.0),
                vec![compaction_part("prt_3", "ses", "msg_3", None)],
            ),
            with_parts(
                assistant_message("msg_2", "ses", 2, "msg_1", None, Some("stop".into())),
                vec![],
            ),
            with_parts(
                user_message("msg_1", "ses", 1.0),
                vec![compaction_part("prt_1", "ses", "msg_1", None)],
            ),
        ];
        let latest = super::latest(&msgs);
        assert!(latest.finished.is_some());
        // Only the message after the finished assistant contributes tasks.
        assert_eq!(latest.tasks.len(), 1);
        assert!(matches!(&latest.tasks[0],
            V1Part::Compaction { id, .. } if id.as_str() == "prt_3"));
    }

    #[test]
    fn latest_tasks_all_when_no_finished() {
        let msgs = vec![
            with_parts(
                user_message("msg_2", "ses", 2.0),
                vec![compaction_part("prt_2", "ses", "msg_2", None)],
            ),
            with_parts(
                user_message("msg_1", "ses", 1.0),
                vec![compaction_part("prt_1", "ses", "msg_1", None)],
            ),
        ];
        let latest = super::latest(&msgs);
        assert_eq!(latest.tasks.len(), 2);
    }
}

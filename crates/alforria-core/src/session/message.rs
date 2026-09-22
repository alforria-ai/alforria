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
        let before = match before {
            Some(encoded) => Some(cursor::decode(encoded)?),
            None => None,
        };
        let mut rows = self.query_page(session_id, limit + 1, before.as_ref())?;
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
        session_id: &str,
        limit: usize,
        before: Option<&Cursor>,
    ) -> Result<Vec<Message>, CoreError> {
        let sql = if before.is_some() {
            "SELECT * FROM message WHERE session_id = ?1 AND (time_created < ?2 OR (time_created = ?2 AND id < ?3)) ORDER BY time_created DESC, id DESC LIMIT ?4"
        } else {
            "SELECT * FROM message WHERE session_id = ?1 ORDER BY time_created DESC, id DESC LIMIT ?4"
        };
        let empty = String::new();
        let (time, id) = match before {
            Some(cursor) => (cursor.time as i64, cursor.id.clone()),
            None => (0, empty),
        };
        self.storage.with_connection(|conn| {
            query_messages(
                conn,
                sql,
                rusqlite::params![session_id, time, id, limit as i64],
            )
        })
    }

    /// `hydrate` (message-v2.ts:98-122): batch part lookup per message,
    /// ordered by (message_id, id).
    fn hydrate(&self, rows: &[Message]) -> Result<Vec<WithParts>, CoreError> {
        let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
        let mut part_lists: HashMap<String, Vec<V1Part>> = HashMap::new();
        if !ids.is_empty() {
            let placeholders = vec!["?"; ids.len()].join(", ");
            let sql = format!(
                "SELECT * FROM part WHERE message_id IN ({placeholders}) ORDER BY message_id, id"
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

    /// `MessageV2.stream` (message-v2.ts:469-490): page-until-exhausted in
    /// batches of 50, newest-first. A missing session resolves to `[]`.
    pub fn stream(&self, session_id: &str) -> Result<Vec<WithParts>, SessionError> {
        let size = 50usize;
        let mut result = Vec::new();
        let mut before: Option<String> = None;
        loop {
            let page = match self.page(session_id, size, before.as_deref()) {
                Ok(page) => page,
                // TS: catchIf(NotFoundError, () => empty page)
                Err(SessionError::NotFound(_)) => return Ok(Vec::new()),
                Err(err) => return Err(err),
            };
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
        Ok(result)
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
        Ok(WithParts {
            info: info_from_row(&row)?,
            parts: self.parts(message_id)?,
        })
    }
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

/// `filterCompacted` (message-v2.ts:521-572 — binding): retain-walk over the
/// newest-first stream, reverse, then reorder around the last compaction
/// into `[compaction-user, summary, tail, remainder]`.
pub fn filter_compacted(msgs: Vec<WithParts>) -> Vec<WithParts> {
    let mut result: Vec<WithParts> = Vec::new();
    let mut completed: HashSet<String> = HashSet::new();
    let mut retain: Option<String> = None;
    for msg in msgs {
        let id = message_id(&msg.info).to_string();
        result.push(msg);
        let msg = result.last().expect("just pushed");
        if let Some(retained) = retain.as_deref() {
            if id == retained {
                break;
            }
            continue;
        }
        if message_role(&msg.info) == "user" && completed.contains(&id) {
            let Some(part) = msg.parts.iter().find(|part| is_compaction(part)) else {
                continue;
            };
            let Some(tail) = compaction_tail(part).and_then(|tail| tail.clone()) else {
                break;
            };
            retain = Some(tail.clone());
            if id == tail {
                break;
            }
            continue;
        }
        if message_role(&msg.info) == "user"
            && completed.contains(&id)
            && msg.parts.iter().any(is_compaction)
        {
            break;
        }
        if let V1Message::Assistant {
            summary: Some(true),
            finish: Some(_),
            error: None,
            parent_id,
            ..
        } = &msg.info
        {
            completed.insert(parent_id.clone());
        }
    }
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

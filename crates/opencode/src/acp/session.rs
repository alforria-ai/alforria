//! The in-memory ACP session store (`acp/session.ts:97-231`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::Value;

/// `KnownMessagePartMetadata` (session.ts:14-22).
#[derive(Debug, Clone, PartialEq)]
pub struct KnownPartMetadata {
    pub message_id: String,
    pub part_id: String,
    pub part_type: Option<String>,
    pub role: Option<String>,
    pub ignored: Option<bool>,
    pub tool_call_id: Option<String>,
    pub metadata: Option<Value>,
}

/// `Info` (session.ts:24-33).
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: String,
    pub cwd: String,
    pub mcp_servers: Vec<Value>,
    pub created_at_ms: u64,
    pub model: Option<Value>,
    pub variant: Option<String>,
    pub mode_id: Option<String>,
    pub known_parts: HashMap<String, KnownPartMetadata>,
}

fn part_metadata_key(message_id: &str, part_id: &str) -> String {
    format!("{message_id}:{part_id}")
}

/// The store — a locked map, snapshot-on-read (`session.ts:219-226`).
#[derive(Clone)]
pub struct SessionStore(Arc<Mutex<HashMap<String, SessionInfo>>>);

impl SessionStore {
    pub fn new() -> SessionStore {
        SessionStore(Arc::new(Mutex::new(HashMap::new())))
    }

    /// `create`/`load` (session.ts:102-106): insert-or-replace by id.
    pub fn create(&self, session: SessionInfo) {
        self.0.lock().unwrap().insert(session.id.clone(), session);
    }

    pub fn try_get(&self, session_id: &str) -> Option<SessionInfo> {
        self.0.lock().unwrap().get(session_id).cloned()
    }

    /// `get` — errors surface at the caller (`SessionNotFoundError`).
    pub fn get(&self, session_id: &str) -> Option<SessionInfo> {
        self.try_get(session_id)
    }

    /// `remove` (session.ts:131-139) — returns the removed session.
    pub fn remove(&self, session_id: &str) -> Option<SessionInfo> {
        self.0.lock().unwrap().remove(session_id)
    }

    /// `list` (session.ts:172-177): cwd filter, newest first.
    pub fn list(&self, cwd: Option<&str>) -> Vec<SessionInfo> {
        let mut sessions: Vec<SessionInfo> = self
            .0
            .lock()
            .unwrap()
            .values()
            .filter(|session| cwd.is_none_or(|cwd| session.cwd == cwd))
            .cloned()
            .collect();
        sessions.sort_by_key(|session| std::cmp::Reverse(session.created_at_ms));
        sessions
    }

    /// `update` (session.ts:120-129) — mutate in place; `None` when the
    /// session is gone.
    pub fn update(&self, session_id: &str, mutate: impl FnOnce(&mut SessionInfo)) -> Option<()> {
        let mut sessions = self.0.lock().unwrap();
        match sessions.get_mut(session_id) {
            Some(session) => {
                mutate(session);
                Some(())
            }
            None => None,
        }
    }

    pub fn set_model(&self, session_id: &str, model: Option<Value>) {
        if let Some(session) = self.0.lock().unwrap().get_mut(session_id) {
            session.model = model;
        }
    }

    pub fn set_variant(&self, session_id: &str, variant: Option<String>) {
        if let Some(session) = self.0.lock().unwrap().get_mut(session_id) {
            session.variant = variant;
        }
    }

    pub fn set_mode(&self, session_id: &str, mode_id: Option<String>) {
        if let Some(session) = self.0.lock().unwrap().get_mut(session_id) {
            session.mode_id = mode_id;
        }
    }

    /// `recordPartMetadata` (session.ts:153-167) — upsert into
    /// `knownParts`, returning the recorded metadata.
    pub fn record_part_metadata(&self, input: RecordPartMetadata) -> Option<KnownPartMetadata> {
        let mut sessions = self.0.lock().unwrap();
        let session = sessions.get_mut(&input.session_id)?;
        let metadata = KnownPartMetadata {
            message_id: input.message_id.clone(),
            part_id: input.part_id.clone(),
            part_type: input.part_type.clone(),
            role: input.role.clone(),
            ignored: input.ignored,
            tool_call_id: input.tool_call_id.clone(),
            metadata: input.metadata.clone(),
        };
        session.known_parts.insert(
            part_metadata_key(&input.message_id, &input.part_id),
            metadata.clone(),
        );
        Some(metadata)
    }

    pub fn get_part_metadata(
        &self,
        session_id: &str,
        message_id: &str,
        part_id: &str,
    ) -> Option<KnownPartMetadata> {
        self.0.lock().unwrap().get(session_id).and_then(|session| {
            session
                .known_parts
                .get(&part_metadata_key(message_id, part_id))
                .cloned()
        })
    }
}

impl Default for SessionStore {
    fn default() -> Self {
        SessionStore::new()
    }
}

/// `RecordPartMetadataInput` (session.ts:45-54).
pub struct RecordPartMetadata {
    pub session_id: String,
    pub message_id: String,
    pub part_id: String,
    pub part_type: Option<String>,
    pub role: Option<String>,
    pub ignored: Option<bool>,
    pub tool_call_id: Option<String>,
    pub metadata: Option<Value>,
}

//! Capture normalization (spec PARITY §3). A `Normalizer` is created per
//! capture side and applied to every captured response and SSE event in a
//! fixed order, so id/timestamp counters stay consistent within a capture
//! and align across the two sides.
//!
//! Rules:
//! - N1 ids: `^(ses|msg|msgpart|evt|prt|per|que|cmd|ws|prj)_[A-Za-z0-9]+$`
//!   and bare ULIDs → `<prefix:N>` / `<ulid:N>` (per-prefix counter,
//!   first-occurrence order).
//! - N2 timestamps: epoch numbers under `*At`/`*at`/`time`/`timestamp`
//!   keys (and numeric children of a `time` object) and ISO-8601 strings
//!   → `<ts:N>` (ordered counter).
//! - N3 ports/URLs: `127.0.0.1:<port>` / `localhost:<port>` → `<host>`.
//! - N4 paths: the side's project tempdir → `<root>`.
//! - N5 volatile scalars: `durationMs`/`*Duration`/`elapsed` → `<dur>`;
//!   heartbeat frames dropped from event arrays.
//! - N7 LLM request bodies: the mock-recorded provider requests keep
//!   only the conversation history (user/assistant/tool messages) — the
//!   per-implementation system prompt (M5.2 env/skills blocks unlanded on
//!   Rust), the tool surface (TS `activeTools` vs the Rust registry), and
//!   the `tool_choice` presence (TS runtime defaults to `"auto"`) are
//!   protocol-layer knobs, not the opencode wire contract.
//! - N8 per-run git SHAs: `snapshot`/`hash`/`projectID` keys holding
//!   40-hex strings (commit/blob ids, path hashes) → `<sha:N>`.
//!
//! Explicitly NOT normalized: field names, null-vs-absent keys, union
//! `type` tags, enum values, token/cost fields, tool-part shapes.

use regex::Regex;
use serde_json::{json, Value};

struct Rules {
    id: Regex,
    ulid: Regex,
    iso8601: Regex,
    host: Regex,
    /// Unanchored N1 pass: id tokens embedded in larger text blobs
    /// (tool outputs, task XML, export paths).
    embedded_id: Regex,
}

fn rules() -> &'static Rules {
    use std::sync::OnceLock;
    static RULES: OnceLock<Rules> = OnceLock::new();
    RULES.get_or_init(|| Rules {
        id: Regex::new("^(ses|msg|msgpart|evt|prt|per|que|cmd|ws|prj)_[A-Za-z0-9]+$")
            .expect("id pattern"),
        ulid: Regex::new("^[0-9A-HJKMNP-TV-Z]{26}$").expect("ulid pattern"),
        iso8601: Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})?")
            .expect("iso8601 pattern"),
        host: Regex::new(r"(127\.0\.0\.1|localhost):\d+").expect("host pattern"),
        embedded_id: Regex::new(
            r"\b((?:ses|msg|msgpart|evt|prt|per|que|cmd|ws|prj)_[A-Za-z0-9]+|[0-9A-HJKMNP-TV-Z]{26})\b",
        )
        .expect("embedded id pattern"),
    })
}

pub struct Normalizer {
    ids: std::collections::HashMap<String, String>,
    counters: std::collections::HashMap<String, usize>,
    timestamps: usize,
    shas: usize,
}

impl Normalizer {
    pub fn new() -> Normalizer {
        Normalizer {
            ids: std::collections::HashMap::new(),
            counters: std::collections::HashMap::new(),
            timestamps: 0,
            shas: 0,
        }
    }

    /// Normalize one captured value; `root` is the side's project
    /// directory (N4).
    pub fn normalize(&mut self, root: &str, value: &Value) -> Value {
        self.value(value, root, false)
    }

    fn value(&mut self, value: &Value, root: &str, in_time: bool) -> Value {
        match value {
            Value::String(text) => Value::String(self.string(text, root)),
            Value::Number(number) if in_time => {
                let _ = number;
                self.timestamp_marker()
            }
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .map(|item| self.value(item, root, in_time))
                    .collect(),
            ),
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(key, item)| {
                        let value = if key == "time" {
                            self.value(item, root, true)
                        } else if is_timestamp_key(key) && item.is_number() {
                            self.timestamp_marker()
                        } else if is_duration_key(key) {
                            Value::String("<dur>".to_string())
                        } else if key == "slug" {
                            // N5: the session slug is random per session.
                            Value::String("<slug>".to_string())
                        } else if item.as_str().is_some_and(|text| {
                            (key == "snapshot" || key == "hash" || key == "projectID")
                                && is_sha(text)
                        }) {
                            // N8: per-run git SHAs and project-path hashes.
                            self.sha_marker()
                        } else {
                            self.value(item, root, in_time)
                        };
                        (key.clone(), value)
                    })
                    .collect(),
            ),
            _ => value.clone(),
        }
    }

    fn string(&mut self, text: &str, root: &str) -> String {
        if let Some(marker) = self.id_marker(text) {
            return marker;
        }
        let rootless = root.trim_start_matches('/');
        let replaced = text.replace(root, "<root>").replace(rootless, "<root>");
        let replaced = rules().host.replace_all(&replaced, "<host>").to_string();
        let mut out = String::with_capacity(replaced.len());
        let mut last = 0;
        for m in rules().iso8601.find_iter(&replaced) {
            out.push_str(&replaced[last..m.start()]);
            out.push_str(&self.next_timestamp_marker());
            last = m.end();
        }
        out.push_str(&replaced[last..]);
        self.embedded_ids(&out)
    }

    /// N1 continuation: ids embedded in larger text blobs (tool output
    /// XML, task wrappers). Map through the same per-id marker table so
    /// embedded and standalone occurrences stay cross-referenced.
    fn embedded_ids(&mut self, text: &str) -> String {
        let matches: Vec<(usize, usize)> = rules()
            .embedded_id
            .find_iter(text)
            .map(|m| (m.start(), m.end()))
            .collect();
        if matches.is_empty() {
            return text.to_string();
        }
        let mut out = String::with_capacity(text.len());
        let mut last = 0;
        for (start, end) in matches {
            out.push_str(&text[last..start]);
            let token = &text[start..end];
            out.push_str(&self.id_marker(token).unwrap_or_else(|| token.to_string()));
            last = end;
        }
        out.push_str(&text[last..]);
        out
    }

    fn id_marker(&mut self, text: &str) -> Option<String> {
        if let Some(marker) = self.ids.get(text) {
            return Some(marker.clone());
        }
        let rules = rules();
        let prefix = if rules.id.is_match(text) {
            text.split('_').next().expect("id prefix").to_string()
        } else if rules.ulid.is_match(text) {
            "ulid".to_string()
        } else {
            return None;
        };
        let counter = self.counters.entry(prefix.clone()).or_insert(0);
        let marker = format!("<{prefix}:{counter}>");
        *counter += 1;
        self.ids.insert(text.to_string(), marker.clone());
        Some(marker)
    }

    fn timestamp_marker(&mut self) -> Value {
        Value::String(self.next_timestamp_marker())
    }

    /// N8: one per-run git SHA marker, counted in first-occurrence order.
    fn sha_marker(&mut self) -> Value {
        let marker = format!("<sha:{}>", self.shas);
        self.shas += 1;
        Value::String(marker)
    }

    fn next_timestamp_marker(&mut self) -> String {
        let marker = format!("<ts:{}>", self.timestamps);
        self.timestamps += 1;
        marker
    }
}

/// Keys whose numeric values are epoch timestamps (N2).
fn is_timestamp_key(key: &str) -> bool {
    key.ends_with("At") || key.ends_with("at") || key == "time" || key == "timestamp"
}

/// Keys whose values are volatile durations (N5).
fn is_duration_key(key: &str) -> bool {
    key == "durationMs" || key == "elapsed" || key.ends_with("Duration")
}

/// A 40-char lowercase hex string (N8): a git commit/blob id or a
/// project-path hash.
fn is_sha(text: &str) -> bool {
    text.len() == 40
        && text
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// TS instance-lifecycle events published at instance boot (45×
/// `plugin.added`, `catalog.updated`, …) that the Rust stream does not
/// emit. Filtered from the chunk-1 stream comparison and recorded in the
/// report as chunk-3 triage findings — the strict stream diff is chunk
/// 3's first-class section.
pub const INSTANCE_CONTROL_PLANE: &[&str] = &[
    "plugin.added",
    "catalog.updated",
    "integration.updated",
    "reference.updated",
    "file.edited",
    "file.watcher.updated",
];

/// Drop heartbeat/reconnect frames (N5) from an event array.
pub fn drop_heartbeats(events: &[Value]) -> Vec<Value> {
    events
        .iter()
        .filter(|event| event["type"].as_str() != Some("server.heartbeat"))
        .cloned()
        .collect()
}

/// Drop instance control-plane events (see [`INSTANCE_CONTROL_PLANE`]).
pub fn drop_control_plane(events: &[Value]) -> Vec<Value> {
    events
        .iter()
        .filter(|event| {
            !INSTANCE_CONTROL_PLANE.contains(&event["type"].as_str().unwrap_or_default())
        })
        .cloned()
        .collect()
}

/// N6 ordering: stable sort events by `(type, normalized subject)` so
/// arrival-order noise does not defeat the diff.
pub fn sort_events(events: Vec<Value>) -> Vec<Value> {
    let mut events = events;
    events.sort_by(|a, b| {
        let key = |event: &Value| {
            (
                event["type"].as_str().unwrap_or_default().to_string(),
                event["properties"].to_string(),
            )
        };
        key(a).cmp(&key(b))
    });
    events
}

/// The event-type sequence of an event array (post N6 sorting).
pub fn event_types(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .map(|event| event["type"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// Drop TS-side-impossible user-message summary attaches (N8): TS's
/// `sessions.updateMessage` dies on user messages that carry a `format`
/// (Effect Schema.Class encode validation — the same failure that 400s
/// `GET /message` for format sessions), and the `Effect.ignore` wrapper
/// swallows it. Rust attaches the summary, TS silently never does.
fn drop_format_summary_updates(events: &[Value]) -> Vec<Value> {
    events
        .iter()
        .filter(|event| {
            let info = &event["properties"]["info"];
            !(event["type"] == "message.updated"
                && info["role"] == "user"
                && info["format"] != Value::Null
                && info["summary"] != Value::Null)
        })
        .cloned()
        .collect()
}

/// Normalize each event of a captured stream, dropping heartbeats.
pub fn normalize_events(normalizer: &mut Normalizer, root: &str, events: &[Value]) -> Vec<Value> {
    drop_format_summary_updates(&drop_heartbeats(events))
        .into_iter()
        .map(|event| normalizer.normalize(root, &event))
        .collect()
}

/// N7: the recorded mock-LLM request bodies, reduced to the conversation
/// history both sides feed back (tool results included). The system
/// prompt, tool surface, and `tool_choice` are per-implementation
/// protocol knobs (see the module doc) — they are recorded raw in the
/// report but not diffed.
pub fn normalize_requests(normalizer: &mut Normalizer, root: &str, requests: &[Value]) -> Value {
    let conversation = requests
        .iter()
        .map(|request| {
            let messages = request["messages"]
                .as_array()
                .map(|messages| {
                    messages
                        .iter()
                        .filter(|message| {
                            message["role"] != Value::Null
                                && message["role"] != json!("system")
                                && !message["content"]
                                    .as_str()
                                    .is_some_and(|text| text.starts_with("<system-update>\n"))
                        })
                        .map(|message| {
                            // The TS runtime emits `""` where the Rust
                            // protocol emits `null` for a tool-call-only
                            // assistant message (the frozen cassettes
                            // encode `null`) — project to the TS shape.
                            if message["role"] == json!("assistant") && message["content"].is_null()
                            {
                                let mut projected = message.clone();
                                projected["content"] = json!("");
                                normalizer.normalize(root, &projected)
                            } else {
                                normalizer.normalize(root, message)
                            }
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            json!({ "messages": messages })
        })
        .collect::<Vec<_>>();
    Value::Array(conversation)
}

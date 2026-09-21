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
//!
//! Explicitly NOT normalized: field names, null-vs-absent keys, union
//! `type` tags, enum values, token/cost fields, tool-part shapes.

use regex::Regex;
use serde_json::Value;

struct Rules {
    id: Regex,
    ulid: Regex,
    iso8601: Regex,
    host: Regex,
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
    })
}

pub struct Normalizer {
    ids: std::collections::HashMap<String, String>,
    counters: std::collections::HashMap<String, usize>,
    timestamps: usize,
}

impl Normalizer {
    pub fn new() -> Normalizer {
        Normalizer {
            ids: std::collections::HashMap::new(),
            counters: std::collections::HashMap::new(),
            timestamps: 0,
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

/// Normalize each event of a captured stream, dropping heartbeats.
pub fn normalize_events(normalizer: &mut Normalizer, root: &str, events: &[Value]) -> Vec<Value> {
    drop_heartbeats(events)
        .into_iter()
        .map(|event| normalizer.normalize(root, &event))
        .collect()
}

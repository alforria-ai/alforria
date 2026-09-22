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
//! - N9 canonical event order: SSE events are sorted by
//!   `(type, projected payload)` — volatile tokens replaced — before
//!   normalization, so marker assignment does not depend on the arrival
//!   order that differs between the two binaries.
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
    shas: std::collections::HashMap<String, String>,
}

impl Normalizer {
    pub fn new() -> Normalizer {
        Normalizer {
            ids: std::collections::HashMap::new(),
            counters: std::collections::HashMap::new(),
            shas: std::collections::HashMap::new(),
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
                self.timestamp_marker(&number.to_string())
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
                            self.timestamp_marker(&item.to_string())
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
                            item.as_str()
                                .map(|text| self.sha_marker(text))
                                .unwrap_or(Value::Null)
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
            out.push_str(&self.next_timestamp_marker(m.as_str()));
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

    /// N2: one constant marker. The raw instants differ per side, so a
    /// per-side counter cannot align across binaries: a side that
    /// completes two steps inside one clock tick collapses two distinct
    /// instants into one marker, shifting every later number. The
    /// constant marker keeps the wire-shape comparison (presence,
    /// ordering, structure) exact and lets the timing artifacts cancel.
    fn timestamp_marker(&mut self, raw: &str) -> Value {
        let _ = raw;
        Value::String("<ts>".to_string())
    }

    /// N8: one marker per distinct per-run git SHA, memoized so the
    /// same commit/blob id keeps the same marker across captures (the
    /// legacy/V2 self-consistency diff compares two normalizations of
    /// the same stream).
    fn sha_marker(&mut self, raw: &str) -> Value {
        if let Some(marker) = self.shas.get(raw) {
            return Value::String(marker.clone());
        }
        let marker = format!("<sha:{}>", self.shas.len());
        self.shas.insert(raw.to_string(), marker.clone());
        Value::String(marker)
    }

    fn next_timestamp_marker(&mut self, raw: &str) -> String {
        let _ = raw;
        "<ts>".to_string()
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
    let payload = "properties";
    let mut events = events;
    events.sort_by(|a, b| {
        let key = |event: &Value| {
            (
                event["type"].as_str().unwrap_or_default().to_string(),
                event[payload].to_string(),
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

/// Drop the V2 envelope `durable` block (`{aggregateID, seq, version}`)
/// from every event — sequence numbers are schema-checked, not diffed
/// (spec PARITY §3.1).
pub fn drop_durable(events: &[Value]) -> Vec<Value> {
    events
        .iter()
        .map(|event| {
            let mut event = event.clone();
            if let Some(object) = event.as_object_mut() {
                object.remove("durable");
            }
            event
        })
        .collect()
}

/// Structural checks on the raw V2 envelope (spec PARITY §3.1): envelope
/// shape (`id`, `type`, `data` present), and for durable events the
/// presence, type and per-aggregate monotonicity of `durable.seq`.
/// Returns one finding per violation.
pub fn check_v2_envelope(events: &[Value]) -> Vec<String> {
    let mut findings = Vec::new();
    let mut last_seq: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for (index, event) in events.iter().enumerate() {
        let event_type = event["type"].as_str().unwrap_or("<missing>");
        for key in ["id", "type", "data"] {
            if event[key].is_null() || event.get(key).is_none() {
                findings.push(format!("v2_events[{index}] ({event_type}): no {key}"));
            }
        }
        let Some(durable) = event.get("durable").filter(|durable| !durable.is_null()) else {
            continue;
        };
        let aggregate = durable["aggregateID"].as_str().unwrap_or("<missing>");
        let seq = match durable["seq"].as_i64() {
            Some(seq) => seq,
            None => {
                findings.push(format!(
                    "v2_events[{index}] ({event_type}): durable.seq is not an integer"
                ));
                continue;
            }
        };
        if durable["version"].as_i64().is_none() {
            findings.push(format!(
                "v2_events[{index}] ({event_type}): durable.version is not an integer"
            ));
        }
        if let Some(previous) = last_seq.get(aggregate) {
            if seq <= *previous {
                findings.push(format!(
                    "v2_events[{index}] ({event_type}): durable.seq {seq} not monotonic \
                     after {previous} for aggregate {aggregate}"
                ));
            }
        }
        last_seq.insert(aggregate.to_string(), seq);
    }
    findings
}

/// Drop TS-side-impossible user-message summary attaches (N8): TS's
/// `sessions.updateMessage` dies on user messages that carry a `format`
/// (Effect Schema.Class encode validation — the same failure that 400s
/// `GET /message` for format sessions), and the `Effect.ignore` wrapper
/// swallows it. Rust attaches the summary, TS silently never does.
fn drop_format_summary_updates_with(events: &[Value], payload: &str) -> Vec<Value> {
    events
        .iter()
        .filter(|event| {
            let info = &event[payload]["info"];
            !(event["type"] == "message.updated"
                && info["role"] == "user"
                && info["format"] != Value::Null
                && info["summary"] != Value::Null)
        })
        .cloned()
        .collect()
}

fn drop_format_summary_updates(events: &[Value]) -> Vec<Value> {
    drop_format_summary_updates_with(events, "properties")
}

/// Normalize each event of a captured stream, dropping heartbeats and
/// ordering canonically (N9) before normalization so N1/N2 markers
/// align across the two sides.
pub fn normalize_events(normalizer: &mut Normalizer, root: &str, events: &[Value]) -> Vec<Value> {
    let ordered = canonicalize_events(
        &session_update_race_with(
            drop_format_summary_updates(&drop_heartbeats(events)),
            "properties",
        ),
        "properties",
    );
    ordered
        .into_iter()
        .map(|event| completed_race(normalizer.normalize(root, &event)))
        .collect()
}

/// N10: the `message.updated` `time.completed` race. TS publishes the
/// assistant message by reference and serializes lazily, so whether the
/// step-finish frame shows the cleanup's `completed` mutation depends on
/// SSE drain timing (p1 shows it, p6 does not). Rust serializes by value
/// and deterministically emits null on that frame; project both to the
/// marker so the racy field cannot fail the diff.
fn completed_race_with(payload: &str, mut event: Value) -> Value {
    if event["type"] == json!("message.updated") {
        let path = format!("/{payload}/info/time");
        if let Some(Value::Object(time)) = event.pointer_mut(&path) {
            time.insert("completed".to_string(), Value::String("<ts>".to_string()));
        }
    }
    if event["type"] == json!("message.part.updated") {
        // N11: the running-state title/metadata race: TS spreads
        // execute's `ctx.metadata` update into the shared part object,
        // so whether the running frame carries the title/metadata
        // depends on SSE drain timing (the completed frame and the
        // store captures verify both strictly).
        let path = format!("/{payload}/part/state");
        let is_running = event
            .pointer(&path)
            .and_then(Value::as_object)
            .is_some_and(|state| state.get("status") == Some(&json!("running")));
        if is_running {
            if let Some(Value::Object(state)) = event.pointer_mut(&path) {
                state.remove("title");
                state.remove("metadata");
            }
        }
    }
    event
}

fn completed_race(event: Value) -> Value {
    completed_race_with("properties", event)
}

/// N12: the title/summary-zero interleaving race. TS forks the title
/// LLM call (prompt.ts:1133) and `summary.summarize` (prompt.ts:1253)
/// in the same step-1 suspension window, so whether the wire carries
/// the title frame before or after the zero-summary frame is scheduler
/// timing — p1 captures show the summary landing first, p6 captures
/// show the title first. Rust is deterministic. A `session.updated`
/// frame whose (title, summary) state is a merge of its neighbors —
/// exactly one of the two transitions applied — is a race intermediate
/// and is dropped; both interleavings then normalize identically.
fn session_update_race_with(events: Vec<Value>, payload: &str) -> Vec<Value> {
    fn state<'a>(event: &'a Value, payload: &str) -> (&'a Value, &'a Value) {
        (
            &event[payload]["info"]["title"],
            &event[payload]["info"]["summary"],
        )
    }
    let updates: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| e["type"] == json!("session.updated"))
        .map(|(i, _)| i)
        .collect();
    if std::env::var("PARITY_DEBUG_N12").is_ok() {
        for (n, i) in updates.iter().enumerate() {
            let (t, s2) = state(&events[*i], payload);
            eprintln!("N12 session.updated[{n}] title={:?} summary={:?}", t, s2);
        }
    }
    let mut drop = vec![false; events.len()];
    for w in 1..updates.len().saturating_sub(1) {
        let prev = state(&events[updates[w - 1]], payload);
        let cur = state(&events[updates[w]], payload);
        let next = state(&events[updates[w + 1]], payload);
        if prev.0 != next.0 && prev.1 != next.1 {
            let title_first = cur.0 == next.0 && cur.1 == prev.1;
            let summary_first = cur.0 == prev.0 && cur.1 == next.1;
            drop[updates[w]] = title_first || summary_first;
        }
    }
    events
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !drop[*i])
        .map(|(_, e)| e)
        .collect()
}

/// Normalize each event of a captured V2 stream (spec PARITY §3.1): the
/// `durable` block is dropped before normalization (its presence and
/// monotonicity are checked on the raw capture, then it leaves the
/// value diff); V2 heartbeats are SSE comments and never reach the log.
pub fn normalize_v2_events(
    normalizer: &mut Normalizer,
    root: &str,
    events: &[Value],
) -> Vec<Value> {
    let ordered = canonicalize_events(
        &session_update_race_with(
            drop_format_summary_updates_with(&drop_durable(events), "data"),
            "data",
        ),
        "data",
    );
    ordered
        .into_iter()
        .map(|event| completed_race_with("data", normalizer.normalize(root, &event)))
        .collect()
}

/// N9 canonical pre-normalization order: arrival order is not stable
/// across the two binaries (parallel tool parts, SSE interleavings), and
/// N1/N2 markers are assigned in first-occurrence order — so markers
/// would be assigned differently per side. Events are therefore sorted
/// by `(type, projected payload)` before normalization, with every
/// volatile token (ids, timestamps, durations, SHAs) projected to a fixed
/// token so the sort key is identical for corresponding events on the
/// two sides. Events that tie on the projected key (identical except
/// volatile content) fall back to their raw timestamps as the
/// tie-breaker, so the two binaries number their markers in the same
/// order within a tie group.
pub fn canonicalize_events(events: &[Value], payload: &str) -> Vec<Value> {
    let mut events = events.to_vec();
    events.sort_by(|a, b| {
        canonical_key(a, payload)
            .cmp(&canonical_key(b, payload))
            .then_with(|| time_kept_key(a, payload).cmp(&time_kept_key(b, payload)))
    });
    events
}

/// The canonical sort key of one raw event: its `type` plus its payload
/// with volatile content projected away.
fn canonical_key(event: &Value, payload: &str) -> (String, String) {
    (
        event["type"].as_str().unwrap_or_default().to_string(),
        project_value(&event[payload]),
    )
}

/// The tie-breaker key: the payload with ids/SHAs/durations projected
/// away but raw timestamps kept, ordering volatile-identical events by
/// their logical instants.
fn time_kept_key(event: &Value, payload: &str) -> String {
    serde_json::to_string(&time_kept(&event[payload])).unwrap_or_default()
}

fn time_kept(value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(project_string(text)),
        Value::Array(items) => Value::Array(items.iter().map(time_kept).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| {
                    let value = if is_duration_key(key) || key == "slug" {
                        Value::String("<volatile>".to_string())
                    } else if (key == "snapshot" || key == "hash" || key == "projectID")
                        && value.as_str().is_some_and(|text| {
                            text.len() == 40
                                && text
                                    .bytes()
                                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
                        })
                    {
                        Value::String("<sha>".to_string())
                    } else {
                        time_kept(value)
                    };
                    (key.clone(), value)
                })
                .collect(),
        ),
        _ => value.clone(),
    }
}

fn project_value(value: &Value) -> String {
    serde_json::to_string(&project(value)).unwrap_or_default()
}

fn project(value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(project_string(text)),
        Value::Array(items) => Value::Array(items.iter().map(project).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| {
                    let value = if is_duration_key(key)
                        || (is_timestamp_key(key) && value.is_number())
                        || key == "time"
                        || (key == "snapshot" || key == "hash" || key == "projectID")
                            && value.as_str().is_some_and(|text| {
                                text.len() == 40
                                    && text
                                        .bytes()
                                        .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
                            }) {
                        Value::String("<volatile>".to_string())
                    } else {
                        project(value)
                    };
                    (key.clone(), value)
                })
                .collect(),
        ),
        _ => value.clone(),
    }
}

fn project_string(text: &str) -> String {
    let projected = rules()
        .embedded_id
        .replace_all(text, |found: &regex::Captures| {
            let token = found.get(0).map(|m| m.as_str()).unwrap_or_default();
            if token.contains('_') {
                token.split('_').next().unwrap_or_default().to_string()
            } else {
                "ulid".to_string()
            }
        });
    let projected = rules().iso8601.replace_all(&projected, "<ts>");
    if projected.len() == 40
        && projected
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return "<sha>".to_string();
    }
    projected.to_string()
}

/// Drop the envelope `id` key from each event before the stream value
/// diff: event ids are opaque, never cross-referenced, and their N1
/// markers stay arrival-order-sensitive within canonical-order ties.
pub fn drop_envelope_id(events: Vec<Value>) -> Vec<Value> {
    events
        .into_iter()
        .map(|mut event| {
            if let Some(object) = event.as_object_mut() {
                object.remove("id");
            }
            event
        })
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

//! openapi conformance harness — spec M1.12.
//!
//! Loads `fixtures/openapi/openapi.json` and provides:
//!
//! - [`assert_conforms`]: walks an object graph; for every object that
//!   matches a `$ref`-ed component by name, validates required keys,
//!   property names, and enum values.
//! - [`assert_union_type_strings`]: a table-driven section asserting, for
//!   every union variant, the wire type strings against the openapi union
//!   (`components/schemas/Event`, `V2Event`, `SessionDurableEvent`).
//!   The counts (89 / 88 / 28 / 32) are hard asserts — a mismatch means the
//!   enum drifted from the contract.
//!
//! Anchoring note (recorded deviation): openapi has no `SessionEvent`
//! component — its 32 wire type strings are the `session.next.*` subset of
//! the `V2Event` union (the components referenced by `SessionDurableEvent`
//! are the same `SessionNext*` envelopes).

use std::borrow::Cow;
use std::path::Path;
use std::sync::OnceLock;

use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use opencode_schema::event::LegacyEnvelope;
use opencode_schema::event::V2Envelope;
use opencode_schema::event_manifest::Event;
use opencode_schema::event_manifest::V2Event;
use opencode_schema::session_event::SessionDurableEvent;
use opencode_schema::session_event::SessionEvent;

fn spec() -> &'static Value {
    static SPEC: OnceLock<Value> = OnceLock::new();
    SPEC.get_or_init(|| {
        let path = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/openapi/openapi.json"
        ));
        let file = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("failed to read openapi.json at {path:?}: {e}"));
        serde_json::from_str(&file).expect("openapi.json must be valid JSON")
    })
}

fn schemas(spec: &Value) -> &Value {
    &spec["components"]["schemas"]
}

fn resolve<'a>(spec: &'a Value, schema: &'a Value) -> Cow<'a, Value> {
    let mut current = schema;
    while let Some(reference) = current.get("$ref").and_then(Value::as_str) {
        let name = reference.rsplit('/').next().unwrap_or(reference);
        current = schemas(spec)
            .get(name)
            .unwrap_or_else(|| panic!("dangling $ref {reference}"));
    }
    Cow::Borrowed(current)
}

// ---------------------------------------------------------------------
// Conformance walker
// ---------------------------------------------------------------------

/// Returns whether `sample` conforms to `schema` (which may be a `$ref`):
///
/// - every required property is present in the sample,
/// - every sample key exists in the component's properties,
/// - every enum-valued property matches the openapi enum.
fn conforms(spec: &Value, schema: &Value, sample: &Value) -> bool {
    let schema = resolve(spec, schema);

    if let Some(branches) = schema.get("anyOf").or_else(|| schema.get("oneOf")) {
        return branches
            .as_array()
            .expect("anyOf/oneOf must be an array")
            .iter()
            .any(|branch| conforms(spec, branch, sample));
    }
    if let Some(branches) = schema.get("allOf") {
        return branches
            .as_array()
            .expect("allOf must be an array")
            .iter()
            .all(|branch| conforms(spec, branch, sample));
    }

    if let Some(r#enum) = schema.get("enum").and_then(Value::as_array) {
        if !r#enum.contains(sample) {
            return false;
        }
    }

    let Some(schema_type) = schema.get("type").and_then(Value::as_str) else {
        // No type and no enum — open ended (e.g. an `additionalProperties`
        // map); nothing more to check.
        return true;
    };
    match schema_type {
        "object" => {
            let Some(sample) = sample.as_object() else {
                return false;
            };
            let Some(properties) = schema.get("properties") else {
                return true;
            };
            for key in schema
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if !sample.contains_key(key.as_str().expect("required keys are strings")) {
                    return false;
                }
            }
            for key in sample.keys() {
                let Some(property_schema) = properties.get(key) else {
                    return false;
                };
                if !conforms(spec, property_schema, &sample[key]) {
                    return false;
                }
            }
            true
        }
        "array" => {
            let Some(elements) = sample.as_array() else {
                return false;
            };
            match schema.get("items") {
                Some(items) => elements
                    .iter()
                    .all(|element| conforms(spec, items, element)),
                None => true,
            }
        }
        "string" => sample.is_string(),
        "number" => sample.is_number(),
        "integer" => sample.is_i64() || sample.is_u64(),
        "boolean" => sample.is_boolean(),
        "null" => sample.is_null(),
        other => panic!("unsupported openapi type {other}"),
    }
}

/// Asserts the sample conforms to the openapi component.
fn assert_conforms(spec: &Value, component: &str, sample: &Value) {
    let schema = schemas(spec)
        .get(component)
        .unwrap_or_else(|| panic!("unknown openapi component {component}"));
    assert!(
        conforms(spec, schema, sample),
        "sample does not conform to openapi component {component}:\n{sample}"
    );
}

// ---------------------------------------------------------------------
// Minimal-sample generator (from the openapi schema itself)
// ---------------------------------------------------------------------

/// Builds the minimal JSON value satisfying `schema`: every required
/// property (from the openapi component) with recursion into `$ref`s.
fn generate(spec: &Value, schema: &Value) -> Value {
    let schema = resolve(spec, schema);

    if let Some(r#enum) = schema.get("enum").and_then(Value::as_array) {
        return r#enum
            .first()
            .expect("enum schemas must be non-empty")
            .clone();
    }

    for combinator in ["anyOf", "oneOf", "allOf"] {
        if let Some(branches) = schema.get(combinator).and_then(Value::as_array) {
            return generate(
                spec,
                branches
                    .first()
                    .expect("union combinator must be non-empty"),
            );
        }
    }

    match schema.get("type").and_then(Value::as_str) {
        Some("string") => json!("x"),
        Some("boolean") => json!(false),
        Some("null") => Value::Null,
        Some("integer") => {
            let minimum = schema.get("minimum").and_then(Value::as_i64).unwrap_or(0);
            json!(minimum)
        }
        Some("number") => {
            // Integral values only: `number` covers both f64 fields
            // (Schema.Finite) and i64 fields (DateTimeUtcFromMillis), and
            // serde_json accepts integers for both.
            let minimum = schema.get("minimum").and_then(Value::as_i64).unwrap_or(0);
            json!(minimum)
        }
        Some("array") => {
            let items = schema.get("items").expect("array schema must have items");
            json!([generate(spec, items)])
        }
        Some("object") => {
            let mut value = serde_json::Map::new();
            for key in schema
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let key = key.as_str().expect("required keys are strings");
                let property_schema = schema
                    .get("properties")
                    .and_then(|p| p.get(key))
                    .unwrap_or_else(|| panic!("no schema for required property {key}"));
                value.insert(key.to_string(), generate(spec, property_schema));
            }
            Value::Object(value)
        }
        Some(other) => panic!("unsupported openapi type {other}"),
        None => panic!("schema without type or combinator: {schema}"),
    }
}

/// Generates a minimal event envelope for `component` (an openapi union
/// member schema such as `EventTodoUpdated` or `SessionNextTextStarted`).
fn generate_envelope(spec: &Value, component: &str) -> Value {
    let schema = schemas(spec)
        .get(component)
        .unwrap_or_else(|| panic!("unknown openapi component {component}"));
    let mut envelope = generate(spec, schema);
    // `session.error`: openapi omits `error` from the payload's required
    // list (union-typed properties are never marked required by the TS
    // generator), but schema-src and spec M1.10 require it. Seed the first
    // union member so the minimal envelope is accepted by the Rust type.
    if component == "EventSessionError" || component == "SessionError" {
        let payload_key = if envelope.get("properties").is_some() {
            "properties"
        } else {
            "data"
        };
        let error_schema = &schema["properties"][payload_key]["properties"]["error"];
        envelope[payload_key]["error"] = generate(
            spec,
            error_schema
                .get("anyOf")
                .or_else(|| error_schema.get("oneOf"))
                .and_then(Value::as_array)
                .and_then(|branches| branches.first())
                .expect("session.error error must be a union"),
        );
    }
    envelope
}

// ---------------------------------------------------------------------
// Union type-string tables
// ---------------------------------------------------------------------

/// Union members as `(component-name, wire type string)` pairs, in openapi
/// order.
fn union_entries<'a>(spec: &'a Value, union: &str, combinator: &str) -> Vec<(&'a str, &'a str)> {
    let schemas = schemas(spec);
    schemas[union][combinator]
        .as_array()
        .unwrap_or_else(|| panic!("union {union} must have a {combinator} list"))
        .iter()
        .map(|member| {
            let reference = member["$ref"]
                .as_str()
                .unwrap_or_else(|| panic!("union members must be $refs, got {member}"));
            let component = reference.rsplit('/').next().unwrap();
            let name = component.rsplit('/').next().unwrap();
            let wire_type = schemas[name]["properties"]["type"]["enum"][0]
                .as_str()
                .unwrap_or_else(|| panic!("member {name} must have a type enum"));
            (name, wire_type)
        })
        .collect()
}

fn wire_type_strings<'a>(entries: &[(&'a str, &'a str)]) -> Vec<&'a str> {
    entries.iter().map(|(_, wire_type)| *wire_type).collect()
}

/// Asserts the legacy-union type-string table (spec M1.12
/// `assert_union_type_strings` for the `{id, type, properties}` envelope).
fn assert_union_type_strings<E>(spec: &Value, entries: &[(&str, &str)])
where
    E: DeserializeOwned + serde::Serialize,
{
    for (component, wire_type) in entries {
        let envelope = generate_envelope(spec, component);
        let envelope: LegacyEnvelope<E> =
            serde_json::from_value(envelope).unwrap_or_else(|e| panic!("{component}: {e}"));
        let back = serde_json::to_value(&envelope).unwrap();
        assert_eq!(
            back["type"],
            json!(*wire_type),
            "wire type mismatch for {component}"
        );
    }
}

/// Asserts the v2-union type-string table (spec M1.12
/// `assert_union_type_strings` for the `{id, metadata?, type, durable?,
/// location?, data}` envelope).
fn assert_v2_union_type_strings<E>(spec: &Value, entries: &[(&str, &str)])
where
    E: DeserializeOwned + serde::Serialize,
{
    for (component, wire_type) in entries {
        let envelope = generate_envelope(spec, component);
        let envelope: V2Envelope<E> =
            serde_json::from_value(envelope).unwrap_or_else(|e| panic!("{component}: {e}"));
        let back = serde_json::to_value(&envelope).unwrap();
        assert_eq!(
            back["type"],
            json!(*wire_type),
            "wire type mismatch for {component}"
        );
    }
}

/// Asserts the inline-tagged shape of the inner `SessionEvent` /
/// `SessionDurableEvent` unions: the openapi V2 envelope is
/// `{id, metadata?, type, durable?, location?, data}`, so the in-process
/// union form merges `type` alongside the `data` fields
/// (`Schema.toTaggedUnion("type")`).
fn assert_session_union_inline_type_strings<E>(spec: &Value, entries: &[(&str, &str)])
where
    E: DeserializeOwned + serde::Serialize,
{
    for (component, wire_type) in entries {
        let envelope = generate_envelope(spec, component);
        let mut inline = envelope["data"].clone();
        inline["type"] = json!(*wire_type);
        let event: E =
            serde_json::from_value(inline.clone()).unwrap_or_else(|e| panic!("{component}: {e}"));
        let back = serde_json::to_value(&event).unwrap();
        assert_eq!(
            back["type"],
            json!(*wire_type),
            "wire type mismatch for {component}"
        );
        for (key, value) in inline.as_object().unwrap().iter() {
            if key == "type" {
                continue;
            }
            assert!(
                json_loose_eq(back.get(key), Some(value)),
                "field {key} drifted for {component}: {:?} != {value:?}",
                back.get(key)
            );
        }
    }
}

/// JSON equality that treats numbers numerically: f64-typed fields
/// serialize `0.0` where the seeded openapi value uses integer `0`
/// (JSON-equal, but `serde_json::Value` distinguishes them).
fn json_loose_eq(a: Option<&Value>, b: Option<&Value>) -> bool {
    match (a, b) {
        (Some(Value::Number(x)), Some(Value::Number(y))) => match (x.as_f64(), y.as_f64()) {
            (Some(x), Some(y)) => x == y,
            _ => x == y,
        },
        (Some(Value::Object(x)), Some(Value::Object(y))) => {
            x.len() == y.len()
                && x.iter().all(|(k, v)| {
                    y.get(k)
                        .map(|w| json_loose_eq(Some(v), Some(w)))
                        .unwrap_or(false)
                })
        }
        (Some(Value::Array(x)), Some(Value::Array(y))) => {
            x.len() == y.len()
                && x.iter()
                    .zip(y)
                    .all(|(x, y)| json_loose_eq(Some(x), Some(y)))
        }
        _ => a == b,
    }
}

/// openapi `Event` union — 89 types (legacy union order).
const EVENT_TYPES: &[&str] = &[
    "models-dev.refreshed",
    "integration.updated",
    "integration.connection.updated",
    "catalog.updated",
    "session.created",
    "session.updated",
    "session.deleted",
    "message.updated",
    "message.removed",
    "message.part.updated",
    "message.part.removed",
    "session.next.agent.switched",
    "session.next.model.switched",
    "session.next.moved",
    "session.next.prompted",
    "session.next.prompt.admitted",
    "session.next.context.updated",
    "session.next.synthetic",
    "session.next.shell.started",
    "session.next.shell.ended",
    "session.next.step.started",
    "session.next.step.ended",
    "session.next.step.failed",
    "session.next.text.started",
    "session.next.text.delta",
    "session.next.text.ended",
    "session.next.reasoning.started",
    "session.next.reasoning.delta",
    "session.next.reasoning.ended",
    "session.next.tool.input.started",
    "session.next.tool.input.delta",
    "session.next.tool.input.ended",
    "session.next.tool.called",
    "session.next.tool.progress",
    "session.next.tool.success",
    "session.next.tool.failed",
    "session.next.retried",
    "session.next.compaction.started",
    "session.next.compaction.delta",
    "session.next.compaction.ended",
    "session.next.revert.staged",
    "session.next.revert.cleared",
    "session.next.revert.committed",
    "message.part.delta",
    "session.diff",
    "session.error",
    "installation.updated",
    "installation.update-available",
    "file.edited",
    "reference.updated",
    "permission.v2.asked",
    "permission.v2.replied",
    "plugin.added",
    "project.directories.updated",
    "file.watcher.updated",
    "pty.created",
    "pty.updated",
    "pty.exited",
    "pty.deleted",
    "question.v2.asked",
    "question.v2.replied",
    "question.v2.rejected",
    "todo.updated",
    "lsp.updated",
    "permission.asked",
    "permission.replied",
    "tui.prompt.append",
    "tui.command.execute",
    "tui.toast.show",
    "tui.session.select",
    "mcp.tools.changed",
    "mcp.browser.open.failed",
    "command.executed",
    "project.updated",
    "session.status",
    "session.idle",
    "question.asked",
    "question.replied",
    "question.rejected",
    "session.compacted",
    "vcs.branch.updated",
    "workspace.ready",
    "workspace.failed",
    "workspace.status",
    "worktree.ready",
    "worktree.failed",
    "server.connected",
    "global.disposed",
    "server.instance.disposed",
];

/// openapi `V2Event` union — 88 types (same order as `EVENT_TYPES` minus
/// the legacy-only `server.instance.disposed`).
const V2_EVENT_TYPES: &[&str] = &[
    "models-dev.refreshed",
    "integration.updated",
    "integration.connection.updated",
    "catalog.updated",
    "session.created",
    "session.updated",
    "session.deleted",
    "message.updated",
    "message.removed",
    "message.part.updated",
    "message.part.removed",
    "session.next.agent.switched",
    "session.next.model.switched",
    "session.next.moved",
    "session.next.prompted",
    "session.next.prompt.admitted",
    "session.next.context.updated",
    "session.next.synthetic",
    "session.next.shell.started",
    "session.next.shell.ended",
    "session.next.step.started",
    "session.next.step.ended",
    "session.next.step.failed",
    "session.next.text.started",
    "session.next.text.delta",
    "session.next.text.ended",
    "session.next.reasoning.started",
    "session.next.reasoning.delta",
    "session.next.reasoning.ended",
    "session.next.tool.input.started",
    "session.next.tool.input.delta",
    "session.next.tool.input.ended",
    "session.next.tool.called",
    "session.next.tool.progress",
    "session.next.tool.success",
    "session.next.tool.failed",
    "session.next.retried",
    "session.next.compaction.started",
    "session.next.compaction.delta",
    "session.next.compaction.ended",
    "session.next.revert.staged",
    "session.next.revert.cleared",
    "session.next.revert.committed",
    "message.part.delta",
    "session.diff",
    "session.error",
    "installation.updated",
    "installation.update-available",
    "file.edited",
    "reference.updated",
    "permission.v2.asked",
    "permission.v2.replied",
    "plugin.added",
    "project.directories.updated",
    "file.watcher.updated",
    "pty.created",
    "pty.updated",
    "pty.exited",
    "pty.deleted",
    "question.v2.asked",
    "question.v2.replied",
    "question.v2.rejected",
    "todo.updated",
    "lsp.updated",
    "permission.asked",
    "permission.replied",
    "tui.prompt.append",
    "tui.command.execute",
    "tui.toast.show",
    "tui.session.select",
    "mcp.tools.changed",
    "mcp.browser.open.failed",
    "command.executed",
    "project.updated",
    "session.status",
    "session.idle",
    "question.asked",
    "question.replied",
    "question.rejected",
    "session.compacted",
    "vcs.branch.updated",
    "workspace.ready",
    "workspace.failed",
    "workspace.status",
    "worktree.ready",
    "worktree.failed",
    "server.connected",
    "global.disposed",
];

/// `SessionEvent` — all 32 `session.next.*` types (durable + live deltas).
const SESSION_EVENT_TYPES: &[&str] = &[
    "session.next.agent.switched",
    "session.next.model.switched",
    "session.next.moved",
    "session.next.prompted",
    "session.next.prompt.admitted",
    "session.next.context.updated",
    "session.next.synthetic",
    "session.next.shell.started",
    "session.next.shell.ended",
    "session.next.step.started",
    "session.next.step.ended",
    "session.next.step.failed",
    "session.next.text.started",
    "session.next.text.delta",
    "session.next.text.ended",
    "session.next.reasoning.started",
    "session.next.reasoning.delta",
    "session.next.reasoning.ended",
    "session.next.tool.input.started",
    "session.next.tool.input.delta",
    "session.next.tool.input.ended",
    "session.next.tool.called",
    "session.next.tool.progress",
    "session.next.tool.success",
    "session.next.tool.failed",
    "session.next.retried",
    "session.next.compaction.started",
    "session.next.compaction.delta",
    "session.next.compaction.ended",
    "session.next.revert.staged",
    "session.next.revert.cleared",
    "session.next.revert.committed",
];

/// openapi `SessionDurableEvent` — 28 types (no deltas).
const SESSION_DURABLE_EVENT_TYPES: &[&str] = &[
    "session.next.agent.switched",
    "session.next.model.switched",
    "session.next.moved",
    "session.next.prompted",
    "session.next.prompt.admitted",
    "session.next.context.updated",
    "session.next.synthetic",
    "session.next.shell.started",
    "session.next.shell.ended",
    "session.next.step.started",
    "session.next.step.ended",
    "session.next.step.failed",
    "session.next.text.started",
    "session.next.text.ended",
    "session.next.reasoning.started",
    "session.next.reasoning.ended",
    "session.next.tool.input.started",
    "session.next.tool.input.ended",
    "session.next.tool.called",
    "session.next.tool.progress",
    "session.next.tool.success",
    "session.next.tool.failed",
    "session.next.retried",
    "session.next.compaction.started",
    "session.next.compaction.ended",
    "session.next.revert.staged",
    "session.next.revert.cleared",
    "session.next.revert.committed",
];

#[test]
fn event_union_type_strings() {
    let spec = spec();
    let entries = union_entries(spec, "Event", "anyOf");
    assert_eq!(
        entries.len(),
        89,
        "openapi Event union must have 89 members"
    );
    assert_eq!(
        wire_type_strings(&entries),
        EVENT_TYPES,
        "Event enum drifted from the openapi contract"
    );
    assert_union_type_strings::<Event>(spec, &entries);
}

#[test]
fn v2_event_union_type_strings() {
    let spec = spec();
    let entries = union_entries(spec, "V2Event", "anyOf");
    assert_eq!(
        entries.len(),
        88,
        "openapi V2Event union must have 88 members"
    );
    assert_eq!(
        wire_type_strings(&entries),
        V2_EVENT_TYPES,
        "V2Event enum drifted from the openapi contract"
    );
    assert_v2_union_type_strings::<V2Event>(spec, &entries);
}

#[test]
fn session_event_union_type_strings() {
    let spec = spec();
    // openapi has no `SessionEvent` component — the 32 `session.next.*`
    // strings are the subset of the V2Event union (recorded deviation).
    let entries: Vec<(&str, &str)> = union_entries(spec, "V2Event", "anyOf")
        .into_iter()
        .filter(|(_, wire_type)| wire_type.starts_with("session.next."))
        .collect();
    assert_eq!(
        entries.len(),
        32,
        "SessionEvent must cover 32 session.next.* types"
    );
    assert_eq!(
        wire_type_strings(&entries),
        SESSION_EVENT_TYPES,
        "SessionEvent enum drifted from the openapi contract"
    );
    // The inner union serializes inline (toTaggedUnion("type")), not inside
    // the V2 envelope's `data` wrapper.
    assert_session_union_inline_type_strings::<SessionEvent>(spec, &entries);
}

#[test]
fn session_durable_event_union_type_strings() {
    let spec = spec();
    let entries = union_entries(spec, "SessionDurableEvent", "oneOf");
    assert_eq!(
        entries.len(),
        28,
        "openapi SessionDurableEvent union must have 28 members"
    );
    // Order-insensitive: the openapi `SessionDurableEvent` component lists
    // `reasoning.started`/`reasoning.ended` after the `tool.*` events, while
    // the Rust enum keeps the spec M1.8 order. Variant order is not
    // wire-visible; membership and count are what matter here.
    let mut wire_types = wire_type_strings(&entries);
    let mut expected = SESSION_DURABLE_EVENT_TYPES.to_vec();
    wire_types.sort_unstable();
    expected.sort_unstable();
    assert_eq!(
        wire_types, expected,
        "SessionDurableEvent enum drifted from the openapi contract"
    );
    assert_session_union_inline_type_strings::<SessionDurableEvent>(spec, &entries);
}

// ---------------------------------------------------------------------
// Golden vectors (spec §6) validated against openapi
// ---------------------------------------------------------------------

/// The §6 golden vectors, validated against their openapi components
/// (round-trip lives in `tests/golden.rs`; the samples here mirror the spec
/// §6 JSON — f64 fields keep their integer spellings since `number`
/// accepts both).
#[test]
fn golden_vectors_conform_to_openapi() {
    let spec = spec();
    let cases: Vec<(&str, Value)> = vec![
        // V1 — LocationRef
        (
            "LocationRef",
            json!({ "directory": "/home/jon/repo", "workspaceID": "wrk_abc" }),
        ),
        // V2 — v2 envelope, session.next.text.started
        (
            "SessionNextTextStarted",
            json!({
                "id": "evt_01JDY",
                "type": "session.next.text.started",
                "data": {
                    "timestamp": 1778031210000i64,
                    "sessionID": "ses_01JDY",
                    "assistantMessageID": "msg_01JDY",
                    "textID": "txt_1",
                },
            }),
        ),
        // V3 — v2 envelope with metadata/durable/location
        (
            "SessionNextToolFailed",
            json!({
                "id": "evt_01JDZ",
                "metadata": { "origin": "test" },
                "durable": { "aggregateID": "ses_01JDY", "seq": 12, "version": 1 },
                "location": { "directory": "/repo", "workspaceID": "wrk_1" },
                "type": "session.next.tool.failed",
                "data": {
                    "timestamp": 1778031210000i64,
                    "sessionID": "ses_01JDY",
                    "assistantMessageID": "msg_01JDY",
                    "callID": "call_1",
                    "error": { "type": "unknown", "message": "boom" },
                    "provider": { "executed": true, "metadata": { "a": { "b": "c" } } },
                },
            }),
        ),
        // V4 — legacy envelope, session.status with retry
        (
            "EventSessionStatus",
            json!({
                "id": "evt_01JE0",
                "type": "session.status",
                "properties": {
                    "sessionID": "ses_01JDY",
                    "status": {
                        "type": "retry",
                        "attempt": 1,
                        "message": "rate limited",
                        "action": {
                            "reason": "429",
                            "provider": "anthropic",
                            "title": "Rate limited",
                            "message": "Backing off",
                            "label": "Retry",
                            "link": "https://docs",
                        },
                        "next": 30,
                    },
                },
            }),
        ),
        // V5 — v2 user message
        (
            "SessionMessageUser",
            json!({
                "id": "msg_01JDY",
                "time": { "created": 1778031210000i64 },
                "text": "hello",
                "files": [
                    {
                        "uri": "file:///tmp/a.txt",
                        "mime": "text/plain",
                        "name": "a.txt",
                        "source": { "start": 0, "end": 5, "text": "hello" },
                    },
                ],
                "type": "user",
            }),
        ),
        // V6 — v2 assistant message with tool content
        (
            "SessionMessageAssistant",
            json!({
                "id": "msg_01JDY",
                "time": { "created": 1778031210000i64 },
                "type": "assistant",
                "agent": "build",
                "model": { "id": "claude-sonnet-4-5", "providerID": "anthropic" },
                "content": [
                    {
                        "type": "tool",
                        "id": "tool_1",
                        "name": "bash",
                        "state": {
                            "status": "completed",
                            "input": { "command": "ls" },
                            "content": [ { "type": "text", "text": "file.txt" } ],
                            "structured": {},
                            "attachments": [
                                {
                                    "uri": "file:///tmp/out",
                                    "mime": "text/plain",
                                    "name": "out",
                                },
                            ],
                        },
                        "time": { "created": 1778031210000i64 },
                    },
                ],
            }),
        ),
        // V7 — legacy envelope, message.part.updated
        (
            "EventMessagePartUpdated",
            json!({
                "id": "evt_01JE1",
                "type": "message.part.updated",
                "properties": {
                    "sessionID": "ses_01JDY",
                    "time": 1778031210000i64,
                    "part": {
                        "id": "prt_01J",
                        "sessionID": "ses_01JDY",
                        "messageID": "msg_01JDY",
                        "type": "text",
                        "text": "hi",
                    },
                },
            }),
        ),
        // V8 — v1 assistant message with APIError
        (
            "AssistantMessage",
            json!({
                "id": "msg_01JDY",
                "sessionID": "ses_01JDY",
                "role": "assistant",
                "time": { "created": 1778031210000i64 },
                "error": {
                    "name": "APIError",
                    "data": { "message": "upstream 500", "statusCode": 500, "isRetryable": true },
                },
                "parentID": "msg_01JDX",
                "modelID": "claude-sonnet-4-5",
                "providerID": "anthropic",
                "mode": "primary",
                "agent": "build",
                "path": { "cwd": "/repo", "root": "/repo" },
                "cost": 0.001,
                "tokens": {
                    "input": 10,
                    "output": 5,
                    "reasoning": 0,
                    "cache": { "read": 0, "write": 0 },
                },
            }),
        ),
        // V9 — v1 ToolPart, completed state
        (
            "ToolPart",
            json!({
                "id": "prt_01J",
                "sessionID": "ses_01JDY",
                "messageID": "msg_01JDY",
                "type": "tool",
                "callID": "call_1",
                "tool": "bash",
                "state": {
                    "status": "completed",
                    "input": { "command": "ls" },
                    "output": "file.txt",
                    "title": "ls",
                    "metadata": {},
                    "time": { "start": 1778031210000i64, "end": 1778031211000i64 },
                },
            }),
        ),
        // V10 — v1 CompactionPart, snake_case tail_start_id
        (
            "CompactionPart",
            json!({
                "id": "prt_01J",
                "sessionID": "ses_01JDY",
                "messageID": "msg_01JDY",
                "type": "compaction",
                "auto": true,
                "overflow": false,
                "tail_start_id": "msg_01JDX",
            }),
        ),
        // V11 — PTY connect token, snake_case expires_in
        (
            "PtyTicketConnectToken",
            json!({ "ticket": "abc", "expires_in": 300 }),
        ),
        // V12 — legacy envelope, permission.v2.asked
        (
            "EventPermissionV2Asked",
            json!({
                "id": "evt_01JE2",
                "type": "permission.v2.asked",
                "properties": {
                    "id": "per_01J",
                    "sessionID": "ses_01JDY",
                    "action": "bash",
                    "resources": [ "rm -rf /tmp/x" ],
                    "save": [ "always" ],
                    "source": { "type": "tool", "messageID": "msg_01JDY", "callID": "call_1" },
                },
            }),
        ),
        // V13 — v2 envelope, tui.toast.show without duration
        (
            "TuiToastShow",
            json!({
                "id": "evt_01JE3",
                "type": "tui.toast.show",
                "data": { "message": "done", "variant": "success" },
            }),
        ),
        // V14 — Integration OAuth method
        (
            "IntegrationOAuthMethod",
            json!({
                "id": "github",
                "type": "oauth",
                "label": "GitHub",
                "prompts": [
                    {
                        "type": "text",
                        "key": "pat",
                        "message": "Paste token",
                        "placeholder": "token",
                    },
                ],
            }),
        ),
        // V15 — Model native api variant
        (
            "ModelApi",
            json!({
                "id": "claude-sonnet-4-5",
                "type": "native",
                "settings": {},
            }),
        ),
        // V16 — legacy envelope, session.next.prompted
        (
            "EventSessionNextPrompted",
            json!({
                "id": "evt_01JE4",
                "type": "session.next.prompted",
                "properties": {
                    "timestamp": 1778031210000i64,
                    "sessionID": "ses_01JDY",
                    "messageID": "msg_01JDY",
                    "prompt": { "text": "hi" },
                    "delivery": "steer",
                },
            }),
        ),
    ];
    for (component, sample) in cases {
        assert_conforms(spec, component, &sample);
    }
}

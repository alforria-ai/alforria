//! run.ts:697-824 — the `run` event loop: one subscribed `/event` SSE
//! stream mapped to formatted output until the session goes idle.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};

use crate::cmd::run_output;
use crate::ui;

/// One renderable (or actuating) step produced by the event loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    /// `UI.println` — a styled line to stderr.
    Println(String),
    /// `UI.empty()`.
    Empty,
    /// stdout write + EOL.
    Stdout(String),
    /// `UI.error(message)`.
    Error(String),
    /// One `--format json` event line: stdout + EOL (run.ts:678-691).
    Emit(String),
    /// `client.permission.reply({requestID, reply})`.
    Reply {
        request_id: String,
        reply: &'static str,
    },
    /// `session.status` idle for the active session — break the loop.
    Stop,
}

/// The loop's mutable state (run.ts:698-700 `toggles`/`sessions`/`error`).
pub struct LoopState {
    pub session_id: String,
    pub sessions: HashSet<String>,
    pub toggles: HashMap<String, bool>,
    pub auto: bool,
    pub thinking: bool,
    pub format_json: bool,
    pub tty: bool,
    pub error: Option<String>,
    /// `Date.now()` seam for the JSON-stream timestamps.
    pub clock: fn() -> u64,
}

/// `Date.now()` in epoch milliseconds.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

impl LoopState {
    pub fn new(
        session_id: String,
        auto: bool,
        thinking: bool,
        format_json: bool,
        tty: bool,
    ) -> Self {
        Self::with_clock(session_id, auto, thinking, format_json, tty, now_ms)
    }

    pub fn with_clock(
        session_id: String,
        auto: bool,
        thinking: bool,
        format_json: bool,
        tty: bool,
        clock: fn() -> u64,
    ) -> Self {
        let mut sessions = HashSet::new();
        sessions.insert(session_id.clone());
        LoopState {
            session_id,
            sessions,
            toggles: HashMap::new(),
            auto,
            thinking,
            format_json,
            tty,
            error: None,
            clock,
        }
    }
}

fn str_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// run.ts:73-99 — render a tool part inline (or as a block with output).
fn tool_outputs(part: &Value) -> Vec<Output> {
    let info = run_output::tool_inline_info(part);
    if info.block {
        let mut out = vec![
            Output::Empty,
            Output::Println(run_output::inline_line(&info)),
        ];
        if let Some(body) = info.body.as_deref().filter(|body| !body.trim().is_empty()) {
            out.push(Output::Println(body.to_string()));
            out.push(Output::Empty);
        }
        return out;
    }
    vec![Output::Println(run_output::inline_line(&info))]
}

/// run.ts:108-124 — `✗ {title} failed`.
fn tool_error_outputs(part: &Value) -> Vec<Output> {
    let info = run_output::tool_inline_info(part);
    vec![Output::Println(run_output::inline_line(
        &run_output::Inline {
            icon: "✗".to_string(),
            title: format!("{} failed", info.title),
            description: info.description,
            block: false,
            body: None,
        },
    ))]
}

fn event_type(event: &Value) -> &str {
    event
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
}

fn properties(event: &Value) -> &Value {
    event.get("properties").unwrap_or(&Value::Null)
}

/// The `emit(type, data)` gate (run.ts:678-691): in json mode the event is
/// consumed by the JSON stream (stdout + EOL); the formatted printer skips
/// it. `data` is spread into the envelope after `sessionID`.
fn emit(state: &LoopState, kind: &str, payload: Value) -> Option<Output> {
    if !state.format_json {
        return None;
    }
    Some(Output::Emit(run_output::emit_envelope(
        (state.clock)(),
        kind,
        &state.session_id,
        &payload,
    )))
}

/// One event → zero or more outputs (run.ts:702-821). Pure: unit-testable
/// without a server.
pub fn map_event(state: &mut LoopState, event: &Value) -> Vec<Output> {
    let properties = properties(event);
    match event_type(event) {
        "session.created" => {
            if let Some(parent) = properties
                .get("info")
                .and_then(|info| info.get("parentID"))
                .and_then(|v| v.as_str())
            {
                if state.sessions.contains(parent) {
                    if let Some(id) = properties
                        .get("info")
                        .and_then(|info| info.get("id"))
                        .and_then(|v| v.as_str())
                    {
                        state.sessions.insert(id.to_string());
                    }
                }
            }
            Vec::new()
        }
        "message.updated" => {
            let session_matches = properties.get("sessionID").and_then(|v| v.as_str())
                == Some(state.session_id.as_str());
            let assistant = properties
                .get("info")
                .and_then(|info| info.get("role"))
                .and_then(|v| v.as_str())
                == Some("assistant");
            if session_matches
                && assistant
                && !state.format_json
                && !state.toggles.get("start").copied().unwrap_or(false)
            {
                state.toggles.insert("start".to_string(), true);
                let info = properties.get("info").unwrap_or(&Value::Null);
                return vec![
                    Output::Empty,
                    Output::Println(format!(
                        "> {} · {}",
                        str_field(info, "agent"),
                        str_field(info, "modelID")
                    )),
                    Output::Empty,
                ];
            }
            Vec::new()
        }
        "message.part.updated" => {
            let Some(part) = properties.get("part") else {
                return Vec::new();
            };
            if part.get("sessionID").and_then(|v| v.as_str()) != Some(state.session_id.as_str()) {
                return Vec::new();
            }
            map_part(state, part)
        }
        "session.error" => {
            let session_matches = properties.get("sessionID").and_then(|v| v.as_str())
                == Some(state.session_id.as_str());
            let Some(error) = properties.get("error") else {
                return Vec::new();
            };
            if !session_matches {
                return Vec::new();
            }
            let mut err = str_field(error, "name");
            if let Some(message) = error
                .get("data")
                .and_then(|data| data.get("message"))
                .and_then(|v| v.as_str())
            {
                err = message.to_string();
            }
            state.error = Some(match state.error.take() {
                Some(previous) => format!("{previous}\n{err}"),
                None => err.clone(),
            });
            if let Some(emitted) = emit(state, "error", json!({"error": error})) {
                return vec![emitted];
            }
            vec![Output::Error(err)]
        }
        "session.status" => {
            let session_matches = properties.get("sessionID").and_then(|v| v.as_str())
                == Some(state.session_id.as_str());
            let idle = properties
                .get("status")
                .and_then(|status| status.get("type"))
                .and_then(|v| v.as_str())
                == Some("idle");
            if session_matches && idle {
                return vec![Output::Stop];
            }
            Vec::new()
        }
        "permission.asked" => {
            let session_id = str_field(properties, "sessionID");
            if !state.sessions.contains(&session_id) {
                return Vec::new();
            }
            if state.auto {
                vec![Output::Reply {
                    request_id: str_field(properties, "id"),
                    reply: "once",
                }]
            } else {
                let patterns = properties
                    .get("patterns")
                    .and_then(|v| v.as_array())
                    .map(|patterns| {
                        patterns
                            .iter()
                            .filter_map(|pattern| pattern.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                vec![
                    Output::Println(format!(
                        "{}! {}permission requested: {} ({}); auto-rejecting",
                        ui::style::TEXT_WARNING_BOLD,
                        ui::style::TEXT_NORMAL,
                        str_field(properties, "permission"),
                        patterns,
                    )),
                    Output::Reply {
                        request_id: str_field(properties, "id"),
                        reply: "reject",
                    },
                ]
            }
        }
        _ => Vec::new(),
    }
}

/// The `message.part.updated` body (run.ts:720-779).
fn map_part(state: &mut LoopState, part: &Value) -> Vec<Output> {
    let part_type = str_field(part, "type");
    let status = str_field(part.get("state").unwrap_or(&Value::Null), "status");
    let part_id = str_field(part, "id");
    if part_type == "tool" && (status == "completed" || status == "error") {
        if let Some(emitted) = emit(state, "tool_use", json!({"part": part})) {
            return vec![emitted];
        }
        if status == "completed" {
            return tool_outputs(part);
        }
        let mut outputs = tool_error_outputs(part);
        outputs.push(Output::Error(str_field(
            part.get("state").unwrap_or(&Value::Null),
            "error",
        )));
        return outputs;
    }
    if part_type == "tool"
        && str_field(part, "tool") == "task"
        && status == "running"
        && !state.format_json
    {
        if state.toggles.get(&part_id).copied().unwrap_or(false) {
            return Vec::new();
        }
        state.toggles.insert(part_id, true);
        return tool_outputs(part);
    }
    if part_type == "step-start" {
        if let Some(emitted) = emit(state, "step_start", json!({"part": part})) {
            return vec![emitted];
        }
        return Vec::new();
    }
    if part_type == "step-finish" {
        if let Some(emitted) = emit(state, "step_finish", json!({"part": part})) {
            return vec![emitted];
        }
        return Vec::new();
    }
    if part_type == "text" {
        // `part.time?.end` (run.ts:753) — a null end is not finished.
        let finished = part
            .get("time")
            .and_then(|time| time.get("end"))
            .is_some_and(|end| !end.is_null());
        if !finished {
            return Vec::new();
        }
        if let Some(emitted) = emit(state, "text", json!({"part": part})) {
            return vec![emitted];
        }
        let text = str_field(part, "text").trim().to_string();
        if text.is_empty() {
            return Vec::new();
        }
        if !state.tty {
            return vec![Output::Stdout(text)];
        }
        return vec![Output::Empty, Output::Println(text), Output::Empty];
    }
    if part_type == "reasoning" {
        // `part.time?.end` (run.ts:766) — a null end is not finished.
        let finished = part
            .get("time")
            .and_then(|time| time.get("end"))
            .is_some_and(|end| !end.is_null());
        if !finished || !state.thinking {
            return Vec::new();
        }
        if let Some(emitted) = emit(state, "reasoning", json!({"part": part})) {
            return vec![emitted];
        }
        let text = str_field(part, "text").trim().to_string();
        if text.is_empty() {
            return Vec::new();
        }
        let line = format!("Thinking: {text}");
        if state.tty {
            return vec![
                Output::Empty,
                Output::Println(format!(
                    "{}\u{1b}[3m{line}\u{1b}[0m{}",
                    ui::style::TEXT_DIM,
                    ui::style::TEXT_NORMAL
                )),
                Output::Empty,
            ];
        }
        return vec![Output::Stdout(line)];
    }
    Vec::new()
}

/// Render one output onto the UI (or perform the permission reply).
pub fn apply<F>(ui: &mut crate::ui::Ui, outputs: &[Output], permission_reply: &mut F)
where
    F: FnMut(&str, &str),
{
    for output in outputs {
        match output {
            Output::Println(line) => ui.println(line),
            Output::Empty => ui.empty(),
            Output::Stdout(text) => ui.write_stdout(&format!("{text}\n")),
            Output::Emit(line) => ui.write_stdout(&format!("{line}\n")),
            Output::Error(message) => ui.error(message),
            Output::Reply { request_id, reply } => permission_reply(request_id, reply),
            Output::Stop => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn state(tty: bool) -> LoopState {
        LoopState::new("ses_1".to_string(), false, false, false, tty)
    }

    fn event(event_type: &str, properties: Value) -> Value {
        json!({"id": "evt_1", "type": event_type, "properties": properties})
    }

    fn part(part_type: &str, extra: Value) -> Value {
        let mut value = json!({
            "id": "prt_1",
            "sessionID": "ses_1",
            "messageID": "msg_1",
            "type": part_type,
        });
        if let Some(object) = value.as_object_mut() {
            for (key, item) in extra.as_object().into_iter().flatten() {
                object.insert(key.clone(), item.clone());
            }
        }
        value
    }

    #[test]
    fn first_assistant_message_prints_header() {
        let mut state = state(true);
        let event = event(
            "message.updated",
            json!({
                "sessionID": "ses_1",
                "info": {"role": "assistant", "agent": "build", "modelID": "claude"},
            }),
        );
        let outputs = map_event(&mut state, &event);
        assert_eq!(
            outputs,
            vec![
                Output::Empty,
                Output::Println("> build · claude".to_string()),
                Output::Empty,
            ]
        );
        assert!(state.toggles.get("start").copied().unwrap_or(false));
    }

    #[test]
    fn header_prints_only_once() {
        let mut state = state(true);
        let event = event(
            "message.updated",
            json!({
                "sessionID": "ses_1",
                "info": {"role": "assistant", "agent": "build", "modelID": "claude"},
            }),
        );
        map_event(&mut state, &event);
        assert_eq!(
            map_event(&mut state, &event),
            Vec::<Output>::new(),
            "second assistant message must not re-print the header"
        );
    }

    #[test]
    fn user_message_updates_do_not_print_header() {
        let mut state = state(true);
        let event = event(
            "message.updated",
            json!({
                "sessionID": "ses_1",
                "info": {"role": "user"},
            }),
        );
        assert_eq!(map_event(&mut state, &event), Vec::<Output>::new());
    }

    #[test]
    fn other_session_is_ignored() {
        let mut state = state(true);
        let event = event(
            "message.updated",
            json!({
                "sessionID": "ses_other",
                "info": {"role": "assistant", "agent": "build", "modelID": "claude"},
            }),
        );
        assert_eq!(map_event(&mut state, &event), Vec::<Output>::new());
    }

    #[test]
    fn text_non_tty_writes_stdout() {
        let mut state = state(false);
        let event = event(
            "message.part.updated",
            json!({"part": part("text", json!({"text": " hello ", "time": {"start": 1, "end": 2}}))}),
        );
        assert_eq!(
            map_event(&mut state, &event),
            vec![Output::Stdout("hello".to_string())]
        );
    }

    #[test]
    fn text_tty_is_bracketed() {
        let mut state = state(true);
        let event = event(
            "message.part.updated",
            json!({"part": part("text", json!({"text": "hi", "time": {"start": 1, "end": 2}}))}),
        );
        assert_eq!(
            map_event(&mut state, &event),
            vec![
                Output::Empty,
                Output::Println("hi".to_string()),
                Output::Empty,
            ]
        );
    }

    #[test]
    fn unfinished_text_is_ignored() {
        let mut state = state(false);
        let event = event(
            "message.part.updated",
            json!({"part": part("text", json!({"text": "hi", "time": {"start": 1}}))}),
        );
        assert_eq!(map_event(&mut state, &event), Vec::<Output>::new());
    }

    #[test]
    fn empty_trimmed_text_is_ignored() {
        let mut state = state(false);
        let event = event(
            "message.part.updated",
            json!({"part": part("text", json!({"text": "  ", "time": {"start": 1, "end": 2}}))}),
        );
        assert_eq!(map_event(&mut state, &event), Vec::<Output>::new());
    }

    #[test]
    fn tool_completed_prints_inline() {
        let mut state = state(false);
        let event = event(
            "message.part.updated",
            json!({"part": part("tool", json!({
                "tool": "read",
                "state": {"status": "completed", "input": {"filePath": "a"}, "metadata": {}, "time": {"start": 1, "end": 2}},
            }))}),
        );
        let outputs = map_event(&mut state, &event);
        assert_eq!(outputs.len(), 1);
        match &outputs[0] {
            Output::Println(line) => assert!(line.contains("Read a"), "{line}"),
            other => panic!("expected inline line, got {other:?}"),
        }
    }

    #[test]
    fn tool_error_prints_failed_and_error() {
        let mut state = state(false);
        let event = event(
            "message.part.updated",
            json!({"part": part("tool", json!({
                "tool": "read",
                "state": {"status": "error", "error": "boom", "input": {"filePath": "a"}, "metadata": {}, "time": {"start": 1, "end": 2}},
            }))}),
        );
        let outputs = map_event(&mut state, &event);
        assert_eq!(outputs.len(), 2);
        assert!(matches!(&outputs[0], Output::Println(line) if line.contains("Read a failed")));
        assert_eq!(outputs[1], Output::Error("boom".to_string()));
    }

    #[test]
    fn task_running_prints_once_per_part() {
        let mut state = state(false);
        let event = event(
            "message.part.updated",
            json!({"part": part("tool", json!({
                "tool": "task",
                "state": {"status": "running", "input": {"subagent_type": "general"}, "metadata": {}, "time": {"start": 1}},
            }))}),
        );
        let first = map_event(&mut state, &event);
        assert!(!first.is_empty());
        assert_eq!(map_event(&mut state, &event), Vec::<Output>::new());
    }

    #[test]
    fn reasoning_requires_thinking() {
        let mut state = state(false);
        let reasoning = event(
            "message.part.updated",
            json!({"part": part("reasoning", json!({"text": "hmm", "time": {"start": 1, "end": 2}}))}),
        );
        assert_eq!(
            map_event(&mut state, &reasoning.clone()),
            Vec::<Output>::new()
        );
        state.thinking = true;
        assert_eq!(
            map_event(&mut state, &reasoning),
            vec![Output::Stdout("Thinking: hmm".to_string())]
        );
    }

    #[test]
    fn reasoning_tty_is_dim_italic() {
        let mut state = state(true);
        state.thinking = true;
        let event = event(
            "message.part.updated",
            json!({"part": part("reasoning", json!({"text": "hmm", "time": {"start": 1, "end": 2}}))}),
        );
        let outputs = map_event(&mut state, &event);
        assert_eq!(outputs.len(), 3);
        match &outputs[1] {
            Output::Println(line) => {
                assert!(line.contains("Thinking: hmm"), "{line}");
                assert!(line.contains("\u{1b}[3m"), "{line}");
                assert!(line.starts_with("\x1b[90m"), "{line}");
            }
            other => panic!("expected println, got {other:?}"),
        }
    }

    #[test]
    fn session_error_accumulates_with_eol() {
        let mut state = state(false);
        let event = |name: &str, message: &str| {
            event(
                "session.error",
                json!({
                    "sessionID": "ses_1",
                    "error": {"name": name, "data": {"message": message}},
                }),
            )
        };
        let outputs = map_event(&mut state, &event("A", "first"));
        assert_eq!(outputs, vec![Output::Error("first".to_string())]);
        map_event(&mut state, &event("B", "second"));
        assert_eq!(state.error.as_deref(), Some("first\nsecond"));
    }

    #[test]
    fn session_error_falls_back_to_name() {
        let mut state = state(false);
        let event = event(
            "session.error",
            json!({"sessionID": "ses_1", "error": {"name": "APIError"}}),
        );
        assert_eq!(
            map_event(&mut state, &event),
            vec![Output::Error("APIError".to_string())]
        );
    }

    #[test]
    fn idle_status_stops() {
        let mut state = state(false);
        let idle = event(
            "session.status",
            json!({"sessionID": "ses_1", "status": {"type": "idle"}}),
        );
        assert_eq!(map_event(&mut state, &idle), vec![Output::Stop]);
        let busy = event(
            "session.status",
            json!({"sessionID": "ses_1", "status": {"type": "busy"}}),
        );
        assert_eq!(map_event(&mut state, &busy), Vec::<Output>::new());
    }

    #[test]
    fn permission_auto_replies_once() {
        let mut state = state(false);
        state.auto = true;
        let event = event(
            "permission.asked",
            json!({"id": "per_1", "sessionID": "ses_1", "permission": "bash", "patterns": ["ls"]}),
        );
        assert_eq!(
            map_event(&mut state, &event),
            vec![Output::Reply {
                request_id: "per_1".to_string(),
                reply: "once",
            }]
        );
    }

    #[test]
    fn permission_rejects_with_warning() {
        let mut state = state(false);
        let event = event(
            "permission.asked",
            json!({"id": "per_1", "sessionID": "ses_1", "permission": "bash", "patterns": ["a", "b"]}),
        );
        let outputs = map_event(&mut state, &event);
        assert_eq!(
            outputs[0],
            Output::Println(
                "\x1b[93m\x1b[1m! \x1b[0mpermission requested: bash (a, b); auto-rejecting"
                    .to_string(),
            )
        );
        assert_eq!(
            outputs[1],
            Output::Reply {
                request_id: "per_1".to_string(),
                reply: "reject",
            }
        );
    }

    #[test]
    fn permission_for_subtask_session_replies() {
        let mut state = state(false);
        let created = event(
            "session.created",
            json!({"info": {"id": "ses_child", "parentID": "ses_1"}}),
        );
        map_event(&mut state, &created);
        let event = event(
            "permission.asked",
            json!({"id": "per_1", "sessionID": "ses_child", "permission": "bash", "patterns": ["x"]}),
        );
        assert_eq!(
            map_event(&mut state, &event),
            vec![
                Output::Println(
                    "\x1b[93m\x1b[1m! \x1b[0mpermission requested: bash (x); auto-rejecting"
                        .to_string(),
                ),
                Output::Reply {
                    request_id: "per_1".to_string(),
                    reply: "reject",
                },
            ]
        );
    }

    #[test]
    fn permission_for_unknown_session_is_ignored() {
        let mut state = state(false);
        let event = event(
            "permission.asked",
            json!({"id": "per_1", "sessionID": "ses_other", "permission": "bash", "patterns": ["x"]}),
        );
        assert_eq!(map_event(&mut state, &event), Vec::<Output>::new());
    }

    #[test]
    fn apply_renders_onto_ui_streams() {
        let (mut ui, captured) = crate::ui::Ui::capture(false);
        let mut replies = Vec::new();
        apply(
            &mut ui,
            &[
                Output::Println("line".to_string()),
                Output::Empty,
                Output::Stdout("data".to_string()),
                Output::Error("boom".to_string()),
                Output::Reply {
                    request_id: "per_1".to_string(),
                    reply: "once",
                },
            ],
            &mut |request, reply| replies.push((request.to_string(), reply.to_string())),
        );
        assert_eq!(captured.stdout(), "data\n");
        let stderr = captured.stderr();
        assert!(stderr.contains("line\n"), "{stderr}");
        assert!(stderr.contains("boom"), "{stderr}");
        assert_eq!(replies, vec![("per_1".to_string(), "once".to_string())]);
    }

    /// Golden formatted-mode output for a scripted event sequence
    /// (non-TTY: text goes to stdout, everything else to stderr).
    #[test]
    fn golden_formatted_output_non_tty() {
        let mut state = state(false);
        let script = vec![
            event(
                "session.created",
                json!({"info": {"id": "ses_1", "parentID": null}}),
            ),
            event(
                "message.updated",
                json!({
                    "sessionID": "ses_1",
                    "info": {"role": "assistant", "agent": "build", "modelID": "claude"},
                }),
            ),
            event(
                "message.part.updated",
                json!({"part": part("text", json!({"text": " hello", "time": {"start": 1, "end": 2}}))}),
            ),
            event(
                "message.part.updated",
                json!({"part": part("tool", json!({
                    "tool": "read",
                    // Relative: renders identically on every platform (an
                    // absolute path would gain a drive prefix on Windows).
                    "state": {"status": "completed", "input": {"filePath": "a"}, "metadata": {}, "time": {"start": 1, "end": 2}},
                }))}),
            ),
            event(
                "session.error",
                json!({"sessionID": "ses_1", "error": {"name": "APIError", "data": {"message": "provider down"}}}),
            ),
            event(
                "session.status",
                json!({"sessionID": "ses_1", "status": {"type": "idle"}}),
            ),
        ];
        let (mut ui, captured) = crate::ui::Ui::capture(false);
        let mut replies = Vec::new();
        for event in &script {
            let outputs = map_event(&mut state, event);
            let has_stop = outputs.iter().any(|o| matches!(o, Output::Stop));
            apply(&mut ui, &outputs, &mut |request, reply| {
                replies.push((request.to_string(), reply.to_string()));
            });
            if has_stop {
                break;
            }
        }
        assert_eq!(captured.stdout(), "hello\n");
        let stderr = captured.stderr();
        assert_eq!(
            stderr,
            "\u{1b}[0m\n> build · claude\n\u{1b}[0m\n\
             \u{1b}[0m→ \u{1b}[0mRead a\n\
             \u{1b}[91m\u{1b}[1mError: \u{1b}[0mprovider down\n"
        );
        assert!(replies.is_empty());
        assert_eq!(state.error.as_deref(), Some("provider down"));
    }

    /// The same script under a TTY: finished text is bracketed onto stderr.
    #[test]
    fn golden_formatted_output_tty() {
        let mut state = state(true);
        let script = vec![
            event(
                "message.updated",
                json!({
                    "sessionID": "ses_1",
                    "info": {"role": "assistant", "agent": "build", "modelID": "claude"},
                }),
            ),
            event(
                "message.part.updated",
                json!({"part": part("text", json!({"text": "hello", "time": {"start": 1, "end": 2}}))}),
            ),
            event(
                "session.status",
                json!({"sessionID": "ses_1", "status": {"type": "idle"}}),
            ),
        ];
        let (mut ui, captured) = crate::ui::Ui::capture(true);
        for event in &script {
            let outputs = map_event(&mut state, event);
            let has_stop = outputs.iter().any(|o| matches!(o, Output::Stop));
            apply(&mut ui, &outputs, &mut |_, _| {});
            if has_stop {
                break;
            }
        }
        assert_eq!(captured.stdout(), "");
        assert_eq!(
            captured.stderr(),
            // The second `empty()` before the text is deduped away (ui.ts:41-46).
            "\u{1b}[0m\n> build · claude\n\u{1b}[0m\nhello\n\u{1b}[0m\n"
        );
    }

    fn json_state() -> LoopState {
        LoopState::with_clock("ses_1".to_string(), false, false, true, false, || 1234)
    }

    #[test]
    fn json_mode_emits_text_with_envelope() {
        let mut state = json_state();
        let event = event(
            "message.part.updated",
            json!({"part": part("text", json!({"text": "hi", "time": {"start": 1, "end": 2}}))}),
        );
        assert_eq!(
            map_event(&mut state, &event),
            vec![Output::Emit(
                "{\"type\":\"text\",\"timestamp\":1234,\"sessionID\":\"ses_1\",\"part\":{\"id\":\"prt_1\",\"messageID\":\"msg_1\",\"sessionID\":\"ses_1\",\"text\":\"hi\",\"time\":{\"end\":2,\"start\":1},\"type\":\"text\"}}".to_string()
            )]
        );
    }

    /// run.ts:753-754 — `emit` runs before the trim/empty checks.
    #[test]
    fn json_mode_emits_empty_text() {
        let mut state = json_state();
        let event = event(
            "message.part.updated",
            json!({"part": part("text", json!({"text": "  ", "time": {"start": 1, "end": 2}}))}),
        );
        let outputs = map_event(&mut state, &event);
        assert!(
            matches!(outputs.first(), Some(Output::Emit(_))),
            "{outputs:?}"
        );
    }

    #[test]
    fn json_mode_emits_reasoning_only_with_thinking() {
        let mut state = json_state();
        let event = event(
            "message.part.updated",
            json!({"part": part("reasoning", json!({"text": "hmm", "time": {"start": 1, "end": 2}}))}),
        );
        assert_eq!(map_event(&mut state, &event), Vec::<Output>::new());
        state.thinking = true;
        assert!(matches!(
            map_event(&mut state, &event).first(),
            Some(Output::Emit(line)) if line.contains("\"type\":\"reasoning\"")
        ));
    }

    #[test]
    fn json_mode_emits_tool_use_and_skips_inline() {
        let mut state = json_state();
        let event = event(
            "message.part.updated",
            json!({"part": part("tool", json!({
                "tool": "read",
                "state": {"status": "completed", "input": {"filePath": "/a"}, "metadata": {}, "time": {"start": 1, "end": 2}},
            }))}),
        );
        let outputs = map_event(&mut state, &event);
        assert_eq!(outputs.len(), 1);
        match &outputs[0] {
            Output::Emit(line) => {
                assert!(line.starts_with("{\"type\":\"tool_use\",\"timestamp\":1234,\"sessionID\":\"ses_1\",\"part\":"), "{line}");
                assert!(line.contains("\"tool\":\"read\""), "{line}");
            }
            other => panic!("expected emit, got {other:?}"),
        }
    }

    #[test]
    fn json_mode_emits_step_parts() {
        let mut state = json_state();
        let step_start = event(
            "message.part.updated",
            json!({"part": part("step-start", json!({"time": {"start": 1}}))}),
        );
        let outputs = map_event(&mut state, &step_start);
        match &outputs[0] {
            Output::Emit(line) => assert!(line.contains("\"type\":\"step_start\""), "{line}"),
            other => panic!("expected emit, got {other:?}"),
        }
        let step_finish = event(
            "message.part.updated",
            json!({"part": part("step-finish", json!({"time": {"start": 1, "end": 2}}))}),
        );
        let outputs = map_event(&mut state, &step_finish);
        match &outputs[0] {
            Output::Emit(line) => assert!(line.contains("\"type\":\"step_finish\""), "{line}"),
            other => panic!("expected emit, got {other:?}"),
        }
    }

    #[test]
    fn json_mode_emits_session_error_and_accumulates() {
        let mut state = json_state();
        let event = event(
            "session.error",
            json!({
                "sessionID": "ses_1",
                "error": {"name": "APIError", "data": {"message": "provider down"}},
            }),
        );
        assert_eq!(
            map_event(&mut state, &event),
            vec![Output::Emit(
                "{\"type\":\"error\",\"timestamp\":1234,\"sessionID\":\"ses_1\",\"error\":{\"data\":{\"message\":\"provider down\"},\"name\":\"APIError\"}}".to_string()
            )]
        );
        assert_eq!(state.error.as_deref(), Some("provider down"));
    }

    /// run.ts:714-718 — the `> agent · model` header is format-only; the
    /// `task` running inline is too (run.ts:735).
    #[test]
    fn json_mode_suppresses_header_and_task_running() {
        let mut state = json_state();
        let header = event(
            "message.updated",
            json!({
                "sessionID": "ses_1",
                "info": {"role": "assistant", "agent": "build", "modelID": "claude"},
            }),
        );
        assert_eq!(map_event(&mut state, &header), Vec::<Output>::new());
        let task = event(
            "message.part.updated",
            json!({"part": part("tool", json!({
                "tool": "task",
                "state": {"status": "running", "input": {"subagent_type": "general"}, "metadata": {}, "time": {"start": 1}},
            }))}),
        );
        assert_eq!(map_event(&mut state, &task), Vec::<Output>::new());
    }

    /// run.ts:678-691 — stdout + EOL, one compact JSON object per line.
    #[test]
    fn apply_writes_json_events_to_stdout_with_eol() {
        let mut state = json_state();
        let event = event(
            "message.part.updated",
            json!({"part": part("text", json!({"text": "hi", "time": {"start": 1, "end": 2}}))}),
        );
        let (mut ui, captured) = crate::ui::Ui::capture(false);
        let outputs = map_event(&mut state, &event);
        apply(&mut ui, &outputs, &mut |_, _| {});
        assert_eq!(
            captured.stdout(),
            "{\"type\":\"text\",\"timestamp\":1234,\"sessionID\":\"ses_1\",\"part\":{\"id\":\"prt_1\",\"messageID\":\"msg_1\",\"sessionID\":\"ses_1\",\"text\":\"hi\",\"time\":{\"end\":2,\"start\":1},\"type\":\"text\"}}\n"
        );
        assert_eq!(captured.stderr(), "");
    }
}

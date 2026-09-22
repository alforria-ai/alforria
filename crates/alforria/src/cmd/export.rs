//! cli/cmd/export.ts port — the `export` command: session JSON to stdout
//! plus the `--sanitize` redaction walker (export.ts:11-220).

use alforria_core::ListInput;
use clap::ArgMatches;
use serde_json::{json, Value};

use crate::error::{CliError, TypedError};
use crate::ui::Ui;

/// `redact` (export.ts:11-13): empty (whitespace-only) values pass through.
fn redact(kind: &str, id: &str, value: &str) -> String {
    if value.trim().is_empty() {
        value.to_string()
    } else {
        format!("[redacted:{kind}:{id}]")
    }
}

/// `data` (export.ts:15-18): undefined/empty objects pass through.
fn redact_data(kind: &str, id: &str, value: Option<&Value>) -> Option<Value> {
    match value {
        None => None,
        Some(Value::Object(map)) => {
            if map.is_empty() {
                Some(Value::Object(map.clone()))
            } else {
                Some(json!({ "redacted": format!("{kind}:{id}") }))
            }
        }
        Some(other) => Some(other.clone()),
    }
}

fn set_or_remove(object: &mut serde_json::Map<String, Value>, key: &str, value: Option<Value>) {
    match value {
        Some(value) => {
            object.insert(key.to_string(), value);
        }
        None => {
            object.remove(key);
        }
    }
}

/// `span` (export.ts:20-25).
fn redact_span(id: &str, value: &Value) -> Value {
    let mut span = value.clone();
    if let Some(map) = span.as_object_mut() {
        if let Some(text) = map.get("value").and_then(Value::as_str) {
            let redacted = redact("file-text", id, text);
            map.insert("value".to_string(), Value::String(redacted));
        }
    }
    span
}

/// `diff` (export.ts:27-33).
fn redact_diffs(kind: &str, diffs: &Value) -> Value {
    let Value::Array(items) = diffs else {
        return diffs.clone();
    };
    Value::Array(
        items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let mut item = item.clone();
                if let Some(map) = item.as_object_mut() {
                    for field in ["file", "patch"] {
                        match map.get(field) {
                            Some(value) if !value.is_null() => {
                                let text = value.as_str().unwrap_or_default();
                                map.insert(
                                    field.to_string(),
                                    Value::String(redact(
                                        &format!("{kind}-{field}"),
                                        &index.to_string(),
                                        text,
                                    )),
                                );
                            }
                            _ => {
                                map.remove(field);
                            }
                        }
                    }
                }
                item
            })
            .collect(),
    )
}

/// `source` (export.ts:35-58).
fn sanitize_source(part_id: &str, source: Option<&Value>) -> Option<Value> {
    let source = source?;
    let mut out = source.clone();
    let object = out.as_object_mut()?;
    match object.get("type").and_then(Value::as_str) {
        Some("symbol") => {
            if let Some(path) = object.get("path").and_then(Value::as_str) {
                object.insert(
                    "path".to_string(),
                    Value::String(redact("file-path", part_id, path)),
                );
            }
            if let Some(name) = object.get("name").and_then(Value::as_str) {
                object.insert(
                    "name".to_string(),
                    Value::String(redact("file-symbol", part_id, name)),
                );
            }
            if let Some(text) = object.get("text") {
                let span = redact_span(part_id, text);
                object.insert("text".to_string(), span);
            }
        }
        Some("resource") => {
            if let Some(client) = object.get("clientName").and_then(Value::as_str) {
                object.insert(
                    "clientName".to_string(),
                    Value::String(redact("file-client", part_id, client)),
                );
            }
            if let Some(uri) = object.get("uri").and_then(Value::as_str) {
                object.insert(
                    "uri".to_string(),
                    Value::String(redact("file-uri", part_id, uri)),
                );
            }
            if let Some(text) = object.get("text") {
                let span = redact_span(part_id, text);
                object.insert("text".to_string(), span);
            }
        }
        _ => {
            if let Some(path) = object.get("path").and_then(Value::as_str) {
                object.insert(
                    "path".to_string(),
                    Value::String(redact("file-path", part_id, path)),
                );
            }
            if let Some(text) = object.get("text") {
                let span = redact_span(part_id, text);
                object.insert("text".to_string(), span);
            }
        }
    }
    Some(out)
}

/// `filepart` (export.ts:60-67).
fn sanitize_file_part(part: &Value) -> Value {
    let mut out = part.clone();
    let Some(object) = out.as_object_mut() else {
        return out;
    };
    let id = part.get("id").and_then(Value::as_str).unwrap_or_default();
    if let Some(url) = object.get("url").and_then(Value::as_str) {
        object.insert(
            "url".to_string(),
            Value::String(redact("file-url", id, url)),
        );
    }
    match object.get("filename") {
        Some(Value::Null) | None => {
            object.remove("filename");
        }
        Some(value) => {
            let filename = value.as_str().unwrap_or_default();
            object.insert(
                "filename".to_string(),
                Value::String(redact("file-name", id, filename)),
            );
        }
    }
    let source = object
        .get("source")
        .filter(|value| !value.is_null())
        .cloned();
    let sanitized = sanitize_source(id, source.as_ref());
    set_or_remove(object, "source", sanitized);
    Value::Object(object.clone())
}

/// `part` (export.ts:69-158) — the per-type redaction walker.
fn sanitize_part(part: &Value) -> Value {
    let mut out = part.clone();
    let Some(object) = out.as_object_mut() else {
        return out;
    };
    let id = part.get("id").and_then(Value::as_str).unwrap_or_default();
    match object.get("type").and_then(Value::as_str) {
        Some("text") => {
            if let Some(text) = object.get("text").and_then(Value::as_str) {
                object.insert("text".to_string(), Value::String(redact("text", id, text)));
            }
            let metadata = object.get("metadata").cloned();
            let sanitized = redact_data("text-metadata", id, metadata.as_ref());
            set_or_remove(object, "metadata", sanitized);
        }
        Some("reasoning") => {
            if let Some(text) = object.get("text").and_then(Value::as_str) {
                object.insert(
                    "text".to_string(),
                    Value::String(redact("reasoning", id, text)),
                );
            }
            let metadata = object.get("metadata").cloned();
            let sanitized = redact_data("reasoning-metadata", id, metadata.as_ref());
            set_or_remove(object, "metadata", sanitized);
        }
        Some("file") => return sanitize_file_part(part),
        Some("subtask") => {
            if let Some(prompt) = object.get("prompt").and_then(Value::as_str) {
                object.insert(
                    "prompt".to_string(),
                    Value::String(redact("subtask-prompt", id, prompt)),
                );
            }
            if let Some(description) = object.get("description").and_then(Value::as_str) {
                object.insert(
                    "description".to_string(),
                    Value::String(redact("subtask-description", id, description)),
                );
            }
            match object.get("command") {
                Some(Value::Null) | None => {
                    object.remove("command");
                }
                Some(value) => {
                    let command = value.as_str().unwrap_or_default();
                    object.insert(
                        "command".to_string(),
                        Value::String(redact("subtask-command", id, command)),
                    );
                }
            }
        }
        Some("tool") => {
            let metadata = object.get("metadata").cloned();
            let sanitized = redact_data("tool-metadata", id, metadata.as_ref());
            set_or_remove(object, "metadata", sanitized);
            if let Some(state) = object.get("state").cloned() {
                object.insert("state".to_string(), sanitize_tool_state(id, &state));
            }
        }
        Some("patch") => {
            if let Some(hash) = object.get("hash").and_then(Value::as_str) {
                object.insert("hash".to_string(), Value::String(redact("patch", id, hash)));
            }
            if let Some(files) = object.get("files").and_then(Value::as_array) {
                let files: Vec<Value> = files
                    .iter()
                    .enumerate()
                    .map(|(index, file)| {
                        Value::String(redact(
                            "patch-file",
                            &format!("{id}-{index}"),
                            file.as_str().unwrap_or_default(),
                        ))
                    })
                    .collect();
                object.insert("files".to_string(), Value::Array(files));
            }
        }
        Some("snapshot") => {
            if let Some(snapshot) = object.get("snapshot").and_then(Value::as_str) {
                object.insert(
                    "snapshot".to_string(),
                    Value::String(redact("snapshot", id, snapshot)),
                );
            }
        }
        Some("step-start") | Some("step-finish") => match object.get("snapshot") {
            Some(Value::Null) | None => {
                object.remove("snapshot");
            }
            Some(value) => {
                let snapshot = value.as_str().unwrap_or_default();
                object.insert(
                    "snapshot".to_string(),
                    Value::String(redact("snapshot", id, snapshot)),
                );
            }
        },
        Some("agent") => {
            if let Some(source) = object.get("source").filter(|value| !value.is_null()) {
                let mut source = source.clone();
                if let Some(source_object) = source.as_object_mut() {
                    if let Some(value) = source_object.get("value").and_then(Value::as_str) {
                        source_object.insert(
                            "value".to_string(),
                            Value::String(redact("agent-source", id, value)),
                        );
                    }
                }
                object.insert("source".to_string(), source);
            }
        }
        _ => {}
    }
    Value::Object(object.clone())
}

/// The `tool` part `state` leg (export.ts:94-124).
fn sanitize_tool_state(part_id: &str, state: &Value) -> Value {
    let mut out = state.clone();
    let Some(object) = out.as_object_mut() else {
        return out;
    };
    let status = object.get("status").and_then(Value::as_str);
    match status {
        Some("pending") => {
            let input = object.get("input").cloned();
            let sanitized = redact_data("tool-input", part_id, input.as_ref());
            set_or_remove(object, "input", sanitized);
            if let Some(raw) = object.get("raw").and_then(Value::as_str) {
                object.insert(
                    "raw".to_string(),
                    Value::String(redact("tool-raw", part_id, raw)),
                );
            }
        }
        Some("running") => {
            let input = object.get("input").cloned();
            let sanitized = redact_data("tool-input", part_id, input.as_ref());
            set_or_remove(object, "input", sanitized);
            if let Some(title) = object.get("title") {
                let title = title.as_str().unwrap_or_default();
                object.insert(
                    "title".to_string(),
                    Value::String(redact("tool-title", part_id, title)),
                );
            }
            let metadata = object.get("metadata").cloned();
            let sanitized = redact_data("tool-state-metadata", part_id, metadata.as_ref());
            set_or_remove(object, "metadata", sanitized);
        }
        Some("completed") => {
            let input = object.get("input").cloned();
            let sanitized = redact_data("tool-input", part_id, input.as_ref());
            set_or_remove(object, "input", sanitized);
            if let Some(output) = object.get("output").and_then(Value::as_str) {
                object.insert(
                    "output".to_string(),
                    Value::String(redact("tool-output", part_id, output)),
                );
            }
            if let Some(title) = object.get("title").and_then(Value::as_str) {
                object.insert(
                    "title".to_string(),
                    Value::String(redact("tool-title", part_id, title)),
                );
            }
            let metadata = object.get("metadata").cloned();
            let sanitized = redact_data("tool-state-metadata", part_id, metadata.as_ref());
            set_or_remove(object, "metadata", sanitized);
            if let Some(attachments) = object.get("attachments").and_then(Value::as_array) {
                let attachments: Vec<Value> = attachments.iter().map(sanitize_file_part).collect();
                object.insert("attachments".to_string(), Value::Array(attachments));
            }
        }
        _ => {
            let input = object.get("input").cloned();
            let sanitized = redact_data("tool-input", part_id, input.as_ref());
            set_or_remove(object, "input", sanitized);
            let metadata = object.get("metadata").cloned();
            let sanitized = redact_data("tool-state-metadata", part_id, metadata.as_ref());
            set_or_remove(object, "metadata", sanitized);
        }
    }
    Value::Object(object.clone())
}

/// `sanitize` (export.ts:163-220) over the serialized
/// `{ info, messages }` export document.
pub fn sanitize(data: &Value) -> Value {
    let mut out = data.clone();
    if let Some(document) = out.get_mut("info").and_then(|info| info.as_object_mut()) {
        let id = document
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if let Some(title) = document.get("title").and_then(Value::as_str) {
            document.insert(
                "title".to_string(),
                Value::String(redact("session-title", &id, title)),
            );
        }
        if let Some(directory) = document.get("directory").and_then(Value::as_str) {
            document.insert(
                "directory".to_string(),
                Value::String(redact("session-directory", &id, directory)),
            );
        }
        if let Some(summary) = document.get_mut("summary").filter(|value| !value.is_null()) {
            if let Some(summary) = summary.as_object_mut() {
                if let Some(diffs) = summary.get("diffs") {
                    let redacted = redact_diffs("session-diff", diffs);
                    summary.insert("diffs".to_string(), redacted);
                }
            }
        }
        if let Some(revert) = document.get_mut("revert").filter(|value| !value.is_null()) {
            let revert = revert.as_object_mut().expect("checked non-null");
            match revert.get("snapshot") {
                Some(Value::Null) | None => {
                    revert.remove("snapshot");
                }
                Some(value) => {
                    let snapshot = value.as_str().unwrap_or_default();
                    revert.insert(
                        "snapshot".to_string(),
                        Value::String(redact("revert-snapshot", &id, snapshot)),
                    );
                }
            }
            match revert.get("diff") {
                Some(Value::Null) | None => {
                    revert.remove("diff");
                }
                Some(value) => {
                    let diff = value.as_str().unwrap_or_default();
                    revert.insert(
                        "diff".to_string(),
                        Value::String(redact("revert-diff", &id, diff)),
                    );
                }
            }
        }
    }
    if let Some(messages) = out
        .get_mut("messages")
        .and_then(|value| value.as_array_mut())
    {
        for message in messages.iter_mut() {
            sanitize_message(message);
        }
    }
    out
}

fn sanitize_message(message: &mut Value) {
    let Some(message_object) = message.as_object_mut() else {
        return;
    };
    let id = message_object
        .get("info")
        .and_then(|info| info.get("id"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let role = message_object
        .get("info")
        .and_then(|info| info.get("role"))
        .and_then(Value::as_str);
    if role == Some("user") {
        match message_object
            .get("info")
            .and_then(|info| info.get("system"))
            .filter(|value| !value.is_null())
            .and_then(Value::as_str)
        {
            Some(system) => {
                let redacted = redact("system", &id, system);
                if let Some(info) = message_object
                    .get_mut("info")
                    .and_then(|info| info.as_object_mut())
                {
                    info.insert("system".to_string(), Value::String(redacted));
                }
            }
            None => {
                if let Some(info) = message_object
                    .get_mut("info")
                    .and_then(|info| info.as_object_mut())
                {
                    info.remove("system");
                }
            }
        }
        if let Some(summary) = message_object
            .get_mut("info")
            .and_then(|info| info.as_object_mut())
            .and_then(|info| info.get_mut("summary"))
            .filter(|value| !value.is_null())
            .and_then(|value| value.as_object_mut())
        {
            if let Some(title) = summary.get("title") {
                let title = title.as_str().unwrap_or_default();
                summary.insert(
                    "title".to_string(),
                    Value::String(redact("summary-title", &id, title)),
                );
            }
            if let Some(body) = summary.get("body") {
                let body = body.as_str().unwrap_or_default();
                summary.insert(
                    "body".to_string(),
                    Value::String(redact("summary-body", &id, body)),
                );
            }
            if let Some(diffs) = summary.get("diffs") {
                let redacted = redact_diffs("message-diff", diffs);
                summary.insert("diffs".to_string(), redacted);
            }
        }
    } else if let Some(path) = message_object
        .get_mut("info")
        .and_then(|info| info.as_object_mut())
        .and_then(|info| info.get_mut("path"))
        .and_then(|value| value.as_object_mut())
    {
        if let Some(cwd) = path.get("cwd") {
            let redacted = redact("cwd", &id, cwd.as_str().unwrap_or_default());
            path.insert("cwd".to_string(), Value::String(redacted));
        }
        if let Some(root) = path.get("root") {
            let redacted = redact("root", &id, root.as_str().unwrap_or_default());
            path.insert("root".to_string(), Value::String(redacted));
        }
    }
    if let Some(parts) = message_object
        .get_mut("parts")
        .and_then(|value| value.as_array_mut())
    {
        for part in parts.iter_mut() {
            *part = sanitize_part(part);
        }
    }
}

/// The export document: `{ info, messages: [{ info, parts }] }` (export.ts:287).
pub fn export_document(
    info: &alforria_schema::session_v1::V1SessionInfo,
    messages: &[alforria_core::WithParts],
) -> Value {
    json!({
        "info": info,
        "messages": messages
            .iter()
            .map(|message| json!({ "info": message.info, "parts": message.parts }))
            .collect::<Vec<_>>(),
    })
}

pub fn run(matches: &ArgMatches, ui: &mut Ui) -> Result<(), TypedError> {
    let session_id = matches
        .get_one::<String>("sessionID")
        .map(String::as_str)
        .filter(|value| !value.is_empty());
    let sanitize_flag = matches.get_flag("sanitize");
    ui.write_stderr(&format!(
        "Exporting session: {}\n",
        session_id.unwrap_or("latest")
    ));
    let instance = crate::instance::boot(None)?;
    let sessions = &instance.services.sessions;
    let resolved = match session_id {
        Some(id) => Some(id.to_string()),
        None => {
            let context = instance
                .services
                .instance_context(&instance.directory, None)
                .map_err(|err| TypedError::Cli(CliError::new(err.to_string())))?;
            let mut roots = sessions
                .list(
                    &context,
                    &ListInput {
                        roots: true,
                        ..ListInput::default()
                    },
                )
                .map_err(|err| TypedError::Cli(CliError::new(err.to_string())))?;
            if roots.is_empty() {
                ui.write_stderr("No sessions found\n");
                ui.write_stderr("Done\n");
                return Ok(());
            }
            roots.sort_by_key(|entry| std::cmp::Reverse(entry.time.updated));
            Some(roots.swap_remove(0).id)
        }
    };
    let resolved = resolved.expect("resolved above");
    let info = sessions
        .get(&resolved)
        .map_err(|_| TypedError::Cli(CliError::new(format!("Session not found: {resolved}"))))?;
    let messages = sessions
        .messages(&resolved, None)
        .map_err(|err| TypedError::Cli(CliError::new(err.to_string())))?;
    let mut document = export_document(&info, &messages);
    if sanitize_flag {
        document = sanitize(&document);
    }
    let json = serde_json::to_string_pretty(&document).unwrap_or_default();
    ui.write_stdout(&format!("{json}\n"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_passes_blank_through() {
        assert_eq!(redact("text", "prt_1", "hello"), "[redacted:text:prt_1]");
        assert_eq!(redact("text", "prt_1", ""), "");
        assert_eq!(redact("text", "prt_1", "   "), "   ");
    }

    #[test]
    fn sanitize_redacts_top_level_and_parts() {
        let data = json!({
            "info": {
                "id": "ses_1",
                "title": "secret title",
                "directory": "/home/me/secret",
            },
            "messages": [
                {
                    "info": {
                        "id": "msg_1",
                        "role": "user",
                        "system": "be nice",
                    },
                    "parts": [
                        {"id": "prt_1", "type": "text", "text": "hello world"}
                    ],
                },
                {
                    "info": {
                        "id": "msg_2",
                        "role": "assistant",
                        "path": {"cwd": "/repo", "root": "/repo"},
                    },
                    "parts": [
                        {
                            "id": "prt_2",
                            "type": "tool",
                            "metadata": {"hint": "x"},
                            "state": {
                                "status": "completed",
                                "input": {"cmd": "ls"},
                                "output": "file-a file-b",
                                "title": "bash",
                                "metadata": {},
                            },
                        },
                    ],
                },
            ],
        });
        let sanitized = sanitize(&data);
        assert_eq!(sanitized["info"]["title"], "[redacted:session-title:ses_1]");
        assert_eq!(
            sanitized["info"]["directory"],
            "[redacted:session-directory:ses_1]"
        );
        assert_eq!(
            sanitized["messages"][0]["info"]["system"],
            "[redacted:system:msg_1]"
        );
        assert_eq!(
            sanitized["messages"][0]["parts"][0]["text"],
            "[redacted:text:prt_1]"
        );
        assert_eq!(
            sanitized["messages"][1]["info"]["path"]["cwd"],
            "[redacted:cwd:msg_2]"
        );
        assert_eq!(
            sanitized["messages"][1]["parts"][0]["metadata"],
            json!({ "redacted": "tool-metadata:prt_2" })
        );
        let state = &sanitized["messages"][1]["parts"][0]["state"];
        assert_eq!(state["input"], json!({ "redacted": "tool-input:prt_2" }));
        assert_eq!(state["output"], "[redacted:tool-output:prt_2]");
        assert_eq!(state["title"], "[redacted:tool-title:prt_2]");
        // Empty metadata objects pass through untouched.
        assert_eq!(state["metadata"], json!({}));
    }

    #[test]
    fn sanitize_handles_file_subtask_and_patch_parts() {
        let data = json!({
            "info": {"id": "ses_1", "title": "t"},
            "messages": [
                {
                    "info": {"id": "msg_1", "role": "assistant"},
                    "parts": [
                        {
                            "id": "prt_1",
                            "type": "file",
                            "url": "file:///tmp/secret.txt",
                            "filename": "secret.txt",
                            "source": {
                                "type": "symbol",
                                "path": "/repo/src/lib.rs",
                                "name": "main",
                                "text": {"value": "fn main() {}", "start": 1.0, "end": 2.0},
                            },
                        },
                        {
                            "id": "prt_2",
                            "type": "subtask",
                            "prompt": "do the thing",
                            "description": "thing",
                            "command": "bash",
                        },
                        {
                            "id": "prt_3",
                            "type": "patch",
                            "hash": "abc123",
                            "files": ["/repo/a.txt", "/repo/b.txt"],
                        },
                        {
                            "id": "prt_4",
                            "type": "agent",
                            "source": {"value": "general"},
                        },
                    ],
                },
            ],
        });
        let sanitized = sanitize(&data);
        let parts = &sanitized["messages"][0]["parts"];
        assert_eq!(parts[0]["url"], "[redacted:file-url:prt_1]");
        assert_eq!(parts[0]["filename"], "[redacted:file-name:prt_1]");
        let source = &parts[0]["source"];
        assert_eq!(source["path"], "[redacted:file-path:prt_1]");
        assert_eq!(source["name"], "[redacted:file-symbol:prt_1]");
        assert_eq!(source["text"]["value"], "[redacted:file-text:prt_1]");
        assert_eq!(parts[1]["prompt"], "[redacted:subtask-prompt:prt_2]");
        assert_eq!(
            parts[1]["description"],
            "[redacted:subtask-description:prt_2]"
        );
        assert_eq!(parts[1]["command"], "[redacted:subtask-command:prt_2]");
        assert_eq!(parts[2]["hash"], "[redacted:patch:prt_3]");
        assert_eq!(parts[2]["files"][0], "[redacted:patch-file:prt_3-0]");
        assert_eq!(parts[2]["files"][1], "[redacted:patch-file:prt_3-1]");
        assert_eq!(parts[3]["source"]["value"], "[redacted:agent-source:prt_4]");
    }

    #[test]
    fn sanitize_spares_summary_and_empty_metadata() {
        let data = json!({
            "info": {
                "id": "ses_1",
                "title": "t",
                "summary": {
                    "additions": 3,
                    "deletions": 1,
                    "files": 2,
                    "diffs": [{"file": "/a", "patch": "--- /a"}],
                },
            },
            "messages": [
                {
                    "info": {"id": "msg_1", "role": "user"},
                    "parts": [
                        {
                            "id": "prt_1",
                            "type": "text",
                            "text": "hi",
                            "metadata": {},
                        }
                    ],
                }
            ],
        });
        let sanitized = sanitize(&data);
        assert_eq!(
            sanitized["info"]["summary"]["diffs"][0]["file"],
            "[redacted:session-diff-file:0]"
        );
        assert_eq!(
            sanitized["info"]["summary"]["diffs"][0]["patch"],
            "[redacted:session-diff-patch:0]"
        );
        assert_eq!(sanitized["messages"][0]["parts"][0]["metadata"], json!({}));
    }
}

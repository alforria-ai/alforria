//! Tool call/update translation (`acp/tool.ts`).

use serde_json::{json, Value};

pub fn string_field(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_string)
}

pub fn string_of(value: &Value, key: &str) -> Option<String> {
    string_field(value.get(key))
}

/// `toToolKind` (tool.ts:38-71).
pub fn to_tool_kind(tool_name: &str) -> &'static str {
    match tool_name.to_ascii_lowercase().as_str() {
        "bash" | "shell" => "execute",
        "webfetch" => "fetch",
        "edit" | "apply_patch" | "patch" | "write" => "edit",
        "grep"
        | "glob"
        | "context"
        | "context7_resolve_library_id"
        | "context7_get_library_docs" => "search",
        "read" => "read",
        "task" => "think",
        _ => "other",
    }
}

/// `toLocations` (tool.ts:73-101).
pub fn to_locations(tool_name: &str, input: &Value, cwd: Option<&str>) -> Vec<Value> {
    match tool_name.to_ascii_lowercase().as_str() {
        "bash" | "shell" => shell_workdir(input, cwd)
            .map(|workdir| vec![json!({ "path": workdir })])
            .unwrap_or_default(),
        "read" | "edit" | "write" => {
            location_from(&[input.get("filePath").or_else(|| input.get("filepath"))])
        }
        "external_directory" => location_from(&[
            input.get("filePath").or_else(|| input.get("filepath")),
            input.get("parentDir"),
            input.get("directories"),
        ]),
        "grep"
        | "glob"
        | "context"
        | "context7_resolve_library_id"
        | "context7_get_library_docs" => location_from(&[input.get("path")]),
        _ => Vec::new(),
    }
}

/// `locationFrom` (tool.ts:310-323): dedup, non-empty strings only.
fn location_from(values: &[Option<&Value>]) -> Vec<Value> {
    let mut paths: Vec<String> = Vec::new();
    for value in values {
        match value {
            Some(Value::Array(items)) => {
                for item in items {
                    if let Some(text) = item.as_str() {
                        if !text.is_empty() {
                            paths.push(text.to_string());
                        }
                    }
                }
            }
            Some(Value::String(text)) if !text.is_empty() => paths.push(text.clone()),
            _ => {}
        }
    }
    // `Array.from(new Set(...))` (tool.ts:310-321) — global dedup, not
    // just adjacent repeats.
    let mut seen = std::collections::HashSet::new();
    paths.retain(|path| seen.insert(path.clone()));
    paths
        .into_iter()
        .map(|path| json!({ "path": path }))
        .collect()
}

/// `pendingToolCall` (tool.ts:124-138).
pub fn pending_tool_call(
    tool_call_id: &str,
    tool_name: &str,
    input: &Value,
    title: Option<&str>,
    cwd: Option<&str>,
) -> Value {
    json!({
        "toolCallId": tool_call_id,
        "title": tool_title(tool_name, input, title),
        "kind": to_tool_kind(tool_name),
        "status": "pending",
        "locations": to_locations(tool_name, input, cwd),
        "rawInput": raw_input(tool_name, input, cwd),
    })
}

/// `runningToolUpdate` (tool.ts:140-168).
pub fn running_tool_update(
    tool_call_id: &str,
    tool_name: &str,
    input: &Value,
    state_title: Option<&str>,
    output: Option<&str>,
    cwd: Option<&str>,
) -> Value {
    let mut update =
        duplicate_running_tool_update(tool_call_id, tool_name, input, state_title, cwd);
    update["status"] = json!("in_progress");
    if let Some(output) = output {
        update["content"] = json!([{
            "type": "content",
            "content": { "type": "text", "text": output },
        }]);
    }
    update
}

/// `duplicateRunningToolUpdate` (tool.ts:170-184) — no content.
pub fn duplicate_running_tool_update(
    tool_call_id: &str,
    tool_name: &str,
    input: &Value,
    state_title: Option<&str>,
    cwd: Option<&str>,
) -> Value {
    json!({
        "toolCallId": tool_call_id,
        "status": "in_progress",
        "kind": to_tool_kind(tool_name),
        "title": tool_title(tool_name, input, state_title),
        "locations": to_locations(tool_name, input, cwd),
        "rawInput": raw_input(tool_name, input, cwd),
    })
}

/// `completedToolUpdate` (tool.ts:186-199).
pub fn completed_tool_update(
    tool_call_id: &str,
    tool_name: &str,
    input: &Value,
    state: &Value,
    state_title: Option<&str>,
    cwd: Option<&str>,
) -> Value {
    let _ = (input, cwd);
    let mut update = json!({
        "toolCallId": tool_call_id,
        "status": "completed",
        "content": completed_tool_content(tool_name, state),
        "rawOutput": completed_tool_raw_output(state),
    });
    if let Some(title) = state_title {
        update["title"] = json!(title);
    }
    update
}

/// `errorToolUpdate` (tool.ts:201-228).
pub fn error_tool_update(
    tool_call_id: &str,
    tool_name: &str,
    input: &Value,
    error: &str,
    state: &Value,
    cwd: Option<&str>,
) -> Value {
    json!({
        "toolCallId": tool_call_id,
        "status": "failed",
        "kind": to_tool_kind(tool_name),
        "title": tool_title(tool_name, input, None),
        "locations": to_locations(tool_name, input, cwd),
        "rawInput": raw_input(tool_name, input, cwd),
        "content": [{
            "type": "content",
            "content": { "type": "text", "text": error },
        }],
        "rawOutput": error_tool_raw_output(state),
    })
}

/// The error variant of `rawOutput` (tool.ts:222-227) — metadata is
/// `undefined`-dropped, not null.
fn error_tool_raw_output(state: &Value) -> Value {
    let mut raw_output = json!({
        "error": state.get("error").cloned().unwrap_or(Value::Null),
    });
    if let Some(metadata) = state.get("metadata") {
        raw_output["metadata"] = metadata.clone();
    }
    raw_output
}

/// `completedToolRawOutput` (tool.ts:230-236).
pub fn completed_tool_raw_output(state: &Value) -> Value {
    let mut raw_output = json!({ "output": state.get("output").cloned().unwrap_or(Value::Null) });
    if let Some(metadata) = state.get("metadata") {
        raw_output["metadata"] = metadata.clone();
    }
    if let Some(attachments) = state
        .get("attachments")
        .filter(|attachments| attachments.as_array().is_some_and(|a| !a.is_empty()))
    {
        raw_output["attachments"] = attachments.clone();
    }
    raw_output
}

/// `completedToolContent` (tool.ts:103-122).
pub fn completed_tool_content(tool_name: &str, state: &Value) -> Vec<Value> {
    let output = state.get("output").and_then(Value::as_str).unwrap_or("");
    let text = if tool_name.eq_ignore_ascii_case("read") {
        read_display_text(state.get("metadata")).unwrap_or_else(|| output.to_string())
    } else {
        output.to_string()
    };
    let mut content = vec![json!({
        "type": "content",
        "content": { "type": "text", "text": text },
    })];
    if to_tool_kind(tool_name) == "edit" {
        content.extend(diff_content(state.get("input")));
    }
    content.extend(image_contents(state.get("attachments")));
    content
}

/// `imageContents` + `dataUrlImage` (tool.ts:238-260, 352-360).
pub fn image_contents(attachments: Option<&Value>) -> Vec<Value> {
    let Some(attachments) = attachments.and_then(Value::as_array) else {
        return Vec::new();
    };
    attachments
        .iter()
        .filter_map(data_url_image)
        .map(|(mime, data)| {
            json!({
                "type": "content",
                "content": { "type": "image", "mimeType": mime, "data": data },
            })
        })
        .collect()
}

fn data_url_image(attachment: &Value) -> Option<(String, String)> {
    let url = attachment.get("url").and_then(Value::as_str)?;
    let captures = url_regex_captures(url)?;
    let mime = captures
        .mime
        .filter(|mime| mime.starts_with("image/"))
        .or_else(|| string_of(attachment, "mime").filter(|mime| mime.starts_with("image/")))?;
    let data = captures.data?;
    Some((mime, data))
}

struct UrlCaptures {
    mime: Option<String>,
    data: Option<String>,
}

fn url_regex_captures(url: &str) -> Option<UrlCaptures> {
    // `/^data:([^;,]+)(?:;[^,]*)*;base64,(.*)$/` (tool.ts:353) — optional
    // parameter segments sit between the mime and the `;base64,` payload.
    let rest = url.strip_prefix("data:")?;
    let mime_end = rest.find([';', ','])?;
    let mime = &rest[..mime_end];
    let mut after = &rest[mime_end..];
    loop {
        if let Some(data) = after.strip_prefix(";base64,") {
            return Some(UrlCaptures {
                mime: Some(mime.to_string()),
                data: Some(data.to_string()),
            });
        }
        let offset = after[1..].find(';')?;
        after = &after[offset + 1..];
    }
}

/// `shellOutputSnapshot` (tool.ts:258-261).
pub fn shell_output_snapshot(state: &Value) -> Option<String> {
    state
        .get("metadata")
        .and_then(|metadata| string_of(metadata, "output"))
}

/// `toolTitle` (tool.ts:265-268): shell tools surface the command.
fn tool_title(tool_name: &str, input: &Value, fallback: Option<&str>) -> String {
    let fallback = fallback.unwrap_or_default();
    if is_shell(tool_name) {
        shell_command(input)
            .or_else(|| Some(fallback.to_string()))
            .unwrap_or_else(|| tool_name.to_string())
    } else if !fallback.is_empty() {
        fallback.to_string()
    } else {
        tool_name.to_string()
    }
}

/// `rawInput` (tool.ts:272-277): enrich shell input with the workdir.
fn raw_input(tool_name: &str, input: &Value, cwd: Option<&str>) -> Value {
    if !is_shell(tool_name) {
        return input.clone();
    }
    if input.get("cwd").is_some() || input.get("workdir").is_some() {
        return input.clone();
    }
    let Some(workdir) = shell_workdir(input, cwd) else {
        return input.clone();
    };
    let mut enriched = input.clone();
    enriched["cwd"] = json!(workdir);
    enriched
}

fn shell_workdir(input: &Value, cwd: Option<&str>) -> Option<String> {
    let explicit = string_of(input, "workdir").or_else(|| string_of(input, "cwd"));
    match (explicit, cwd) {
        (Some(path), _) if std::path::Path::new(&path).is_absolute() => Some(path),
        (Some(path), Some(cwd)) => {
            let joined = std::path::Path::new(cwd).join(&path);
            Some(joined.to_string_lossy().into_owned())
        }
        (None, Some(cwd)) => Some(cwd.to_string()),
        (None, None) => None,
        (Some(path), None) => Some(path),
    }
}

fn shell_command(input: &Value) -> Option<String> {
    string_of(input, "command").or_else(|| string_of(input, "cmd"))
}

fn is_shell(tool_name: &str) -> bool {
    let tool = tool_name.to_ascii_lowercase();
    tool == "bash" || tool == "shell"
}

/// `diffContent` (tool.ts:325-338).
fn diff_content(input: Option<&Value>) -> Vec<Value> {
    let Some(input) = input else {
        return Vec::new();
    };
    let Some(old_text) = string_of(input, "oldString") else {
        return Vec::new();
    };
    let Some(new_text) = string_of(input, "newString").or_else(|| string_of(input, "content"))
    else {
        return Vec::new();
    };
    vec![json!({
        "type": "diff",
        "path": string_of(input, "filePath").unwrap_or_default(),
        "oldText": old_text,
        "newText": new_text,
    })]
}

/// `readDisplayText` (tool.ts:340-350).
fn read_display_text(metadata: Option<&Value>) -> Option<String> {
    let display = metadata?.get("display")?;
    match display.get("type").and_then(Value::as_str) {
        Some("file") => string_of(display, "text"),
        Some("directory") => display
            .get("entries")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("\n")
            }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn kind_table() {
        assert_eq!(to_tool_kind("bash"), "execute");
        assert_eq!(to_tool_kind("Bash"), "execute");
        assert_eq!(to_tool_kind("webfetch"), "fetch");
        assert_eq!(to_tool_kind("apply_patch"), "edit");
        assert_eq!(to_tool_kind("Grep"), "search");
        assert_eq!(to_tool_kind("read"), "read");
        assert_eq!(to_tool_kind("task"), "think");
        assert_eq!(to_tool_kind("unknown"), "other");
    }

    #[test]
    fn bash_title_is_the_command() {
        let update =
            pending_tool_call("cal_1", "bash", &json!({ "command": "ls -la" }), None, None);
        assert_eq!(update["title"], json!("ls -la"));
        assert_eq!(update["kind"], json!("execute"));
        assert_eq!(update["status"], json!("pending"));
    }

    #[test]
    fn bash_raw_input_gains_cwd() {
        let update = running_tool_update(
            "cal_1",
            "bash",
            &json!({ "command": "ls" }),
            None,
            Some("out"),
            Some("/repo"),
        );
        assert_eq!(update["rawInput"]["cwd"], json!("/repo"));
        assert_eq!(update["content"][0]["content"]["text"], json!("out"));
    }

    #[test]
    fn read_locations_come_from_file_path() {
        let locations = to_locations("read", &json!({ "filePath": "/tmp/a.txt" }), None);
        assert_eq!(locations, vec![json!({ "path": "/tmp/a.txt" })]);
    }

    #[test]
    fn completed_edit_carries_diff_content() {
        let state = json!({
            "input": {
                "filePath": "/tmp/a.txt",
                "oldString": "a",
                "newString": "b",
            },
            "output": "done",
        });
        let update = completed_tool_update(
            "cal_1",
            "edit",
            state.get("input").unwrap(),
            &state,
            None,
            None,
        );
        assert_eq!(update["status"], json!("completed"));
        let content = update["content"].as_array().unwrap();
        assert_eq!(content[0]["content"]["text"], json!("done"));
        assert_eq!(content[1]["type"], json!("diff"));
    }

    #[test]
    fn error_update_carries_error_text() {
        let update = error_tool_update(
            "cal_1",
            "read",
            &json!({ "filePath": "/tmp/a.txt" }),
            "boom",
            &json!({ "metadata": { "interrupted": true } }),
            None,
        );
        assert_eq!(update["status"], json!("failed"));
        assert_eq!(update["content"][0]["content"]["text"], json!("boom"));
        assert_eq!(update["rawOutput"]["metadata"]["interrupted"], json!(true));
    }

    #[test]
    fn image_attachments_become_content() {
        let content = image_contents(Some(&json!([
            { "url": "data:image/png;base64,AAAA", "mime": "image/png" },
            { "url": "file:///tmp/no.txt" },
        ])));
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["content"]["data"], json!("AAAA"));
    }
}

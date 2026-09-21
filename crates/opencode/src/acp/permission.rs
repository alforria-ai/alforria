//! `permission.asked` handling (`acp/permission.ts`) — bridges opencode
//! permission asks into ACP `session/request_permission` requests, plus
//! the proposed-edit preview (`applyPatch`).

use std::sync::Arc;

use serde_json::{json, Value};

use crate::acp::jsonrpc::Connection;
use crate::acp::server::ServerClient;
use crate::acp::tool;

/// `permissionOptions` (permission.ts:20-24).
const PERMISSION_OPTIONS: &[(&str, &str, &str)] = &[
    ("once", "allow_once", "Allow once"),
    ("always", "allow_always", "Always allow"),
    ("reject", "reject_once", "Reject"),
];

fn string_value(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_string)
}

/// `permissionTitle` (permission.ts:139-163).
fn permission_title(tool_name: &str, input: &Value) -> Option<String> {
    let title = match tool_name.to_ascii_lowercase().as_str() {
        "external_directory" => input
            .get("description")
            .or_else(|| input.get("command"))
            .or_else(|| input.get("parentDir"))
            .and_then(Value::as_str)
            .map(str::to_string),
        "webfetch" => tool::string_of(input, "url"),
        "websearch" => tool::string_of(input, "query"),
        "grep" | "glob" => tool::string_of(input, "pattern"),
        "read" | "edit" | "write" => edit_title(input),
        _ => None,
    };
    title
}

fn edit_title(input: &Value) -> Option<String> {
    let files = file_metadata(input);
    if files.len() == 1 {
        return files[0]
            .get("relativePath")
            .or_else(|| files[0].get("filePath"))
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    if files.len() > 1 {
        return Some(format!("{} files", files.len()));
    }
    tool::string_of(input, "filePath")
        .or_else(|| tool::string_of(input, "filepath"))
        .or_else(|| tool::string_of(input, "path"))
}

/// `PermissionFileMetadata` (permission.ts:229-234).
fn file_metadata(input: &Value) -> Vec<Value> {
    let files = match input.get("files").and_then(Value::as_array) {
        Some(files) => files,
        None => return Vec::new(),
    };
    files
        .iter()
        .filter_map(|file| {
            let file_path = file.get("filePath").and_then(Value::as_str)?;
            Some(json!({
                "filePath": file_path,
                "relativePath": file.get("relativePath").and_then(Value::as_str),
                "movePath": file.get("movePath").and_then(Value::as_str),
                "patch": file.get("patch").and_then(Value::as_str),
            }))
        })
        .collect()
}

/// `permissionLocations` (permission.ts:172-181).
fn permission_locations(tool_name: &str, input: &Value) -> Vec<Value> {
    let files = file_metadata(input);
    if !files.is_empty() {
        let mut paths: Vec<String> = Vec::new();
        for file in &files {
            for key in ["filePath", "movePath"] {
                if let Some(path) = file.get(key).and_then(Value::as_str) {
                    if !path.is_empty() && !paths.iter().any(|p| p == path) {
                        paths.push(path.to_string());
                    }
                }
            }
        }
        return paths
            .into_iter()
            .map(|path| json!({ "path": path }))
            .collect();
    }
    tool::to_locations(tool_name, input, None)
}

/// `permissionContent` (permission.ts:183-194) — the edit diff preview.
async fn permission_content(tool_name: &str, input: &Value) -> Vec<Value> {
    if !tool_name.eq_ignore_ascii_case("edit") {
        return Vec::new();
    }
    let files = file_metadata(input);
    if !files.is_empty() {
        let mut content = Vec::new();
        for file in &files {
            if let Some(patch) = file.get("patch").and_then(Value::as_str) {
                if let Some(diff) = diff_content_for_patch(file, patch) {
                    content.push(diff);
                }
            }
        }
        return content;
    }
    let filepath =
        tool::string_of(input, "filepath").or_else(|| tool::string_of(input, "filePath"));
    let diff = tool::string_of(input, "diff");
    match (filepath, diff) {
        (Some(filepath), Some(diff)) => {
            diff_content_for_patch(&json!({ "filePath": filepath }), &diff)
                .into_iter()
                .collect()
        }
        _ => Vec::new(),
    }
}

/// `diffContentForPatch` (permission.ts:207-217).
fn diff_content_for_patch(file: &Value, diff: &str) -> Option<Value> {
    let filepath = file.get("filePath").and_then(Value::as_str)?;
    let content = std::fs::read_to_string(filepath).ok().unwrap_or_default();
    let next = apply_patch(&content, diff)?;
    let display_path = file
        .get("movePath")
        .and_then(Value::as_str)
        .unwrap_or(filepath);
    Some(json!({
        "type": "diff",
        "path": display_path,
        "oldText": content,
        "newText": next,
    }))
}

/// `permissionToolCall` (permission.ts:118-137).
async fn permission_tool_call(tool_call_id: &str, tool_name: &str, input: &Value) -> Value {
    let title = permission_title(tool_name, input)
        .map(Value::String)
        .unwrap_or(Value::Null);
    let tool_call = tool::pending_tool_call(tool_call_id, tool_name, input, title.as_str(), None);
    let content = permission_content(tool_name, input).await;
    let mut tool_call = tool_call;
    tool_call["locations"] = json!(permission_locations(tool_name, input));
    if !content.is_empty() {
        tool_call["content"] = json!(content);
    }
    tool_call
}

/// `selectedReply` (permission.ts:219-223).
fn selected_reply(outcome: &Value) -> Option<&'static str> {
    if outcome.get("outcome") != Some(&json!("selected")) {
        return None;
    }
    match outcome.get("optionId").and_then(Value::as_str) {
        Some("once") => Some("once"),
        Some("always") => Some("always"),
        _ => None,
    }
}

/// `Handler.process` (permission.ts:51-89).
pub async fn handle(
    connection: std::sync::Arc<Connection>,
    server: &ServerClient,
    permission: &Value,
    try_get_cwd: impl Fn(&str) -> Option<String>,
) {
    let session_id = permission.get("sessionID").and_then(Value::as_str);
    let Some(session_id) = session_id else {
        return;
    };
    let Some(cwd) = try_get_cwd(session_id) else {
        return;
    };

    let permission_id = permission
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let tool_name = permission
        .get("permission")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let metadata = permission.get("metadata").cloned().unwrap_or(json!({}));
    let call_id = permission
        .get("tool")
        .and_then(|tool| tool.get("callID"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| permission_id.to_string());

    let tool_call = permission_tool_call(&call_id, tool_name, &metadata).await;
    let request = json!({
        "sessionId": session_id,
        "toolCall": tool_call,
        "options": PERMISSION_OPTIONS
            .iter()
            .map(|(option_id, kind, name)| {
                json!({ "optionId": option_id, "kind": kind, "name": name })
            })
            .collect::<Vec<_>>(),
    });

    let result = connection
        .send_request("session/request_permission", request)
        .await;
    let result = match result {
        Ok(result) => Some(result),
        Err(_) => {
            // The TS auto-rejects when the client request fails
            // (permission.ts:71-74).
            let _ = server.permission_reply(&cwd, permission_id, "reject").await;
            return;
        }
    };

    let Some(result) = result else {
        return;
    };
    let Some(reply) = selected_reply(&result) else {
        let _ = server.permission_reply(&cwd, permission_id, "reject").await;
        return;
    };

    if tool_name == "edit" {
        write_proposed_edit(connection.clone(), session_id, &metadata);
    }

    let _ = server.permission_reply(&cwd, permission_id, reply).await;
}

/// `writeProposedEdit` (permission.ts:99-115) — fire and forget.
fn write_proposed_edit(connection: Arc<Connection>, session_id: &str, metadata: &Value) {
    let Some(filepath) =
        string_value(metadata.get("filepath")).or_else(|| string_value(metadata.get("filePath")))
    else {
        return;
    };
    let Some(diff) = string_value(metadata.get("diff")) else {
        return;
    };
    let content = std::fs::read_to_string(&filepath).ok().unwrap_or_default();
    let Some(next) = apply_patch(&content, &diff) else {
        return;
    };
    let session_id = session_id.to_string();
    tokio::spawn(async move {
        let _ = connection
            .send_request(
                "fs/write_text_file",
                json!({
                    "sessionId": session_id,
                    "path": filepath,
                    "content": next,
                }),
            )
            .await;
    });
}

// ---------------------------------------------------------------------------
// applyPatch — a port of the `diff` npm package's unified-diff applier
// (first-match with exact context), sufficient for the edit previews.
// ---------------------------------------------------------------------------

struct Hunk {
    before: Vec<String>,
    after: Vec<String>,
}

fn parse_patch(diff: &str) -> Vec<Hunk> {
    let mut hunks = Vec::new();
    let mut current: Option<(Vec<String>, Vec<String>)> = None;
    for line in diff.lines() {
        if line.starts_with("@@") {
            if let Some(hunk) = current.take() {
                hunks.push(Hunk {
                    before: hunk.0,
                    after: hunk.1,
                });
            }
            current = Some((Vec::new(), Vec::new()));
            continue;
        }
        let Some((before, after)) = &mut current else {
            continue;
        };
        if let Some(text) = line.strip_prefix('+') {
            after.push(text.to_string());
        } else if let Some(text) = line.strip_prefix('-') {
            before.push(text.to_string());
        } else if line.starts_with(' ') || line.starts_with('\t') {
            before.push(line[1..].to_string());
            after.push(line[1..].to_string());
        } else if line.is_empty() {
            before.push(String::new());
            after.push(String::new());
        }
    }
    if let Some(hunk) = current {
        hunks.push(Hunk {
            before: hunk.0,
            after: hunk.1,
        });
    }
    hunks
}

/// Apply the unified diff to `content`, returning `None` when any hunk
/// cannot be placed (the TS `applyPatch` returning `false`).
pub fn apply_patch(content: &str, diff: &str) -> Option<String> {
    let mut lines: Vec<String> = content.lines().map(str::to_string).collect();
    for hunk in parse_patch(diff) {
        let start = lines
            .windows(hunk.before.len().max(1))
            .position(|window| window.iter().zip(&hunk.before).all(|(a, b)| a == b))
            .or_else(|| (hunk.before.is_empty() && !lines.is_empty()).then_some(0))?;
        let end = start + hunk.before.len();
        if end > lines.len() {
            return None;
        }
        lines.splice(start..end, hunk.after.iter().cloned());
    }
    let mut out = lines.join("\n");
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn applies_a_simple_hunk() {
        let content = "a\nb\nc\n";
        let diff = "\
--- a/file
+++ b/file
@@ -1,3 +1,3 @@
 a
-b
+B
 c
";
        let patched = apply_patch(content, diff).unwrap();
        assert_eq!(patched, "a\nB\nc\n");
    }

    #[test]
    fn mismatching_patch_returns_none() {
        let content = "x\n";
        let diff = "\
@@ -1,1 +1,1 @@
-y
+y
";
        assert!(apply_patch(content, diff).is_none());
    }

    #[test]
    fn title_prefers_command_for_external_directory() {
        assert_eq!(
            permission_title(
                "external_directory",
                &json!({ "description": "spooky", "command": "rm" })
            )
            .unwrap(),
            "spooky"
        );
        assert_eq!(
            permission_title("webfetch", &json!({ "url": "https://x" })).unwrap(),
            "https://x"
        );
    }

    #[test]
    fn selected_reply_maps_options() {
        assert_eq!(
            selected_reply(&json!({ "outcome": "selected", "optionId": "once" })),
            Some("once")
        );
        assert_eq!(
            selected_reply(&json!({ "outcome": "selected", "optionId": "nope" })),
            None
        );
        assert_eq!(selected_reply(&json!({ "outcome": "cancelled" })), None);
    }
}

//! cli/cmd/import.ts port — the `import` command: read a JSON file or a
//! share URL and recreate the session locally.

use clap::ArgMatches;
use serde_json::Value;

use crate::error::{CliError, TypedError};
use crate::ui::Ui;

/// `parseShareUrl` (import.ts:28-30):
/// `^https?://[^/]+/share/([a-zA-Z0-9_-]+)$`.
pub fn parse_share_url(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))?;
    let (host, path) = rest.split_once('/')?;
    if host.is_empty() || host.contains('/') {
        return None;
    }
    let slug = path.strip_prefix("share/")?;
    if slug.is_empty()
        || !slug
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        || path.ends_with('/')
    {
        return None;
    }
    Some(slug.to_string())
}

/// `transformShareData` (import.ts:60-90) — group the flat share array into
/// the nested `{ info, messages }` shape. `None` = "Share not found or
/// empty".
pub fn transform_share_data(share_data: &[Value]) -> Option<Value> {
    let session = share_data
        .iter()
        .find(|item| item.get("type").and_then(Value::as_str) == Some("session"))
        .map(|item| item.get("data"))
        .filter(|data| data.is_some())?;
    let mut messages: Vec<(String, Value)> = Vec::new();
    let mut parts: Vec<(String, Vec<Value>)> = Vec::new();
    for item in share_data {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                let Some(id) = item["data"]["id"].as_str() else {
                    continue;
                };
                match messages.iter_mut().find(|(existing, _)| existing == id) {
                    Some(message) => message.1 = item["data"].clone(),
                    None => messages.push((id.to_string(), item["data"].clone())),
                }
            }
            Some("part") => {
                let Some(message_id) = item["data"]["messageID"].as_str() else {
                    continue;
                };
                match parts
                    .iter_mut()
                    .find(|(existing, _)| existing == message_id)
                {
                    Some(list) => list.1.push(item["data"].clone()),
                    None => parts.push((message_id.to_string(), vec![item["data"].clone()])),
                }
            }
            _ => {}
        }
    }
    if messages.is_empty() {
        return None;
    }
    let messages: Vec<Value> = messages
        .into_iter()
        .map(|(id, info)| {
            let empty: Vec<Value> = Vec::new();
            let message_parts = parts
                .iter()
                .find(|(existing, _)| *existing == id)
                .map(|(_, list)| list)
                .unwrap_or(&empty);
            serde_json::json!({ "info": info, "parts": message_parts })
        })
        .collect();
    Some(serde_json::json!({
        "info": session,
        "messages": messages,
    }))
}

/// `formatImportFileError` (import.ts:41-50) for the local-file leg.
fn read_export_file(file: &str) -> Result<Value, CliError> {
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(CliError::new(format!("File not found: {file}")));
        }
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(CliError::new("Failed to read file: Permission denied"));
        }
        Err(err) => {
            return Err(CliError::new(format!("Failed to read file: {err}")));
        }
    };
    serde_json::from_str(&text)
        .map_err(|err| CliError::new(format!("Invalid JSON in {file}: {err}")))
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImportMessage {
    info: opencode_schema::session_v1::V1Message,
    parts: Vec<opencode_schema::session_v1::V1Part>,
}

#[derive(serde::Deserialize)]
struct ImportDocument {
    info: opencode_schema::session_v1::V1SessionInfo,
    messages: Vec<ImportMessage>,
}

/// Serialize the message minus `id`/`sessionID` (`messageData`,
/// projector.ts:80-84).
fn message_row_data(info: &opencode_schema::session_v1::V1Message) -> Value {
    let mut value = serde_json::to_value(info).unwrap_or(Value::Null);
    if let Some(object) = value.as_object_mut() {
        object.remove("id");
        object.remove("sessionID");
    }
    value
}

/// Serialize the part minus `id`/`sessionID`/`messageID` (`partData`,
/// projector.ts:80-84).
fn part_row_data(part: &opencode_schema::session_v1::V1Part) -> Value {
    let mut value = serde_json::to_value(part).unwrap_or(Value::Null);
    if let Some(object) = value.as_object_mut() {
        object.remove("id");
        object.remove("sessionID");
        object.remove("messageID");
    }
    value
}

/// `json_opt_to_string` (storage schema — JSON columns serialize to text).
fn json_opt_to_string(value: &Option<Value>) -> Result<Option<String>, TypedError> {
    value
        .as_ref()
        .map(|value| {
            serde_json::to_string(value)
                .map_err(|err| TypedError::Cli(CliError::new(err.to_string())))
        })
        .transpose()
}

/// Write the imported session into storage (import.ts:179-227): the session
/// row upserts its project/directory/path columns; messages and parts skip
/// existing ids.
fn write_import(
    instance: &crate::instance::Instance,
    document: &ImportDocument,
    project_id: &str,
    directory: &str,
    path: &str,
) -> Result<(), TypedError> {
    let row = opencode_core::session::store::to_row(&document.info)
        .map_err(|err| TypedError::Cli(CliError::new(err.to_string())))?;
    let summary_diffs = json_opt_to_string(&row.summary_diffs)?;
    let metadata = json_opt_to_string(&row.metadata)?;
    let revert = json_opt_to_string(&row.revert)?;
    let permission = json_opt_to_string(&row.permission)?;
    instance
        .services
        .storage
        .with_connection(|conn| {
            conn.execute(
                "INSERT INTO session (id, project_id, workspace_id, parent_id, slug, directory, path, title, version, share_url, summary_additions, summary_deletions, summary_files, summary_diffs, metadata, cost, tokens_input, tokens_output, tokens_reasoning, tokens_cache_read, tokens_cache_write, revert, permission, agent, model, time_created, time_updated, time_compacting, time_archived)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29)
                 ON CONFLICT (id) DO UPDATE SET project_id = excluded.project_id, directory = excluded.directory, path = excluded.path",
                rusqlite::params![
                    row.id,
                    project_id,
                    row.workspace_id,
                    row.parent_id,
                    row.slug,
                    directory,
                    path,
                    row.title,
                    row.version,
                    row.share_url,
                    row.summary_additions,
                    row.summary_deletions,
                    row.summary_files,
                    summary_diffs,
                    metadata,
                    row.cost,
                    row.tokens_input,
                    row.tokens_output,
                    row.tokens_reasoning,
                    row.tokens_cache_read,
                    row.tokens_cache_write,
                    revert,
                    permission,
                    row.agent,
                    row.model,
                    row.time_created,
                    row.time_updated,
                    row.time_compacting,
                    row.time_archived,
                ],
            )?;
            Ok::<_, rusqlite::Error>(())
        })
        .map_err(|err| TypedError::Cli(CliError::new(err.to_string())))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default();
    for message in &document.messages {
        let message_id = opencode_core::session::message::message_id(&message.info).to_string();
        let data = serde_json::to_string(&message_row_data(&message.info)).unwrap_or_default();
        let created = opencode_core::session::message::message_time_created(&message.info) as i64;
        instance
            .services
            .storage
            .with_connection(|conn| {
                conn.execute(
                    "INSERT INTO message (id, session_id, time_created, time_updated, data)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT (id) DO NOTHING",
                    rusqlite::params![message_id, document.info.id, created, now, data],
                )?;
                Ok::<_, rusqlite::Error>(())
            })
            .map_err(|err| TypedError::Cli(CliError::new(err.to_string())))?;
        for part in &message.parts {
            let part_data = serde_json::to_string(&part_row_data(part)).unwrap_or_default();
            instance
                .services
                .storage
                .with_connection(|conn| {
                    conn.execute(
                        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                         ON CONFLICT (id) DO NOTHING",
                        rusqlite::params![
                            part_id(part),
                            part_message_id(part),
                            document.info.id,
                            now,
                            now,
                            part_data
                        ],
                    )?;
                    Ok::<_, rusqlite::Error>(())
                })
                .map_err(|err| TypedError::Cli(CliError::new(err.to_string())))?;
        }
    }
    Ok(())
}

fn part_id(part: &opencode_schema::session_v1::V1Part) -> String {
    let value = serde_json::to_value(part).unwrap_or(Value::Null);
    value
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn part_message_id(part: &opencode_schema::session_v1::V1Part) -> String {
    let value = serde_json::to_value(part).unwrap_or(Value::Null);
    value
        .get("messageID")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// The `POST`-less share fetch (import.ts:132-158): no account headers are
/// attached (N7 — the console account flows are out of scope).
async fn fetch_share(base_url: &str, slug: &str) -> Result<Option<Value>, String> {
    let url = format!("{base_url}/api/share/{slug}/data");
    let client = reqwest::Client::new();
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|err| err.to_string())?;
    if !response.status().is_success() {
        return Ok(None);
    }
    let text = response
        .text()
        .await
        .map_err(|err| format!("Share data was not valid JSON: {err}"))?;
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|_| "Share data was not valid JSON".to_string())
}

pub fn run(matches: &ArgMatches, ui: &mut Ui) -> Result<(), TypedError> {
    let file = matches
        .get_one::<String>("file")
        .map(String::as_str)
        .unwrap_or_default();
    let runtime = super::runtime()?;
    runtime.block_on(run_async(file, ui))
}

async fn run_async(file: &str, ui: &mut Ui) -> Result<(), TypedError> {
    let is_url = file.starts_with("http://") || file.starts_with("https://");
    let document = if is_url {
        let Some(slug) = parse_share_url(file) else {
            let base = "https://opencodectl.ai";
            ui.write_stdout(&format!(
                "Invalid URL format. Expected: {base}/share/<slug>\n"
            ));
            return Ok(());
        };
        let origin = match file.split_once("://") {
            Some((scheme, rest)) => match rest.split_once('/') {
                Some((host, _)) => format!("{scheme}://{host}"),
                None => format!("{scheme}://{rest}"),
            },
            None => file.to_string(),
        };
        let share = fetch_share(&origin, &slug).await.map_err(|message| {
            TypedError::Cli(CliError::new(format!(
                "Failed to fetch share data: {message}"
            )))
        })?;
        let Some(share) = share else {
            ui.write_stdout("Failed to fetch share data\n");
            return Ok(());
        };
        let Some(share) = share.as_array() else {
            ui.write_stdout("Share data was not valid JSON\n");
            return Ok(());
        };
        let Some(document) = transform_share_data(share) else {
            ui.write_stdout(&format!("Share not found or empty: {slug}\n"));
            return Ok(());
        };
        document
    } else {
        read_export_file(file).map_err(TypedError::Cli)?
    };
    let document: ImportDocument = serde_json::from_value(document)
        .map_err(|err| TypedError::Cli(CliError::new(err.to_string())))?;
    let instance = crate::instance::boot(None)?;
    let context = instance
        .services
        .instance_context(&instance.directory, None)
        .map_err(|err| TypedError::Cli(CliError::new(err.to_string())))?;
    let path = opencode_core::session::store::session_path(&instance.worktree, &instance.directory);
    write_import(
        &instance,
        &document,
        &context.project_id,
        &context.directory.to_string_lossy(),
        &path,
    )?;
    ui.write_stdout(&format!("Imported session: {}\n", document.info.id));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_share_urls() {
        assert_eq!(
            parse_share_url("https://opencd.ai/share/abc123"),
            Some("abc123".to_string())
        );
        assert_eq!(
            parse_share_url("http://localhost:3000/share/a-b_C9"),
            Some("a-b_C9".to_string())
        );
        assert_eq!(parse_share_url("https://opencd.ai/other/abc"), None);
        assert_eq!(parse_share_url("https://opencd.ai/share/"), None);
        assert_eq!(parse_share_url("https://opencd.ai/share/a/b"), None);
        assert_eq!(parse_share_url("opencd.ai/share/abc"), None);
    }

    #[test]
    fn transform_groups_flat_share_items() {
        let items = vec![
            serde_json::json!({"type": "session", "data": {"id": "ses_1"}}),
            serde_json::json!({"type": "message", "data": {"id": "msg_1", "role": "user"}}),
            serde_json::json!({
                "type": "part",
                "data": {"id": "prt_1", "messageID": "msg_1", "type": "text", "text": "hi"},
            }),
        ];
        let transformed = transform_share_data(&items).expect("transforms");
        assert_eq!(transformed["info"]["id"], "ses_1");
        assert_eq!(transformed["messages"][0]["info"]["id"], "msg_1");
        assert_eq!(transformed["messages"][0]["parts"][0]["id"], "prt_1");
    }

    #[test]
    fn transform_requires_session_and_messages() {
        assert_eq!(transform_share_data(&[]), None);
        assert_eq!(
            transform_share_data(&[serde_json::json!({
                "type": "message",
                "data": {"id": "msg_1"},
            })]),
            None
        );
        assert_eq!(
            transform_share_data(&[serde_json::json!({
                "type": "session",
                "data": {"id": "ses_1"},
            })]),
            None
        );
    }

    #[test]
    fn missing_file_is_reported() {
        let err = read_export_file("/nonexistent/session.json").unwrap_err();
        assert_eq!(err.message, "File not found: /nonexistent/session.json");
    }

    #[test]
    fn invalid_json_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("bad.json");
        std::fs::write(&file, "{not json").unwrap();
        let err = read_export_file(file.to_str().unwrap()).unwrap_err();
        assert!(
            err.message.starts_with("Invalid JSON in"),
            "{}",
            err.message
        );
    }
}

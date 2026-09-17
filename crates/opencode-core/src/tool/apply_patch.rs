//! `apply_patch` tool — port of `tool/apply_patch.ts` (spec M4.7).

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::format::Formatter;
use crate::tool::bom;
use crate::tool::def::{define, Agents, AskRequest, ExecuteResult, ToolCtxRef, ToolDef};
use crate::tool::diff::create_two_files_patch;
use crate::tool::edit::{diagnostic_report, trim_diff, FileEvents, Lsp};
use crate::tool::error::ToolError;
use crate::tool::external_directory::{assert_external_directory, ExternalOptions, Kind};
use crate::tool::patch_parser::{derive_new_contents_from_chunks, parse_patch, Hunk};
use crate::tool::truncate::Truncate;

#[derive(Debug, Deserialize)]
pub struct ApplyPatchParameters {
    #[serde(rename = "patchText")]
    pub patch_text: String,
}

/// Hand-authored JSON Schema (byte-matches the Effect-generated schema; see
/// `tests/golden/toolschema/apply_patch.json`).
pub fn parameters() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {
            "patchText": {
                "type": "string",
                "description": "The full patch text that describes all changes to be made"
            }
        },
        "required": ["patchText"]
    })
}

/// One planned file change (apply_patch.ts:58-69).
#[derive(Debug)]
struct FileChange {
    file_path: String,
    new_content: String,
    kind: &'static str, // "add" | "update" | "delete" | "move"
    move_path: Option<String>,
    diff: String,
    additions: usize,
    deletions: usize,
    bom: bool,
}

/// Build the `apply_patch` tool.
pub fn apply_patch_tool(
    truncate: Arc<dyn Truncate>,
    agents: Arc<dyn Agents>,
    lsp: Option<Arc<dyn Lsp>>,
    format: Option<Arc<dyn Formatter>>,
    events: Option<Arc<dyn FileEvents>>,
) -> ToolDef {
    define(
        "apply_patch",
        include_str!("txt/apply_patch.txt"),
        parameters(),
        None,
        truncate,
        agents,
        move |params: ApplyPatchParameters, ctx: ToolCtxRef<'_>| {
            let lsp = lsp.clone();
            let format = format.clone();
            let events = events.clone();
            Box::pin(async move { run(params, ctx, lsp, format, events).await })
        },
    )
}

fn relative(instance_worktree: &std::path::Path, target: &str) -> String {
    let rel = std::path::Path::new(target)
        .strip_prefix(instance_worktree)
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|_| target.to_string());
    rel.replace('\\', "/")
}

async fn run(
    params: ApplyPatchParameters,
    ctx: ToolCtxRef<'_>,
    lsp: Option<Arc<dyn Lsp>>,
    format: Option<Arc<dyn Formatter>>,
    events: Option<Arc<dyn FileEvents>>,
) -> Result<ExecuteResult, ToolError> {
    if params.patch_text.is_empty() {
        return Err(ToolError::Failed("patchText is required".to_string()));
    }

    let hunks = match parse_patch(&params.patch_text) {
        Ok(hunks) => hunks,
        Err(error) => {
            return Err(ToolError::Failed(format!(
                "apply_patch verification failed: {error}"
            )));
        }
    };

    if hunks.is_empty() {
        let normalized = params
            .patch_text
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .trim()
            .to_string();
        if normalized == "*** Begin Patch\n*** End Patch" {
            return Err(ToolError::Failed("patch rejected: empty patch".to_string()));
        }
        return Err(ToolError::Failed(
            "apply_patch verification failed: no hunks found".to_string(),
        ));
    }

    let instance = ctx.instance;

    let mut file_changes: Vec<FileChange> = Vec::new();
    let mut total_diff = String::new();

    for hunk in &hunks {
        let (hunk_path, kind) = match hunk {
            Hunk::Add { path, .. } => (path, "add"),
            Hunk::Delete { path } => (path, "delete"),
            Hunk::Update { path, .. } => (path, "update"),
        };
        let file_path = instance.directory.join(hunk_path);
        let file_path_str = file_path.to_string_lossy().to_string();
        assert_external_directory(
            &ctx,
            Some(&file_path_str),
            ExternalOptions {
                bypass: ctx.extra.bypass_cwd_check,
                kind: Kind::File,
            },
        )
        .await?;

        match hunk {
            Hunk::Add { contents, .. } => {
                let new_content = if contents.is_empty() || contents.ends_with('\n') {
                    contents.clone()
                } else {
                    format!("{contents}\n")
                };
                let next = bom::split(&new_content);
                let diff = trim_diff(&create_two_files_patch(&file_path_str, "", &next.text));
                let (additions, deletions) = crate::tool::diff::diff_line_counts("", &next.text);
                file_changes.push(FileChange {
                    file_path: file_path_str,
                    new_content: next.text.clone(),
                    kind: "add",
                    move_path: None,
                    diff: diff.clone(),
                    additions,
                    deletions,
                    bom: next.bom,
                });
                total_diff.push_str(&diff);
                total_diff.push('\n');
            }
            Hunk::Update {
                move_path, chunks, ..
            } => {
                let stat = tokio::fs::metadata(&file_path).await;
                if stat.map(|s| s.is_dir()).unwrap_or(true) {
                    return Err(ToolError::Failed(format!(
                        "apply_patch verification failed: Failed to read file to update: {file_path_str}"
                    )));
                }
                let raw = tokio::fs::read(&file_path)
                    .await
                    .map_err(|error| {
                        ToolError::Failed(format!(
                            "apply_patch verification failed: Failed to read file to update: {file_path_str}: {error}"
                        ))
                    })?;
                let source = bom::split(&String::from_utf8_lossy(&raw));
                let update = match derive_new_contents_from_chunks(
                    &file_path_str,
                    chunks,
                    &bom::join(&source.text, source.bom),
                ) {
                    Ok(update) => update,
                    Err(error) => {
                        return Err(ToolError::Failed(format!(
                            "apply_patch verification failed: {error}"
                        )));
                    }
                };

                let diff = trim_diff(&create_two_files_patch(
                    &file_path_str,
                    &source.text,
                    &update.content,
                ));
                let (additions, deletions) =
                    crate::tool::diff::diff_line_counts(&source.text, &update.content);

                let move_path = move_path
                    .as_ref()
                    .map(|move_path| instance.directory.join(move_path));
                if let Some(move_path) = &move_path {
                    let move_str = move_path.to_string_lossy().to_string();
                    assert_external_directory(
                        &ctx,
                        Some(&move_str),
                        ExternalOptions {
                            bypass: ctx.extra.bypass_cwd_check,
                            kind: Kind::File,
                        },
                    )
                    .await?;
                }

                file_changes.push(FileChange {
                    file_path: file_path_str,
                    new_content: update.content,
                    kind: if move_path.is_some() {
                        "move"
                    } else {
                        "update"
                    },
                    move_path: move_path.map(|path| path.to_string_lossy().to_string()),
                    diff: diff.clone(),
                    additions,
                    deletions,
                    bom: update.bom,
                });
                total_diff.push_str(&diff);
                total_diff.push('\n');
            }
            Hunk::Delete { .. } => {
                let raw = tokio::fs::read(&file_path).await.map_err(|error| {
                    ToolError::Failed(format!("apply_patch verification failed: {error}"))
                })?;
                let source = bom::split(&String::from_utf8_lossy(&raw));
                let delete_diff =
                    trim_diff(&create_two_files_patch(&file_path_str, &source.text, ""));
                let deletions = source.text.split('\n').count();
                file_changes.push(FileChange {
                    file_path: file_path_str,
                    new_content: String::new(),
                    kind: "delete",
                    move_path: None,
                    diff: delete_diff.clone(),
                    additions: 0,
                    deletions,
                    bom: source.bom,
                });
                total_diff.push_str(&delete_diff);
                total_diff.push('\n');
            }
        }
        let _ = kind;
    }

    // Per-file metadata for UI rendering (apply_patch.ts:194-202).
    let files: Vec<Value> = file_changes
        .iter()
        .map(|change| {
            let mut entry = json!({
                "filePath": change.file_path,
                "relativePath": relative(&instance.worktree, change.move_path.as_deref().unwrap_or(&change.file_path)),
                "type": change.kind,
                "patch": change.diff,
                "additions": change.additions,
                "deletions": change.deletions,
            });
            if let Some(move_path) = &change.move_path {
                entry
                    .as_object_mut()
                    .unwrap()
                    .insert("movePath".to_string(), json!(move_path));
            }
            entry
        })
        .collect();

    // One ask with all relative paths (apply_patch.ts:205-215).
    let relative_paths: Vec<String> = file_changes
        .iter()
        .map(|change| relative(&instance.worktree, &change.file_path))
        .collect();
    ctx.ask
        .ask(AskRequest {
            permission: "edit".to_string(),
            patterns: relative_paths.clone(),
            always: vec!["*".to_string()],
            metadata: json!({
                "filepath": relative_paths.join(", "),
                "diff": total_diff,
                "files": files,
            }),
        })
        .await?;

    // Apply the changes (apply_patch.ts:217-258).
    let mut updates: Vec<(String, &'static str)> = Vec::new();
    for change in &file_changes {
        let edited = if change.kind == "delete" {
            None
        } else {
            change
                .move_path
                .as_deref()
                .or(Some(change.file_path.as_str()))
        };
        match change.kind {
            "add" => {
                write_all(
                    &change.file_path,
                    &bom::join(&change.new_content, change.bom),
                )
                .await?;
                updates.push((change.file_path.clone(), "add"));
            }
            "update" => {
                write_all(
                    &change.file_path,
                    &bom::join(&change.new_content, change.bom),
                )
                .await?;
                updates.push((change.file_path.clone(), "change"));
            }
            "move" => {
                if let Some(move_path) = &change.move_path {
                    write_all(move_path, &bom::join(&change.new_content, change.bom)).await?;
                    tokio::fs::remove_file(&change.file_path)
                        .await
                        .map_err(|error| {
                            ToolError::Failed(format!(
                                "Failed to remove file {}: {error}",
                                change.file_path
                            ))
                        })?;
                    updates.push((change.file_path.clone(), "unlink"));
                    updates.push((move_path.clone(), "add"));
                }
            }
            "delete" => {
                tokio::fs::remove_file(&change.file_path)
                    .await
                    .map_err(|error| {
                        ToolError::Failed(format!(
                            "Failed to remove file {}: {error}",
                            change.file_path
                        ))
                    })?;
                updates.push((change.file_path.clone(), "unlink"));
            }
            _ => {}
        }

        if let Some(edited) = edited {
            if let Some(formatter) = &format {
                if formatter.file(edited).await {
                    let raw = tokio::fs::read(edited).await.unwrap_or_default();
                    if !raw.is_empty() {
                        let source = bom::split(&String::from_utf8_lossy(&raw));
                        let joined = bom::join(&source.text, source.bom);
                        if !raw.is_empty() && joined.as_bytes() != raw.as_slice() {
                            tokio::fs::write(edited, joined).await.map_err(|error| {
                                ToolError::Failed(format!("Failed to write file {edited}: {error}"))
                            })?;
                        }
                    }
                }
            }
            if let Some(events) = &events {
                events.edited(edited).await;
            }
        }
    }

    for (file, event) in &updates {
        if let Some(events) = &events {
            events.updated(file, event).await;
        }
    }

    // Notify LSP and collect diagnostics (apply_patch.ts:265-271).
    let diagnostics = match &lsp {
        Some(lsp) => {
            for change in &file_changes {
                if change.kind == "delete" {
                    continue;
                }
                let target = change.move_path.as_deref().unwrap_or(&change.file_path);
                lsp.touch_file(target).await;
            }
            lsp.diagnostics().await
        }
        None => json!({}),
    };

    // Output summary (apply_patch.ts:273-293).
    let summary_lines: Vec<String> = file_changes
        .iter()
        .map(|change| {
            let target = change.move_path.as_deref().unwrap_or(&change.file_path);
            format!(
                "{} {}",
                match change.kind {
                    "add" => "A",
                    "delete" => "D",
                    _ => "M",
                },
                relative(&instance.worktree, target),
            )
        })
        .collect();
    let mut output = format!(
        "Success. Updated the following files:\n{}",
        summary_lines.join("\n")
    );

    for change in &file_changes {
        if change.kind == "delete" {
            continue;
        }
        let target = change.move_path.as_deref().unwrap_or(&change.file_path);
        let issues = diagnostics
            .get(target)
            .cloned()
            .unwrap_or_else(|| json!([]));
        let block = diagnostic_report(target, &issues);
        if block.is_empty() {
            continue;
        }
        output.push_str(&format!(
            "\n\nLSP errors detected in {}, please fix:\n{}",
            relative(&instance.worktree, target),
            block
        ));
    }

    Ok(ExecuteResult {
        title: output.clone(),
        metadata: json!({
            "diff": total_diff,
            "files": files,
            "diagnostics": diagnostics,
        }),
        output,
        attachments: None,
    })
}

async fn write_all(path: &str, contents: &str) -> Result<(), ToolError> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|error| {
            ToolError::Failed(format!(
                "Failed to create directory {}: {error}",
                parent.display()
            ))
        })?;
    }
    tokio::fs::write(path, contents)
        .await
        .map_err(|error| ToolError::Failed(format!("Failed to write file {path}: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::def::Extra;
    use crate::tool::ripgrep::test_support::{ctx, fixed_agents, instance, RecordingAsk};
    use crate::tool::truncate::TruncateService;

    fn tool() -> ToolDef {
        apply_patch_tool(
            Arc::new(TruncateService::default_limits(std::path::PathBuf::from(
                "/tmp/opencode",
            ))),
            fixed_agents(),
            None,
            None,
            None,
        )
    }

    async fn call(
        args: Value,
        name: &str,
    ) -> (
        Result<ExecuteResult, ToolError>,
        Vec<crate::tool::def::AskRequest>,
        crate::storage::test_support::TempDir,
    ) {
        let temp = crate::storage::test_support::TempDir::new(name);
        let def = tool();
        let ask = RecordingAsk::new();
        let inst = instance(temp.path());
        let extra = Extra::default();
        let ctx = ctx(&ask, &inst, &extra);
        let result = (def.execute)(args, ctx).await;
        (result, ask.requests(), temp)
    }

    #[test]
    fn golden_parameters_schema() {
        let golden = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/golden/toolschema/apply_patch.json"
        ))
        .unwrap();
        let golden: Value = serde_json::from_str(&golden).unwrap();
        assert_eq!(parameters(), golden);
    }

    #[tokio::test]
    async fn empty_patch_text_is_an_error() {
        let (result, _, temp) = call(json!({ "patchText": "" }), "apply-empty-text").await;
        assert_eq!(result.unwrap_err().to_string(), "patchText is required");
        drop(temp);
    }

    #[tokio::test]
    async fn missing_markers_are_an_error() {
        let (result, _, temp) = call(
            json!({ "patchText": "not a patch" }),
            "apply-missing-markers",
        )
        .await;
        assert_eq!(
            result.unwrap_err().to_string(),
            "apply_patch verification failed: Invalid patch format: missing Begin/End markers"
        );
        drop(temp);
    }

    #[tokio::test]
    async fn empty_patch_is_rejected() {
        let (result, _, temp) = call(
            json!({ "patchText": "*** Begin Patch\n*** End Patch" }),
            "apply-empty-patch",
        )
        .await;
        assert_eq!(
            result.unwrap_err().to_string(),
            "patch rejected: empty patch"
        );
        drop(temp);
    }

    #[tokio::test]
    async fn add_hunk_writes_file() {
        let patch = "*** Begin Patch\n*** Add File: new.txt\n+hello\n*** End Patch";
        let (result, asks, temp) = call(json!({ "patchText": patch }), "apply-add").await;
        let result = result.unwrap();
        assert!(
            result
                .output
                .starts_with("Success. Updated the following files:\nA new.txt"),
            "{}",
            result.output
        );
        assert_eq!(
            result.output,
            "Success. Updated the following files:\nA new.txt"
        );
        assert_eq!(asks.len(), 1);
        assert_eq!(asks[0].permission, "edit");
        assert_eq!(asks[0].patterns, vec!["new.txt".to_string()]);
        let written = std::fs::read_to_string(temp.path().join("new.txt")).unwrap();
        assert_eq!(written, "hello\n");
        drop(temp);
    }

    #[tokio::test]
    async fn update_hunk_modifies_file() {
        let temp = crate::storage::test_support::TempDir::new("apply-update");
        std::fs::write(temp.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let patch = "*** Begin Patch\n*** Update File: a.txt\n@@\n-two\n+TWO\n*** End Patch";
        let def = tool();
        let ask = RecordingAsk::new();
        let inst = instance(temp.path());
        let extra = Extra::default();
        let ctx = ctx(&ask, &inst, &extra);
        let result = (def.execute)(json!({ "patchText": patch }), ctx)
            .await
            .unwrap();
        assert_eq!(
            result.output,
            "Success. Updated the following files:\nM a.txt"
        );
        let written = std::fs::read_to_string(temp.path().join("a.txt")).unwrap();
        assert_eq!(written, "one\nTWO\nthree\n");
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn move_hunk_renames_file() {
        let temp = crate::storage::test_support::TempDir::new("apply-move");
        std::fs::write(temp.path().join("a.txt"), "one\ntwo\n").unwrap();
        let patch = concat!(
            "*** Begin Patch\n",
            "*** Update File: a.txt\n",
            "*** Move to: b.txt\n",
            "@@\n",
            "-two\n",
            "+deux\n",
            "*** End Patch",
        );
        let def = tool();
        let ask = RecordingAsk::new();
        let inst = instance(temp.path());
        let extra = Extra::default();
        let ctx = ctx(&ask, &inst, &extra);
        let result = (def.execute)(json!({ "patchText": patch }), ctx)
            .await
            .unwrap();
        assert_eq!(
            result.output,
            "Success. Updated the following files:\nM b.txt"
        );
        assert!(!temp.path().join("a.txt").exists());
        assert_eq!(
            std::fs::read_to_string(temp.path().join("b.txt")).unwrap(),
            "one\ndeux\n"
        );
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn delete_hunk_removes_file() {
        let temp = crate::storage::test_support::TempDir::new("apply-delete");
        std::fs::write(temp.path().join("a.txt"), "bye\n").unwrap();
        let patch = "*** Begin Patch\n*** Delete File: a.txt\n*** End Patch";
        let def = tool();
        let ask = RecordingAsk::new();
        let inst = instance(temp.path());
        let extra = Extra::default();
        let ctx = ctx(&ask, &inst, &extra);
        let result = (def.execute)(json!({ "patchText": patch }), ctx)
            .await
            .unwrap();
        assert_eq!(
            result.output,
            "Success. Updated the following files:\nD a.txt"
        );
        assert!(!temp.path().join("a.txt").exists());
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn update_missing_file_is_an_error() {
        let temp = crate::storage::test_support::TempDir::new("apply-missing");
        std::fs::write(temp.path().join("dir.txt"), "x\n").unwrap();
        let patch = "*** Begin Patch\n*** Update File: missing.txt\n@@\n-a\n+b\n*** End Patch";
        let def = tool();
        let ask = RecordingAsk::new();
        let inst = instance(temp.path());
        let extra = Extra::default();
        let ctx = ctx(&ask, &inst, &extra);
        let result = (def.execute)(json!({ "patchText": patch }), ctx).await;
        let error = result.unwrap_err().to_string();
        assert!(
            error.starts_with("apply_patch verification failed: Failed to read file to update:"),
            "{error}"
        );
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn multi_file_patch_single_ask() {
        let temp = crate::storage::test_support::TempDir::new("apply-multi");
        std::fs::write(temp.path().join("keep.txt"), "x\n").unwrap();
        let patch = concat!(
            "*** Begin Patch\n",
            "*** Add File: one.txt\n",
            "+first\n",
            "*** Delete File: keep.txt\n",
            "*** End Patch",
        );
        let def = tool();
        let ask = RecordingAsk::new();
        let inst = instance(temp.path());
        let extra = Extra::default();
        let ctx = ctx(&ask, &inst, &extra);
        let result = (def.execute)(json!({ "patchText": patch }), ctx)
            .await
            .unwrap();
        assert!(result.output.contains("A one.txt"), "{}", result.output);
        assert!(result.output.contains("D keep.txt"), "{}", result.output);
        let asks = ask.requests();
        assert_eq!(asks.len(), 1);
        assert_eq!(
            asks[0].patterns,
            vec!["one.txt".to_string(), "keep.txt".to_string()]
        );
        let files = asks[0].metadata["files"].as_array().unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0]["type"], "add");
        assert_eq!(files[1]["type"], "delete");
        assert_eq!(files[1]["deletions"], 2);
        std::fs::remove_dir_all(temp.path()).ok();
    }
}

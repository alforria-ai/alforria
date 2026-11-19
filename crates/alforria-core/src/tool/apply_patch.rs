//! `apply_patch` tool — port of `tool/apply_patch.ts` (spec M4.7).

use std::path::Path;
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
use crate::tool::ripgrep::ts_resolve;
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
    io_path: String,
    new_content: String,
    kind: &'static str, // "add" | "update" | "delete" | "move"
    move_path: Option<String>,
    move_io_path: Option<String>,
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
    // `path.relative(worktree, target)` — `../` sequences for targets
    // outside the worktree, not the raw absolute path.
    crate::tool::ripgrep::ts_relative(instance_worktree, Path::new(target))
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
        let file_path = ts_resolve(&instance.directory, hunk_path);
        let file_path_str = file_path.to_string_lossy().to_string();
        let io_path = crate::tool::file_path::resolve(&file_path)
            .to_string_lossy()
            .into_owned();
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
                    io_path,
                    new_content: next.text.clone(),
                    kind: "add",
                    move_path: None,
                    move_io_path: None,
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
                let stat = tokio::fs::metadata(&io_path).await;
                if stat.map(|s| s.is_dir()).unwrap_or(true) {
                    return Err(ToolError::Failed(format!(
                        "apply_patch verification failed: Failed to read file to update: {file_path_str}"
                    )));
                }
                let raw = tokio::fs::read(&io_path)
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
                    .map(|move_path| ts_resolve(&instance.directory, move_path))
                    // A move onto itself is a plain update; treating it as a
                    // move would write the file and then delete it.
                    .filter(|move_path| *move_path != file_path);
                if let Some(move_path) = &move_path {
                    crate::tool::file_path::reject_move_aliases(&file_path, move_path)?;
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
                    io_path,
                    new_content: update.content,
                    kind: if move_path.is_some() {
                        "move"
                    } else {
                        "update"
                    },
                    move_io_path: move_path.as_ref().map(|path| {
                        crate::tool::file_path::resolve(path)
                            .to_string_lossy()
                            .into_owned()
                    }),
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
                let raw = tokio::fs::read(&io_path).await.map_err(|error| {
                    ToolError::Failed(format!("apply_patch verification failed: {error}"))
                })?;
                let source = bom::split(&String::from_utf8_lossy(&raw));
                let delete_diff =
                    trim_diff(&create_two_files_patch(&file_path_str, &source.text, ""));
                let deletions = source.text.split('\n').count();
                file_changes.push(FileChange {
                    file_path: file_path_str,
                    io_path,
                    new_content: String::new(),
                    kind: "delete",
                    move_path: None,
                    move_io_path: None,
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

    // Authorize every mutation before writing anything. Moves replace the
    // destination contents, so both endpoints require edit permission.
    let relative_paths = crate::tool::file_path::edit_patterns(
        &instance.worktree,
        file_changes.iter().flat_map(|change| {
            [
                Some(Path::new(&change.file_path)),
                Some(Path::new(&change.io_path)),
                change.move_path.as_deref().map(Path::new),
                change.move_io_path.as_deref().map(Path::new),
            ]
            .into_iter()
            .flatten()
        }),
    );
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

    // Every path the patch is about to write, create, rename onto or
    // delete — a move's destination as much as its source, as requested and
    // as resolved — is put to the write guard before anything is applied,
    // so a refused patch changes nothing. (The ask above carries paths for
    // display; it is not what enforces this.)
    for change in &file_changes {
        crate::tool::file_path::verify(Path::new(&change.file_path), Path::new(&change.io_path))?;
        instance.about_to_modify(Path::new(&change.file_path))?;
        instance.about_to_modify(Path::new(&change.io_path))?;
        if let (Some(destination), Some(resolved)) = (&change.move_path, &change.move_io_path) {
            crate::tool::file_path::reject_move_aliases(
                Path::new(&change.file_path),
                Path::new(destination),
            )?;
            crate::tool::file_path::verify(Path::new(destination), Path::new(resolved))?;
            instance.about_to_modify(Path::new(destination))?;
            instance.about_to_modify(Path::new(resolved))?;
        }
    }

    // Apply the changes (apply_patch.ts:217-258).
    let mut updates: Vec<(String, &'static str)> = Vec::new();
    for change in &file_changes {
        let edited = if change.kind == "delete" {
            None
        } else {
            change
                .move_io_path
                .as_deref()
                .or(Some(change.io_path.as_str()))
        };
        match change.kind {
            "add" => {
                write_all(
                    instance,
                    &change.io_path,
                    &bom::join(&change.new_content, change.bom),
                )
                .await?;
                updates.push((change.file_path.clone(), "add"));
            }
            "update" => {
                write_all(
                    instance,
                    &change.io_path,
                    &bom::join(&change.new_content, change.bom),
                )
                .await?;
                updates.push((change.file_path.clone(), "change"));
            }
            "move" => {
                if let Some(move_path) = &change.move_io_path {
                    write_all(
                        instance,
                        move_path,
                        &bom::join(&change.new_content, change.bom),
                    )
                    .await?;
                    remove(instance, &change.io_path).await?;
                    updates.push((change.file_path.clone(), "unlink"));
                    updates.push((change.move_path.clone().unwrap(), "add"));
                }
            }
            "delete" => {
                remove(instance, &change.file_path).await?;
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
                            write_all(instance, edited, &joined).await?;
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

async fn write_all(
    instance: &crate::tool::def::InstanceContext,
    path: &str,
    contents: &str,
) -> Result<(), ToolError> {
    instance
        .write_file(Path::new(path), contents.as_bytes())
        .await
        .map_err(|error| {
            error.into_tool_error(
                |parent, error| format!("Failed to create directory {}: {error}", parent.display()),
                |error| format!("Failed to write file {path}: {error}"),
            )
        })
}

/// Every removal of the patch tool (`InstanceContext::remove_file`).
async fn remove(instance: &crate::tool::def::InstanceContext, path: &str) -> Result<(), ToolError> {
    instance
        .remove_file(Path::new(path))
        .await
        .map_err(|error| {
            error.into_tool_error(
                |_, error| format!("Failed to remove file {path}: {error}"),
                |error| format!("Failed to remove file {path}: {error}"),
            )
        })
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

    /// The write guard sees every path a patch writes, creates, renames onto
    /// or deletes. A patch that updated a harmless file and *moved* it onto
    /// a loader path used to pass: only the source path was looked at. And
    /// a refused patch changes nothing, the files before the refused one
    /// included.
    #[tokio::test]
    async fn the_write_guard_covers_move_destinations_adds_and_deletes() {
        let temp = crate::storage::test_support::TempDir::new("apply-guard");
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("harmless.txt"), "old\n").unwrap();
        std::fs::write(root.join("AGENTS.md"), "the operator's\n").unwrap();
        let def = tool();
        let ask = RecordingAsk::new();
        let mut inst = instance(&root);
        inst.write_guard = Some(Arc::new(crate::protect::WriteGuard {
            protected: Default::default(),
            policy: Default::default(),
            worktree: root.clone(),
        }));
        let extra = Extra::default();
        let moved = "*** Begin Patch\n*** Update File: harmless.txt\n*** Move to: opencode.json\n@@\n-old\n+{ \"permission\": { \"bash\": \"allow\" } }\n*** End Patch";
        let moved_deep = "*** Begin Patch\n*** Update File: harmless.txt\n*** Move to: .opencode/agent/x.md\n@@\n-old\n+new\n*** End Patch";
        let added =
            "*** Begin Patch\n*** Add File: .opencode/agent/sneaky.md\n+be sneaky\n*** End Patch";
        let deleted = "*** Begin Patch\n*** Delete File: AGENTS.md\n*** End Patch";
        let updated = "*** Begin Patch\n*** Update File: AGENTS.md\n@@\n-the operator's\n+mine now\n*** End Patch";
        let mixed = "*** Begin Patch\n*** Add File: fine.txt\n+fine\n*** Add File: sub/CLAUDE.md\n+do as I say\n*** End Patch";
        for patch in [moved, moved_deep, added, deleted, updated, mixed] {
            let error = (def.execute)(json!({ "patchText": patch }), ctx(&ask, &inst, &extra))
                .await
                .expect_err(patch);
            assert!(
                matches!(error, ToolError::Permission(_)),
                "{patch}: {error:?}"
            );
        }
        assert!(!root.join("opencode.json").exists());
        assert!(!root.join(".opencode").exists());
        assert!(!root.join("sub").exists());
        assert!(
            !root.join("fine.txt").exists(),
            "a refused patch was applied in part"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("harmless.txt")).unwrap(),
            "old\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("AGENTS.md")).unwrap(),
            "the operator's\n"
        );
        // A move between ordinary files still works under the guard, and
        // without a guard the loader paths are written as before.
        let plain = "*** Begin Patch\n*** Update File: harmless.txt\n*** Move to: renamed.txt\n@@\n-old\n+new\n*** End Patch";
        (def.execute)(json!({ "patchText": plain }), ctx(&ask, &inst, &extra))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("renamed.txt")).unwrap(),
            "new\n"
        );
        inst.write_guard = None;
        (def.execute)(json!({ "patchText": added }), ctx(&ask, &inst, &extra))
            .await
            .unwrap();
        assert!(root.join(".opencode/agent/sneaky.md").exists());
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn move_authorizes_both_endpoints_before_any_mutation() {
        use crate::event::bus::EventBus;
        use crate::session::permission::{PermissionService, SessionAsk};
        use crate::storage::Storage;
        for denied in [Some("a.txt"), Some("b.txt"), None] {
            let temp = crate::storage::test_support::TempDir::new("apply-move-rules");
            std::fs::write(temp.path().join("a.txt"), "source\n").unwrap();
            std::fs::write(temp.path().join("b.txt"), "protected\n").unwrap();
            let events = Arc::new(EventBus::new(Storage::open_in_memory().unwrap(), None));
            let service = Arc::new(PermissionService::new(events));
            let mut rules = json!([{"permission":"edit", "pattern":"*", "action":"allow"}]);
            if let Some(path) = denied {
                rules
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"permission":"edit", "pattern":path, "action":"deny"}));
            }
            let ask = SessionAsk::new(
                service,
                "session",
                "message",
                "call",
                serde_json::from_value::<crate::tool::permission::Ruleset>(rules).unwrap(),
            );
            let recorder = RecordingAsk::new();
            let inst = instance(temp.path());
            let extra = Extra::default();
            let mut context = ctx(&recorder, &inst, &extra);
            context.ask = &ask;
            let patch = "*** Begin Patch\n*** Add File: new.txt\n+new\n*** Update File: a.txt\n*** Move to: ./sub/../b.txt\n@@\n-source\n+replacement\n*** End Patch";
            let result = (tool().execute)(json!({"patchText":patch}), context).await;
            if denied.is_some() {
                assert!(result.is_err(), "{denied:?}");
                assert_eq!(
                    std::fs::read_to_string(temp.path().join("a.txt")).unwrap(),
                    "source\n"
                );
                assert_eq!(
                    std::fs::read_to_string(temp.path().join("b.txt")).unwrap(),
                    "protected\n"
                );
                assert!(!temp.path().join("new.txt").exists());
            } else {
                result.unwrap();
                assert!(!temp.path().join("a.txt").exists());
                assert_eq!(
                    std::fs::read_to_string(temp.path().join("b.txt")).unwrap(),
                    "replacement\n"
                );
            }
        }
    }

    #[tokio::test]
    async fn move_onto_itself_keeps_the_file() {
        let temp = crate::storage::test_support::TempDir::new("apply-self-move");
        std::fs::write(temp.path().join("a.txt"), "one\ntwo\n").unwrap();
        let patch = concat!(
            "*** Begin Patch\n",
            "*** Update File: a.txt\n",
            "*** Move to: ./a.txt\n",
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
        (def.execute)(json!({ "patchText": patch }), ctx)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(temp.path().join("a.txt")).unwrap(),
            "one\ndeux\n"
        );
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn dotdot_path_asks_external_directory() {
        let temp = crate::storage::test_support::TempDir::new("apply-dotdot");
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let patch = concat!(
            "*** Begin Patch\n",
            "*** Add File: ../escaped.txt\n",
            "+hi\n",
            "*** End Patch",
        );
        let def = tool();
        let ask = RecordingAsk::new();
        let inst = instance(&project);
        let extra = Extra::default();
        let ctx = ctx(&ask, &inst, &extra);
        (def.execute)(json!({ "patchText": patch }), ctx)
            .await
            .unwrap();
        let external: Vec<_> = ask
            .requests()
            .into_iter()
            .filter(|request| request.permission == "external_directory")
            .collect();
        assert_eq!(external.len(), 1, "a ../ write must prompt");
        assert_eq!(
            external[0].patterns,
            vec![format!("{}/*", temp.path().display())]
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

//! `write` tool — port of `tool/write.ts` (spec M4.3).

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::format::Formatter;
use crate::tool::bom;
use crate::tool::def::{define, Agents, AskRequest, BoxFuture, ExecuteResult, ToolCtxRef, ToolDef};
use crate::tool::diff::create_two_files_patch;
use crate::tool::edit::{diagnostic_report, trim_diff, FileEvents, Lsp};
use crate::tool::error::ToolError;
use crate::tool::external_directory::{assert_external_directory, ExternalOptions};
use crate::tool::ripgrep::{ts_relative, ts_resolve};
use crate::tool::truncate::Truncate;

const MAX_PROJECT_DIAGNOSTICS_FILES: usize = 5;

#[derive(Debug, Deserialize)]
pub struct WriteParameters {
    pub content: String,
    #[serde(rename = "filePath")]
    pub file_path: String,
}

/// Hand-authored JSON Schema (byte-matches the Effect-generated schema; see
/// `tests/golden/toolschema/write.json`).
pub fn parameters() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {
            "content": {
                "type": "string",
                "description": "The content to write to the file"
            },
            "filePath": {
                "type": "string",
                "description": "The absolute path to the file to write (must be absolute, not relative)"
            }
        },
        "required": ["content", "filePath"]
    })
}

/// Build the `write` tool. `lsp`/`format`/`events` are the M4.3 seams
/// (`None` = no LSP, no formatter, no event bridge).
pub fn write_tool(
    truncate: Arc<dyn Truncate>,
    agents: Arc<dyn Agents>,
    lsp: Option<Arc<dyn Lsp>>,
    format: Option<Arc<dyn Formatter>>,
    events: Option<Arc<dyn FileEvents>>,
) -> ToolDef {
    define(
        "write",
        include_str!("txt/write.txt"),
        parameters(),
        None,
        truncate,
        agents,
        move |params: WriteParameters, ctx: ToolCtxRef<'_>| {
            run(params, ctx, lsp.clone(), format.clone(), events.clone())
        },
    )
}

fn run(
    params: WriteParameters,
    ctx: ToolCtxRef<'_>,
    lsp: Option<Arc<dyn Lsp>>,
    format: Option<Arc<dyn Formatter>>,
    events: Option<Arc<dyn FileEvents>>,
) -> BoxFuture<'_, Result<ExecuteResult, ToolError>> {
    Box::pin(async move {
        let instance = ctx.instance.clone();
        let file_path = ts_resolve(&instance.directory, &params.file_path);
        let file_str = file_path.to_string_lossy().to_string();
        assert_external_directory(&ctx, Some(&file_str), ExternalOptions::default()).await?;

        let exists = tokio::fs::metadata(&file_path).await.is_ok();
        let source = if exists {
            bom::read_file(&file_path).await?
        } else {
            bom::split("")
        };
        let next = bom::split(&params.content);
        let desired_bom = source.bom || next.bom;
        let content_old = source.text;
        let content_new = next.text;

        let diff = trim_diff(&create_two_files_patch(
            &file_str,
            &content_old,
            &content_new,
        ));
        ctx.ask
            .ask(AskRequest {
                permission: "edit".to_string(),
                patterns: vec![ts_relative(&instance.worktree, &file_path)],
                always: vec!["*".to_string()],
                metadata: json!({
                    "filepath": file_str,
                    "diff": diff,
                }),
            })
            .await?;

        bom::write_with_dirs(&file_path, &bom::join(&content_new, desired_bom)).await?;
        if let Some(formatter) = &format {
            if formatter.file(&file_str).await {
                bom::sync_file(&file_path, desired_bom).await?;
            }
        }
        if let Some(events) = &events {
            events.edited(&file_str).await;
            events
                .updated(&file_str, if exists { "change" } else { "add" })
                .await;
        }

        let mut output = "Wrote file successfully.".to_string();
        let diagnostics = match &lsp {
            Some(lsp) => {
                lsp.touch_file(&file_str).await;
                lsp.diagnostics().await
            }
            None => json!({}),
        };
        // FSUtil.normalizePath is identity on non-win32.
        let mut project_diagnostics_count = 0usize;
        if let Some(map) = diagnostics.as_object() {
            for (file, issues) in map {
                let current = file == &file_str;
                if !current && project_diagnostics_count >= MAX_PROJECT_DIAGNOSTICS_FILES {
                    continue;
                }
                let block =
                    diagnostic_report(if current { &file_str } else { file.as_str() }, issues);
                if block.is_empty() {
                    continue;
                }
                if current {
                    output.push_str(&format!(
                        "\n\nLSP errors detected in this file, please fix:\n{block}"
                    ));
                    continue;
                }
                project_diagnostics_count += 1;
                output.push_str(&format!("\n\nLSP errors detected in other files:\n{block}"));
            }
        }

        Ok(ExecuteResult {
            title: ts_relative(&instance.worktree, &file_path),
            metadata: json!({
                "diagnostics": diagnostics,
                "filepath": file_str,
                "exists": exists,
            }),
            output,
            attachments: None,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::def::{Extra, MetadataInput};
    use crate::tool::ripgrep::test_support::{ctx, fixed_agents, instance, RecordingAsk};
    use crate::tool::truncate::TruncateService;
    use std::path::Path;
    use std::sync::Mutex;

    struct FixedLsp {
        diagnostics: Value,
    }

    impl Lsp for FixedLsp {
        fn touch_file<'a>(&'a self, _file: &'a str) -> BoxFuture<'a, ()> {
            Box::pin(async {})
        }

        fn diagnostics<'a>(&'a self) -> BoxFuture<'a, Value> {
            Box::pin(async move { self.diagnostics.clone() })
        }
    }

    #[derive(Default)]
    struct RecordingEvents {
        calls: Mutex<Vec<String>>,
    }

    impl RecordingEvents {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl FileEvents for RecordingEvents {
        fn edited<'a>(&'a self, file: &'a str) -> BoxFuture<'a, ()> {
            Box::pin(async move {
                self.calls.lock().unwrap().push(format!("edited:{file}"));
            })
        }

        fn updated<'a>(&'a self, file: &'a str, event: &'a str) -> BoxFuture<'a, ()> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .unwrap()
                    .push(format!("updated:{file}:{event}"));
            })
        }
    }

    fn tool(dir: &Path, lsp: Option<Arc<dyn Lsp>>, events: Arc<dyn FileEvents>) -> ToolDef {
        write_tool(
            Arc::new(TruncateService::default_limits(dir.join("tool-output"))),
            fixed_agents(),
            lsp,
            None,
            Some(events),
        )
    }

    async fn call(
        dir: &Path,
        lsp: Option<Arc<dyn Lsp>>,
        events: Arc<dyn FileEvents>,
        args: Value,
    ) -> (
        Result<ExecuteResult, ToolError>,
        Vec<crate::tool::def::AskRequest>,
        Vec<MetadataInput>,
    ) {
        let def = tool(dir, lsp, events);
        let ask = RecordingAsk::new();
        let inst = instance(dir);
        let extra = Extra::default();
        let c = ctx(&ask, &inst, &extra);
        let result = (def.execute)(args, c).await;
        let metadata = ask.metadata_calls.lock().unwrap().clone();
        (result, ask.requests(), metadata)
    }

    fn events() -> Arc<RecordingEvents> {
        Arc::new(RecordingEvents::default())
    }

    #[test]
    fn golden_parameters_schema() {
        let golden = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/golden/toolschema/write.json"
        ))
        .unwrap();
        let golden: Value = serde_json::from_str(&golden).unwrap();
        assert_eq!(parameters(), golden);
    }

    #[tokio::test]
    async fn writes_a_new_file_with_parent_dirs() {
        let temp = crate::storage::test_support::TempDir::new("write-new");
        let file = temp.path().join("nested/dir/file.txt");
        let events = events();
        let (result, requests, _) = call(
            temp.path(),
            None,
            events.clone(),
            json!({
                "content": "hello\n",
                "filePath": file.to_string_lossy(),
            }),
        )
        .await;

        let result = result.unwrap();
        assert_eq!(result.output, "Wrote file successfully.");
        assert_eq!(result.title, "nested/dir/file.txt");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello\n");
        assert_eq!(result.metadata["exists"], json!(false));
        assert_eq!(
            result.metadata["filepath"],
            json!(file.to_string_lossy().to_string())
        );
        assert_eq!(result.metadata["diagnostics"], json!({}));

        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].permission, "edit");
        assert_eq!(
            requests[0].patterns,
            vec!["nested/dir/file.txt".to_string()]
        );
        // The patch names the resolved absolute file path.
        assert_eq!(
            requests[0].metadata["diff"].as_str().unwrap(),
            format!(
                "Index: {file}\n===================================================================\n--- {file}\n+++ {file}\n@@ -0,0 +1,1 @@\n+hello\n",
                file = file.to_string_lossy()
            )
        );

        assert_eq!(
            events.calls(),
            vec![
                format!("edited:{}", file.display()),
                format!("updated:{}:add", file.display()),
            ]
        );
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn overwrites_existing_file() {
        let temp = crate::storage::test_support::TempDir::new("write-existing");
        let file = temp.path().join("file.txt");
        std::fs::write(&file, "old\ncontents\n").unwrap();
        let events = events();
        let (result, _, _) = call(
            temp.path(),
            None,
            events.clone(),
            json!({
                "content": "new\n",
                "filePath": file.to_string_lossy(),
            }),
        )
        .await;

        let result = result.unwrap();
        assert_eq!(result.metadata["exists"], json!(true));
        assert_eq!(
            events.calls(),
            vec![
                format!("edited:{}", file.display()),
                format!("updated:{}:change", file.display()),
            ]
        );
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn source_bom_is_preferred_over_next() {
        let temp = crate::storage::test_support::TempDir::new("write-bom");
        let file = temp.path().join("file.txt");
        std::fs::write(&file, "\u{feff}old\n").unwrap();
        let (result, _, _) = call(
            temp.path(),
            None,
            events(),
            json!({
                "content": "new\n",
                "filePath": file.to_string_lossy(),
            }),
        )
        .await;

        result.unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "\u{feff}new\n");
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn diagnostics_are_capped_at_five_project_files() {
        let temp = crate::storage::test_support::TempDir::new("write-cap");
        let file = temp.path().join("file.txt");
        let file_str = file.to_string_lossy().to_string();
        let issue = |line: u64| {
            json!({
                "severity": 1,
                "message": format!("err {line}"),
                "range": { "start": { "line": line, "character": 0 } },
            })
        };
        let mut map = serde_json::Map::new();
        map.insert(file_str.clone(), json!([issue(0)]));
        for index in 0..7 {
            map.insert(format!("/other/file{index}.rs"), json!([issue(1)]));
        }
        let lsp = Arc::new(FixedLsp {
            diagnostics: Value::from(map),
        });
        let (result, _, _) = call(
            temp.path(),
            Some(lsp),
            events(),
            json!({
                "content": "new\n",
                "filePath": file.to_string_lossy(),
            }),
        )
        .await;

        let output = result.unwrap().output;
        assert!(output.contains("Wrote file successfully."), "{output}");
        assert!(
            output.contains("LSP errors detected in this file, please fix:"),
            "{output}"
        );
        assert_eq!(
            output
                .matches("LSP errors detected in other files:")
                .count(),
            MAX_PROJECT_DIAGNOSTICS_FILES,
            "{output}"
        );
        std::fs::remove_dir_all(temp.path()).ok();
    }
}

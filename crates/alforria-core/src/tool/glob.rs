//! `glob` tool — port of `tool/glob.ts` (spec M4.2).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::tool::def::{define, Agents, AskRequest, BoxFuture, ExecuteResult, ToolCtxRef, ToolDef};
use crate::tool::error::ToolError;
use crate::tool::external_directory::{assert_external_directory, ExternalOptions, Kind};
use crate::tool::ripgrep::{ts_relative, ts_resolve, Ripgrep};
use crate::tool::truncate::Truncate;

const LIMIT: usize = 100;

#[derive(Debug, Deserialize)]
pub struct GlobParameters {
    pub pattern: String,
    pub path: Option<String>,
}

/// Hand-authored JSON Schema (byte-matches the Effect-generated schema; see
/// `tests/golden/toolschema/glob.json`).
pub fn parameters() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {
            "pattern": {
                "type": "string",
                "description": "The glob pattern to match files against"
            },
            "path": {
                "type": "string",
                "description": "The directory to search in. If not specified, the current working directory will be used. IMPORTANT: Omit this field to use the default directory. DO NOT enter \"undefined\" or \"null\" - simply omit it for the default behavior. Must be a valid directory path if provided."
            }
        },
        "required": ["pattern"]
    })
}

/// Build the `glob` tool.
pub fn glob_tool(
    truncate: Arc<dyn Truncate>,
    agents: Arc<dyn Agents>,
    ripgrep: Arc<dyn Ripgrep>,
) -> ToolDef {
    define(
        "glob",
        include_str!("txt/glob.txt"),
        parameters(),
        None,
        truncate,
        agents,
        move |params: GlobParameters, ctx: ToolCtxRef<'_>| run(params, ctx, Arc::clone(&ripgrep)),
    )
}

fn run(
    params: GlobParameters,
    ctx: ToolCtxRef<'_>,
    ripgrep: Arc<dyn Ripgrep>,
) -> BoxFuture<'_, Result<ExecuteResult, ToolError>> {
    Box::pin(async move {
        // ask happens FIRST — before any path validation (glob.ts:28-37).
        ctx.ask
            .ask(AskRequest {
                permission: "glob".to_string(),
                patterns: vec![params.pattern.clone()],
                always: vec!["*".to_string()],
                metadata: ask_metadata(&params),
            })
            .await?;

        let instance = ctx.instance;
        // `params.path ?? ins.directory`; absolute paths pass through
        // untouched, relative ones resolve against the instance dir.
        let search = match params.path.as_deref() {
            Some(path) if Path::new(path).is_absolute() => PathBuf::from(path),
            Some(path) => ts_resolve(&instance.directory, path),
            None => instance.directory.clone(),
        };

        let info = tokio::fs::metadata(&search).await.ok();
        if info.map(|info| info.is_file()).unwrap_or(false) {
            return Err(ToolError::Failed(format!(
                "glob path must be a directory: {}",
                search.display()
            )));
        }
        assert_external_directory(
            &ctx,
            Some(search.to_string_lossy().as_ref()),
            ExternalOptions {
                bypass: false,
                kind: Kind::Directory,
            },
        )
        .await?;

        let files = ripgrep.glob(&search, &params.pattern, LIMIT);
        let truncated = files.len() == LIMIT;

        let mut output: Vec<String> = Vec::new();
        if files.is_empty() {
            output.push("No files found".to_string());
        } else {
            for file in &files {
                output.push(
                    ts_resolve(&search, file.to_string_lossy().as_ref())
                        .display()
                        .to_string(),
                );
            }
            if truncated {
                output.push(String::new());
                output.push("(Results are truncated: showing first 100 results. Consider using a more specific path or pattern.)".to_string());
            }
        }

        Ok(ExecuteResult {
            title: ts_relative(&instance.worktree, &search),
            metadata: json!({
                "count": files.len(),
                "truncated": truncated,
            }),
            output: output.join("\n"),
            attachments: None,
        })
    })
}

fn ask_metadata(params: &GlobParameters) -> Value {
    let mut metadata = serde_json::Map::new();
    metadata.insert("pattern".to_string(), json!(params.pattern));
    if let Some(path) = &params.path {
        metadata.insert("path".to_string(), json!(path));
    }
    Value::Object(metadata)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::def::Extra;
    use crate::tool::ripgrep::test_support::{ctx, fixed_agents, instance, RecordingAsk};
    use crate::tool::truncate::TruncateService;
    use serde_json::json;

    fn tool(dir: &std::path::Path) -> ToolDef {
        glob_tool(
            Arc::new(TruncateService::default_limits(dir.join("tool-output"))),
            fixed_agents(),
            Arc::new(crate::tool::ripgrep::RipgrepService),
        )
    }

    fn write(dir: &std::path::Path, name: &str, contents: &str) {
        if let Some(parent) = dir.join(name).parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(dir.join(name), contents).unwrap();
    }

    async fn call(
        dir: &std::path::Path,
        args: serde_json::Value,
    ) -> (
        Result<ExecuteResult, ToolError>,
        Vec<crate::tool::def::AskRequest>,
    ) {
        let def = tool(dir);
        let ask = RecordingAsk::new();
        let inst = instance(dir);
        let extra = Extra::default();
        let ctx = ctx(&ask, &inst, &extra);
        let result = (def.execute)(args, ctx).await;
        (result, ask.requests())
    }

    #[test]
    fn golden_parameters_schema() {
        let golden = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/golden/toolschema/glob.json"
        ))
        .unwrap();
        let golden: serde_json::Value = serde_json::from_str(&golden).unwrap();
        assert_eq!(parameters(), golden);
    }

    #[tokio::test]
    async fn ask_precedes_path_validation() {
        let temp = crate::storage::test_support::TempDir::new("glob-ask");
        write(temp.path(), "a.txt", "x");

        let (result, requests) = call(temp.path(), json!({ "pattern": "*.txt" })).await;

        result.unwrap();
        assert_eq!(requests[0].permission, "glob");
        assert_eq!(requests[0].patterns, vec!["*.txt".to_string()]);
        assert_eq!(requests[0].always, vec!["*".to_string()]);
        assert_eq!(requests[0].metadata, json!({"pattern": "*.txt"}));
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn lists_resolved_paths_with_relative_title() {
        let temp = crate::storage::test_support::TempDir::new("glob-list");
        write(temp.path(), "a.txt", "x");
        write(temp.path(), "sub/b.txt", "x");
        write(temp.path(), "c.md", "x");

        let (result, _) = call(temp.path(), json!({ "pattern": "**/*.txt" })).await;

        let result = result.unwrap();
        assert_eq!(result.title, "");
        // Walk order is filesystem dependent; both files must be resolved
        // to absolute paths, one per line.
        let mut listed = result
            .output
            .split('\n')
            .map(String::from)
            .collect::<Vec<_>>();
        listed.sort();
        assert_eq!(
            listed,
            vec![
                temp.path().join("a.txt").display().to_string(),
                temp.path().join("sub/b.txt").display().to_string(),
            ]
        );
        assert_eq!(result.metadata, json!({ "count": 2, "truncated": false }));
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn no_files_found_and_glob_on_file_errors() {
        let temp = crate::storage::test_support::TempDir::new("glob-empty");
        write(temp.path(), "a.md", "x");

        let (result, _) = call(temp.path(), json!({ "pattern": "*.txt" })).await;
        let out = result.unwrap();
        assert_eq!(out.output, "No files found");
        assert_eq!(out.metadata, json!({ "count": 0, "truncated": false }));

        // A file path errors out.
        let (result, _) = call(
            temp.path(),
            json!({ "pattern": "*.md", "path": temp.path().join("a.md").to_string_lossy() }),
        )
        .await;
        assert_eq!(
            result.unwrap_err().to_string(),
            format!(
                "glob path must be a directory: {}",
                temp.path().join("a.md").display()
            )
        );
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn truncates_at_exactly_one_hundred_results() {
        for (count, truncated) in [(99usize, false), (100usize, true), (101usize, true)] {
            let temp = crate::storage::test_support::TempDir::new("glob-limit");
            for i in 0..count {
                write(temp.path(), &format!("f{i}.txt"), "x");
            }
            let (result, _) = call(temp.path(), json!({ "pattern": "*.txt" })).await;
            let result = result.unwrap();
            assert_eq!(
                result.metadata["truncated"],
                json!(truncated),
                "{count} files"
            );
            assert_eq!(result.metadata["count"], json!(count.min(100)));
            if truncated {
                assert!(
                    result.output.ends_with(
                        "\n\n(Results are truncated: showing first 100 results. Consider using a more specific path or pattern.)"
                    ),
                    "{}",
                    result.output
                );
            } else {
                assert!(!result.output.contains("Results are truncated"));
            }
            std::fs::remove_dir_all(temp.path()).ok();
        }
    }

    #[tokio::test]
    async fn relative_path_resolves_against_the_instance_dir() {
        let temp = crate::storage::test_support::TempDir::new("glob-relpath");
        write(temp.path(), "sub/b.txt", "x");

        let (result, _) = call(temp.path(), json!({ "pattern": "*.txt", "path": "sub" })).await;

        let result = result.unwrap();
        assert_eq!(result.title, "sub");
        assert_eq!(
            result.output,
            temp.path().join("sub/b.txt").display().to_string()
        );
        std::fs::remove_dir_all(temp.path()).ok();
    }
}

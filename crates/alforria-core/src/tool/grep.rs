//! `grep` tool — port of `tool/grep.ts` (spec M4.2).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::tool::def::{define, Agents, AskRequest, BoxFuture, ExecuteResult, ToolCtxRef, ToolDef};
use crate::tool::error::ToolError;
use crate::tool::external_directory::{assert_external_directory, ExternalOptions, Kind};
use crate::tool::ripgrep::{ts_dirname, ts_resolve, Ripgrep};
use crate::tool::truncate::Truncate;

const LIMIT: usize = 100;

#[derive(Debug, Deserialize)]
pub struct GrepParameters {
    pub pattern: String,
    pub path: Option<String>,
    pub include: Option<String>,
}

/// Hand-authored JSON Schema (byte-matches the Effect-generated schema; see
/// `tests/golden/toolschema/grep.json`).
pub fn parameters() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {
            "pattern": {
                "type": "string",
                "description": "The regex pattern to search for in file contents"
            },
            "path": {
                "type": "string",
                "description": "The directory to search in. Defaults to the current working directory."
            },
            "include": {
                "type": "string",
                "description": "File pattern to include in the search (e.g. \"*.js\", \"*.{ts,tsx}\")"
            }
        },
        "required": ["pattern"]
    })
}

/// Build the `grep` tool.
pub fn grep_tool(
    truncate: Arc<dyn Truncate>,
    agents: Arc<dyn Agents>,
    ripgrep: Arc<dyn Ripgrep>,
) -> ToolDef {
    define(
        "grep",
        include_str!("txt/grep.txt"),
        parameters(),
        None,
        truncate,
        agents,
        move |params: GrepParameters, ctx: ToolCtxRef<'_>| run(params, ctx, Arc::clone(&ripgrep)),
    )
}

fn run(
    params: GrepParameters,
    ctx: ToolCtxRef<'_>,
    ripgrep: Arc<dyn Ripgrep>,
) -> BoxFuture<'_, Result<ExecuteResult, ToolError>> {
    Box::pin(async move {
        if params.pattern.is_empty() {
            return Err(ToolError::Failed("pattern is required".to_string()));
        }

        ctx.ask
            .ask(AskRequest {
                permission: "grep".to_string(),
                patterns: vec![params.pattern.clone()],
                always: vec!["*".to_string()],
                metadata: ask_metadata(&params),
            })
            .await?;

        let instance = ctx.instance;
        let requested: PathBuf = match params.path.as_deref() {
            Some(path) if Path::new(path).is_absolute() => PathBuf::from(path),
            Some(path) => instance.directory.join(path),
            None => instance.directory.clone(),
        };

        let requested_info = tokio::fs::metadata(&requested).await.ok();
        assert_external_directory(
            &ctx,
            Some(requested.to_string_lossy().as_ref()),
            ExternalOptions {
                bypass: false,
                kind: match &requested_info {
                    Some(info) if info.is_dir() => Kind::Directory,
                    _ => Kind::File,
                },
            },
        )
        .await?;

        let search = fsutil_resolve(&requested);
        let info = tokio::fs::metadata(&search).await.ok();
        let cwd = match info {
            Some(info) if info.is_dir() => search,
            _ => ts_dirname(&search),
        };

        let result = ripgrep
            .grep(&cwd, &params.pattern, params.include.as_deref(), LIMIT)
            .map_err(ToolError::Failed)?;
        if result.is_empty() {
            return empty_result(&params.pattern);
        }

        let base = match &requested_info {
            Some(info) if info.is_dir() => requested.clone(),
            _ => ts_dirname(&requested),
        };
        let rows: Vec<(PathBuf, u64, String)> = result
            .iter()
            .map(|item| (ts_resolve(&base, &item.path), item.line, item.text.clone()))
            .collect();
        if rows.is_empty() {
            return empty_result(&params.pattern);
        }

        let truncated = rows.len() == LIMIT;
        let has_more = truncated || result.len() == LIMIT;
        let mut output = vec![format!(
            "Found {} matches{}",
            rows.len(),
            if has_more {
                " (more matches available)"
            } else {
                ""
            }
        )];

        let mut current = String::new();
        for (path, line, text) in &rows {
            let path = path.display().to_string();
            if current != path {
                if !current.is_empty() {
                    output.push(String::new());
                }
                current = path;
                output.push(format!("{current}:"));
            }
            output.push(format!("  Line {line}: {text}"));
        }

        if truncated {
            output.push(String::new());
            output.push(
                "(Results truncated. Consider using a more specific path or pattern.)".to_string(),
            );
        }

        Ok(ExecuteResult {
            title: params.pattern.clone(),
            metadata: json!({
                "matches": rows.len(),
                "truncated": truncated,
            }),
            output: output.join("\n"),
            attachments: None,
        })
    })
}

fn empty_result(pattern: &str) -> Result<ExecuteResult, ToolError> {
    Ok(ExecuteResult {
        title: pattern.to_string(),
        metadata: json!({ "matches": 0, "truncated": false }),
        output: "No files found".to_string(),
        attachments: None,
    })
}

/// `FSUtil.resolve` (fs-util.ts:247-253) — realpath, falling back to the
/// lexical path when the file does not exist.
fn fsutil_resolve(path: &Path) -> PathBuf {
    match std::fs::canonicalize(path) {
        Ok(resolved) => resolved,
        Err(_) => path.to_path_buf(),
    }
}

fn ask_metadata(params: &GrepParameters) -> Value {
    let mut metadata = serde_json::Map::new();
    metadata.insert("pattern".to_string(), json!(params.pattern));
    if let Some(path) = &params.path {
        metadata.insert("path".to_string(), json!(path));
    }
    if let Some(include) = &params.include {
        metadata.insert("include".to_string(), json!(include));
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

    fn tool(dir: &Path) -> ToolDef {
        grep_tool(
            Arc::new(TruncateService::default_limits(dir.join("tool-output"))),
            fixed_agents(),
            Arc::new(crate::tool::ripgrep::RipgrepService),
        )
    }

    fn write(dir: &Path, name: &str, contents: &str) {
        std::fs::write(dir.join(name), contents).unwrap();
    }

    async fn call(
        dir: &Path,
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
            "/tests/golden/toolschema/grep.json"
        ))
        .unwrap();
        let golden: serde_json::Value = serde_json::from_str(&golden).unwrap();
        assert_eq!(parameters(), golden);
    }

    #[tokio::test]
    async fn empty_pattern_is_required() {
        let temp = crate::storage::test_support::TempDir::new("grep-empty");
        let (result, _) = call(temp.path(), json!({ "pattern": "" })).await;
        assert_eq!(result.unwrap_err().to_string(), "pattern is required");
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn groups_matches_by_path() {
        let temp = crate::storage::test_support::TempDir::new("grep-group");
        write(temp.path(), "a.txt", "hello world\nother\n");
        write(temp.path(), "b.txt", "hello again\n");

        let (result, _) = call(temp.path(), json!({ "pattern": "hello" })).await;
        let result = result.unwrap();

        let a = temp.path().join("a.txt").display().to_string();
        let b = temp.path().join("b.txt").display().to_string();
        // Group order follows the (filesystem-dependent) walk order; both
        // groupings are acceptable. Note the blank line after every match:
        // the row text keeps rg's trailing newline.
        let expected_a = format!("{a}:\n  Line 1: hello world\n");
        let expected_b = format!("{b}:\n  Line 1: hello again\n");
        assert!(
            result.output == format!("Found 2 matches\n{expected_a}\n\n{expected_b}")
                || result.output == format!("Found 2 matches\n{expected_b}\n\n{expected_a}"),
            "{}",
            result.output
        );
        assert_eq!(result.title, "hello");
        assert_eq!(result.metadata, json!({ "matches": 2, "truncated": false }));
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn multiple_matches_in_one_file_share_a_group() {
        let temp = crate::storage::test_support::TempDir::new("grep-multi");
        write(temp.path(), "a.txt", "x\ny\nx\n");

        let (result, _) = call(temp.path(), json!({ "pattern": "x" })).await;
        let result = result.unwrap();
        let a = temp.path().join("a.txt").display().to_string();
        assert_eq!(
            result.output,
            format!("Found 2 matches\n{a}:\n  Line 1: x\n\n  Line 3: x\n")
        );
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn no_matches_and_include_filter() {
        let temp = crate::storage::test_support::TempDir::new("grep-include");
        write(temp.path(), "a.js", "match\n");
        write(temp.path(), "b.ts", "match\n");

        let (result, _) = call(temp.path(), json!({ "pattern": "nomatch" })).await;
        let out = result.unwrap();
        assert_eq!(out.output, "No files found");
        assert_eq!(out.metadata, json!({ "matches": 0, "truncated": false }));

        let (result, _) = call(
            temp.path(),
            json!({ "pattern": "match", "include": "*.js" }),
        )
        .await;
        let result = result.unwrap();
        assert_eq!(
            result.output,
            format!(
                "Found 1 matches\n{}:\n  Line 1: match\n",
                temp.path().join("a.js").display()
            )
        );
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn ask_metadata_and_truncation_boundaries() {
        let temp = crate::storage::test_support::TempDir::new("grep-limit");
        for i in 0..101 {
            write(temp.path(), &format!("f{i}.txt"), "match\n");
        }
        let (result, requests) = call(
            temp.path(),
            json!({ "pattern": "match", "include": "*.txt" }),
        )
        .await;
        let result = result.unwrap();

        assert_eq!(
            requests[0].metadata,
            json!({ "pattern": "match", "include": "*.txt" })
        );
        assert_eq!(result.metadata["matches"], json!(100));
        assert_eq!(result.metadata["truncated"], json!(true));
        assert!(result
            .output
            .starts_with("Found 100 matches (more matches available)"));
        assert!(result
            .output
            .ends_with("\n\n(Results truncated. Consider using a more specific path or pattern.)"));
        std::fs::remove_dir_all(temp.path()).ok();

        // 99 matches -> no truncation footer.
        let temp = crate::storage::test_support::TempDir::new("grep-limit2");
        for i in 0..99 {
            write(temp.path(), &format!("f{i}.txt"), "match\n");
        }
        let (result, _) = call(
            temp.path(),
            json!({ "pattern": "match", "include": "*.txt" }),
        )
        .await;
        let result = result.unwrap();
        assert_eq!(result.metadata["truncated"], json!(false));
        assert!(result.output.starts_with("Found 99 matches\n"));
        assert!(!result.output.contains("(Results truncated"));
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn searching_a_file_path_uses_its_parent() {
        let temp = crate::storage::test_support::TempDir::new("grep-file");
        write(temp.path(), "a.txt", "match\n");

        let (result, _) = call(
            temp.path(),
            json!({ "pattern": "match", "path": temp.path().join("a.txt").to_string_lossy() }),
        )
        .await;
        let result = result.unwrap();
        assert_eq!(
            result.output,
            format!(
                "Found 1 matches\n{}:\n  Line 1: match\n",
                temp.path().join("a.txt").display()
            )
        );
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn invalid_regex_fails() {
        let temp = crate::storage::test_support::TempDir::new("grep-bad");
        let (result, _) = call(temp.path(), json!({ "pattern": "(" })).await;
        let error = result.unwrap_err();
        assert!(
            matches!(&error, ToolError::Failed(msg) if msg.contains("regex parse error")),
            "{error}"
        );
        std::fs::remove_dir_all(temp.path()).ok();
    }
}

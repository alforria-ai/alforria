//! The tool definition seam — port of `tool/tool.ts` (spec M4.1).
//!
//! [`ToolDef`] is a registered tool (id, description, JSON-Schema parameters
//! and the *wrapped* execute fn). [`define`] is the TS `Tool.define` +
//! `wrap()` seam: it deserializes the LLM's arguments into the tool's typed
//! `Parameters` struct, runs the raw execute, and post-truncates the result
//! unless its metadata already carries a `truncated` key (tool.ts:107-148).

use std::borrow::Cow;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use crate::tool::error::ToolError;
use crate::tool::permission::Ruleset;
use crate::tool::truncate::{Options, Truncate};

/// Future alias — `futures::future::BoxFuture` without the `futures` dep
/// (spec §9 S3 keeps the dependency set of §7).
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The wrapped execute fn every tool registers. HRTB over the per-invocation
/// context lifetime.
pub type ExecuteFn = Arc<
    dyn for<'a> Fn(Value, ToolCtxRef<'a>) -> BoxFuture<'a, Result<ExecuteResult, ToolError>>
        + Send
        + Sync,
>;

/// `formatValidationError` (tool.ts:64) — custom formatting of a decode error
/// into the `detail` of an `InvalidArguments` failure.
pub type FormatValidationError = Arc<dyn Fn(&serde_json::Error) -> String + Send + Sync>;

/// A registered tool (TS `Tool.Def`). `execute` is the *wrapped* execute.
#[derive(Clone)]
pub struct ToolDef {
    pub id: &'static str,
    pub description: Cow<'static, str>,
    /// Hand-authored JSON Schema (spec §2.3).
    pub parameters: Value,
    pub format_validation_error: Option<FormatValidationError>,
    pub execute: ExecuteFn,
}

impl std::fmt::Debug for ToolDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolDef")
            .field("id", &self.id)
            .field("description", &self.description)
            .field("parameters", &self.parameters)
            .finish_non_exhaustive()
    }
}

/// `Omit<PermissionV1.Request, "id" | "sessionID" | "tool">`
/// (schema/v1/permission.ts:27-35). Shape-checked against the
/// `alforria-schema` `PermissionV1Request` (spec §9 S2).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct AskRequest {
    pub permission: String,
    pub patterns: Vec<String>,
    pub always: Vec<String>,
    pub metadata: Value,
}

/// `ctx.metadata(input)` — `{ title?, metadata? }`.
#[derive(Debug, Clone, Default)]
pub struct MetadataInput {
    pub title: Option<String>,
    pub metadata: Option<Value>,
}

/// TS `Context.extra` — the string-keyed hooks individual tools look up
/// (`bypassCwdCheck`, `bypassAgentCheck`, `promptOps`, `model`). Known keys
/// are typed; M4.5/M4.8 extend it with their seams.
#[derive(Debug, Clone, Default)]
pub struct Extra {
    pub bypass_cwd_check: bool,
    pub bypass_agent_check: bool,
    /// `extra.model` (websearch model name, spec M4.5).
    pub model: Option<Value>,
}

/// TS `InstanceState.context` (project/instance-context.ts:5-9).
#[derive(Debug, Clone)]
pub struct InstanceContext {
    pub directory: PathBuf,
    pub worktree: PathBuf,
}

/// `ctx.ask(...)` — permission requests flow through this seam; failures map
/// to [`ToolError::Permission`]. Ask answer semantics (once/always/reject)
/// live in M5.
pub trait Ask: Send + Sync {
    fn ask<'a>(&'a self, request: AskRequest) -> BoxFuture<'a, Result<(), ToolError>>;
}

/// `ctx.metadata(...)` — mid-execution metadata updates.
pub trait MetadataSink: Send + Sync {
    fn metadata<'a>(&'a self, input: MetadataInput) -> BoxFuture<'a, Result<(), ToolError>>;
}

/// Per-call context (TS `Tool.Context`, tool.ts:36-46).
pub struct ToolCtxRef<'a> {
    pub session_id: &'a str,
    pub message_id: &'a str,
    pub agent: &'a str,
    pub call_id: Option<&'a str>,
    /// AbortSignal.
    pub abort: tokio_util::sync::CancellationToken,
    /// `SessionV1.WithParts[]` — opaque to M4 tools except read's
    /// instruction seam.
    pub messages: &'a [Value],
    pub extra: &'a Extra,
    pub instance: &'a InstanceContext,
    pub ask: &'a dyn Ask,
    pub metadata: &'a dyn MetadataSink,
}

/// `Omit<SessionV1.FilePart, "id" | "sessionID" | "messageID">` (attachment
/// part of [`ExecuteResult`]).
#[derive(Debug, Clone, serde::Serialize)]
pub struct Attachment {
    #[serde(rename = "type")]
    pub kind: &'static str, // "file"
    pub mime: String,
    /// `data:{mime};base64,...`
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
}

/// TS `ExecuteResult` (tool.ts:48-53).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ExecuteResult {
    pub title: String,
    pub metadata: Value,
    pub output: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<Attachment>>,
}

/// `Agent.Info.mode` (agent/agent.ts:35-56).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentMode {
    Subagent,
    Primary,
    All,
}

/// The slice of `Agent.Info` the tool system needs in M4 (agent/agent.ts:35).
#[derive(Debug, Clone)]
pub struct AgentInfo {
    pub name: String,
    pub description: Option<String>,
    pub mode: AgentMode,
    pub permission: Ruleset,
}

/// The `Agent.Service` seam (`agents.get` / `agents.list`).
pub trait Agents: Send + Sync {
    fn get<'a>(&'a self, agent: &'a str) -> BoxFuture<'a, Result<AgentInfo, ToolError>>;
    fn list<'a>(&'a self) -> BoxFuture<'a, Vec<AgentInfo>>;
}

/// `Tool.define` + `wrap` (tool.ts:99-148): register a raw execute fn against
/// the seam.
///
/// * deserialize args into `P` — failure produces
///   [`ToolError::InvalidArguments`] with `detail = format_validation_error(err)`
///   when provided, else the serde error string (TS: `String(error)`);
/// * run the raw execute;
/// * if `result.metadata["truncated"]` is present, return the result unchanged;
/// * otherwise run `truncate.output(&result.output, Options::default(),
///   Some(&agent))` and set `output`, `metadata.truncated` and — when
///   truncated — `metadata.outputPath`.
pub fn define<P, F>(
    id: &'static str,
    description: impl Into<Cow<'static, str>>,
    parameters: Value,
    format_validation_error: Option<FormatValidationError>,
    truncate: Arc<dyn Truncate>,
    agents: Arc<dyn Agents>,
    execute: F,
) -> ToolDef
where
    P: for<'de> Deserialize<'de> + Send + 'static,
    F: for<'a> Fn(P, ToolCtxRef<'a>) -> BoxFuture<'a, Result<ExecuteResult, ToolError>>
        + Send
        + Sync
        + 'static,
{
    let format_validation_error = format_validation_error;
    let validation_error_for_closure = format_validation_error.clone();
    let execute = Arc::new(execute);
    let wrapped: ExecuteFn =
        Arc::new(
            move |args: Value, ctx: ToolCtxRef<'_>| match serde_json::from_value::<P>(args) {
                Err(err) => {
                    let detail = match &validation_error_for_closure {
                        Some(f) => f(&err),
                        None => err.to_string(),
                    };
                    Box::pin(async move {
                        Err(ToolError::InvalidArguments {
                            tool: id.to_string(),
                            detail,
                        })
                    }) as BoxFuture<'_, Result<ExecuteResult, ToolError>>
                }
                Ok(decoded) => {
                    let truncate = Arc::clone(&truncate);
                    let agents = Arc::clone(&agents);
                    let execute = Arc::clone(&execute);
                    Box::pin(async move {
                        let agent_name = ctx.agent;
                        let mut result = execute(decoded, ctx).await?;
                        if result.metadata.get("truncated").is_some() {
                            return Ok(result);
                        }
                        let agent = agents.get(agent_name).await?;
                        match truncate
                            .output(&result.output, Options::default(), Some(&agent))
                            .await
                        {
                            crate::tool::truncate::TruncResult::Unchanged { content } => {
                                result.output = content;
                                inject_truncation_metadata(&mut result.metadata, false, None);
                            }
                            crate::tool::truncate::TruncResult::Truncated {
                                content,
                                output_path,
                            } => {
                                result.output = content;
                                inject_truncation_metadata(
                                    &mut result.metadata,
                                    true,
                                    Some(&output_path),
                                );
                            }
                        }
                        Ok(result)
                    }) as BoxFuture<'_, Result<ExecuteResult, ToolError>>
                }
            },
        );
    ToolDef {
        id,
        description: description.into(),
        parameters,
        format_validation_error,
        execute: wrapped,
    }
}

/// `{ ...metadata, truncated, ...(truncated && { outputPath }) }` — the TS
/// spread tolerates non-object metadata (spreading a non-object yields `{}`).
fn inject_truncation_metadata(
    metadata: &mut Value,
    truncated: bool,
    output_path: Option<&PathBuf>,
) {
    if !metadata.is_object() {
        *metadata = Value::Object(serde_json::Map::new());
    }
    let obj = metadata.as_object_mut().expect("metadata is an object");
    obj.insert("truncated".to_string(), Value::Bool(truncated));
    if let Some(path) = output_path {
        obj.insert(
            "outputPath".to_string(),
            Value::String(path.display().to_string()),
        );
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::tool::permission::Ruleset;
    use crate::tool::truncate::{Direction, TruncResult, TruncateService};

    struct RecordingAsk {
        requests: std::sync::Mutex<Vec<AskRequest>>,
    }

    impl RecordingAsk {
        fn new() -> Self {
            RecordingAsk {
                requests: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl Ask for RecordingAsk {
        fn ask<'a>(&'a self, request: AskRequest) -> BoxFuture<'a, Result<(), ToolError>> {
            Box::pin(async move {
                self.requests.lock().unwrap().push(request);
                Ok(())
            })
        }
    }

    impl MetadataSink for RecordingAsk {
        fn metadata<'a>(&'a self, _input: MetadataInput) -> BoxFuture<'a, Result<(), ToolError>> {
            Box::pin(async move { Ok(()) })
        }
    }

    struct FixedAgents;

    impl Agents for FixedAgents {
        fn get<'a>(&'a self, _agent: &'a str) -> BoxFuture<'a, Result<AgentInfo, ToolError>> {
            Box::pin(async move {
                Ok(AgentInfo {
                    name: "build".to_string(),
                    description: None,
                    mode: AgentMode::Primary,
                    permission: Ruleset::new(),
                })
            })
        }

        fn list<'a>(&'a self) -> BoxFuture<'a, Vec<AgentInfo>> {
            Box::pin(async move { Vec::new() })
        }
    }

    fn truncate(dir: &Path) -> Arc<dyn Truncate> {
        Arc::new(TruncateService::new(dir.to_path_buf(), 2, 10_000))
    }

    fn agents() -> Arc<dyn Agents> {
        Arc::new(FixedAgents)
    }

    fn ctx<'a>(
        ask: &'a RecordingAsk,
        instance: &'a InstanceContext,
        extra: &'a Extra,
    ) -> ToolCtxRef<'a> {
        ToolCtxRef {
            session_id: "ses_1",
            message_id: "msg_1",
            agent: "build",
            call_id: None,
            abort: tokio_util::sync::CancellationToken::new(),
            messages: &[],
            extra,
            instance,
            ask,
            metadata: ask,
        }
    }

    #[derive(Deserialize)]
    #[allow(dead_code)]
    struct Params {
        message: String,
    }

    #[tokio::test]
    async fn define_invalid_args_message_is_byte_exact() {
        let temp = crate::storage::test_support::TempDir::new("define-invalid");
        let def = define::<Params, _>(
            "read",
            "description",
            serde_json::json!({}),
            None,
            truncate(temp.path()),
            agents(),
            |_p: Params, _ctx: ToolCtxRef<'_>| Box::pin(async move { unreachable!() }),
        );
        let ask = RecordingAsk::new();
        let instance = InstanceContext {
            directory: temp.path().to_path_buf(),
            worktree: temp.path().to_path_buf(),
        };
        let extra = Extra::default();
        let ctx = ctx(&ask, &instance, &extra);
        let err = (def.execute)(serde_json::json!({}), ctx)
            .await
            .expect_err("decode must fail");
        match err {
            ToolError::InvalidArguments {
                ref tool,
                ref detail,
            } => {
                assert_eq!(tool, "read");
                let expected = format!(
                    "The read tool was called with invalid arguments: {detail}.\nPlease rewrite the input so it satisfies the expected schema."
                );
                assert_eq!(err.to_string(), expected);
            }
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn define_format_validation_error_used_for_detail() {
        let temp = crate::storage::test_support::TempDir::new("define-fve");
        let def = define::<Params, _>(
            "read",
            "description",
            serde_json::json!({}),
            Some(Arc::new(|_e: &serde_json::Error| {
                "custom detail".to_string()
            })),
            truncate(temp.path()),
            agents(),
            |_p: Params, _ctx: ToolCtxRef<'_>| Box::pin(async move { unreachable!() }),
        );
        let ask = RecordingAsk::new();
        let instance = InstanceContext {
            directory: temp.path().to_path_buf(),
            worktree: temp.path().to_path_buf(),
        };
        let extra = Extra::default();
        let ctx = ctx(&ask, &instance, &extra);
        let err = (def.execute)(serde_json::json!({}), ctx)
            .await
            .expect_err("decode must fail");
        assert_eq!(
            err.to_string(),
            "The read tool was called with invalid arguments: custom detail.\nPlease rewrite the input so it satisfies the expected schema."
        );
    }

    #[tokio::test]
    async fn define_truncates_result_and_injects_metadata() {
        let temp = crate::storage::test_support::TempDir::new("define-trunc");
        let def = define::<Params, _>(
            "bash",
            "description",
            serde_json::json!({}),
            None,
            truncate(temp.path()),
            agents(),
            |p: Params, _ctx: ToolCtxRef<'_>| {
                Box::pin(async move {
                    Ok(ExecuteResult {
                        title: p.message,
                        metadata: serde_json::json!({}),
                        output: "one\ntwo\nthree\nfour\nfive\n".to_string(),
                        attachments: None,
                    })
                })
            },
        );
        let ask = RecordingAsk::new();
        let instance = InstanceContext {
            directory: temp.path().to_path_buf(),
            worktree: temp.path().to_path_buf(),
        };
        let extra = Extra::default();
        let ctx = ctx(&ask, &instance, &extra);
        let result = (def.execute)(serde_json::json!({ "message": "hi" }), ctx)
            .await
            .expect("execute succeeds");
        // 6 lines of input, limit 2 lines -> truncated, preview keeps 2 lines.
        assert!(result
            .output
            .starts_with("one\ntwo\n\n...4 lines truncated..."));
        assert_eq!(result.metadata["truncated"], serde_json::json!(true));
        assert!(result.metadata.get("outputPath").is_some());
        let output_path = result.metadata["outputPath"].as_str().unwrap();
        let spilled = std::fs::read_to_string(output_path).expect("spill file exists");
        assert_eq!(spilled, "one\ntwo\nthree\nfour\nfive\n");
    }

    #[tokio::test]
    async fn define_untruncated_result_gets_truncated_false() {
        let temp = crate::storage::test_support::TempDir::new("define-untrunc");
        let def = define::<Params, _>(
            "bash",
            "description",
            serde_json::json!({}),
            None,
            truncate(temp.path()),
            agents(),
            |_p: Params, _ctx: ToolCtxRef<'_>| {
                Box::pin(async move {
                    Ok(ExecuteResult {
                        title: "t".to_string(),
                        metadata: serde_json::json!({"exit": 0}),
                        output: "short".to_string(),
                        attachments: None,
                    })
                })
            },
        );
        let ask = RecordingAsk::new();
        let instance = InstanceContext {
            directory: temp.path().to_path_buf(),
            worktree: temp.path().to_path_buf(),
        };
        let extra = Extra::default();
        let ctx = ctx(&ask, &instance, &extra);
        let result = (def.execute)(serde_json::json!({ "message": "hi" }), ctx)
            .await
            .expect("execute succeeds");
        assert_eq!(result.output, "short");
        assert_eq!(result.metadata["truncated"], serde_json::json!(false));
        assert_eq!(result.metadata["exit"], serde_json::json!(0));
        assert!(result.metadata.get("outputPath").is_none());
    }

    #[tokio::test]
    async fn define_result_with_truncated_metadata_is_untouched() {
        let temp = crate::storage::test_support::TempDir::new("define-skip");
        let def = define::<Params, _>(
            "bash",
            "description",
            serde_json::json!({}),
            None,
            truncate(temp.path()),
            agents(),
            |_p: Params, _ctx: ToolCtxRef<'_>| {
                Box::pin(async move {
                    Ok(ExecuteResult {
                        title: "t".to_string(),
                        metadata: serde_json::json!({"truncated": true}),
                        output: "one\ntwo\nthree\nfour\nfive\nsix\n".to_string(),
                        attachments: None,
                    })
                })
            },
        );
        let ask = RecordingAsk::new();
        let instance = InstanceContext {
            directory: temp.path().to_path_buf(),
            worktree: temp.path().to_path_buf(),
        };
        let extra = Extra::default();
        let ctx = ctx(&ask, &instance, &extra);
        let result = (def.execute)(serde_json::json!({ "message": "hi" }), ctx)
            .await
            .expect("execute succeeds");
        // Not re-truncated: output and metadata pass through untouched.
        assert_eq!(result.output, "one\ntwo\nthree\nfour\nfive\nsix\n");
        assert_eq!(result.metadata, serde_json::json!({"truncated": true}));
        // No spill file was written because truncation was skipped.
        let entries: Vec<_> = std::fs::read_dir(temp.path()).unwrap().collect();
        assert!(
            entries.is_empty(),
            "no spill file expected, found {entries:?}"
        );
    }

    #[test]
    fn direction_default_is_head() {
        assert_eq!(Direction::default(), Direction::Head);
        let _ = TruncResult::Unchanged {
            content: String::new(),
        };
    }
}

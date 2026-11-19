//! The tool definition seam — port of `tool/tool.ts` (spec M4.1).
//!
//! [`ToolDef`] is a registered tool (id, description, JSON-Schema parameters
//! and the *wrapped* execute fn). [`define`] is the TS `Tool.define` +
//! `wrap()` seam: it deserializes the LLM's arguments into the tool's typed
//! `Parameters` struct, runs the raw execute, and post-truncates the result
//! unless its metadata already carries a `truncated` key (tool.ts:107-148).

use std::borrow::Cow;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use crate::tool::error::ToolError;
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
    /// alforria extension: the tool is not one of the engine's own — a
    /// plugin's, registered in the registry's custom slot
    /// (`ToolRegistry::new` sets it). What it answers was written
    /// elsewhere, whatever its id (`tool::outside::classify_result`).
    pub external: bool,
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
    /// The agents the user named in their message. When any are listed,
    /// `bypass_agent_check` covers only those: mentioning one agent is not
    /// leave for the model to start a different one unasked. Empty on the
    /// command path, where the caller itself picks the agent.
    pub bypass_agents: Vec<String>,
    /// `extra.model` (websearch model name, spec M4.5).
    pub model: Option<Value>,
    /// alforria extension: where `read`, `grep` and `glob` report what
    /// they opened and returned, for the session's provenance mark
    /// (`tool::outside::ReturnedPaths`). Not part of the result.
    pub returned_paths: crate::tool::outside::ReturnedPathsSlot,
}

/// TS `InstanceState.context` (project/instance-context.ts:5-9).
#[derive(Debug, Clone)]
pub struct InstanceContext {
    pub directory: PathBuf,
    pub worktree: PathBuf,
    /// Set for a session that is restricted or that outside input started
    /// (`SessionMark::OutsideStart`; not for one that has merely seen
    /// outside content):
    /// the guard its tools' writes go through (`protect::WriteGuard`).
    /// `None`: no guard.
    pub write_guard: Option<std::sync::Arc<crate::protect::WriteGuard>>,
}

impl InstanceContext {
    /// **Every mutating file operation of a tool calls this with the path
    /// it is about to create, overwrite, rename onto or delete**, as it
    /// will pass it to the filesystem, before it does. Enforcement lives
    /// here, at the write, and does not depend on what the tool put in a
    /// permission ask; a grant is looked up here too, for this path.
    ///
    /// This is the check alone — what a tool uses to refuse a whole batch
    /// before it applies any of it. The write itself goes through
    /// [`InstanceContext::write_file`] or [`InstanceContext::remove_file`],
    /// which judge again and, in a guarded session, write what they judged.
    pub fn about_to_modify(&self, path: &Path) -> Result<(), ToolError> {
        match &self.write_guard {
            None => Ok(()),
            Some(guard) => guard.check(path).map_err(ToolError::Permission),
        }
    }

    /// **Every tool that returns a file's content, or searches a
    /// directory, calls this with the path it is about to open**, before
    /// it raises any ask: a guarded session does not read the host's own
    /// locations (`protect::WriteGuard::admit_read`), and is not asked.
    ///
    /// The answer is the path to use for everything that follows — the
    /// stat, the open ([`InstanceContext::open_for_read`]), the listing,
    /// the walk: for a guarded session the resolved path that was judged,
    /// so that a link swapped into the spelling afterwards changes nothing;
    /// for any other session the path as given.
    pub fn about_to_read(&self, path: &Path) -> Result<PathBuf, ToolError> {
        match &self.write_guard {
            None => Ok(path.to_path_buf()),
            Some(guard) => guard.admit_read(path).map_err(ToolError::Permission),
        }
    }

    /// Open the file at `judged` ([`InstanceContext::about_to_read`]'s
    /// answer) for reading, without waiting on it (a FIFO opens at once
    /// and is for the caller to refuse on the handle).
    pub async fn open_for_read(&self, judged: &Path) -> std::io::Result<std::fs::File> {
        let (guarded, judged) = (self.write_guard.is_some(), judged.to_path_buf());
        tokio::task::spawn_blocking(move || match guarded {
            true => crate::protect::open_read_at(&judged),
            false => crate::beneath::open_nonblocking(&judged),
        })
        .await
        .unwrap_or_else(|join| Err(std::io::Error::other(join.to_string())))
    }

    /// After a listing or a walk of `judged`: in a guarded session it must
    /// still be the place that was judged — nothing on the way swapped for
    /// a link since — and still one the session may read.
    pub fn still_judged(&self, judged: &Path) -> Result<(), ToolError> {
        let Some(guard) = &self.write_guard else {
            return Ok(());
        };
        match guard.admit_read(judged) {
            Ok(resolved) if resolved == judged => Ok(()),
            Ok(_) => Err(ToolError::Permission(format!(
                "{} changed while it was being read",
                judged.display()
            ))),
            Err(reason) => Err(ToolError::Permission(reason)),
        }
    }

    /// Whether a search result at `path` may be returned
    /// ([`InstanceContext::about_to_read`]): a walk that started outside
    /// a host location can still reach into one.
    pub fn may_return(&self, path: &Path) -> bool {
        self.about_to_read(path).is_ok()
    }

    /// Create or overwrite `path` with `contents`, creating its
    /// directories: **the one way a tool writes a file.**
    ///
    /// Without a write guard this is `create_dir_all` and `write` on the
    /// path as given, as it always was. With one, the guard judges where
    /// the path lands and the write is made *there*, through the resolved
    /// parent directory's handle with no link followed
    /// (`protect::write_at`): there is no second lookup of the name between
    /// the judgement and the write for a concurrent rename or link swap to
    /// redirect.
    pub async fn write_file(&self, path: &Path, contents: &[u8]) -> Result<(), ModifyError> {
        let Some(guard) = &self.write_guard else {
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|error| ModifyError::CreateDir(parent.to_path_buf(), error))?;
            }
            let (path, contents) = (path.to_path_buf(), contents.to_vec());
            return tokio::task::spawn_blocking(move || {
                crate::tool::bom::replace_file(&path, &contents)
            })
            .await
            .unwrap_or_else(|join| Err(std::io::Error::other(join.to_string())))
            .map_err(ModifyError::Io);
        };
        let landing = guard.admit(path).map_err(ModifyError::Refused)?;
        let contents = contents.to_vec();
        tokio::task::spawn_blocking(move || crate::protect::write_at(&landing, &contents))
            .await
            .unwrap_or_else(|join| Err(std::io::Error::other(join.to_string())))
            .map_err(ModifyError::Io)
    }

    /// Remove the file `path`: the one way a tool deletes a file. Guarded
    /// like [`InstanceContext::write_file`].
    pub async fn remove_file(&self, path: &Path) -> Result<(), ModifyError> {
        let Some(guard) = &self.write_guard else {
            return tokio::fs::remove_file(path).await.map_err(ModifyError::Io);
        };
        let landing = guard.admit(path).map_err(ModifyError::Refused)?;
        tokio::task::spawn_blocking(move || crate::protect::remove_at(&landing))
            .await
            .unwrap_or_else(|join| Err(std::io::Error::other(join.to_string())))
            .map_err(ModifyError::Io)
    }
}

/// Why [`InstanceContext::write_file`] or [`InstanceContext::remove_file`]
/// did not go through. The caller words the I/O failures as it always did.
#[derive(Debug)]
pub enum ModifyError {
    /// The write guard refused the path.
    Refused(String),
    /// A parent directory could not be created.
    CreateDir(std::path::PathBuf, std::io::Error),
    /// The write or the removal itself failed.
    Io(std::io::Error),
}

impl ModifyError {
    /// The tool error, with `io` wording a failed write or removal and
    /// `create_dir` a failed directory creation.
    pub fn into_tool_error(
        self,
        create_dir: impl FnOnce(&Path, &std::io::Error) -> String,
        io: impl FnOnce(&std::io::Error) -> String,
    ) -> ToolError {
        match self {
            ModifyError::Refused(reason) => ToolError::Permission(reason),
            ModifyError::CreateDir(parent, error) => ToolError::Failed(create_dir(&parent, &error)),
            ModifyError::Io(error) => ToolError::Failed(io(&error)),
        }
    }
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
    pub permission: crate::tool::permission::Policy,
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
        external: false,
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
                    permission: Ruleset::new().into(),
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
            write_guard: None,
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
            write_guard: None,
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
            write_guard: None,
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
            write_guard: None,
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
            write_guard: None,
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

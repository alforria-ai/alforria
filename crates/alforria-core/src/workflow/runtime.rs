//! The confined QuickJS workflow runtime.
//!
//! The script sees `agent`, `pipeline`, `parallel`, `phase`, `log`, and
//! `args`. It has no filesystem, shell, timer, or randomness access; work
//! happens in agents. `agent()` returns the last assistant text (or parsed
//! JSON when `options.schema` is set) and throws when the agent fails.

use std::sync::Arc;
use std::time::{Duration, Instant};

use alforria_schema::workflow::WorkflowState;
use rquickjs::function::Async;
use rquickjs::{AsyncContext, AsyncRuntime, Ctx, Function, Promise};
use serde_json::Value;

use crate::tool::def::BoxFuture;

/// The runtime's callbacks into a workflow run.
pub trait WorkflowHost: Send + Sync {
    /// Start a new `phase()` group.
    fn phase(&self, title: String);
    /// Append a `log()` line.
    fn log(&self, message: String);
    /// Run one agent. `options` is the object passed to `agent()` decoded to
    /// JSON (`label`, `agent`, `schema`). Returns the agent's text; an `Err`
    /// string is thrown into the script.
    fn agent(&self, prompt: String, options: Value) -> BoxFuture<'static, Result<String, String>>;
    /// Current state, used to short-circuit a stopped run.
    fn state(&self) -> WorkflowState;
}

const PRELUDE: &str = r#"
const args = JSON.parse(__workflow_args_json);
function phase(title) { __workflow_phase(String(title)); return title; }
function log(message) { __workflow_log(String(message)); }
async function agent(prompt, options = {}) {
  options = options || {};
  const payload = await __workflow_agent(String(prompt), JSON.stringify(options));
  let reply;
  try { reply = JSON.parse(payload); } catch (e) { reply = { ok: false, error: 'invalid workflow runtime reply' }; }
  if (!reply || reply.ok !== true) throw new Error((reply && reply.error) || 'agent failed');
  if (options.schema) {
    try { return JSON.parse(reply.output); } catch (e) { return reply.output; }
  }
  return reply.output;
}
async function parallel(tasks) {
  if (!Array.isArray(tasks)) throw new Error('parallel() expects an array');
  if (tasks.length > 4096) throw new Error('parallel() accepts at most 4096 tasks');
  const settled = await Promise.allSettled(tasks.map((task) => typeof task === 'function' ? task() : task));
  return settled.map((item) => item.status === 'fulfilled' ? item.value : null);
}
async function pipeline(items, fn) {
  if (typeof fn !== 'function') throw new Error('pipeline() expects a function');
  if (!Array.isArray(items)) throw new Error('pipeline() expects an array');
  if (items.length > 4096) throw new Error('pipeline() accepts at most 4096 items');
  const settled = await Promise.allSettled(items.map((item, index) => fn(item, index)));
  return settled.map((item) => item.status === 'fulfilled' ? item.value : null);
}
Date.now = () => { throw new Error('Date.now() is not available in workflows; pass timestamps through args'); };
Math.random = () => { throw new Error('Math.random() is not available in workflows; pass values through args'); };
"#;

/// Run a workflow script to completion and return its `return` value.
pub async fn execute(
    host: Arc<dyn WorkflowHost>,
    script: &str,
    args: Value,
) -> Result<Value, String> {
    if host.state() == WorkflowState::Cancelled {
        return Err("workflow cancelled".to_string());
    }
    let runtime = AsyncRuntime::new().map_err(|err| err.to_string())?;
    // A tight JS loop never awaits, so the abort token can only reach it
    // through QuickJS's interrupt handler.
    let guard = host.clone();
    let started = Instant::now();
    runtime
        .set_interrupt_handler(Some(Box::new(move || {
            guard.state() == WorkflowState::Cancelled
                || started.elapsed() > Duration::from_secs(60 * 60)
        })))
        .await;
    let context = AsyncContext::full(&runtime)
        .await
        .map_err(|err| err.to_string())?;
    let body = strip_export(script);
    let program = format!(
        r#"{prelude}
(async () => {{
  try {{
    const __result = await (async () => {{
{body}
    }})();
    return JSON.stringify({{ ok: true, value: __result === undefined ? null : __result }});
  }} catch (error) {{
    const message = error && error.message ? error.message : String(error);
    return JSON.stringify({{ ok: false, error: message }});
  }}
}})()
"#,
        prelude = PRELUDE,
        body = body
    );
    let args_json = serde_json::to_string(&args).unwrap_or_else(|_| "null".to_string());

    let outcome = context
        .async_with(async move |ctx| {
            let globals = ctx.globals();
            globals
                .set("__workflow_args_json", args_json)
                .map_err(|err| err.to_string())?;

            let host_phase = host.clone();
            let phase = Function::new(ctx.clone(), move |title: String| host_phase.phase(title))
                .map_err(|err| err.to_string())?;
            globals
                .set("__workflow_phase", phase)
                .map_err(|err| err.to_string())?;

            let host_log = host.clone();
            let log = Function::new(ctx.clone(), move |message: String| host_log.log(message))
                .map_err(|err| err.to_string())?;
            globals
                .set("__workflow_log", log)
                .map_err(|err| err.to_string())?;

            let host_agent = host.clone();
            let agent = Function::new(
                ctx.clone(),
                Async(move |_ctx: Ctx<'_>, prompt: String, options: String| {
                    let host = host_agent.clone();
                    async move {
                        let options: Value = serde_json::from_str(&options).unwrap_or(Value::Null);
                        let reply = match host.agent(prompt, options).await {
                            Ok(output) => serde_json::json!({ "ok": true, "output": output }),
                            Err(error) => serde_json::json!({ "ok": false, "error": error }),
                        };
                        Ok::<String, rquickjs::Error>(reply.to_string())
                    }
                }),
            )
            .map_err(|err| err.to_string())?;
            globals
                .set("__workflow_agent", agent)
                .map_err(|err| err.to_string())?;

            let promise: Promise = ctx.eval(program).map_err(|err| err.to_string())?;
            let raw: String = promise.into_future().await.map_err(|err| err.to_string())?;
            let reply: Value = serde_json::from_str(&raw)
                .map_err(|err| format!("invalid workflow runtime reply: {err}"))?;
            if reply.get("ok").and_then(Value::as_bool) == Some(true) {
                Ok(reply.get("value").cloned().unwrap_or(Value::Null))
            } else {
                Err(reply
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("workflow failed")
                    .to_string())
            }
        })
        .await
        .map_err(|err| err.to_string())?;
    Ok(outcome)
}

/// Remove `export` from top-level declarations so the body can run inside an
/// async function (Claude Code scripts start with `export const meta = ...`).
fn strip_export(script: &str) -> String {
    script
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            if let Some(rest) = trimmed.strip_prefix("export ") {
                let indent = &line[..line.len() - trimmed.len()];
                format!("{indent}{rest}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    struct FakeHost {
        prompts: Mutex<Vec<String>>,
        phases: Mutex<Vec<String>>,
        logs: Mutex<Vec<String>>,
        state: Mutex<WorkflowState>,
    }

    impl FakeHost {
        fn new() -> Arc<Self> {
            Arc::new(FakeHost {
                prompts: Mutex::new(Vec::new()),
                phases: Mutex::new(Vec::new()),
                logs: Mutex::new(Vec::new()),
                state: Mutex::new(WorkflowState::Running),
            })
        }
    }

    impl WorkflowHost for FakeHost {
        fn phase(&self, title: String) {
            self.phases.lock().unwrap().push(title);
        }

        fn log(&self, message: String) {
            self.logs.lock().unwrap().push(message);
        }

        fn agent(
            &self,
            prompt: String,
            _options: Value,
        ) -> BoxFuture<'static, Result<String, String>> {
            self.prompts.lock().unwrap().push(prompt.clone());
            let prompt = prompt.clone();
            Box::pin(async move {
                if prompt.contains("FAIL") {
                    return Err("agent exploded".to_string());
                }
                Ok(format!("done:{prompt}"))
            })
        }

        fn state(&self) -> WorkflowState {
            *self.state.lock().unwrap()
        }
    }

    #[tokio::test]
    async fn runs_script_and_collects_phases_and_logs() {
        let host = FakeHost::new();
        let result = execute(
            host.clone(),
            r#"
export const meta = { name: 'test' }
phase('One')
log('hello')
const out = await agent('a')
return { out, args }
"#,
            json!({ "x": 1 }),
        )
        .await
        .unwrap();
        assert_eq!(result, json!({ "out": "done:a", "args": { "x": 1 } }));
        assert_eq!(*host.phases.lock().unwrap(), vec!["One"]);
        assert_eq!(*host.logs.lock().unwrap(), vec!["hello"]);
    }

    #[tokio::test]
    async fn pipeline_runs_every_item_and_keeps_nulls() {
        let host = FakeHost::new();
        let result = execute(
            host.clone(),
            r#"
const out = await pipeline([1, 2, 'FAIL', 4], (item) => agent(String(item)))
return out
"#,
            json!(null),
        )
        .await
        .unwrap();
        assert_eq!(result, json!(["done:1", "done:2", null, "done:4"]));
        assert_eq!(host.prompts.lock().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn awaited_failure_fails_the_run() {
        let host = FakeHost::new();
        let error = execute(host, "return await agent('FAIL')", json!(null))
            .await
            .unwrap_err();
        assert!(error.contains("agent exploded"));
    }

    #[tokio::test]
    async fn schema_parses_json_output() {
        struct JsonHost;
        impl WorkflowHost for JsonHost {
            fn phase(&self, _title: String) {}
            fn log(&self, _message: String) {}
            fn agent(
                &self,
                _prompt: String,
                _options: Value,
            ) -> BoxFuture<'static, Result<String, String>> {
                Box::pin(async { Ok("{\"files\":[\"a\"]}".to_string()) })
            }
            fn state(&self) -> WorkflowState {
                WorkflowState::Running
            }
        }
        let result = execute(
            Arc::new(JsonHost),
            "return await agent('list', { schema: { type: 'object' } })",
            json!(null),
        )
        .await
        .unwrap();
        assert_eq!(result, json!({ "files": ["a"] }));
    }
}

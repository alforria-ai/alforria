//! Dynamic workflow routes (alforria extension): list/save/start/get/abort
//! through the full middleware stack with a stub engine.

use std::sync::{Arc, Mutex};

use alforria_core::{CoreError, EventBus, SessionServices, Storage};
use alforria_schema::workflow::{
    WorkflowDefinition, WorkflowRun, WorkflowSaveRequest, WorkflowSource, WorkflowStartRequest,
    WorkflowState,
};
use alforria_server::routes;
use alforria_server::state::{
    AuthConfig, EmptyUiBackend, InstanceStore, ServerContext, SessionEngine,
};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures::future::BoxFuture;
use serde_json::json;
use tower::ServiceExt;

struct NoJobs;
impl alforria_core::BackgroundJobs for NoJobs {
    fn list(&self) -> Result<Vec<alforria_core::BackgroundJobInfo>, CoreError> {
        Ok(Vec::new())
    }
    fn cancel(&self, _id: &str) -> Result<(), CoreError> {
        Ok(())
    }
}

struct FixedClock;
impl alforria_core::Clock for FixedClock {
    fn now_ms(&self) -> u64 {
        1
    }
}

#[derive(Default)]
struct WorkflowEngine {
    runs: Mutex<Vec<WorkflowRun>>,
}

fn definition(name: &str) -> WorkflowDefinition {
    WorkflowDefinition {
        name: name.to_string(),
        description: Some("test workflow".to_string()),
        path: "/tmp/test.js".to_string(),
        source: WorkflowSource::Project,
        phases: vec!["Plan".to_string()],
        script: "return await agent('hi')".to_string(),
    }
}

fn run(name: &str) -> WorkflowRun {
    WorkflowRun {
        id: "run_1".to_string(),
        name: name.to_string(),
        description: None,
        directory: "/tmp".to_string(),
        session_id: Some("ses_1".to_string()),
        state: WorkflowState::Running,
        args: json!({}),
        script: "return 1".to_string(),
        phases: Vec::new(),
        agents: Vec::new(),
        logs: Vec::new(),
        result: None,
        error: None,
        started: 1.0,
        finished: None,
    }
}

impl SessionEngine for WorkflowEngine {
    fn workflow_list(
        &self,
    ) -> BoxFuture<'static, Result<Vec<WorkflowDefinition>, alforria_server::ServerError>> {
        Box::pin(async { Ok(vec![definition("audit")]) })
    }

    fn workflow_runs(
        &self,
    ) -> BoxFuture<'static, Result<Vec<WorkflowRun>, alforria_server::ServerError>> {
        let runs = self.runs.lock().unwrap().clone();
        Box::pin(async move { Ok(runs) })
    }

    fn workflow_start(
        &self,
        request: WorkflowStartRequest,
    ) -> BoxFuture<'static, Result<WorkflowRun, alforria_server::ServerError>> {
        let mut run = run(request.name.as_deref().unwrap_or("inline"));
        run.args = request.args;
        self.runs.lock().unwrap().push(run.clone());
        Box::pin(async move { Ok(run) })
    }

    fn workflow_get(
        &self,
        run_id: String,
    ) -> BoxFuture<'static, Result<Option<WorkflowRun>, alforria_server::ServerError>> {
        let found = self
            .runs
            .lock()
            .unwrap()
            .iter()
            .find(|run| run.id == run_id)
            .cloned();
        Box::pin(async move { Ok(found) })
    }

    fn workflow_wait(
        &self,
        run_id: String,
    ) -> BoxFuture<'static, Result<WorkflowRun, alforria_server::ServerError>> {
        let found = self
            .runs
            .lock()
            .unwrap()
            .iter()
            .find(|run| run.id == run_id)
            .cloned()
            .ok_or_else(|| {
                alforria_server::ServerError::Core(CoreError::Storage("run not found".into()))
            });
        Box::pin(async move { found })
    }

    fn workflow_abort(
        &self,
        run_id: String,
    ) -> BoxFuture<'static, Result<WorkflowRun, alforria_server::ServerError>> {
        let mut runs = self.runs.lock().unwrap();
        if let Some(run) = runs.iter_mut().find(|run| run.id == run_id) {
            run.state = WorkflowState::Cancelled;
            let run = run.clone();
            drop(runs);
            return Box::pin(async move { Ok(run) });
        }
        drop(runs);
        Box::pin(async {
            Err(alforria_server::ServerError::Core(CoreError::Storage(
                "run not found".into(),
            )))
        })
    }

    fn workflow_save(
        &self,
        request: WorkflowSaveRequest,
    ) -> Result<WorkflowDefinition, alforria_server::ServerError> {
        Ok(definition(&request.name))
    }

    fn workflow_delete(&self, _name: String) -> Result<bool, alforria_server::ServerError> {
        Ok(true)
    }

    fn prompt(
        &self,
        _input: alforria_core::session::prompt_input::PromptInput,
    ) -> BoxFuture<
        'static,
        Result<alforria_core::WithParts, alforria_core::session::prompt_input::PromptError>,
    > {
        Box::pin(async { unimplemented!() })
    }

    fn loop_(
        &self,
        _session_id: String,
    ) -> BoxFuture<
        'static,
        Result<alforria_core::WithParts, alforria_core::RunnerError<alforria_core::SessionError>>,
    > {
        Box::pin(async { unimplemented!() })
    }

    fn command(
        &self,
        _input: alforria_core::session::prompt::CommandInput,
    ) -> BoxFuture<
        'static,
        Result<alforria_core::WithParts, alforria_core::session::prompt_input::PromptError>,
    > {
        Box::pin(async { unimplemented!() })
    }

    fn shell(
        &self,
        _input: alforria_core::session::prompt::ShellInput,
    ) -> BoxFuture<'static, Result<alforria_core::WithParts, alforria_core::SessionError>> {
        Box::pin(async { unimplemented!() })
    }

    fn revert(
        &self,
        _input: alforria_core::session::revert::RevertInput,
    ) -> BoxFuture<
        'static,
        Result<alforria_schema::session_v1::V1SessionInfo, alforria_core::SessionError>,
    > {
        Box::pin(async { unimplemented!() })
    }

    fn unrevert(
        &self,
        _session_id: String,
    ) -> BoxFuture<
        'static,
        Result<alforria_schema::session_v1::V1SessionInfo, alforria_core::SessionError>,
    > {
        Box::pin(async { unimplemented!() })
    }

    fn cleanup(
        &self,
        _session: &alforria_schema::session_v1::V1SessionInfo,
    ) -> Result<(), alforria_core::SessionError> {
        unimplemented!()
    }

    fn diff(
        &self,
        _session_id: &str,
        _message_id: Option<&str>,
    ) -> Result<Vec<alforria_schema::file_diff::SnapshotFileDiff>, alforria_core::SessionError>
    {
        unimplemented!()
    }

    fn share(&self, _session: &alforria_schema::session_v1::V1SessionInfo) -> Result<(), String> {
        unimplemented!()
    }

    fn unshare(&self, _session_id: &str) -> Result<(), String> {
        unimplemented!()
    }
}

fn fixture() -> (Arc<ServerContext>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let worktree = dir.path().join("repo");
    std::fs::create_dir_all(&worktree).unwrap();
    let storage = Arc::new(Storage::open(dir.path().join("db.sqlite")).unwrap());
    let agent_input = alforria_core::AgentRegistryInput {
        config: serde_json::from_value(json!({})).unwrap(),
        skill_dirs: Vec::new(),
        reference_dirs: Vec::new(),
        worktree: worktree.clone(),
        data_dir: dir.path().to_path_buf(),
        tmp_dir: dir.path().to_path_buf(),
        home: dir.path().to_path_buf(),
    };
    let services = Arc::new(SessionServices::new(
        storage.clone(),
        Arc::new(NoJobs),
        Arc::new(FixedClock),
        &agent_input,
    ));
    let instances = InstanceStore::new({
        let services = services.clone();
        Arc::new(move |_directory| Ok(services.clone()))
    });
    let mut ctx = ServerContext::new(
        AuthConfig::new("alforria", None),
        instances,
        storage.clone(),
        Arc::new(EventBus::new_shared(storage, None)),
        Vec::new(),
        Arc::new(EmptyUiBackend),
    );
    let engine: Arc<dyn SessionEngine> = Arc::new(WorkflowEngine::default());
    ctx.engine_factory = Arc::new(move |_location| Ok(engine.clone()));
    (Arc::new(ctx), dir)
}

async fn send(
    ctx: &Arc<ServerContext>,
    method: &str,
    uri: &str,
    body: &str,
) -> axum::http::Response<Body> {
    routes::build_router(ctx.clone())
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn body_string(response: axum::http::Response<Body>) -> String {
    String::from_utf8(
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

#[tokio::test]
async fn workflow_routes_list_save_start_get_and_abort() {
    let (ctx, _dir) = fixture();
    let directory = urlencode(&_dir.path().join("repo").to_string_lossy());
    let base = format!("/workflow?directory={directory}");

    let response = send(&ctx, "GET", &base, "").await;
    assert_eq!(response.status(), StatusCode::OK);
    let list: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(list[0]["name"], "audit");

    let response = send(
        &ctx,
        "POST",
        &base,
        r#"{"name":"audit","script":"return 1"}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let saved: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(saved["name"], "audit");

    let response = send(
        &ctx,
        "POST",
        &format!("/workflow/run?directory={directory}"),
        r#"{"name":"audit","args":{"x":1}}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let run: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(run["id"], "run_1");
    assert_eq!(run["args"]["x"], 1);

    let response = send(
        &ctx,
        "GET",
        &format!("/workflow/run/run_1?directory={directory}"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let response = send(
        &ctx,
        "POST",
        &format!("/workflow/run/run_1/abort?directory={directory}"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let aborted: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(aborted["state"], "cancelled");

    let response = send(
        &ctx,
        "DELETE",
        &format!("/workflow/audit?directory={directory}"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let deleted: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(deleted["deleted"], true);
}

#[tokio::test]
async fn a_workflow_named_run_can_be_deleted() {
    let (ctx, dir) = fixture();
    let directory = urlencode(&dir.path().join("repo").to_string_lossy());
    let response = send(
        &ctx,
        "DELETE",
        &format!("/workflow/run?directory={directory}"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(body["deleted"], true);
}

fn urlencode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

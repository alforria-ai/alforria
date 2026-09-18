//! M7.1 acceptance: the v1 `/project` family — `current` golden, `list`,
//! `git/init` bootstrap, `update` + `ProjectNotFoundError`, `directories`.

use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use opencode_core::{EventBus, GitRunner, SessionServices, Storage};
use opencode_server::routes;
use opencode_server::state::{
    AuthConfig, EmptyUiBackend, InstanceFactory, InstanceStore, ServerContext,
};
use tower::ServiceExt;

struct NoJobs;
impl opencode_core::BackgroundJobs for NoJobs {
    fn list(&self) -> Result<Vec<opencode_core::BackgroundJobInfo>, opencode_core::CoreError> {
        Ok(Vec::new())
    }
    fn cancel(&self, _id: &str) -> Result<(), opencode_core::CoreError> {
        Ok(())
    }
}

struct FixedClock;
impl opencode_core::Clock for FixedClock {
    fn now_ms(&self) -> u64 {
        0
    }
}

/// The production-shaped fixture: every instance shares the context storage,
/// so `SessionServices::instance()` and `ctx.projects` observe the same
/// project rows.
fn fixture() -> Arc<ServerContext> {
    let storage = Arc::new(Storage::open_in_memory().unwrap());
    let bus = Arc::new(EventBus::new_shared(storage.clone(), None));
    let factory_storage = storage.clone();
    let factory: InstanceFactory = Arc::new(move |directory: &Path| {
        let agent_input = opencode_core::AgentRegistryInput {
            config: serde_json::from_value(serde_json::json!({})).unwrap(),
            skill_dirs: Vec::new(),
            reference_dirs: Vec::new(),
            worktree: directory.to_path_buf(),
            data_dir: std::env::temp_dir(),
            tmp_dir: std::env::temp_dir(),
            home: std::env::temp_dir(),
        };
        Ok(Arc::new(SessionServices::new(
            factory_storage.clone(),
            Arc::new(NoJobs),
            Arc::new(FixedClock),
            &agent_input,
        )))
    });
    Arc::new(ServerContext::new(
        AuthConfig::new("opencode", None),
        InstanceStore::new(factory),
        storage,
        bus,
        Vec::new(),
        Arc::new(EmptyUiBackend),
    ))
}

fn init_git_repo(dir: &Path, remote: &str) {
    let git = opencode_core::git::SubprocessGit;
    let result = git.run(Some(dir), &["init", "--quiet"]);
    assert_eq!(result.exit_code, 0, "git init: {}", result.stderr);
    let result = git.run(Some(dir), &["remote", "add", "origin", remote]);
    assert_eq!(result.exit_code, 0, "git remote add: {}", result.stderr);
}

async fn request_json(
    ctx: &Arc<ServerContext>,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let router = routes::build_router(ctx.clone());
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header(axum::http::header::CONTENT_TYPE, "application/json");
    }
    let request = builder
        .body(Body::from(
            body.map(|body| body.to_string()).unwrap_or_default(),
        ))
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

fn q(directory: &Path) -> String {
    format!("?directory={}", directory.display())
}

#[tokio::test]
async fn current_project_is_the_resolved_git_project() {
    let ctx = fixture();
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo, "https://github.com/acme/widgets.git");

    let (status, json) =
        request_json(&ctx, "GET", &format!("/project/current{}", q(&repo)), None).await;
    assert_eq!(status, StatusCode::OK);
    let expected_id = opencode_core::project::hash_fast("git-remote:github.com/acme/widgets");
    assert_eq!(
        json,
        serde_json::json!({
            "id": expected_id,
            "worktree": repo.display().to_string(),
            "vcs": "git",
            "time": { "created": 0, "updated": 0 },
            "sandboxes": [],
        }),
    );
}

#[tokio::test]
async fn list_and_directories_after_boot() {
    let ctx = fixture();
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo, "https://github.com/acme/widgets.git");

    // Boot the instance first — `list` and `directories` are global views
    // over the registry rows the boot persisted.
    request_json(&ctx, "GET", &format!("/project/current{}", q(&repo)), None).await;
    let expected_id = opencode_core::project::hash_fast("git-remote:github.com/acme/widgets");

    let (status, json) = request_json(&ctx, "GET", "/project", None).await;
    assert_eq!(status, StatusCode::OK);
    let projects = json.as_array().unwrap();
    assert_eq!(projects.len(), 1, "projects: {projects:?}");
    assert_eq!(projects[0]["id"], serde_json::json!(expected_id));

    let (status, json) = request_json(
        &ctx,
        "GET",
        &format!("/project/{expected_id}/directories"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json,
        serde_json::json!([{ "directory": repo.display().to_string() }]),
    );
}

#[tokio::test]
async fn update_project_and_not_found() {
    let ctx = fixture();
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo, "https://github.com/acme/widgets.git");
    let (_, project) =
        request_json(&ctx, "GET", &format!("/project/current{}", q(&repo)), None).await;
    let project_id = project["id"].as_str().unwrap().to_string();

    let (status, json) = request_json(
        &ctx,
        "PATCH",
        &format!("/project/{project_id}"),
        Some(serde_json::json!({ "name": "Widgets" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["name"], serde_json::json!("Widgets"));
    assert_eq!(json["id"], serde_json::json!(project_id));

    // Unknown id → 404 ProjectNotFoundError (handlers/project.ts:36-50).
    let (status, json) = request_json(
        &ctx,
        "PATCH",
        "/project/prj_missing",
        Some(serde_json::json!({ "name": "x" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        json,
        serde_json::json!({
            "_tag": "ProjectNotFoundError",
            "projectID": "prj_missing",
            "message": "Project not found: prj_missing",
        }),
    );
}

#[tokio::test]
async fn git_init_bootstraps_a_repository() {
    let ctx = fixture();
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();

    // Pre-init: a non-git directory resolves to the "global" project with
    // the fs root as worktree.
    let (status, json) =
        request_json(&ctx, "GET", &format!("/project/current{}", q(&plain)), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["id"], serde_json::json!("global"));
    assert_eq!(json["worktree"], serde_json::json!("/"));

    let (status, json) = request_json(
        &ctx,
        "POST",
        &format!("/project/git/init{}", q(&plain)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["vcs"], serde_json::json!("git"), "{json}");
    assert!(
        plain.join(".git").exists(),
        "git init must have run in the directory"
    );
}

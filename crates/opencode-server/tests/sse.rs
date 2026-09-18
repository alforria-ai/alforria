//! M6.4 acceptance: the four SSE streams — golden byte frames (event ids
//! normalized), the heartbeat matrix (v1 events after `drop(1)`, v2
//! immediate comments), the /event filter + disposal terminator and the
//! durable replay boundary.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use futures::{Stream, StreamExt};
use opencode_core::{
    CreateInput, EventBus, PublishOptions, SessionContext, SessionError, SessionServices, Storage,
};
use opencode_schema::location::LocationRef;
use opencode_server::routes;
use opencode_server::state::{
    AuthConfig, EmptyUiBackend, HeartbeatConfig, InstanceFactory, InstanceStore, ServerContext,
};
use tower::ServiceExt;

const HEARTBEAT: Duration = Duration::from_millis(25);

fn test_services() -> SessionServices {
    let dir = tempfile::tempdir().unwrap();
    let worktree = dir.path().join("repo");
    std::fs::create_dir_all(&worktree).unwrap();
    let storage = Arc::new(Storage::open(dir.path().join("db.sqlite")).unwrap());
    let agent_input = opencode_core::AgentRegistryInput {
        config: serde_json::from_value(serde_json::json!({})).unwrap(),
        skill_dirs: Vec::new(),
        reference_dirs: Vec::new(),
        worktree: worktree.clone(),
        data_dir: dir.path().to_path_buf(),
        tmp_dir: dir.path().to_path_buf(),
        home: dir.path().to_path_buf(),
    };
    SessionServices::new(
        storage,
        Arc::new(NoJobs),
        Arc::new(FixedClock),
        &agent_input,
    )
}

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

fn fixture() -> Arc<ServerContext> {
    let storage = Arc::new(Storage::open_in_memory().unwrap());
    let manifest = Arc::new(opencode_core::session::event_definitions::SessionManifest::new());
    let bus = Arc::new(EventBus::new_shared(storage.clone(), Some(manifest)));
    let factory: InstanceFactory = Arc::new(|_| Ok(Arc::new(test_services())));
    let mut ctx = ServerContext::new(
        AuthConfig::new("opencode", None),
        InstanceStore::new(factory),
        storage,
        bus,
        Vec::new(),
        Arc::new(EmptyUiBackend),
    );
    ctx.heartbeat = HeartbeatConfig {
        v1: HEARTBEAT,
        v2: HEARTBEAT,
    };

    Arc::new(ctx)
}

/// Publish a `models-dev.refreshed` event with the given location fields.
fn publish(ctx: &ServerContext, directory: Option<&str>, workspace: Option<String>) {
    ctx.bus
        .publish(
            &opencode_core::MODELS_DEV_REFRESHED,
            serde_json::json!({}),
            PublishOptions {
                location: directory.map(|directory| LocationRef {
                    directory: directory.to_string(),
                    workspace_id: workspace,
                }),
                ..Default::default()
            },
        )
        .unwrap();
}

/// Add one more durable event to the session aggregate: a `message.removed`
/// publish (its projector tolerates a missing message row).
fn publish_durable_session(ctx: &ServerContext, session_id: &str) {
    ctx.sessions
        .remove_message(session_id, "msg_missing")
        .unwrap();
}

fn create_session(ctx: &ServerContext, id: &str, directory: &str) -> Result<(), SessionError> {
    ctx.storage
        .with_connection(|conn| {
            conn.execute(
                "INSERT OR IGNORE INTO project (id, worktree, sandboxes, time_created, time_updated)
                 VALUES ('prj_test', '/repo', '[]', 1, 1)",
                [],
            )
        })
        .unwrap();
    ctx.sessions
        .create(
            &SessionContext {
                project_id: "prj_test".to_string(),
                directory: PathBuf::from(directory),
                worktree: PathBuf::from(directory),
                workspace_id: None,
            },
            &CreateInput {
                id: Some(id.to_string()),
                directory: Some(directory.to_string()),
                workspace_id: None,
                ..Default::default()
            },
        )
        .map(|_| ())
}

/// Reads complete SSE frames (split on `\n\n`) off a response body stream.
struct Frames<S> {
    stream: S,
    buffer: Vec<u8>,
}

impl<S> Frames<S>
where
    S: Stream<Item = Result<axum::body::Bytes, axum::Error>> + Unpin,
{
    fn new(stream: S) -> Self {
        Frames {
            stream,
            buffer: Vec::new(),
        }
    }

    async fn next(&mut self) -> Option<String> {
        loop {
            if let Some(pos) = self.buffer.windows(2).position(|w| w == b"\n\n") {
                let frame: Vec<u8> = self.buffer.drain(..pos + 2).collect();
                return Some(String::from_utf8(frame).unwrap());
            }
            match self.stream.next().await {
                Some(Ok(chunk)) => self.buffer.extend_from_slice(&chunk),
                Some(Err(_)) | None => return None,
            }
        }
    }

    /// Skip `heartbeat`-spam tolerant read: returns the next event frame,
    /// ignoring heartbeat comments/events.
    async fn next_event(&mut self) -> Option<String> {
        loop {
            let frame = self.next().await?;
            if frame.contains("\"type\":\"server.heartbeat\"") || frame == ": heartbeat\n\n" {
                continue;
            }
            return Some(frame);
        }
    }
}

/// Event ids are random `evt_…` ULIDs — normalize them for byte comparison.
fn normalize(frame: &str) -> String {
    let re = regex::Regex::new("\"evt_[^\"]*\"").unwrap();
    re.replace_all(frame, "\"evt_X\"").to_string()
}

type BodyStream = axum::body::BodyDataStream;

async fn open(
    ctx: &Arc<ServerContext>,
    uri: &str,
) -> (
    axum::http::StatusCode,
    axum::http::HeaderMap,
    Frames<BodyStream>,
) {
    let router = routes::build_router(ctx.clone());
    let response = router
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let frames = Frames::new(response.into_body().into_data_stream());
    (status, headers, frames)
}

// ---------------------------------------------------------------- v1 /event

#[tokio::test]
async fn v1_event_stream_connected_filter_and_dispose() {
    let ctx = fixture();
    let (status, headers, mut frames) = open(&ctx, "/event?directory=/repo").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "text/event-stream");
    assert_eq!(headers[header::CACHE_CONTROL], "no-cache, no-transform");
    assert_eq!(headers["x-accel-buffering"], "no");
    assert_eq!(headers["x-content-type-options"], "nosniff");

    // server.connected is the first frame (handlers/event.ts:70).
    let connected = frames.next().await.unwrap();
    assert_eq!(
        normalize(&connected),
        "data: {\"id\":\"evt_X\",\"type\":\"server.connected\",\"properties\":{}}\n\n"
    );

    // Matching directory flows through (handlers/event.ts:36-41).
    publish(&ctx, Some("/repo"), None);
    let frame = frames.next_event().await.unwrap();
    assert_eq!(
        normalize(&frame),
        "data: {\"id\":\"evt_X\",\"type\":\"models-dev.refreshed\",\"properties\":{}}\n\n"
    );

    // Other directories, workspace-tagged events and location-less events
    // are filtered out — no frame may arrive, only the heartbeat.
    publish(&ctx, Some("/other"), None);
    publish(&ctx, Some("/repo"), Some("wrk_other".to_string()));
    publish(&ctx, None, None);
    let frame = frames.next().await.unwrap();
    assert!(
        frame.contains("\"type\":\"server.heartbeat\""),
        "filtered events must not frame, only the heartbeat may appear"
    );

    // Disposal terminates the stream after the disposed frame
    // (handlers/event.ts:42-62).
    ctx.instances.dispose_directory(Path::new("/repo"));
    let frame = frames.next_event().await.unwrap();
    assert_eq!(
        normalize(&frame),
        "data: {\"id\":\"evt_X\",\"type\":\"server.instance.disposed\",\"properties\":{\"directory\":\"/repo\"}}\n\n"
    );
    assert!(
        frames.next().await.is_none(),
        "stream must end after dispose"
    );
}

#[tokio::test]
async fn v1_event_heartbeats_start_after_one_interval() {
    let ctx = fixture();
    let (_status, _headers, mut frames) = open(&ctx, "/event?directory=/repo").await;

    let connected = frames.next().await.unwrap();
    assert!(connected.contains("server.connected"));

    // `Stream.tick("10 seconds").pipe(Stream.drop(1))` — no heartbeat until
    // one full interval has passed.
    let started = std::time::Instant::now();
    let frame = frames.next().await.unwrap();
    assert!(
        frame.contains("\"type\":\"server.heartbeat\""),
        "heartbeat frame expected"
    );
    assert!(
        started.elapsed() >= HEARTBEAT,
        "first heartbeat must respect drop(1)"
    );
    assert_eq!(
        normalize(&frame),
        "data: {\"id\":\"evt_X\",\"type\":\"server.heartbeat\",\"properties\":{}}\n\n"
    );
}

// ------------------------------------------------------- v1 /global/event

#[tokio::test]
async fn global_event_stream() {
    let ctx = fixture();
    let (_status, _headers, mut frames) = open(&ctx, "/global/event").await;

    // server.connected with the {directory?, project?, workspace?, payload}
    // envelope — no outer keys here (global.ts:41).
    let connected = frames.next().await.unwrap();
    assert_eq!(
        normalize(&connected),
        "data: {\"payload\":{\"id\":\"evt_X\",\"type\":\"server.connected\",\"properties\":{}}}\n\n"
    );

    // Every bridge event flows through unfiltered; durable events are
    // additionally mirrored as a sync frame (event-v2-bridge.ts:26-62).
    create_session(&ctx, "ses_1", "/repo").unwrap();
    // The M5 session store publishes without a location (there is no
    // ambient instance context yet), so the frames carry no directory key.
    let frame = frames.next_event().await.unwrap();
    assert!(
        normalize(&frame).starts_with(
            "data: {\"payload\":{\"id\":\"evt_X\",\"type\":\"session.created\",\"properties\":"
        ),
        "regular global frame: {frame}"
    );
    let frame = frames.next_event().await.unwrap();
    assert!(
        normalize(&frame).starts_with(
            "data: {\"payload\":{\"type\":\"sync\",\"syncEvent\":{\"id\":\"evt_X\",\"type\":\"session.created.1\",\"seq\":0,\"aggregateID\":\"ses_1\",\"data\":"
        ),
        "sync global frame: {frame}"
    );

    // Disposal events flow through but never terminate the stream.
    ctx.instances.load(Path::new("/repo")).unwrap();
    ctx.instances.dispose_directory(Path::new("/repo"));
    let frame = frames.next_event().await.unwrap();
    assert_eq!(
        normalize(&frame),
        "data: {\"directory\":\"/repo\",\"payload\":{\"type\":\"server.instance.disposed\",\"properties\":{\"directory\":\"/repo\"},\"id\":\"evt_X\"}}\n\n"
    );
    publish(&ctx, Some("/repo"), None);
    let frame = frames.next_event().await.unwrap();
    assert!(frame.contains("models-dev.refreshed"));
}

#[tokio::test]
async fn global_event_heartbeat() {
    let ctx = fixture();
    let (_status, _headers, mut frames) = open(&ctx, "/global/event").await;
    let _ = frames.next().await.unwrap(); // connected

    let frame = frames.next().await.unwrap();
    assert_eq!(
        normalize(&frame),
        "data: {\"payload\":{\"id\":\"evt_X\",\"type\":\"server.heartbeat\",\"properties\":{}}}\n\n"
    );
}

// ------------------------------------------------------------ v2 /api/event

#[tokio::test]
async fn api_event_stream() {
    let ctx = fixture();
    let (status, headers, mut frames) = open(&ctx, "/api/event").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "text/event-stream");

    // server.connected with the full V2 envelope (data, not properties).
    let connected = frames.next().await.unwrap();
    assert_eq!(
        normalize(&connected),
        "data: {\"id\":\"evt_X\",\"type\":\"server.connected\",\"data\":{}}\n\n"
    );

    // `Stream.tick("15 seconds")` has no drop — the heartbeat comment is
    // immediate (packages/server/src/handlers/event.ts:37).
    let comment = frames.next().await.unwrap();
    assert_eq!(comment, ": heartbeat\n\n");

    publish(&ctx, Some("/repo"), None);
    let frame = frames.next_event().await.unwrap();
    assert_eq!(
        normalize(&frame),
        "data: {\"id\":\"evt_X\",\"type\":\"models-dev.refreshed\",\"location\":{\"directory\":\"/repo\"},\"data\":{}}\n\n"
    );

    // Comments keep coming on the interval.
    let comment = frames.next().await.unwrap();
    assert_eq!(comment, ": heartbeat\n\n");
}

// --------------------------------------------- v2 /api/session/:id/event

#[tokio::test]
async fn api_session_event_replays_durable_events() {
    let ctx = fixture();
    create_session(&ctx, "ses_sse", "/repo").unwrap();

    let (_status, _headers, mut frames) = open(&ctx, "/api/session/ses_sse/event").await;
    let frame = frames.next().await.unwrap();
    let frame = normalize(&frame);
    assert!(
        frame.starts_with(
            "data: {\"id\":\"evt_X\",\"type\":\"session.created\",\"durable\":{\"aggregateID\":\"ses_sse\",\"seq\":0,\"version\":1},\"data\":"
        ),
        "durable envelope frame: {frame}"
    );
    assert!(frame.ends_with("\"sessionID\":\"ses_sse\"}}\n\n"));
}

#[tokio::test]
async fn api_session_event_after_boundary_and_live_tail() {
    let ctx = fixture();
    create_session(&ctx, "ses_sse", "/repo").unwrap(); // seq 0
    publish_durable_session(&ctx, "ses_sse"); // seq 1

    let (_status, _headers, mut frames) = open(&ctx, "/api/session/ses_sse/event?after=0").await;
    let frame = frames.next().await.unwrap();
    assert!(
        frame.contains("\"durable\":{\"aggregateID\":\"ses_sse\",\"seq\":1"),
        "after=0 must skip seq 0"
    );

    // Live tailing continues after the replay.
    publish_durable_session(&ctx, "ses_sse");
    let frame = frames.next().await.unwrap();
    assert!(frame.contains("\"seq\":2"), "seq 2 after live publish");
}

#[tokio::test]
async fn api_session_event_errors() {
    let ctx = fixture();
    create_session(&ctx, "ses_sse", "/repo").unwrap();
    let router = routes::build_router(ctx.clone());

    // Missing session → 404 SessionNotFoundError from the location
    // middleware.
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/session/ses_missing/event")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // Invalid `after` → 400 InvalidRequestError (Query schema rejection).
    for query in ["after=abc", "after=-1"] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/session/ses_sse/event?{query}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query}");
        let body = String::from_utf8(
            axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["_tag"], "InvalidRequestError");
        assert_eq!(parsed["kind"], "Query");
    }
}

// ---------------------------------------------------------- subscription

#[tokio::test]
async fn client_disconnect_releases_the_subscription() {
    let ctx = fixture();
    let baseline = ctx.global_bus.receiver_count();

    let (_status, _headers, _frames) = open(&ctx, "/global/event").await;
    assert_eq!(ctx.global_bus.receiver_count(), baseline + 1);

    // Drop the response (and its body): the next send — heartbeat or event —
    // fails the pump, which drops its receivers on exit.
    drop(_frames);
    publish(&ctx, Some("/repo"), None);
    tokio::time::sleep(HEARTBEAT * 4).await;
    assert_eq!(ctx.global_bus.receiver_count(), baseline);
}

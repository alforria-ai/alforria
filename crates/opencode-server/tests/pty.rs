//! M6.8 acceptance: the PTY service, connect tickets and the v1+v2 connect
//! websockets, exercised against a live listener with real `/bin/sh`
//! sessions.

use std::sync::Arc;
use std::time::Duration;

use opencode_server::{ListenOptions, ServerContext};
use reqwest::{Client, StatusCode};
use serde_json::Value;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::{connect_async, MaybeTlsStream};

use futures::{SinkExt, StreamExt};

use opencode_core::{BackgroundJobInfo, BackgroundJobs, CoreError, Storage};
use tempfile::TempDir;

/// A live server backed by a tempdir instance store.
struct Fixture {
    listener: opencode_server::Listener,
    client: Client,
    directory: String,
    _dir: TempDir,
}

fn test_services(directory: &std::path::Path) -> Arc<opencode_core::SessionServices> {
    let storage = Arc::new(Storage::open(directory.join("db.sqlite")).unwrap());
    let agent_input = opencode_core::AgentRegistryInput {
        config: serde_json::from_value(serde_json::json!({})).unwrap(),
        skill_dirs: Vec::new(),
        reference_dirs: Vec::new(),
        worktree: directory.to_path_buf(),
        data_dir: directory.to_path_buf(),
        tmp_dir: directory.to_path_buf(),
        home: directory.to_path_buf(),
    };
    Arc::new(opencode_core::SessionServices::new(
        storage,
        Arc::new(NoJobs),
        Arc::new(SystemClock),
        &agent_input,
    ))
}

struct SystemClock;

impl opencode_core::Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }
}

struct NoJobs;

impl BackgroundJobs for NoJobs {
    fn list(&self) -> Result<Vec<BackgroundJobInfo>, CoreError> {
        Ok(Vec::new())
    }

    fn cancel(&self, _id: &str) -> Result<(), CoreError> {
        Ok(())
    }
}

async fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let directory = dir.path().canonicalize().unwrap();
    let instances = opencode_server::state::InstanceStore::new(Arc::new(move |directory| {
        Ok(test_services(directory))
    }));
    let mut ctx = ServerContext::for_tests();
    ctx.instances = instances;
    let listener = opencode_server::listen_with(&ListenOptions::default(), Arc::new(ctx))
        .await
        .unwrap();
    Fixture {
        listener,
        client: Client::new(),
        directory: directory.display().to_string(),
        _dir: dir,
    }
}

impl Fixture {
    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.listener.port)
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        let param = if path.starts_with("/api") {
            "location[directory]"
        } else {
            "directory"
        };
        self.client
            .get(format!("{}{path}", self.base()))
            .query(&[(param, self.directory.as_str())])
            .send()
            .await
            .unwrap()
    }

    async fn post(&self, path: &str, body: Value) -> reqwest::Response {
        let request = self
            .client
            .post(format!("{}{path}", self.base()))
            .query(&[("directory", self.directory.as_str())])
            .json(&body)
            .build()
            .unwrap();
        self.client.execute(request).await.unwrap()
    }
}

/// Create a session through the v1 surface and return its info.
async fn create(fx: &Fixture, command: &str, args: &[&str]) -> Value {
    let response = fx
        .post(
            "/pty",
            serde_json::json!({ "command": command, "args": args }),
        )
        .await;
    assert_eq!(response.status(), 200);
    response.json().await.unwrap()
}

async fn connect_ticket(fx: &Fixture, path: &str, headers: &[(&str, &str)]) -> reqwest::Response {
    let param = if path.starts_with("/api") {
        "location[directory]"
    } else {
        "directory"
    };
    let mut request = fx
        .client
        .post(format!("{}{path}", fx.base()))
        .query(&[(param, fx.directory.as_str())]);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request.send().await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v1_pty_http_family() {
    let fx = fixture().await;
    let shells = fx.get("/pty/shells").await;
    assert_eq!(shells.status(), 200);
    let shells: Value = shells.json().await.unwrap();
    assert!(shells.as_array().unwrap().iter().all(|s| {
        s.get("path").is_some() && s.get("name").is_some() && s.get("acceptable").is_some()
    }));

    let created = create(&fx, "/bin/sh", &["-c", "sleep 30"]).await;
    let pty_id = created["id"].as_str().unwrap().to_string();
    assert!(pty_id.starts_with("pty_"));
    assert_eq!(created["status"], "running");
    assert_eq!(
        created["title"],
        format!("Terminal {}", &pty_id[pty_id.len() - 4..])
    );
    assert!(created["pid"].as_u64().unwrap() > 0);
    assert_eq!(created["command"], "/bin/sh");
    assert_eq!(created["cwd"], fx.directory);

    let list = fx.get("/pty").await;
    let list: Value = list.json().await.unwrap();
    assert_eq!(
        list.as_array()
            .unwrap()
            .iter()
            .filter(|info| info["id"] == pty_id.as_str())
            .count(),
        1
    );

    // Update.
    let update = fx
        .client
        .put(format!("{}/pty/{pty_id}", fx.base()))
        .query(&[("directory", fx.directory.as_str())])
        .json(&serde_json::json!({ "title": "renamed" }))
        .send()
        .await
        .unwrap();
    assert_eq!(update.status(), 200);
    let update: Value = update.json().await.unwrap();
    assert_eq!(update["title"], "renamed");

    // Missing sessions carry the typed error.
    let missing = fx.get("/pty/pty_nope").await;
    assert_eq!(missing.status(), 404);
    let missing: Value = missing.json().await.unwrap();
    assert_eq!(
        missing,
        serde_json::json!({
            "_tag": "PtyNotFoundError",
            "ptyID": "pty_nope",
            "message": "PTY session not found: pty_nope",
        })
    );

    let removed = fx
        .client
        .delete(format!("{}/pty/{pty_id}", fx.base()))
        .query(&[("directory", fx.directory.as_str())])
        .send()
        .await
        .unwrap();
    assert_eq!(removed.status(), 200);
    assert_eq!(removed.text().await.unwrap(), "true");

    let gone = fx.get(&format!("/pty/{pty_id}")).await;
    assert_eq!(gone.status(), 404);
    fx.listener.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_family_keeps_exited_sessions() {
    let fx = fixture().await;
    let created = create(&fx, "/bin/sh", &["-c", "exit 4"]).await;
    let pty_id = created["id"].as_str().unwrap().to_string();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let found = fx.get(&format!("/api/pty/{pty_id}")).await;
        assert_eq!(found.status(), 200);
        let info: Value = found.json().await.unwrap();
        if info["data"]["status"] == "exited" {
            assert_eq!(info["data"]["exitCode"], 4);
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "session must exit");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The canonical surface keeps exited sessions; v1 hides them.
    let v2_list = fx.get("/api/pty").await;
    let v2_list: Value = v2_list.json().await.unwrap();
    assert_eq!(
        v2_list["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|info| info["id"] == pty_id.as_str())
            .count(),
        1
    );
    let v1_list = fx.get("/pty").await;
    let v1_list: Value = v1_list.json().await.unwrap();
    assert_eq!(
        v1_list
            .as_array()
            .unwrap()
            .iter()
            .filter(|info| info["id"] == pty_id.as_str())
            .count(),
        0
    );

    let removed = fx
        .client
        .delete(format!("{}/api/pty/{pty_id}", fx.base()))
        .query(&[("location[directory]", fx.directory.as_str())])
        .send()
        .await
        .unwrap();
    assert_eq!(removed.status(), 204);
    let gone = fx.get(&format!("/api/pty/{pty_id}")).await;
    assert_eq!(gone.status(), 404);

    fx.listener.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_token_enforcement() {
    let fx = fixture().await;
    let created = create(&fx, "/bin/sh", &["-c", "sleep 30"]).await;
    let pty_id = created["id"].as_str().unwrap().to_string();
    let path = format!("/pty/{pty_id}/connect-token");

    // Without the CSRF header.
    let forbidden = connect_ticket(&fx, &path, &[]).await;
    assert_eq!(forbidden.status(), 403);
    assert_eq!(
        forbidden.json::<Value>().await.unwrap(),
        serde_json::json!({
            "_tag": "PtyForbiddenError",
            "message": "Invalid PTY connect token request",
        })
    );

    // Wrong origin.
    let forbidden = connect_ticket(
        &fx,
        &path,
        &[
            ("x-opencode-ticket", "1"),
            ("origin", "http://evil.example.com"),
        ],
    )
    .await;
    assert_eq!(forbidden.status(), 403);

    // Allowed origin.
    let ok = connect_ticket(
        &fx,
        &path,
        &[
            ("x-opencode-ticket", "1"),
            ("origin", "http://localhost:3000"),
        ],
    )
    .await;
    assert_eq!(ok.status(), 200);
    let token: Value = ok.json().await.unwrap();
    assert_eq!(token["expires_in"], 60);
    assert!(token["ticket"].as_str().unwrap().len() > 20);

    // Missing session.
    let missing = connect_ticket(
        &fx,
        "/pty/pty_nope/connect-token",
        &[("x-opencode-ticket", "1")],
    )
    .await;
    assert_eq!(missing.status(), 404);
    assert_eq!(
        missing.json::<Value>().await.unwrap()["_tag"],
        "PtyNotFoundError"
    );

    // v2 uses the generic ForbiddenError.
    let v2 = connect_ticket(&fx, &format!("/api/pty/{pty_id}/connect-token"), &[]).await;
    assert_eq!(v2.status(), 403);
    assert_eq!(v2.json::<Value>().await.unwrap()["_tag"], "ForbiddenError");

    fx.listener.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_handshake_order() {
    let fx = fixture().await;
    let created = create(&fx, "/bin/sh", &["-c", "sleep 30"]).await;
    let pty_id = created["id"].as_str().unwrap().to_string();

    // Missing session → 404 before decoding the query.
    let missing = fx.get("/pty/pty_nope/connect?cursor=a&cursor=b").await;
    assert_eq!(missing.status(), 404);
    assert_eq!(missing.text().await.unwrap().len(), 0);

    // Existing session + repeated cursor params → 400 (empty body).
    let bad_query = fx
        .client
        .get(format!("{}/pty/{pty_id}/connect", fx.base()))
        .query(&[
            ("directory", fx.directory.as_str()),
            ("cursor", "a"),
            ("cursor", "b"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(bad_query.status(), 400);
    assert_eq!(bad_query.text().await.unwrap().len(), 0);

    // A garbage ticket → 403 (empty body).
    let bad_ticket = fx
        .client
        .get(format!("{}/pty/{pty_id}/connect", fx.base()))
        .query(&[("directory", fx.directory.as_str()), ("ticket", "garbage")])
        .send()
        .await
        .unwrap();
    assert_eq!(bad_ticket.status(), 403);
    assert_eq!(bad_ticket.text().await.unwrap().len(), 0);

    // v2 skips the 400 query step and has no v1 cursor decode failure.
    let v2_missing = fx.get("/api/pty/pty_nope/connect").await;
    assert_eq!(v2_missing.status(), 404);

    fx.listener.stop().await.unwrap();
}

/// Connect a websocket to the v1 (or v2) connect route.
async fn ws_connect(
    fx: &Fixture,
    path: &str,
) -> WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>> {
    let url = format!("ws://127.0.0.1:{}{path}", fx.listener.port);
    let request = url.into_client_request().unwrap();
    let (stream, _) = connect_async(request).await.unwrap();
    stream
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ws_replay_meta_frame_and_live_output() {
    let fx = fixture().await;
    let created = create(&fx, "/bin/sh", &["-c", "sleep 30"]).await;
    let pty_id = created["id"].as_str().unwrap().to_string();

    // Seed the buffer with output before connecting.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut stream = ws_connect(
        &fx,
        &format!("/pty/{pty_id}/connect?directory={}", fx.directory),
    )
    .await;

    // First write "seed" through the pty, wait for it in the output.
    let _ = stream.send(Message::Text("echo seed\n".into())).await;

    let mut saw_meta = false;
    let mut saw_seed = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !(saw_meta && saw_seed) {
        assert!(tokio::time::Instant::now() < deadline);
        let message = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        match message {
            Message::Text(text) => saw_seed |= text.contains("seed"),
            Message::Binary(bytes) => {
                assert_eq!(bytes[0], 0);
                let meta: Value = serde_json::from_slice(&bytes[1..]).unwrap();
                assert!(meta["cursor"].as_u64().is_some());
                saw_meta = true;
            }
            Message::Close(frame) => panic!("unexpected close: {frame:?}"),
            _ => {}
        }
    }
    assert!(saw_meta, "the meta frame must arrive after replay");
    let _ = stream.close(None).await;
    fx.listener.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ws_close_1000_when_the_session_is_removed() {
    let fx = fixture().await;
    let created = create(&fx, "/bin/sh", &["-c", "sleep 30"]).await;
    let pty_id = created["id"].as_str().unwrap().to_string();
    let mut stream = ws_connect(
        &fx,
        &format!("/pty/{pty_id}/connect?directory={}", fx.directory),
    )
    .await;

    // Removing the session tears down the attachment with close 1000.
    let removed = fx
        .client
        .delete(format!("{}/pty/{pty_id}", fx.base()))
        .query(&[("directory", fx.directory.as_str())])
        .send()
        .await
        .unwrap();
    assert_eq!(removed.status(), 200);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let message = tokio::time::timeout_at(deadline, stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if let Message::Close(Some(frame)) = message {
            assert_eq!(u16::from(frame.code), 1000);
            break;
        }
    }
    fx.listener.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ws_1001_on_server_close() {
    let fx = fixture().await;
    let created = create(&fx, "/bin/sh", &["-c", "sleep 30"]).await;
    let pty_id = created["id"].as_str().unwrap().to_string();
    let mut stream = ws_connect(
        &fx,
        &format!("/pty/{pty_id}/connect?directory={}", fx.directory),
    )
    .await;
    let _ = stream.next().await; // meta frame etc — ignore frames until close

    fx.listener.stop().await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let message = tokio::time::timeout_at(deadline, stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if let Message::Close(Some(frame)) = message {
            assert_eq!(u16::from(frame.code), 1001);
            assert_eq!(frame.reason, "server closing");
            break;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_exited_session_closes_with_4404() {
    let fx = fixture().await;
    let created = create(&fx, "/bin/sh", &["-c", "exit 4"]).await;
    let pty_id = created["id"].as_str().unwrap().to_string();
    // Wait for the exit.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let info = fx.get(&format!("/api/pty/{pty_id}")).await;
        let info: Value = info.json().await.unwrap();
        if info["data"]["status"] == "exited" {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let mut stream = ws_connect(
        &fx,
        &format!(
            "/api/pty/{pty_id}/connect?location[directory]={}",
            fx.directory
        ),
    )
    .await;
    let message = stream.next().await.unwrap().unwrap();
    match message {
        Message::Close(Some(frame)) => {
            assert_eq!(u16::from(frame.code), 4404);
            assert_eq!(frame.reason, "session exited");
        }
        other => panic!("expected close 4404, got {other:?}"),
    }
    fx.listener.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v1_exited_session_connects_are_404_before_upgrade() {
    let fx = fixture().await;
    let created = create(&fx, "/bin/sh", &["-c", "exit 4"]).await;
    let pty_id = created["id"].as_str().unwrap().to_string();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let info = fx.get(&format!("/api/pty/{pty_id}")).await;
        let info: Value = info.json().await.unwrap();
        if info["data"]["status"] == "exited" {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // v1 existence check requires a running session → plain 404.
    let response = fx.get(&format!("/pty/{pty_id}/connect")).await;
    assert_eq!(response.status(), 404);
    fx.listener.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tickets_are_single_use_and_scope_bound_over_the_ws() {
    let fx = fixture().await;
    let created = create(&fx, "/bin/sh", &["-c", "sleep 30"]).await;
    let other = create(&fx, "/bin/sh", &["-c", "sleep 30"]).await;
    let pty_id = created["id"].as_str().unwrap().to_string();
    let other_id = other["id"].as_str().unwrap().to_string();

    let token = connect_ticket(
        &fx,
        &format!("/pty/{pty_id}/connect-token"),
        &[("x-opencode-ticket", "1")],
    )
    .await;
    let token: Value = token.json().await.unwrap();
    let ticket = token["ticket"].as_str().unwrap().to_string();

    // A ticket minted for one pty is invalid for another.
    let url = format!(
        "ws://127.0.0.1:{}/pty/{other_id}/connect?directory={}&ticket={ticket}",
        fx.listener.port, fx.directory
    );
    let response = reqwest::get(url.replace("ws://", "http://")).await.unwrap();
    assert_eq!(response.status(), 403);
    assert_eq!(response.text().await.unwrap().len(), 0);

    // Valid connect consumes the ticket...
    let url = format!(
        "ws://127.0.0.1:{}/pty/{pty_id}/connect?directory={}&ticket={ticket}",
        fx.listener.port, fx.directory
    );
    let (stream, _) = connect_async(url.into_client_request().unwrap())
        .await
        .unwrap();
    drop(stream);

    // ...and the same ticket cannot connect again.
    let url = format!(
        "ws://127.0.0.1:{}/pty/{pty_id}/connect?directory={}&ticket={ticket}",
        fx.listener.port, fx.directory
    );
    let response = reqwest::get(url.replace("ws://", "http://")).await.unwrap();
    assert_eq!(response.status(), 403);
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    fx.listener.stop().await.unwrap();
}

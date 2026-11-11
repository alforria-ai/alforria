//! The webhook outbox (`hook.webhook`) against a local HTTP receiver:
//! delivery, signatures, retries, restart and retention.

mod support;

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alforria_core::config::host::HookInfo;
use alforria_core::event::definition::Payload;
use alforria_core::{Clock, Storage};
use alforria_schema::location::LocationRef;
use alforria_server::outbox::Outbox;
use axum::http::{HeaderMap, StatusCode};
use base64::Engine;
use ring::signature::KeyPair;
use serde_json::{json, Value};

/// One request the receiver got.
#[derive(Debug, Clone)]
struct Received {
    headers: HeaderMap,
    body: String,
}

/// A local webhook receiver: records every request and answers with the
/// next scripted status (200 once the script runs out).
struct Receiver {
    url: String,
    port: u16,
    received: Arc<Mutex<Vec<Received>>>,
    statuses: Arc<Mutex<Vec<u16>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Receiver {
    async fn start() -> Receiver {
        Receiver::start_on(0).await
    }

    async fn start_on(port: u16) -> Receiver {
        let received: Arc<Mutex<Vec<Received>>> = Arc::default();
        let statuses: Arc<Mutex<Vec<u16>>> = Arc::default();
        let (sink, script) = (received.clone(), statuses.clone());
        let app = axum::Router::new().route(
            "/hook",
            axum::routing::post(move |headers: HeaderMap, body: String| {
                let (sink, script) = (sink.clone(), script.clone());
                async move {
                    sink.lock().unwrap().push(Received { headers, body });
                    let mut script = script.lock().unwrap();
                    let status = if script.is_empty() {
                        200
                    } else {
                        script.remove(0)
                    };
                    StatusCode::from_u16(status).unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Receiver {
            url: format!("http://127.0.0.1:{port}/hook"),
            port,
            received,
            statuses,
            task,
        }
    }

    fn answer_with(&self, statuses: &[u16]) {
        *self.statuses.lock().unwrap() = statuses.to_vec();
    }

    fn requests(&self) -> Vec<Received> {
        self.received.lock().unwrap().clone()
    }

    async fn wait_for(&self, count: usize) -> Vec<Received> {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            let requests = self.requests();
            if requests.len() >= count {
                return requests;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the receiver got {} of {count} requests",
                requests.len()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn stop(self) -> u16 {
        self.task.abort();
        let _ = self.task.await;
        self.port
    }
}

fn header<'a>(request: &'a Received, name: &str) -> &'a str {
    request.headers[name].to_str().unwrap()
}

/// A clock tests can move, starting at the real time.
struct TestClock(AtomicU64);

impl TestClock {
    fn new() -> Arc<TestClock> {
        Arc::new(TestClock(AtomicU64::new(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
        )))
    }

    fn advance(&self, by: Duration) {
        self.0.fetch_add(by.as_millis() as u64, Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

fn hook(value: Value) -> HookInfo {
    serde_json::from_value(value).unwrap()
}

fn event(event_type: &str, data: Value) -> Payload {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    Payload {
        id: format!("evt_{}", NEXT.fetch_add(1, Ordering::SeqCst)),
        metadata: None,
        r#type: event_type.to_string(),
        durable: None,
        location: Some(LocationRef {
            directory: "/home/agent/workspace".to_string(),
            workspace_id: None,
            project: None,
        }),
        data,
    }
}

fn storage(dir: &tempfile::TempDir) -> Arc<Storage> {
    Arc::new(Storage::open(dir.path().join("db.sqlite")).unwrap())
}

fn hmac_hex(secret: &str, message: &str) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes());
    hex::encode(ring::hmac::sign(&key, message.as_bytes()).as_ref())
}

/// A selected event is delivered once, signed with the shared secret over
/// timestamp and body, with a delivery id to deduplicate on — and carries
/// ids and labels, never content.
#[tokio::test]
async fn a_selected_event_is_delivered_signed_and_small() {
    let receiver = Receiver::start().await;
    let dir = tempfile::tempdir().unwrap();
    let outbox = Outbox::new(
        storage(&dir),
        Some(&hook(json!({ "webhook": [{
            "url": receiver.url, "secret": "whsec_test",
            "events": ["permission.asked", "session.*"]
        }] }))),
    )
    .unwrap();
    assert!(outbox.enabled());

    outbox.capture(&event(
        "permission.asked",
        json!({
            "id": "per_1", "sessionID": "ses_1", "permission": "bash",
            "patterns": ["cat /home/agent/.ssh/id_ed25519"],
            "metadata": { "command": "cat /home/agent/.ssh/id_ed25519" },
            "always": ["cat *"]
        }),
    ));
    // Not selected by any pattern.
    outbox.capture(&event(
        "message.part.updated",
        json!({ "sessionID": "ses_1", "text": "hello" }),
    ));
    outbox.capture(&event(
        "permission.replied",
        json!({ "sessionID": "ses_1" }),
    ));
    outbox.flush();
    assert_eq!(
        outbox.deliveries().len(),
        1,
        "only the selected event is kept"
    );
    assert_eq!(outbox.deliver_due().await, 1);

    let requests = receiver.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(header(request, "content-type"), "application/json");
    assert_eq!(header(request, "x-alforria-event"), "permission.asked");
    assert_eq!(header(request, "x-alforria-attempt"), "1");
    let delivery = header(request, "x-alforria-delivery").to_string();
    assert!(delivery.starts_with("whd_"));

    // Signature: `t=<unix seconds>,v1=<hex hmac of "<t>.<body>">`.
    let timestamp = header(request, "x-alforria-timestamp");
    let expected = hmac_hex("whsec_test", &format!("{timestamp}.{}", request.body));
    assert_eq!(
        header(request, "x-alforria-signature"),
        format!("t={timestamp},v1={expected}")
    );
    assert_ne!(
        expected,
        hmac_hex("another-secret", &format!("{timestamp}.{}", request.body))
    );

    let body: Value = serde_json::from_str(&request.body).unwrap();
    assert_eq!(body["id"], json!(delivery));
    assert_eq!(body["type"], "permission.asked");
    assert_eq!(body["kind"], "permission");
    assert_eq!(body["sessionID"], "ses_1");
    assert_eq!(body["directory"], "/home/agent/workspace");
    assert!(body["time"].as_u64().unwrap() > 1_600_000_000_000);
    assert_eq!(
        body["properties"],
        json!({ "id": "per_1", "sessionID": "ses_1", "permission": "bash" })
    );
    assert!(
        !request.body.contains("id_ed25519"),
        "content left the machine"
    );
    assert!(request.body.len() < 600, "{} bytes", request.body.len());

    // Delivered: nothing is due any more.
    assert!(outbox.deliveries()[0].delivered);
    assert_eq!(outbox.deliver_due().await, 0);
    assert_eq!(receiver.requests().len(), 1);
}

/// A failed delivery is retried with growing delays until the receiver
/// accepts it, under the same delivery id every time.
#[tokio::test]
async fn failed_deliveries_are_retried_with_backoff() {
    let receiver = Receiver::start().await;
    receiver.answer_with(&[500, 503, 429]);
    let dir = tempfile::tempdir().unwrap();
    let clock = TestClock::new();
    let outbox = Outbox::with_clock(
        storage(&dir),
        Some(&hook(json!({
            "webhook": [{ "url": receiver.url, "secret": "s", "events": ["session.idle"] }],
            "outbox": { "retry_initial": "10s", "retry_max": "25s" }
        }))),
        clock.clone(),
    )
    .unwrap();
    outbox.capture(&event("session.idle", json!({ "sessionID": "ses_1" })));
    outbox.flush();

    assert_eq!(outbox.deliver_due().await, 1);
    let row = outbox.deliveries().remove(0);
    assert!(!row.delivered);
    assert_eq!(row.attempts, 1);
    assert_eq!(row.last_error.as_deref(), Some("the receiver answered 500"));
    assert_eq!(
        row.time_next - clock.now_ms(),
        10_000,
        "first retry after retry_initial"
    );

    // Not due yet.
    clock.advance(Duration::from_secs(9));
    assert_eq!(outbox.deliver_due().await, 0);
    clock.advance(Duration::from_secs(1));
    assert_eq!(outbox.deliver_due().await, 1);
    let row = outbox.deliveries().remove(0);
    assert_eq!(row.attempts, 2);
    assert_eq!(row.time_next - clock.now_ms(), 20_000, "doubled");

    clock.advance(Duration::from_secs(20));
    assert_eq!(outbox.deliver_due().await, 1);
    let row = outbox.deliveries().remove(0);
    assert_eq!(row.attempts, 3);
    assert_eq!(
        row.time_next - clock.now_ms(),
        25_000,
        "capped at retry_max"
    );

    clock.advance(Duration::from_secs(25));
    assert_eq!(outbox.deliver_due().await, 1);
    let row = outbox.deliveries().remove(0);
    assert!(row.delivered);
    assert_eq!(row.attempts, 4);
    assert_eq!(row.last_error, None);

    let requests = receiver.requests();
    assert_eq!(requests.len(), 4);
    let ids: Vec<&str> = requests
        .iter()
        .map(|request| header(request, "x-alforria-delivery"))
        .collect();
    assert!(
        ids.iter().all(|id| *id == ids[0]),
        "one delivery id: {ids:?}"
    );
    let attempts: Vec<&str> = requests
        .iter()
        .map(|request| header(request, "x-alforria-attempt"))
        .collect();
    assert_eq!(attempts, ["1", "2", "3", "4"]);
    assert!(requests
        .iter()
        .all(|request| request.body == requests[0].body));
}

/// With a signing key the receiver verifies with the public key alone.
#[tokio::test]
async fn deliveries_can_be_signed_with_ed25519() {
    let receiver = Receiver::start().await;
    let seed = [42u8; 32];
    let pair = ring::signature::Ed25519KeyPair::from_seed_unchecked(&seed).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let outbox = Outbox::new(
        storage(&dir),
        Some(&hook(json!({ "webhook": [{
            "url": receiver.url,
            "signing_key": base64::engine::general_purpose::STANDARD.encode(seed),
            "events": ["question.asked"]
        }] }))),
    )
    .unwrap();
    outbox.capture(&event(
        "question.asked",
        json!({ "id": "que_1", "sessionID": "ses_1" }),
    ));
    outbox.flush();
    assert_eq!(outbox.deliver_due().await, 1);

    let request = receiver.requests().remove(0);
    let timestamp = header(&request, "x-alforria-timestamp");
    let signature = header(&request, "x-alforria-signature")
        .strip_prefix(&format!("t={timestamp},ed25519="))
        .expect("an ed25519 signature")
        .to_string();
    let signature = base64::engine::general_purpose::STANDARD
        .decode(signature)
        .unwrap();
    let verifier = ring::signature::UnparsedPublicKey::new(
        &ring::signature::ED25519,
        pair.public_key().as_ref(),
    );
    verifier
        .verify(
            format!("{timestamp}.{}", request.body).as_bytes(),
            &signature,
        )
        .expect("the signature verifies with the public key");
    assert!(verifier
        .verify(
            format!("{timestamp}.{}x", request.body).as_bytes(),
            &signature
        )
        .is_err());
    let body: Value = serde_json::from_str(&request.body).unwrap();
    assert_eq!(body["kind"], "question");
}

/// The rows are the queue: a process that dies while backing off leaves its
/// deliveries for the next one, which completes them under the same ids.
#[tokio::test]
async fn deliveries_survive_a_restart_mid_backoff() {
    // A receiver that is down: find a port, then free it.
    let down = Receiver::start().await;
    let url = down.url.clone();
    let port = down.stop().await;
    let dir = tempfile::tempdir().unwrap();
    let config = hook(json!({
        "webhook": [{ "name": "control-plane", "url": url, "secret": "s", "events": ["session.*"] }],
        "outbox": { "retry_initial": "50ms", "retry_max": "200ms", "timeout": "2s" }
    }));

    let delivery_id = {
        let outbox = Outbox::new(storage(&dir), Some(&config)).unwrap();
        outbox.capture(&event("session.idle", json!({ "sessionID": "ses_1" })));
        outbox.capture(&event(
            "session.error",
            json!({
                "sessionID": "ses_1",
                "error": { "name": "ProviderAuthError", "data": { "message": "secret detail" } }
            }),
        ));
        outbox.flush();
        assert_eq!(outbox.deliver_due().await, 2);
        let rows = outbox.deliveries();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| !row.delivered && row.attempts == 1));
        assert_eq!(
            rows[0].last_error.as_deref(),
            Some("could not connect to the receiver")
        );
        rows[0].id.clone()
        // The process "dies" here: the outbox and its storage are dropped
        // with both deliveries waiting on their backoff.
    };

    // The next process, over the same database, with the receiver back.
    let receiver = Receiver::start_on(port).await;
    let outbox = Outbox::new(storage(&dir), Some(&config)).unwrap();
    let rows = outbox.deliveries();
    assert_eq!(rows.len(), 2, "the deliveries were on disk");
    let worker = tokio::spawn(outbox.clone().run());
    let requests = receiver.wait_for(2).await;
    outbox.stop();
    let _ = worker.await;

    assert_eq!(header(&requests[0], "x-alforria-delivery"), delivery_id);
    assert_eq!(header(&requests[0], "x-alforria-attempt"), "2");
    let second: Value = serde_json::from_str(&requests[1].body).unwrap();
    assert_eq!(second["type"], "session.error");
    assert_eq!(second["properties"]["error"], "ProviderAuthError");
    assert!(!requests[1].body.contains("secret detail"));
    let rows = outbox.deliveries();
    assert!(rows.iter().all(|row| row.delivered), "{rows:?}");
    assert_eq!(receiver.requests().len(), 2, "each delivered once");
}

/// Retention is bounded: delivered rows go after `retention`, an event
/// that cannot be delivered is abandoned after `max_age`, and undelivered
/// rows are capped.
#[tokio::test]
async fn the_outbox_is_bounded() {
    let receiver = Receiver::start().await;
    let dir = tempfile::tempdir().unwrap();
    let clock = TestClock::new();
    let outbox = Outbox::with_clock(
        storage(&dir),
        Some(&hook(json!({
            "webhook": [{ "url": receiver.url, "secret": "s", "events": ["session.idle"] }],
            "outbox": {
                "retention": "1h", "max_age": "2h",
                "retry_initial": "1m", "retry_max": "30m", "max_pending": 3
            }
        }))),
        clock.clone(),
    )
    .unwrap();
    let idle = |session: &str| event("session.idle", json!({ "sessionID": session }));

    // Delivered, then kept for the retention period and no longer.
    outbox.capture(&idle("ses_delivered"));
    outbox.flush();
    assert_eq!(outbox.deliver_due().await, 1);
    clock.advance(Duration::from_secs(59 * 60));
    outbox.prune();
    assert_eq!(outbox.deliveries().len(), 1, "inside retention");
    clock.advance(Duration::from_secs(2 * 60));
    outbox.prune();
    assert!(outbox.deliveries().is_empty(), "pruned after retention");

    // Undeliverable: retried until max_age, then abandoned, then pruned.
    receiver.answer_with(&[500; 64]);
    outbox.capture(&idle("ses_stuck"));
    outbox.flush();
    let mut attempts = 0;
    for _ in 0..40 {
        attempts += outbox.deliver_due().await;
        clock.advance(Duration::from_secs(10 * 60));
        if outbox.deliveries().first().is_some_and(|row| row.failed) {
            break;
        }
    }
    let row = outbox.deliveries().remove(0);
    assert!(row.failed, "abandoned after max_age: {row:?}");
    assert!(!row.delivered);
    assert!(
        (3..12).contains(&attempts),
        "{attempts} attempts in two hours of backoff"
    );
    assert_eq!(row.last_error.as_deref(), Some("the receiver answered 500"));
    let before = receiver.requests().len();
    assert_eq!(
        outbox.deliver_due().await,
        0,
        "an abandoned delivery is not retried"
    );
    assert_eq!(receiver.requests().len(), before);
    clock.advance(Duration::from_secs(61 * 60));
    outbox.prune();
    assert!(outbox.deliveries().is_empty());

    // The cap on undelivered rows drops the oldest.
    for index in 0..6 {
        clock.advance(Duration::from_millis(5));
        outbox.capture(&idle(&format!("ses_{index}")));
    }
    outbox.flush();
    let kept: Vec<String> = outbox
        .deliveries()
        .iter()
        .map(|row| {
            serde_json::from_str::<Value>(&row.body).unwrap()["sessionID"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(kept, ["ses_3", "ses_4", "ses_5"]);
}

/// Hooks select by name or pattern, each gets its own delivery, and a hook
/// that leaves the config abandons what it was owed.
#[tokio::test]
async fn hooks_select_events_by_pattern_and_own_their_deliveries() {
    let first = Receiver::start().await;
    let second = Receiver::start().await;
    let dir = tempfile::tempdir().unwrap();
    let both = hook(json!({ "webhook": [
        { "name": "asks", "url": first.url, "secret": "a", "events": ["*.asked"] },
        { "name": "everything-triggered", "url": second.url, "secret": "b",
          "events": ["trigger.*", "permission.asked"] }
    ] }));
    let outbox = Outbox::new(storage(&dir), Some(&both)).unwrap();
    outbox.capture(&event(
        "permission.asked",
        json!({ "id": "per_1", "sessionID": "s" }),
    ));
    outbox.capture(&event(
        "question.asked",
        json!({ "id": "que_1", "sessionID": "s" }),
    ));
    // An event type this code has never heard of, from another subsystem.
    outbox.capture(&event(
        "trigger.fired",
        json!({ "triggerID": "trg_1", "name": "nightly" }),
    ));
    outbox.capture(&event("session.idle", json!({ "sessionID": "s" })));
    outbox.flush();
    let rows = outbox.deliveries();
    let mut owed: Vec<(String, String)> = rows
        .iter()
        .map(|row| (row.hook.clone(), row.event_type.clone()))
        .collect();
    owed.sort();
    assert_eq!(
        owed,
        [
            ("asks".to_string(), "permission.asked".to_string()),
            ("asks".to_string(), "question.asked".to_string()),
            (
                "everything-triggered".to_string(),
                "permission.asked".to_string()
            ),
            (
                "everything-triggered".to_string(),
                "trigger.fired".to_string()
            ),
        ]
    );
    let ids: std::collections::BTreeSet<&str> = rows.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(ids.len(), 4, "a delivery id per (hook, event)");
    drop(outbox);

    // The second hook is removed from the config: its rows are abandoned,
    // the first hook's are delivered.
    let only_first = hook(json!({ "webhook": [
        { "name": "asks", "url": first.url, "secret": "a", "events": ["*.asked"] }
    ] }));
    let outbox = Outbox::new(storage(&dir), Some(&only_first)).unwrap();
    assert_eq!(outbox.deliver_due().await, 2);
    assert_eq!(first.requests().len(), 2);
    assert!(second.requests().is_empty());
    let trigger = first
        .requests()
        .iter()
        .any(|request| request.body.contains("trigger"));
    assert!(!trigger);
    for row in outbox.deliveries() {
        if row.hook == "asks" {
            assert!(row.delivered);
        } else {
            assert!(row.failed);
            assert_eq!(
                row.last_error.as_deref(),
                Some("the hook is no longer configured")
            );
        }
    }

    // No hooks at all: nothing is captured.
    let idle = Outbox::new(storage(&dir), None).unwrap();
    assert!(!idle.enabled());
    idle.capture(&event("permission.asked", json!({})));
    idle.flush();
}

/// End to end through the server: a turn that asks for permission with no
/// client attached reaches the receiver as `permission.asked`, with the
/// session's title, and `session.idle` follows when the turn ends.
#[tokio::test]
async fn a_waiting_agent_reaches_the_control_plane() {
    let receiver = Receiver::start().await;
    let url = receiver.url.clone();
    let f = support::fixture_with(
        move |_| {
            json!({
                "permission": { "bash": "ask" },
                "hook": {
                    "webhook": [{
                        "url": url, "secret": "whsec_e2e",
                        "events": ["permission.asked", "question.asked", "session.idle", "session.error"]
                    }],
                    "outbox": { "retry_initial": "50ms" }
                }
            })
        },
        None,
        vec![support::tool_turn(
            "call_1",
            "bash",
            json!({ "command": "echo top-secret-command-text", "description": "d" }),
        )],
    );
    let worker = tokio::spawn(f.ctx.outbox.clone().run());
    let directory = support::urlencode(&f.dirs.repo.display().to_string());
    let created = support::call(
        &f.router,
        "POST",
        &format!("/session?directory={directory}"),
        None,
        Some(json!({ "title": "Nightly deploy" })),
    )
    .await;
    let session = support::body_json(created).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let prompted = support::call(
        &f.router,
        "POST",
        &format!("/session/{session}/prompt_async"),
        None,
        Some(json!({ "parts": [{ "type": "text", "text": "the user's private prompt text" }] })),
    )
    .await;
    assert!(prompted.status().is_success());

    let asked = receiver.wait_for(1).await.remove(0);
    assert_eq!(header(&asked, "x-alforria-event"), "permission.asked");
    let timestamp = header(&asked, "x-alforria-timestamp");
    assert_eq!(
        header(&asked, "x-alforria-signature"),
        format!(
            "t={timestamp},v1={}",
            hmac_hex("whsec_e2e", &format!("{timestamp}.{}", asked.body))
        )
    );
    let body: Value = serde_json::from_str(&asked.body).unwrap();
    assert_eq!(body["kind"], "permission");
    assert_eq!(body["sessionID"], json!(session));
    assert_eq!(body["title"], "Nightly deploy");
    assert_eq!(body["directory"], json!(f.dirs.repo.display().to_string()));
    let request_id = body["properties"]["id"].as_str().unwrap().to_string();
    assert!(request_id.starts_with("per"));
    assert!(!asked.body.contains("top-secret-command-text"));
    assert!(!asked.body.contains("private prompt"));

    // The control plane answers; the turn ends; `session.idle` arrives.
    let replied = support::call(
        &f.router,
        "POST",
        &format!("/permission/{request_id}/reply?directory={directory}"),
        None,
        Some(json!({ "reply": "once" })),
    )
    .await;
    assert_eq!(replied.status(), StatusCode::OK);
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let idle = receiver
            .requests()
            .iter()
            .any(|request| header(request, "x-alforria-event") == "session.idle");
        if idle {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "session.idle never arrived"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    for request in receiver.requests() {
        assert!(!request.body.contains("top-secret-command-text"));
        assert!(!request.body.contains("private prompt"));
    }
    f.ctx.outbox.stop();
    let _ = worker.await;
}

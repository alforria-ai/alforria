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
        // The hook's first delivery fails; the second waits behind it.
        assert_eq!(outbox.deliver_due().await, 1);
        let rows = outbox.deliveries();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| !row.delivered));
        assert_eq!((rows[0].attempts, rows[1].attempts), (1, 0));
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
    assert_eq!(header(&requests[1], "x-alforria-attempt"), "1");
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
    // What went is counted.
    let stats = outbox.stats();
    assert_eq!((stats.pending, stats.dropped), (3, 3));
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

/// The events of a trigger's run reach a webhook like any other: selected
/// by pattern, small, ids and labels only. A run's final answer is content
/// and stays behind unless the hook asks for it by name (`trigger_text`).
/// A permission ask raised inside the run is delivered too, so someone can
/// be told a scheduled turn is waiting.
#[tokio::test]
async fn a_triggers_events_are_delivered_small_and_its_answer_only_on_request() {
    let plain = Receiver::start().await;
    let wordy = Receiver::start().await;
    let (plain_url, wordy_url) = (plain.url.clone(), wordy.url.clone());
    let f = support::fixture_with(
        move |_dirs| {
            json!({
                "permission": { "bash": "ask" },
                "memory": { "enabled": true },
                "trigger": { "brief": { "on": { "cron": "0 8 * * *" }, "prompt": "Write the brief" } },
                "hook": { "webhook": [
                    { "name": "plain", "url": plain_url, "secret": "whsec_a",
                      "events": ["trigger.*", "permission.asked", "session.interrupted",
                                 "session.media.omitted", "memory.updated"] },
                    { "name": "wordy", "url": wordy_url, "secret": "whsec_b",
                      "events": ["trigger.completed"], "trigger_text": true }
                ] }
            })
        },
        None,
        vec![
            support::tool_turn(
                "call_1",
                "bash",
                json!({ "command": "echo secret-command", "description": "d" }),
            ),
            support::text_turn("The brief: all quiet. secret-answer"),
        ],
    );
    tokio::spawn(f.ctx.outbox.clone().run());
    let dir = format!(
        "directory={}",
        support::urlencode(&f.dirs.repo.display().to_string())
    );

    let response = support::call(
        &f.router,
        "POST",
        &format!("/trigger/brief/run?{dir}"),
        None,
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    // The run waits on its ask: the ask is delivered while it does.
    let asked = loop {
        let requests = plain.requests();
        if let Some(request) = requests
            .iter()
            .find(|request| header(request, "x-alforria-event") == "permission.asked")
        {
            break serde_json::from_str::<Value>(&request.body).unwrap();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(asked["kind"], "permission");
    assert_eq!(asked["properties"]["permission"], "bash");
    assert!(asked["sessionID"].as_str().unwrap().starts_with("ses"));
    assert!(!asked.to_string().contains("secret-command"), "{asked}");
    let ask = asked["properties"]["id"].as_str().unwrap();
    let reply = support::call(
        &f.router,
        "POST",
        &format!("/permission/{ask}/reply?{dir}"),
        None,
        Some(json!({ "reply": "once" })),
    )
    .await;
    assert_eq!(reply.status(), StatusCode::OK);

    let wordy_got = wordy.wait_for(1).await;
    let completed_with_text: Value = serde_json::from_str(&wordy_got[0].body).unwrap();
    assert_eq!(completed_with_text["type"], "trigger.completed");
    assert_eq!(
        completed_with_text["properties"]["text"],
        "The brief: all quiet. secret-answer"
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let completed = loop {
        let requests = plain.requests();
        if let Some(request) = requests
            .iter()
            .find(|request| header(request, "x-alforria-event") == "trigger.completed")
        {
            break serde_json::from_str::<Value>(&request.body).unwrap();
        }
        assert!(std::time::Instant::now() < deadline, "no trigger.completed");
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let properties = &completed["properties"];
    assert_eq!(properties["triggerID"], "brief");
    assert!(properties["runID"].as_str().unwrap().starts_with("trr_"));
    assert_eq!(properties["sessionID"], asked["sessionID"]);
    assert_eq!(properties["report"], "final");
    assert_eq!(properties["quiet"], false);
    assert!(properties["duration"].is_number());
    assert!(properties.get("text").is_none(), "{completed}");
    assert!(
        !completed.to_string().contains("secret-answer"),
        "{completed}"
    );
    assert!(completed.to_string().len() < 1024, "{completed}");

    let types: Vec<String> = plain
        .requests()
        .iter()
        .map(|request| header(request, "x-alforria-event").to_string())
        .collect();
    assert!(types.contains(&"trigger.fired".to_string()), "{types:?}");

    // The other new events are selectable by name and carry no content.
    let services = f.ctx.instances.load(&f.dirs.repo).unwrap();
    for (definition, data) in [
        (
            &alforria_core::session::recovery::SESSION_INTERRUPTED,
            json!({ "sessionID": "ses_x", "action": "notify", "tools": 2, "detail": "secret-detail text that is long" }),
        ),
        (
            &alforria_core::session::media::MEDIA_OMITTED,
            json!({ "sessionID": "ses_x", "providerID": "p", "modelID": "m",
                    "omitted": [{ "mime": "image/png", "filename": "secret-name.png" }] }),
        ),
        (
            &alforria_core::memory::MEMORY_UPDATED,
            json!({ "id": "mem_1", "action": "created", "content": "secret-memory" }),
        ),
        (
            &alforria_core::trigger::TRIGGER_FAILED,
            json!({ "triggerID": "brief", "runID": "trr_9", "duration": 3, "error": "secret-error" }),
        ),
        (
            &alforria_core::trigger::TRIGGER_SKIPPED,
            json!({ "triggerID": "brief", "runID": "trr_8", "reason": "schedule", "cause": "busy" }),
        ),
    ] {
        services
            .events
            .publish(definition, data, Default::default())
            .unwrap();
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let requests = plain.requests();
        let seen = |name: &str| {
            requests
                .iter()
                .any(|request| header(request, "x-alforria-event") == name)
        };
        if [
            "session.interrupted",
            "session.media.omitted",
            "memory.updated",
            "trigger.failed",
            "trigger.skipped",
        ]
        .iter()
        .all(|name| seen(name))
        {
            let all: String = requests
                .iter()
                .map(|request| request.body.clone())
                .collect();
            assert!(!all.contains("secret-"), "{all}");
            assert!(all.contains("\"action\":\"created\""), "{all}");
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "not every event arrived"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Only what it selected, and nothing with the answer in it.
    assert_eq!(wordy.requests().len(), 1);
    f.ctx.outbox.stop();
}

/// A receiver that accepts connections and never answers.
struct Hung {
    url: String,
    accepted: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Hung {
    async fn start() -> Hung {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted: Arc<AtomicUsize> = Arc::default();
        let count = accepted.clone();
        let task = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
                count.fetch_add(1, Ordering::SeqCst);
            }
        });
        Hung {
            url: format!("http://127.0.0.1:{port}/hook"),
            accepted,
            task,
        }
    }

    async fn wait_for(&self, count: usize) {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while self.accepted.load(Ordering::SeqCst) < count {
            assert!(
                std::time::Instant::now() < deadline,
                "the hung receiver was never called"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

impl Drop for Hung {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn session_of(request: &Received) -> String {
    serde_json::from_str::<Value>(&request.body).unwrap()["sessionID"]
        .as_str()
        .unwrap()
        .to_string()
}

/// One receiver that never answers holds up its own deliveries and nobody
/// else's: the other hook's event arrives while the first attempt at the
/// dead one is still in flight (its timeout is a minute away).
#[tokio::test]
async fn a_hung_receiver_does_not_delay_another_hooks_delivery() {
    let dead = Hung::start().await;
    let healthy = Receiver::start().await;
    let dir = tempfile::tempdir().unwrap();
    let outbox = Outbox::new(
        storage(&dir),
        Some(&hook(json!({
            "webhook": [
                { "name": "dead", "url": dead.url, "secret": "s", "events": ["session.idle"] },
                { "name": "healthy", "url": healthy.url, "secret": "s", "events": ["permission.asked"] }
            ],
            "outbox": { "timeout": "60s" }
        }))),
    )
    .unwrap();
    // Five idle events owed to the dead receiver, recorded first, then the
    // one a control plane is waiting for.
    for index in 0..5 {
        outbox.capture(&event(
            "session.idle",
            json!({ "sessionID": format!("ses_{index}") }),
        ));
    }
    outbox.capture(&event(
        "permission.asked",
        json!({ "id": "per_1", "sessionID": "ses_x", "permission": "bash" }),
    ));
    let pass = tokio::spawn({
        let outbox = outbox.clone();
        async move { outbox.deliver_due().await }
    });
    let arrived = healthy.wait_for(1).await;
    assert_eq!(header(&arrived[0], "x-alforria-event"), "permission.asked");
    dead.wait_for(1).await;
    assert!(!pass.is_finished(), "the dead hook's attempt is in flight");
    // The dead hook's queue is worked in order: one attempt, at its head.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(dead.accepted.load(Ordering::SeqCst), 1);
    let rows = outbox.deliveries();
    assert!(rows[5].delivered, "{:?}", rows[5]);
    assert!(rows[..5].iter().all(|row| !row.delivered && !row.failed));
    pass.abort();
}

/// A hook's deliveries stay in the order they were recorded: what follows a
/// failed one waits for it, and goes out after it once it succeeds.
#[tokio::test]
async fn a_hooks_later_deliveries_wait_behind_its_first_failure() {
    let receiver = Receiver::start().await;
    let other = Receiver::start().await;
    let dir = tempfile::tempdir().unwrap();
    let clock = TestClock::new();
    let outbox = Outbox::with_clock(
        storage(&dir),
        Some(&hook(json!({
            "webhook": [
                { "name": "first", "url": receiver.url, "secret": "s", "events": ["session.idle"] },
                { "name": "other", "url": other.url, "secret": "s", "events": ["session.idle"] }
            ],
            "outbox": { "retry_initial": "1m", "retry_max": "1m" }
        }))),
        clock.clone(),
    )
    .unwrap();
    receiver.answer_with(&[500]);
    for index in 0..3 {
        outbox.capture(&event(
            "session.idle",
            json!({ "sessionID": format!("ses_{index}") }),
        ));
    }
    // One attempt at the failing hook's head; the other hook gets all three.
    assert_eq!(outbox.deliver_due().await, 4);
    assert_eq!(receiver.requests().len(), 1);
    let sessions = |receiver: &Receiver| -> Vec<String> {
        receiver.requests().iter().map(session_of).collect()
    };
    assert_eq!(sessions(&other), ["ses_0", "ses_1", "ses_2"]);
    // While the head backs off, nothing behind it is tried.
    clock.advance(Duration::from_secs(30));
    assert_eq!(outbox.deliver_due().await, 0);
    assert_eq!(receiver.requests().len(), 1);
    let waiting: Vec<u32> = outbox
        .deliveries()
        .iter()
        .filter(|row| row.hook == "first")
        .map(|row| row.attempts)
        .collect();
    assert_eq!(waiting, [1, 0, 0]);
    // The head is due again and succeeds: the rest follow, in order.
    clock.advance(Duration::from_secs(31));
    assert_eq!(outbox.deliver_due().await, 3);
    assert_eq!(sessions(&receiver), ["ses_0", "ses_0", "ses_1", "ses_2"]);
    assert!(outbox.deliveries().iter().all(|row| row.delivered));
}

/// A receiver that refuses a delivery for good (404, 410, any 4xx that is
/// not about timing) is not asked again: the row is closed at once with the
/// status. A 500, a 429, a 408 and a timeout are retried.
#[tokio::test]
async fn a_refused_delivery_is_given_up_and_a_failed_one_is_retried() {
    use alforria_server::outbox::permanent_status;
    for status in [400, 401, 403, 404, 405, 410, 413, 422, 499] {
        assert!(permanent_status(status), "{status}");
    }
    for status in [200, 301, 302, 408, 425, 429, 500, 502, 503, 504] {
        assert!(!permanent_status(status), "{status}");
    }

    let receiver = Receiver::start().await;
    let dir = tempfile::tempdir().unwrap();
    let clock = TestClock::new();
    let outbox = Outbox::with_clock(
        storage(&dir),
        Some(&hook(json!({
            "webhook": [{ "url": receiver.url, "secret": "s", "events": ["session.idle"] }],
            "outbox": { "retry_initial": "1m", "retry_max": "1m", "max_age": "72h" }
        }))),
        clock.clone(),
    )
    .unwrap();
    let idle = |session: &str| event("session.idle", json!({ "sessionID": session }));

    for (index, status) in [404u16, 410].into_iter().enumerate() {
        receiver.answer_with(&[status]);
        outbox.capture(&idle(&format!("ses_gone_{index}")));
        assert_eq!(outbox.deliver_due().await, 1);
        let row = outbox.deliveries().remove(index);
        assert!(row.failed && !row.delivered, "{row:?}");
        assert_eq!(row.attempts, 1);
        assert_eq!(
            row.last_error.as_deref(),
            Some(format!("the receiver answered {status}").as_str())
        );
        // Not now, and not after any backoff.
        let before = receiver.requests().len();
        assert_eq!(outbox.deliver_due().await, 0);
        clock.advance(Duration::from_secs(3600));
        assert_eq!(outbox.deliver_due().await, 0);
        assert_eq!(receiver.requests().len(), before, "{status} is not retried");
    }
    assert_eq!(outbox.stats().failed, 2);
    assert_eq!(outbox.stats().pending, 0);

    // The ones worth another attempt get one, with the same delivery id.
    receiver.answer_with(&[500, 429, 408]);
    outbox.capture(&idle("ses_retried"));
    for attempt in 1..=3u32 {
        assert_eq!(outbox.deliver_due().await, 1);
        let row = outbox.deliveries().remove(2);
        assert!(!row.failed && !row.delivered, "{row:?}");
        assert_eq!(row.attempts, attempt);
        clock.advance(Duration::from_secs(61));
    }
    assert_eq!(outbox.deliver_due().await, 1);
    let row = outbox.deliveries().remove(2);
    assert!(row.delivered, "{row:?}");
    assert_eq!(row.attempts, 4);
    let requests = receiver.requests();
    let retried: Vec<&str> = requests[2..]
        .iter()
        .map(|request| header(request, "x-alforria-delivery"))
        .collect();
    assert_eq!(retried, [row.id.as_str(); 4]);

    // A receiver that does not answer in time is retried too.
    let hung = Hung::start().await;
    let dir = tempfile::tempdir().unwrap();
    let outbox = Outbox::new(
        storage(&dir),
        Some(&hook(json!({
            "webhook": [{ "url": hung.url, "secret": "s", "events": ["session.idle"] }],
            "outbox": { "timeout": "200ms" }
        }))),
    )
    .unwrap();
    outbox.capture(&idle("ses_slow"));
    assert_eq!(outbox.deliver_due().await, 1);
    let row = outbox.deliveries().remove(0);
    assert!(!row.failed && !row.delivered, "{row:?}");
    assert_eq!(
        row.last_error.as_deref(),
        Some("the receiver did not answer in time")
    );
}

/// The delivery is in the database before the publish that carried the
/// event returns: a process killed right after `session.idle` — here, an
/// outbox dropped at once, with nothing flushed or awaited — leaves it for
/// the next one.
#[tokio::test]
async fn the_row_is_on_disk_before_the_event_is_published() {
    use alforria_core::event::definition::Definition;
    let receiver = Receiver::start().await;
    let dir = tempfile::tempdir().unwrap();
    let config = hook(json!({
        "webhook": [{ "url": receiver.url, "secret": "s", "events": ["session.idle"] }]
    }));
    {
        let storage = storage(&dir);
        let bus = alforria_core::EventBus::new_shared(storage.clone(), None);
        let outbox = Outbox::new(storage, Some(&config)).unwrap();
        outbox.attach(&bus);
        bus.publish(
            &Definition::ephemeral("session.idle"),
            json!({ "sessionID": "ses_killed" }),
            Default::default(),
        )
        .unwrap();
        // Another connection, as another process would have, sees the row
        // the moment the publish has returned.
        let other = Storage::open(dir.path().join("db.sqlite")).unwrap();
        let rows: i64 = other
            .with_connection(|conn| {
                conn.query_row("SELECT COUNT(*) FROM outbox", [], |row| row.get(0))
            })
            .unwrap();
        assert_eq!(rows, 1);
        // The "kill": everything goes, nothing was delivered.
    }
    assert!(receiver.requests().is_empty());
    let outbox = Outbox::new(storage(&dir), Some(&config)).unwrap();
    assert_eq!(outbox.deliveries().len(), 1);
    assert_eq!(outbox.deliver_due().await, 1);
    assert_eq!(session_of(&receiver.requests()[0]), "ses_killed");
}

/// Events dropped beyond `max_pending` are counted, and `GET /attention`
/// shows the count on a server that has hooks — and nothing new on one that
/// has none.
#[tokio::test]
async fn dropped_deliveries_are_counted_and_shown() {
    let dead = Hung::start().await;
    let url = dead.url.clone();
    let f = support::fixture_with(
        move |_| {
            json!({
                "hook": {
                    "webhook": [{ "url": url, "secret": "s", "events": ["session.idle"] }],
                    "outbox": { "max_pending": 2 }
                }
            })
        },
        None,
        Vec::new(),
    );
    let attention = || async {
        support::body_json(support::call(&f.router, "GET", "/attention", None, None).await).await
    };
    assert_eq!(
        attention().await["outbox"],
        json!({ "pending": 0, "failed": 0, "dropped": 0 })
    );
    for index in 0..5 {
        f.ctx.outbox.capture(&event(
            "session.idle",
            json!({ "sessionID": format!("ses_{index}") }),
        ));
    }
    let stats = f.ctx.outbox.stats();
    assert_eq!((stats.pending, stats.dropped), (2, 3));
    let body = attention().await;
    assert_eq!(
        body["outbox"],
        json!({ "pending": 2, "failed": 0, "dropped": 3 })
    );
    assert_eq!(body["count"], 0);

    // No hooks: the answer is what it always was.
    let plain = support::fixture_with(|_| json!({}), None, Vec::new());
    let body =
        support::body_json(support::call(&plain.router, "GET", "/attention", None, None).await)
            .await;
    assert_eq!(body, json!({ "count": 0, "items": [] }));
}

/// Stopping never waits for a receiver: with an attempt hanging (its
/// timeout a minute away) the loop ends at once, and the delivery it was
/// making is still owed.
#[tokio::test]
async fn shutdown_does_not_wait_for_a_hung_receiver() {
    let dead = Hung::start().await;
    let dir = tempfile::tempdir().unwrap();
    let outbox = Outbox::new(
        storage(&dir),
        Some(&hook(json!({
            "webhook": [{ "url": dead.url, "secret": "s", "events": ["session.idle"] }],
            "outbox": { "timeout": "60s" }
        }))),
    )
    .unwrap();
    outbox.spawn();
    outbox.capture(&event("session.idle", json!({ "sessionID": "ses_1" })));
    dead.wait_for(1).await;
    tokio::time::timeout(Duration::from_secs(5), outbox.shutdown())
        .await
        .expect("shutdown waited for the receiver");
    let row = outbox.deliveries().remove(0);
    assert!(!row.delivered && !row.failed, "{row:?}");
    assert_eq!(row.attempts, 0, "an attempt cut short is not counted");
}

/// Two servers on one data directory both record what their own events owe,
/// and one of them delivers: a row is sent once. When that one stops, the
/// other takes over.
#[tokio::test]
async fn two_processes_on_one_data_directory_deliver_a_row_once() {
    use alforria_server::outbox::outbox_lock;
    let receiver = Receiver::start().await;
    let dir = tempfile::tempdir().unwrap();
    let config = hook(json!({
        "webhook": [{ "url": receiver.url, "secret": "s", "events": ["session.idle"] }]
    }));
    let process = || {
        let outbox = Outbox::new(storage(&dir), Some(&config)).unwrap();
        outbox.deliver_as(outbox_lock(dir.path()));
        outbox
    };
    let (first, second) = (process(), process());
    let idle = |session: &str| event("session.idle", json!({ "sessionID": session }));

    // The first to make a pass is the one that delivers.
    assert_eq!(first.deliver_due().await, 0);
    second.capture(&idle("ses_from_second"));
    assert_eq!(second.deliver_due().await, 0, "a follower sends nothing");
    assert!(receiver.requests().is_empty());
    assert_eq!(
        first.deliver_due().await,
        1,
        "the other's row is its to send"
    );
    assert_eq!(second.deliver_due().await, 0);
    assert_eq!(receiver.requests().len(), 1);
    assert!(second.deliveries()[0].delivered);

    // Both loops running: every row goes out once.
    first.spawn();
    second.spawn();
    first.capture(&idle("ses_a"));
    second.capture(&idle("ses_b"));
    receiver.wait_for(3).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(receiver.requests().len(), 3);

    // The one delivering stops: the other has the place on its next pass.
    first.shutdown().await;
    second.capture(&idle("ses_after"));
    let requests = receiver.wait_for(4).await;
    assert_eq!(session_of(&requests[3]), "ses_after");
    second.shutdown().await;
    assert_eq!(receiver.requests().len(), 4);
}

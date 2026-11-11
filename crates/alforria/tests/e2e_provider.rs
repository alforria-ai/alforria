//! Provider robustness, end to end: the built binary against a scripted
//! OpenAI-compatible server that misbehaves the way real ones do.

#[path = "e2e_agent/harness.rs"]
#[allow(dead_code)]
mod harness;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::routing::post;
use axum::Router;
use serde_json::{json, Value};

use harness::{Env, MockServer};

#[derive(Default)]
struct Seen {
    /// When each turn request arrived.
    turns: Vec<Instant>,
    /// Title requests, which are not turns and have their own retries.
    titles: usize,
    /// Bodies of the turn requests.
    bodies: Vec<Value>,
}

/// A model whose backend is down: every request gets the 503 an
/// OpenAI-compatible gateway answers with, `headers` included.
fn unavailable(
    headers: &'static [(&'static str, &'static str)],
) -> (MockServer, String, Arc<Mutex<Seen>>) {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let app = Router::new().route(
        "/v1/chat/completions",
        post({
            let seen = seen.clone();
            move |axum::Json(body): axum::Json<Value>| {
                let seen = seen.clone();
                async move {
                    {
                        let mut seen = seen.lock().unwrap();
                        if body
                            .to_string()
                            .contains("Generate a title for this conversation")
                        {
                            seen.titles += 1;
                        } else {
                            seen.turns.push(Instant::now());
                            seen.bodies.push(body);
                        }
                    }
                    let mut response = axum::response::Response::builder()
                        .status(503)
                        .header("content-type", "application/json");
                    for (name, value) in headers {
                        response = response.header(*name, *value);
                    }
                    response
                        .body(axum::body::Body::from(
                            json!({ "detail": "All servers unavailable for model mock-model" })
                                .to_string(),
                        ))
                        .expect("response")
                }
            }
        }),
    );
    let server = MockServer::new(app);
    let url = format!("http://127.0.0.1:{}/v1", server.port);
    (server, url, seen)
}

/// The `error` event of a `run --format json` stream.
fn error_event(stdout: &str) -> Value {
    stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|event| event["type"] == "error")
        .unwrap_or_else(|| panic!("no error event in {stdout}"))
}

/// The retry policy allows five retries. Each used to be three requests,
/// because the HTTP client retried twice by itself under every attempt:
/// eighteen requests, and with the backoff the turn's error came after
/// some eighty seconds.
#[test]
fn a_model_that_stays_unavailable_ends_the_turn_after_five_retries() {
    // `retry-after: 0` takes the waiting out of the test, not the counting.
    let (_server, url, seen) = unavailable(&[("retry-after", "0")]);
    let env = Env::new("unavailable");
    env.write_project_config(&url, "unavailable");
    let output = env.run(&[
        "run",
        "--model",
        "mock/mock-model",
        "--format",
        "json",
        "hi",
    ]);

    assert_eq!(output.code, Some(1), "stdout={}", output.stdout);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.turns.len(), 6, "one attempt and five retries");
    let error = error_event(&output.stdout);
    assert_eq!(error["error"]["name"], "APIError", "{error}");
    assert_eq!(error["error"]["data"]["statusCode"], 503);
    assert_eq!(
        error["error"]["data"]["message"],
        "Service Unavailable: {\"detail\":\"All servers unavailable for model mock-model\"}"
    );
}

/// The same without the header, at the real backoff: how long a turn takes
/// to give up. `cargo nextest run --run-ignored all -E 'test(measure_)' --no-capture`.
#[test]
#[ignore = "measurement: takes the full backoff, about a minute"]
fn measure_a_model_that_stays_unavailable() {
    let (_server, url, seen) = unavailable(&[]);
    let env = Env::new("unavailable-measure");
    env.write_project_config(&url, "unavailable-measure");
    let started = Instant::now();
    let output = env.run_with_timeout(
        &[
            "run",
            "--model",
            "mock/mock-model",
            "--format",
            "json",
            "hi",
        ],
        None,
        Duration::from_secs(240),
    );
    let seen = seen.lock().unwrap();
    eprintln!(
        "exit {:?} after {:.1}s: {} turn requests (+{} title) at {:?}",
        output.code,
        started.elapsed().as_secs_f32(),
        seen.turns.len(),
        seen.titles,
        seen.turns
            .iter()
            .map(|at| (at.duration_since(started).as_secs_f32() * 10.0).round() / 10.0)
            .collect::<Vec<_>>()
    );
    assert_eq!(output.code, Some(1));
    assert_eq!(seen.turns.len(), 6);
    assert_eq!(error_event(&output.stdout)["error"]["name"], "APIError");
}

/// A model that answers, keeping the raw text of every turn request.
fn recording() -> (MockServer, String, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new().route(
        "/v1/chat/completions",
        post({
            let seen = seen.clone();
            move |body: String| {
                let seen = seen.clone();
                async move {
                    if !body.contains("Generate a title for this conversation") {
                        seen.lock().unwrap().push(body);
                    }
                    let mut reply = format!(
                        "data: {}\n\n",
                        json!({ "choices": [{ "delta": { "content": "ok" } }] })
                    );
                    reply.push_str(&format!(
                        "data: {}\n\n",
                        json!({
                            "choices": [{ "delta": {}, "finish_reason": "stop" }],
                            "usage": { "prompt_tokens": 10, "completion_tokens": 1 },
                        })
                    ));
                    reply.push_str("data: [DONE]\n\n");
                    axum::response::Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(axum::body::Body::from(reply))
                        .expect("response")
                }
            }
        }),
    );
    let server = MockServer::new(app);
    let url = format!("http://127.0.0.1:{}/v1", server.port);
    (server, url, seen)
}

/// The smallest PNG there is: one transparent pixel.
const PIXEL: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae,
    0x42, 0x60, 0x82,
];

/// Ask about an image on a model declared with `model`'s extra fields, and
/// return the request that reached the provider.
fn ask_about_an_image(tag: &str, model: Value) -> String {
    let (_server, url, seen) = recording();
    let env = Env::new(tag);
    let project = env.project_dir();
    let mut entry =
        json!({ "name": "Mock Model", "limit": { "context": 100_000, "output": 4096 } });
    for (key, value) in model.as_object().unwrap() {
        entry[key] = value.clone();
    }
    std::fs::write(
        project.join("opencode.json"),
        json!({
            "provider": { "mock": {
                "options": { "apiKey": "test-key", "baseURL": url },
                "models": { "mock-model": entry },
            } },
            "share": "disabled",
        })
        .to_string(),
    )
    .unwrap();
    let image = project.join("shot.png");
    std::fs::write(&image, PIXEL).unwrap();
    let output = env.run(&[
        "run",
        "--model",
        "mock/mock-model",
        "--file",
        image.to_str().unwrap(),
        "what is in this picture?",
    ]);
    assert_eq!(output.code, Some(0), "stderr={}", output.stderr);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "{seen:?}");
    seen[0].clone()
}

/// An OpenAI-compatible server drops an image it cannot use without a
/// word, and the model says nothing was attached. A model declared
/// text-only is told an image was there instead.
#[test]
fn an_image_is_not_sent_to_a_model_declared_without_image_input() {
    let request = ask_about_an_image(
        "image-text-only",
        json!({ "attachment": false, "modalities": { "input": ["text"], "output": ["text"] } }),
    );
    assert!(!request.contains("image_url"), "{request}");
    assert!(
        request.contains("[image omitted: the current model cannot read images]"),
        "{request}"
    );
    assert!(request.contains("what is in this picture?"), "{request}");
    // Whole numbers go out as integers: some servers refuse `4096.0`.
    assert!(request.contains("\"max_tokens\":4096"), "{request}");
    assert!(!request.contains("4096.0"), "{request}");
}

#[test]
fn an_image_is_sent_to_a_model_that_reads_images() {
    for (tag, model) in [
        (
            "image-modality",
            json!({ "modalities": { "input": ["text", "image"], "output": ["text"] } }),
        ),
        ("image-attachment", json!({ "attachment": true })),
    ] {
        let request = ask_about_an_image(tag, model);
        assert!(request.contains("image_url"), "{tag}: {request}");
        assert!(!request.contains("image omitted"), "{tag}: {request}");
    }
}

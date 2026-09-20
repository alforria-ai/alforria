//! The backend seam (spec E2E §2.2) + the scripted wire mock.

use std::sync::{Arc, Mutex};

use axum::routing::post;
use axum::Router;
use serde_json::Value;

use crate::harness::MockServer;
use crate::transcript::{summarizer_turn, turn_chunks, Chunk, Transcript, Turn};

/// One scripted response body: the lowered turn chunks, streamed with
/// their mid-stream stalls (spec E2E §2.3 `sleep_ms`).
fn turn_body(turn: Turn) -> axum::body::Body {
    use futures::StreamExt;
    let chunks: Vec<Chunk> = turn_chunks(&turn);
    let stream = futures::stream::iter(chunks).then(|chunk| async move {
        if let Some(ms) = chunk.delay_ms {
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        }
        Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(chunk.body))
    });
    axum::body::Body::from_stream(stream)
}

/// Everything a scenario needs to wire and drive one LLM provider.
pub trait LlmBackend {
    /// OpenAI-compatible base URL wired into the provider options.
    fn base_url(&self) -> String;
    fn api_key(&self) -> String;
    fn provider_id(&self) -> &str;
    fn model_id(&self) -> &str;
    /// The provider/model context limit for the config.
    fn context_limit(&self) -> f64;
    /// `Some` = a scripted transcript exists for the scenario (mock).
    fn transcript(&self, scenario: &str) -> Option<Transcript>;
    /// Relaxation policy: which assertions hold for this backend.
    fn live(&self) -> bool;
}

/// An axum SSE server speaking the real OpenAI-compatible wire protocol:
/// one transcript turn is popped and streamed per HTTP request, and every
/// request body is recorded for post-hoc assertions.
pub struct MockBackend {
    port: u16,
    transcript: Transcript,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl MockBackend {
    pub fn new(scenario: &str) -> MockBackend {
        let transcript = Transcript::load(scenario);
        let requests: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let turns = Arc::new(Mutex::new(std::collections::VecDeque::from(
            transcript.turns.clone(),
        )));
        let recorder = requests.clone();
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |body: String| {
                let requests = recorder.clone();
                let turns = turns.clone();
                async move {
                    let value = serde_json::from_str::<Value>(&body).expect("request body");
                    // Tool-bearing requests are the agent loop's; the
                    // title/summary fork carries no tools and gets a canned
                    // response so it cannot consume scenario turns.
                    let turn = if value.get("tools").is_some() {
                        requests.lock().unwrap().push(value);
                        turns.lock().unwrap().pop_front()
                    } else {
                        Some(summarizer_turn())
                    };
                    match turn {
                        Some(turn) => axum::response::Response::builder()
                            .header("content-type", "text/event-stream")
                            .body(turn_body(turn))
                            .expect("response"),
                        None => axum::response::Response::builder()
                            .status(500)
                            .header("content-type", "application/json")
                            .body(axum::body::Body::from(
                                "{\"error\": \"transcript exhausted\"}".to_string(),
                            ))
                            .expect("response"),
                    }
                }
            }),
        );
        let server = MockServer::new(app);
        MockBackend {
            port: server.port,
            transcript,
            requests,
        }
    }

    /// The recorded request bodies, in arrival order.
    pub fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }

    /// The recorded request body at `index` (0-based).
    pub fn request(&self, index: usize) -> Value {
        self.requests()
            .get(index)
            .expect("recorded request")
            .clone()
    }
}

impl LlmBackend for MockBackend {
    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    fn api_key(&self) -> String {
        "test-key".to_string()
    }

    fn provider_id(&self) -> &str {
        "mock"
    }

    fn model_id(&self) -> &str {
        "mock-model"
    }

    fn context_limit(&self) -> f64 {
        100_000.0
    }

    fn transcript(&self, _scenario: &str) -> Option<Transcript> {
        Some(self.transcript.clone())
    }

    fn live(&self) -> bool {
        false
    }
}

//! The backend seam (spec E2E §2.2) + the scripted wire mock.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
    /// Per-turn LLM budget for live backends (spec E2E §3.4).
    fn turn_timeout(&self) -> Duration;
}

/// The compaction fork rides the same endpoint tool-less (compaction.rs
/// `build_prompt`): its summary request is served from the scenario
/// transcript, so the fork consumes slot N+1 (spec E2E §2.4 A6).
const COMPACTION_MARKER: &str = "Create a new anchored summary from the conversation history";

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
                    // Three keys: agent-loop requests carry `tools`; the
                    // compaction fork carries the summary prompt marker;
                    // the title/summary fork carries neither and gets a
                    // canned response so it cannot consume scenario turns.
                    let compaction = body.contains(COMPACTION_MARKER);
                    let turn = if value.get("tools").is_some() || compaction {
                        if value.get("tools").is_some() {
                            requests.lock().unwrap().push(value);
                        }
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

    fn turn_timeout(&self) -> Duration {
        Duration::from_secs(60)
    }
}

/// The live LibertAI model tiers (spec E2E §3.4): the cheap tier drives
/// the full live scenario set; the thinking-quality tiers only the smoke
/// scenarios (file mutation, read-answer, structured output).
#[derive(Clone, Copy)]
pub enum LiveModel {
    Cheap,
    QualityGlm,
    QualityDeepseek,
}

impl LiveModel {
    fn id(self) -> &'static str {
        match self {
            LiveModel::Cheap => "qwen3.5-4b",
            LiveModel::QualityGlm => "glm-5.3",
            LiveModel::QualityDeepseek => "deepseek-v4.1-flash",
        }
    }

    fn context_limit(self) -> f64 {
        match self {
            LiveModel::Cheap => 32768.0,
            _ => 262144.0,
        }
    }

    fn turn_timeout(self) -> Duration {
        match self {
            LiveModel::Cheap => Duration::from_secs(90),
            _ => Duration::from_secs(180),
        }
    }
}

/// The live LibertAI endpoint (spec E2E §3.1): no server — `opencode.json`
/// points `baseURL` straight at the OpenAI-compatible API. The key is
/// resolved once, never logged, never asserted on.
pub struct LibertaiBackend {
    model_id: String,
    context_limit: f64,
    turn_timeout: Duration,
    api_key: String,
}

impl LibertaiBackend {
    pub fn new(model: LiveModel) -> LibertaiBackend {
        LibertaiBackend {
            api_key: libertai_api_key(),
            turn_timeout: model.turn_timeout(),
            context_limit: model.context_limit(),
            model_id: model.id().to_string(),
        }
    }
}

impl LlmBackend for LibertaiBackend {
    fn base_url(&self) -> String {
        std::env::var("LIBERTAI_API_BASE")
            .ok()
            .filter(|base| !base.trim().is_empty())
            .unwrap_or_else(|| "https://api.libertai.io/v1".to_string())
    }

    fn api_key(&self) -> String {
        self.api_key.clone()
    }

    fn provider_id(&self) -> &str {
        "libertai"
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn context_limit(&self) -> f64 {
        self.context_limit
    }

    fn transcript(&self, _scenario: &str) -> Option<Transcript> {
        None
    }

    fn live(&self) -> bool {
        true
    }

    fn turn_timeout(&self) -> Duration {
        self.turn_timeout
    }
}

/// Key-resolution order (spec E2E §3.1): env, then `[auth] api_key` from
/// the libertai config, then a one-shot `libertai run` injection. Missing
/// key ⇒ fail fast, never a network retry loop.
fn libertai_api_key() -> String {
    static KEY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    KEY.get_or_init(|| {
        for name in ["LIBERTAI_API_KEY", "OPENAI_API_KEY"] {
            if let Ok(key) = std::env::var(name) {
                if !key.trim().is_empty() {
                    return key;
                }
            }
        }
        config_api_key()
            .or_else(cli_api_key)
            .expect("libertai CLI not authenticated: set LIBERTAI_API_KEY or run `libertai login`")
    })
    .clone()
}

fn config_api_key() -> Option<String> {
    let path = match std::process::Command::new("libertai")
        .args(["config", "path"])
        .output()
    {
        Ok(output) if output.status.success() => {
            PathBuf::from(String::from_utf8_lossy(&output.stdout).trim())
        }
        _ => std::env::var("HOME")
            .ok()
            .map(|home| PathBuf::from(home).join(".config/libertai/config.toml"))?,
    };
    parse_api_key(&std::fs::read_to_string(path).ok()?)
}

fn parse_api_key(text: &str) -> Option<String> {
    let mut in_auth = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_auth = line == "[auth]";
        } else if in_auth {
            if let Some((key, value)) = line.split_once('=') {
                let value = value.trim().trim_matches(['"', '\'']);
                if key.trim() == "api_key" && !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    None
}

fn cli_api_key() -> Option<String> {
    let output = std::process::Command::new("libertai")
        .args(["run", "--", "sh", "-c", "echo $LIBERTAI_API_KEY"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let key = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!key.is_empty()).then_some(key)
}

//! Fixture transcript type + SSE replayer (spec E2E §2.3): a transcript
//! is a list of turns; the mock backend pops one turn per HTTP request and
//! streams its frames as real OpenAI-compatible SSE chunks so the full
//! production protocol decode runs end-to-end.

use serde::Deserialize;
use serde_json::{json, Value};

/// A scripted response sequence, loaded from
/// `tests/e2e_agent/fixtures/<scenario>.json`. The optional `comment`
/// fixture key documents the scenario and is ignored by the parser.
#[derive(Debug, Clone, Deserialize)]
pub struct Transcript {
    pub turns: Vec<Turn>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Turn {
    pub frames: Vec<Frame>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Frame {
    Text { text: String },
    ToolCall { tool_call: ToolCall },
    Finish { finish: Finish },
    Sleep { sleep_ms: u64 },
}

/// One lowered wire chunk: the SSE bytes to flush and an optional
/// mid-stream stall before flushing them (spec E2E §2.3 `sleep_ms`).
pub struct Chunk {
    pub body: String,
    pub delay_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Finish {
    pub reason: String,
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Usage {
    pub input: f64,
    pub output: f64,
}

fn data_line(payload: &Value) -> String {
    format!("data: {payload}\n\n")
}

fn chunk(delta: Value) -> Value {
    json!({"choices": [{"index": 0, "delta": delta}]})
}

/// Split the serialized arguments at a char boundary to exercise the
/// incremental `tool_calls` delta assembly (openai-chat streams arguments
/// across multiple chunks).
fn split_arguments(arguments: &str) -> Vec<String> {
    if arguments.is_empty() {
        return Vec::new();
    }
    let mut mid = arguments.len() / 2;
    while !arguments.is_char_boundary(mid) {
        mid += 1;
    }
    vec![arguments[..mid].to_string(), arguments[mid..].to_string()]
}

/// Lower one turn to wire chunks: every `sleep_ms` frame stalls the
/// emission of the chunks after it (the abort-test seam).
pub fn turn_chunks(turn: &Turn) -> Vec<Chunk> {
    let mut out: Vec<Chunk> = Vec::new();
    let mut current = String::new();
    let mut delay: Option<u64> = None;
    let mut tool_index = 0usize;
    for frame in &turn.frames {
        if let Frame::Sleep { sleep_ms } = frame {
            if !current.is_empty() {
                out.push(Chunk {
                    body: std::mem::take(&mut current),
                    delay_ms: None,
                });
            }
            delay = Some(delay.unwrap_or_default() + sleep_ms);
            continue;
        }
        match frame {
            Frame::Text { text } => {
                current.push_str(&data_line(&chunk(json!({"content": text}))));
            }
            Frame::ToolCall { tool_call } => {
                current.push_str(&data_line(&chunk(json!({
                    "tool_calls": [{
                        "index": tool_index,
                        "id": tool_call.id,
                        "type": "function",
                        "function": {"name": tool_call.name, "arguments": ""},
                    }]
                }))));
                let arguments = tool_call.arguments.to_string();
                for piece in split_arguments(&arguments) {
                    current.push_str(&data_line(&chunk(json!({
                        "tool_calls": [{
                            "index": tool_index,
                            "function": {"arguments": piece},
                        }]
                    }))));
                }
                tool_index += 1;
            }
            Frame::Finish { finish } => {
                let mut payload = json!({
                    "choices": [{"index": 0, "delta": {}, "finish_reason": finish.reason}]
                });
                if let Some(usage) = &finish.usage {
                    payload["usage"] = json!({
                        "prompt_tokens": usage.input,
                        "completion_tokens": usage.output,
                    });
                }
                current.push_str(&data_line(&payload));
            }
            Frame::Sleep { .. } => unreachable!("handled above"),
        }
    }
    current.push_str("data: [DONE]\n\n");
    out.push(Chunk {
        body: current,
        delay_ms: delay,
    });
    out
}

impl Transcript {
    pub fn load(scenario: &str) -> Transcript {
        let path = format!(
            "{}/tests/e2e_agent/fixtures/{scenario}.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let text =
            std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("fixture {path}: {err}"));
        serde_json::from_str(&text).unwrap_or_else(|err| panic!("fixture {path} is invalid: {err}"))
    }
    /// The scripted value of one argument of the tool call with the given
    /// id (used to derive deterministic expectations from the fixture).
    pub fn tool_argument(&self, id: &str, key: &str) -> Option<Value> {
        self.turns
            .iter()
            .flat_map(|turn| turn.frames.iter())
            .find_map(|frame| match frame {
                Frame::ToolCall { tool_call } if tool_call.id == id => {
                    tool_call.arguments.get(key).cloned()
                }
                _ => None,
            })
    }

    /// The concatenated `text` frames of one turn (fixture-derived
    /// expectations, e.g. the compaction summary marker).
    pub fn turn_text(&self, index: usize) -> String {
        self.turns[index]
            .frames
            .iter()
            .filter_map(|frame| match frame {
                Frame::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

/// The canned response served to summarizer/title requests: those fork on
/// the session engine (summary.ts) with no tools in the request body, so
/// the mock keys on `tools` presence to keep the scenario turn queue for
/// the agent loop.
pub fn summarizer_turn() -> Turn {
    Turn {
        frames: vec![
            Frame::Text {
                text: "e2e session title".to_string(),
            },
            Frame::Finish {
                finish: Finish {
                    reason: "stop".to_string(),
                    usage: Some(Usage {
                        input: 5.0,
                        output: 2.0,
                    }),
                },
            },
        ],
    }
}

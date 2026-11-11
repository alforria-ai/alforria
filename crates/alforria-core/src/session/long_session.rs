//! Test support for long sessions: a generator that writes a synthetic
//! history straight into SQLite, the way `alforria import` does (so no
//! message carries a provider token count), and a provider double that
//! rejects any request over its context limit with a 400.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use alforria_llm::schema::errors::{
    HttpContext, HttpRequestDetails, HttpResponseDetails, LlmError,
};
use alforria_llm::schema::events::{LlmEvent, Usage};
use alforria_llm::schema::ids::FinishReason;
use alforria_schema::session_v1::{
    ToolStateCompletedTime, V1Message, V1Part, V1StepTokens, V1TokenCache, V1ToolState,
};
use futures::StreamExt;
use serde_json::json;

use crate::session::llm::{LlmEventStream, LlmStream, StreamInput};
use crate::session::test_support::{assistant_message, text_part, user_message};
use crate::storage::Storage;

/// One request the provider double received.
#[derive(Debug, Clone)]
pub(crate) struct SeenRequest {
    pub(crate) agent: String,
    pub(crate) bytes: usize,
    pub(crate) rejected: bool,
}

/// A provider with a context limit: a request whose size, at four bytes a
/// token, exceeds it is refused with a 400 the way OpenAI-compatible servers
/// word it. Everything else is answered with a short text and a usage count.
pub(crate) struct LimitedLlm {
    limit_tokens: usize,
    /// Agents whose requests are refused whatever their size.
    refuse: Vec<String>,
    requests: Mutex<Vec<SeenRequest>>,
}

impl LimitedLlm {
    pub(crate) fn new(limit_tokens: usize) -> Arc<Self> {
        Self::refusing(limit_tokens, &[])
    }

    pub(crate) fn refusing(limit_tokens: usize, agents: &[&str]) -> Arc<Self> {
        Arc::new(LimitedLlm {
            limit_tokens,
            refuse: agents.iter().map(|agent| agent.to_string()).collect(),
            requests: Mutex::new(Vec::new()),
        })
    }

    pub(crate) fn requests(&self) -> Vec<SeenRequest> {
        self.requests.lock().unwrap().clone()
    }

    pub(crate) fn largest(&self) -> usize {
        self.requests()
            .iter()
            .map(|request| request.bytes)
            .max()
            .unwrap_or(0)
    }

    pub(crate) fn rejected(&self) -> usize {
        self.requests()
            .iter()
            .filter(|request| request.rejected)
            .count()
    }
}

/// The bytes a request puts on the wire, near enough: messages, system
/// prompt and tool definitions.
pub(crate) fn request_bytes(input: &StreamInput) -> usize {
    let messages = serde_json::to_string(&input.messages).map_or(0, |text| text.len());
    let system: usize = input.system.iter().map(String::len).sum();
    let tools: usize = input
        .tools
        .iter()
        .map(|tool| tool.name.len() + tool.description.len() + tool.input_schema.to_string().len())
        .sum();
    messages + system + tools
}

pub(crate) fn context_overflow_error(tokens: usize, limit: usize) -> LlmError {
    use alforria_llm::route::executor::{status_reason, StatusReasonInput};
    let body = json!({
        "error": {
            "message": format!(
                "This model's maximum context length is {limit} tokens. However, your messages resulted in {tokens} tokens. Please reduce the length of the messages."
            ),
            "type": "invalid_request_error",
            "code": "context_length_exceeded",
        }
    })
    .to_string();
    let http = HttpContext {
        request: HttpRequestDetails {
            method: "POST".to_string(),
            url: "https://provider.test/v1/chat/completions".to_string(),
            headers: Default::default(),
        },
        response: Some(HttpResponseDetails {
            status: 400.0,
            headers: BTreeMap::from([("content-type".to_string(), "application/json".to_string())]),
        }),
        body: Some(body.clone()),
        body_truncated: None,
        request_id: None,
        rate_limit: None,
    };
    LlmError {
        module: "RequestExecutor".to_string(),
        method: "execute".to_string(),
        reason: status_reason(StatusReasonInput {
            status: 400,
            message: format!("Provider request failed with HTTP 400: {body}"),
            retry_after_ms: None,
            rate_limit: None,
            http,
        }),
    }
}

impl LlmStream for LimitedLlm {
    fn stream(&self, input: StreamInput) -> LlmEventStream {
        let bytes = request_bytes(&input);
        let tokens = bytes / 4;
        let agent = input.agent.name.clone();
        let rejected = tokens > self.limit_tokens || self.refuse.contains(&agent);
        self.requests.lock().unwrap().push(SeenRequest {
            agent: agent.clone(),
            bytes,
            rejected,
        });
        if rejected {
            return futures::stream::iter(vec![Err(context_overflow_error(
                tokens,
                self.limit_tokens,
            ))])
            .boxed();
        }
        let text = if agent == "compaction" {
            format!("## Goal\nSummary of a {bytes}-byte request.")
        } else {
            "answer".to_string()
        };
        let usage = Usage {
            input_tokens: Some(tokens as f64),
            output_tokens: Some(10.0),
            ..Usage::default()
        };
        futures::stream::iter(vec![
            Ok(LlmEvent::StepStart { index: 0.0 }),
            Ok(LlmEvent::TextStart {
                id: "t1".to_string(),
                provider_metadata: None,
            }),
            Ok(LlmEvent::TextDelta {
                id: "t1".to_string(),
                text,
                provider_metadata: None,
            }),
            Ok(LlmEvent::TextEnd {
                id: "t1".to_string(),
                provider_metadata: None,
            }),
            Ok(LlmEvent::StepFinish {
                index: 0.0,
                reason: FinishReason::Stop,
                usage: Some(usage.clone()),
                provider_metadata: None,
            }),
            Ok(LlmEvent::Finish {
                reason: FinishReason::Stop,
                usage: Some(usage),
                provider_metadata: None,
            }),
        ])
        .boxed()
    }
}

/// The shape of a generated history.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LongSession {
    /// Messages to write. A turn is three: the user's, an assistant step
    /// that calls a tool, and the assistant's closing answer.
    pub(crate) messages: usize,
    /// The smallest and largest tool output, in bytes.
    pub(crate) tool_output: (usize, usize),
}

/// What [`write_long_session`] wrote.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Written {
    pub(crate) messages: usize,
    pub(crate) tool_parts: usize,
    pub(crate) bytes: usize,
}

fn row_data<T: serde::Serialize>(value: &T, drop: &[&str]) -> String {
    let mut value = serde_json::to_value(value).expect("serializable");
    if let Some(object) = value.as_object_mut() {
        for key in drop {
            object.remove(*key);
        }
    }
    value.to_string()
}

/// Deterministic filler that does not compress to nothing and reads like
/// tool output: numbered lines of words.
fn filler(seed: usize, bytes: usize) -> String {
    const WORDS: [&str; 12] = [
        "export", "function", "handler", "request", "response", "session", "storage", "widget",
        "render", "config", "result", "status",
    ];
    let mut out = String::with_capacity(bytes + 32);
    let mut state = (seed as u64)
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    let mut line = 1usize;
    while out.len() < bytes {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let word = WORDS[(state >> 33) as usize % WORDS.len()];
        out.push_str(&format!("{line:>5}: {word} {:08x}\n", state >> 32));
        line += 1;
    }
    out.truncate(bytes);
    out
}

/// Write a synthetic history into `session_id` with plain inserts, in one
/// transaction. No message gets a token count, as after `alforria import`.
/// Ids and timestamps ascend from 1, so anything the engine appends later
/// sorts after them.
pub(crate) fn write_long_session(
    storage: &Storage,
    session_id: &str,
    shape: LongSession,
) -> Written {
    let mut written = Written::default();
    let (low, high) = shape.tool_output;
    storage
        .with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            {
                let mut message = tx.prepare(
                    "INSERT INTO message (id, session_id, time_created, time_updated, data)
                     VALUES (?1, ?2, ?3, ?3, ?4)",
                )?;
                let mut part = tx.prepare(
                    "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
                     VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
                )?;
                let mut user_id = String::new();
                for index in 0..shape.messages {
                    let created = index as i64 + 1;
                    let id = format!("msg_{index:026}");
                    let part_id = |n: usize| format!("prt_{:026}", index * 4 + n);
                    let (info, parts): (V1Message, Vec<V1Part>) = match index % 3 {
                        0 => {
                            user_id = id.clone();
                            (
                                user_message(session_id, &id, created as f64),
                                vec![text_part(
                                    session_id,
                                    &id,
                                    &part_id(0),
                                    &format!(
                                        "Request {}: look at module {index} and report.",
                                        index / 3
                                    ),
                                )],
                            )
                        }
                        1 => {
                            let size = low + (index * 7_919) % (high - low + 1);
                            let tool = V1Part::Tool {
                                id: part_id(1),
                                session_id: session_id.to_string(),
                                message_id: id.clone(),
                                call_id: format!("call_{index}"),
                                tool: "read".to_string(),
                                state: V1ToolState::Completed {
                                    input: json!({ "filePath": format!("src/module_{index}.ts") })
                                        .as_object()
                                        .unwrap()
                                        .clone(),
                                    output: filler(index, size),
                                    title: format!("src/module_{index}.ts"),
                                    metadata: Default::default(),
                                    time: ToolStateCompletedTime {
                                        start: created as u64,
                                        end: created as u64,
                                        compacted: None,
                                    },
                                    attachments: None,
                                },
                                metadata: None,
                            };
                            written.tool_parts += 1;
                            (
                                finished(
                                    assistant_message(
                                        session_id,
                                        &id,
                                        &user_id,
                                        created as u64,
                                        None,
                                        Some("tool-calls".to_string()),
                                    ),
                                    created as u64,
                                ),
                                vec![
                                    text_part(session_id, &id, &part_id(0), "Reading the module."),
                                    tool,
                                ],
                            )
                        }
                        _ => (
                            finished(
                                assistant_message(
                                    session_id,
                                    &id,
                                    &user_id,
                                    created as u64,
                                    None,
                                    Some("stop".to_string()),
                                ),
                                created as u64,
                            ),
                            vec![text_part(
                                session_id,
                                &id,
                                &part_id(0),
                                &format!(
                                    "Module {} exports a handler and a config loader.",
                                    index - 1
                                ),
                            )],
                        ),
                    };
                    let data = row_data(&info, &["id", "sessionID"]);
                    written.bytes += data.len();
                    message.execute(rusqlite::params![id, session_id, created, data])?;
                    for item in &parts {
                        let data = row_data(item, &["id", "sessionID", "messageID"]);
                        written.bytes += data.len();
                        let part_id = match item {
                            V1Part::Text { id, .. } | V1Part::Tool { id, .. } => id.clone(),
                            _ => unreachable!("the generator writes text and tool parts"),
                        };
                        part.execute(rusqlite::params![part_id, id, session_id, created, data])?;
                    }
                    written.messages += 1;
                }
            }
            tx.commit()?;
            Ok::<_, crate::CoreError>(())
        })
        .expect("long session written");
    written
}

fn finished(mut message: V1Message, at: u64) -> V1Message {
    if let V1Message::Assistant { time, tokens, .. } = &mut message {
        time.completed = Some(at);
        *tokens = V1StepTokens {
            total: None,
            input: 0.0,
            output: 0.0,
            reasoning: 0.0,
            cache: V1TokenCache {
                read: 0.0,
                write: 0.0,
            },
        };
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::e2e::prompt_input_for;
    use crate::session::message::WithParts;
    use crate::session::test_support::{
        create_engine_session, engine_with_provider, test_model_with_context, Engine,
    };

    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

    /// The `n`th prompt after a generated history. The harness clock never
    /// moves, so ids alone order what follows: this one sorts after
    /// everything the generator wrote and before the ids the engine makes.
    async fn prompt(e: &Engine, session_id: &str, n: usize, text: &str) -> WithParts {
        let id = format!("msg_{:026}", 9_000_000 + n);
        tokio::time::timeout(
            TIMEOUT,
            e.prompt.prompt(prompt_input_for(&id, session_id, text)),
        )
        .await
        .expect("the turn ends")
        .expect("the turn succeeds")
    }

    fn text_of(message: &WithParts) -> String {
        message
            .parts
            .iter()
            .filter_map(|part| match part {
                V1Part::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// 60 messages whose 20 tool outputs total 600 KB: far over a 100k
    /// window when sent whole, well under it once a summary request has cut
    /// each output down.
    fn heavy() -> LongSession {
        LongSession {
            messages: 60,
            tool_output: (30_000, 30_000),
        }
    }

    #[tokio::test]
    async fn an_uncounted_history_is_compacted_before_it_is_sent() {
        let llm = LimitedLlm::new(100_000);
        let e = engine_with_provider(
            "long-preflight",
            llm.clone(),
            test_model_with_context(100_000.0, 8_192.0),
            json!({}),
        );
        let session = create_engine_session(&e);
        write_long_session(&e.services.storage, &session.id, heavy());

        let answer = prompt(&e, &session.id, 1, "what did module 4 export?").await;

        let requests = llm.requests();
        assert_eq!(
            llm.rejected(),
            0,
            "nothing oversized was sent: {requests:?}"
        );
        assert_eq!(requests[0].agent, "compaction", "{requests:?}");
        assert_eq!(requests.last().unwrap().agent, "build", "{requests:?}");
        assert!(llm.largest() / 4 <= 100_000, "{requests:?}");
        assert_eq!(text_of(&answer), "answer");
    }

    #[tokio::test]
    async fn a_model_without_a_context_limit_is_held_to_the_default_ceiling() {
        let llm = LimitedLlm::new(10_000_000);
        let e = engine_with_provider(
            "long-unknown-limit",
            llm.clone(),
            test_model_with_context(0.0, 8_192.0),
            json!({}),
        );
        let session = create_engine_session(&e);
        write_long_session(&e.services.storage, &session.id, heavy());
        prompt(&e, &session.id, 1, "go on").await;
        let requests = llm.requests();
        assert_eq!(requests[0].agent, "compaction", "{requests:?}");

        // A configured ceiling the history fits under sends it whole.
        let llm = LimitedLlm::new(10_000_000);
        let e = engine_with_provider(
            "long-configured-limit",
            llm.clone(),
            test_model_with_context(0.0, 8_192.0),
            json!({ "compaction": { "default_context": 2_000_000 } }),
        );
        let session = create_engine_session(&e);
        write_long_session(&e.services.storage, &session.id, heavy());
        prompt(&e, &session.id, 1, "go on").await;
        let requests = llm.requests();
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert_eq!(requests[0].agent, "build");
        assert!(requests[0].bytes > 600_000, "{requests:?}");
    }

    #[tokio::test]
    async fn a_request_that_never_fits_ends_the_turn_with_an_error() {
        // The provider takes the summaries and refuses every turn request.
        let llm = LimitedLlm::refusing(100_000, &["build"]);
        let e = engine_with_provider(
            "long-never-fits",
            llm.clone(),
            test_model_with_context(100_000.0, 8_192.0),
            json!({}),
        );
        let session = create_engine_session(&e);
        write_long_session(
            &e.services.storage,
            &session.id,
            LongSession {
                messages: 6,
                tool_output: (100, 100),
            },
        );
        let errors = e.services.events.subscribe("session.error");

        prompt(&e, &session.id, 1, "go on").await;

        let requests = llm.requests();
        let turns = requests.iter().filter(|r| r.agent == "build").count();
        assert_eq!(
            turns, 4,
            "one attempt, then one after each compaction: {requests:?}"
        );
        let mut errors = errors;
        let mut last = None;
        while let Ok(payload) = errors.try_recv() {
            last = Some(payload.data);
        }
        let last = last.expect("a session.error");
        assert_eq!(last["error"]["name"], "ContextOverflowError", "{last}");
        assert!(
            last["error"]["data"]["message"]
                .as_str()
                .unwrap()
                .contains("after 3 compactions"),
            "{last}"
        );
    }
}

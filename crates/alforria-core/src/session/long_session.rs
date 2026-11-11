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
    /// Whether the request carried an earlier summary to build on.
    pub(crate) prior_summary: bool,
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
            prior_summary: input.messages.iter().any(|message| {
                message.content.iter().any(|part| {
                    matches!(
                        part,
                        alforria_llm::schema::messages::ContentPart::Text { text, .. }
                            if text.contains("<prior-summary>")
                    )
                })
            }),
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

impl LongSession {
    /// A third of the messages call a tool, with 2–8 KB outputs: 3,000
    /// messages make the session the v0.3.0 experiments could not continue.
    pub(crate) fn of(messages: usize) -> LongSession {
        LongSession {
            messages,
            tool_output: (2_000, 8_000),
        }
    }
}

/// The most memory this process has held at once, in bytes (Linux; 0
/// elsewhere).
pub(crate) fn peak_resident_bytes() -> usize {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            let line = status.lines().find(|line| line.starts_with("VmHWM:"))?;
            line.split_whitespace().nth(1)?.parse::<usize>().ok()
        })
        .map_or(0, |kib| kib * 1024)
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
    write_long_session_from(storage, session_id, shape, 0)
}

/// [`write_long_session`], numbering messages from `first`: more history
/// for a session that already has some.
pub(crate) fn write_long_session_from(
    storage: &Storage,
    session_id: &str,
    shape: LongSession,
    first: usize,
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
                for index in first..first + shape.messages {
                    let created = index as i64 + 1;
                    let id = format!("msg_{index:026}");
                    let part_id = |n: usize| format!("prt_{:026}", index * 4 + n);
                    let (info, parts): (V1Message, Vec<V1Part>) = match (index - first) % 3 {
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

    /// The first prompt after a generated history. The harness clock never
    /// moves, so ids alone order what follows: this one sorts after
    /// everything the generator wrote and before the ids the engine makes.
    async fn prompt(e: &Engine, session_id: &str, text: &str) -> WithParts {
        let id = format!("msg_{:026}", 9_000_000);
        run(e, prompt_input_for(&id, session_id, text)).await
    }

    /// A later prompt, with an id of the engine's own making.
    async fn prompt_again(e: &Engine, session_id: &str, text: &str) -> WithParts {
        let mut input = prompt_input_for("msg_x", session_id, text);
        input.message_id = None;
        run(e, input).await
    }

    async fn run(e: &Engine, input: crate::session::prompt_input::PromptInput) -> WithParts {
        tokio::time::timeout(TIMEOUT, e.prompt.prompt(input))
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

        let answer = prompt(&e, &session.id, "what did module 4 export?").await;

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
        prompt(&e, &session.id, "go on").await;
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
        prompt(&e, &session.id, "go on").await;
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

        prompt(&e, &session.id, "go on").await;

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

    /// Prompt a generated session of `messages` against a provider with a
    /// 100k window, twice, and check that nothing oversized was ever sent.
    async fn compacts_and_continues(name: &str, shape: LongSession, paged: bool) {
        let messages = shape.messages;
        let before = peak_resident_bytes();
        let llm = LimitedLlm::new(100_000);
        let e = engine_with_provider(
            name,
            llm.clone(),
            test_model_with_context(100_000.0, 8_192.0),
            json!({}),
        );
        let session = create_engine_session(&e);
        let written = write_long_session(&e.services.storage, &session.id, shape);
        assert_eq!(written.messages, messages);
        // Whether the history is more than a turn will hold, and so is
        // summarised straight from the database.
        let window = e
            .services
            .messages
            .window(&session.id, crate::session::r#loop::HISTORY_WINDOW_BYTES)
            .unwrap();
        assert_eq!(!window.complete, paged, "{} loaded", window.messages.len());
        drop(window);

        let answer = prompt(&e, &session.id, "what did module 4 export?").await;
        assert_eq!(text_of(&answer), "answer");

        let requests = llm.requests();
        let summaries: Vec<&SeenRequest> = requests
            .iter()
            .filter(|request| request.agent == "compaction")
            .collect();
        let peak = peak_resident_bytes();
        eprintln!(
            "{messages} messages, {} bytes stored: {} summary requests, largest request {} bytes, peak RSS {} MB (was {} MB)",
            written.bytes,
            summaries.len(),
            llm.largest(),
            peak >> 20,
            before >> 20,
        );
        assert_eq!(llm.rejected(), 0, "nothing oversized was sent");
        assert!(llm.largest() / 4 <= 100_000);
        // The history is several times the window, so it took several
        // requests, each building on the summary before it.
        assert!(summaries.len() > 2, "{}", summaries.len());
        assert!(!summaries[0].prior_summary);
        assert!(summaries[1..].iter().all(|request| request.prior_summary));
        assert_eq!(requests.last().unwrap().agent, "build");
        if peak > 0 {
            assert!(peak < 2 << 30, "peak RSS {} MB", peak >> 20);
        }

        // The session carries on from the summary: one small request.
        let sent = llm.requests().len();
        let answer = prompt_again(&e, &session.id, "and module 7?").await;
        assert_eq!(text_of(&answer), "answer");
        let requests = llm.requests();
        assert_eq!(requests.len(), sent + 1, "{:?}", &requests[sent..]);
        assert!(requests[sent].bytes < 200_000, "{:?}", requests[sent]);
    }

    #[tokio::test]
    async fn a_3000_message_session_compacts_and_continues() {
        compacts_and_continues("long-3000", LongSession::of(3_000), false).await;
    }

    /// The same number of messages with outputs large enough that the
    /// history is not read into memory at all.
    #[tokio::test]
    async fn a_history_too_large_to_hold_is_compacted_from_the_database() {
        compacts_and_continues(
            "long-paged",
            LongSession {
                messages: 3_000,
                tool_output: (6_000, 14_000),
            },
            true,
        )
        .await;
    }

    /// The session that was OOM-killed at 2 GB before any request went out.
    /// Slow in a debug build: `cargo nextest run --run-ignored all`.
    #[tokio::test]
    #[ignore = "slow: 12,000 messages"]
    async fn a_12000_message_session_compacts_and_continues() {
        compacts_and_continues("long-12000", LongSession::of(12_000), true).await;
    }

    /// The state v0.3.0 left such a session in: the turn's request refused,
    /// then a compaction whose own request was refused, on every prompt.
    #[tokio::test]
    async fn a_session_stuck_too_large_to_compact_works_again() {
        let llm = LimitedLlm::new(100_000);
        let e = engine_with_provider(
            "long-stuck",
            llm.clone(),
            test_model_with_context(100_000.0, 8_192.0),
            json!({}),
        );
        let session = create_engine_session(&e);
        write_long_session(&e.services.storage, &session.id, LongSession::of(3_000));
        let sessions = &e.services.sessions;
        let user = "msg_00000000000000000000005000";
        let marker = "msg_00000000000000000000005002";
        sessions
            .update_message(&user_message(&session.id, user, 5_000.0))
            .unwrap();
        sessions
            .update_part(&text_part(
                &session.id,
                user,
                "prt_00000000000000000000050000",
                "continue",
            ))
            .unwrap();
        // The refused turn: no finish, no error.
        sessions
            .update_message(&assistant_message(
                &session.id,
                "msg_00000000000000000000005001",
                user,
                5_001,
                None,
                None,
            ))
            .unwrap();
        sessions
            .update_message(&user_message(&session.id, marker, 5_002.0))
            .unwrap();
        sessions
            .update_part(&crate::session::test_support::compaction_part(
                &session.id,
                marker,
                "prt_00000000000000000000050001",
                true,
                Some(true),
                None,
            ))
            .unwrap();
        let mut failed = assistant_message(
            &session.id,
            "msg_00000000000000000000005003",
            marker,
            5_003,
            Some(true),
            Some("error".to_string()),
        );
        if let V1Message::Assistant { error, .. } = &mut failed {
            *error = Some(
                alforria_schema::session_v1::AssistantError::ContextOverflow {
                    message:
                        "Conversation history too large to compact - exceeds model context limit"
                            .to_string(),
                    response_body: None,
                },
            );
        }
        sessions.update_message(&failed).unwrap();

        let answer = prompt(&e, &session.id, "are you there?").await;

        assert_eq!(text_of(&answer), "answer");
        assert_eq!(llm.rejected(), 0, "{:?}", llm.requests());
        let V1Message::Assistant { error, .. } = &answer.info else {
            panic!("an assistant message");
        };
        assert!(error.is_none(), "{error:?}");
    }

    /// A model that declares no limit is sized against the default ceiling,
    /// which this provider is well under: its refusals shrink the requests
    /// until they fit.
    #[tokio::test]
    async fn summary_requests_shrink_until_the_provider_takes_them() {
        let llm = LimitedLlm::new(20_000);
        let e = engine_with_provider(
            "long-shrink",
            llm.clone(),
            test_model_with_context(0.0, 4_096.0),
            json!({}),
        );
        let session = create_engine_session(&e);
        write_long_session(&e.services.storage, &session.id, LongSession::of(600));

        let answer = prompt(&e, &session.id, "go on").await;

        assert_eq!(text_of(&answer), "answer");
        let requests = llm.requests();
        let refused = requests.iter().filter(|request| request.rejected).count();
        assert!((1..=4).contains(&refused), "{requests:?}");
        assert!(!requests.last().unwrap().rejected);

        // The size that worked is remembered for the next compaction.
        let other = create_engine_session(&e);
        write_long_session_from(&e.services.storage, &other.id, LongSession::of(600), 10_000);
        let sent = requests.len();
        prompt_again(&e, &other.id, "go on").await;
        let later = &llm.requests()[sent..];
        assert!(later.iter().any(|request| request.agent == "compaction"));
        assert!(later.iter().all(|request| !request.rejected), "{later:?}");
    }

    /// `messages` of history that a completed compaction has put behind a
    /// summary, keeping the last two turns in full.
    fn write_compacted(e: &Engine, session_id: &str, messages: usize) {
        write_long_session(
            &e.services.storage,
            session_id,
            LongSession {
                messages,
                tool_output: (200, 400),
            },
        );
        let sessions = &e.services.sessions;
        let marker = format!("msg_{:026}", messages);
        let summary = format!("msg_{:026}", messages + 1);
        let tail = format!("msg_{:026}", messages - 6);
        sessions
            .update_message(&user_message(session_id, &marker, messages as f64 + 1.0))
            .unwrap();
        sessions
            .update_part(&crate::session::test_support::compaction_part(
                session_id,
                &marker,
                &format!("prt_{:026}", messages * 4),
                true,
                None,
                Some(&tail),
            ))
            .unwrap();
        sessions
            .update_message(&assistant_message(
                session_id,
                &summary,
                &marker,
                messages as u64 + 2,
                Some(true),
                Some("stop".to_string()),
            ))
            .unwrap();
        sessions
            .update_part(&text_part(
                session_id,
                &summary,
                &format!("prt_{:026}", messages * 4 + 1),
                "## Goal\nThe earlier modules were read and reported on.",
            ))
            .unwrap();
    }

    /// Message rows read by a three-step turn (two tool calls, then the
    /// answer) on a session with `messages` of compacted history.
    async fn rows_read_by_a_turn(name: &str, messages: usize) -> u64 {
        use crate::session::test_support::{engine, text_stream};
        let call = |id: &str| {
            let mut events = vec![
                Ok(LlmEvent::StepStart { index: 0.0 }),
                Ok(LlmEvent::ToolCall {
                    id: id.to_string(),
                    name: "read".to_string(),
                    input: json!({ "filePath": "a.txt" }),
                    provider_executed: None,
                    provider_metadata: None,
                }),
            ];
            events.extend([
                Ok(LlmEvent::StepFinish {
                    index: 0.0,
                    reason: FinishReason::ToolCalls,
                    usage: None,
                    provider_metadata: None,
                }),
                Ok(LlmEvent::Finish {
                    reason: FinishReason::ToolCalls,
                    usage: None,
                    provider_metadata: None,
                }),
            ]);
            events
        };
        let e = engine(
            name,
            vec![call("call_a"), call("call_b"), text_stream("done")],
        );
        std::fs::write(e.worktree.join("a.txt"), "x\n").unwrap();
        let session = create_engine_session(&e);
        write_compacted(&e, &session.id, messages);

        let before = e.services.storage.message_rows_read();
        let answer = prompt_again(&e, &session.id, "read a.txt twice").await;
        assert_eq!(text_of(&answer), "done");
        assert_eq!(e.llm.calls(), 3);
        // The summary and the kept turns are what the model is sent.
        let sent = serde_json::to_string(&e.llm.input(0).messages).unwrap();
        assert!(sent.contains("The earlier modules were read"), "{sent}");
        assert!(!sent.contains("Request 0:"), "{sent}");
        // Let the forked diff summary and prune finish reading.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        e.services.storage.message_rows_read() - before
    }

    #[tokio::test]
    async fn a_step_reads_the_same_rows_however_long_the_history_is() {
        let short = rows_read_by_a_turn("rows-short", 300).await;
        let long = rows_read_by_a_turn("rows-long", 3_000).await;
        eprintln!("rows read by a three-step turn: {short} with 300 messages of history, {long} with 3,000");
        assert_eq!(short, long);
        // Four passes over a window of about a dozen messages, read a page
        // at a time, plus the turn's own bookkeeping: nowhere near the
        // history.
        assert!(long < 200, "{long}");
    }

    /// The event log after a turn of `steps` file writes: rows and bytes
    /// by event type, largest first.
    async fn event_log_after_writes(name: &str, steps: usize) -> Vec<(String, u64, u64, u64)> {
        use crate::session::test_support::{engine, text_stream};
        let mut script: Vec<_> = (0..steps)
            .map(|step| {
                vec![
                    Ok(LlmEvent::StepStart { index: 0.0 }),
                    Ok(LlmEvent::ToolCall {
                        id: format!("call_{step}"),
                        name: "write".to_string(),
                        input: json!({
                            "filePath": format!("src/file_{}.ts", step % 12),
                            "content": filler(step, 2_000),
                        }),
                        provider_executed: None,
                        provider_metadata: None,
                    }),
                    Ok(LlmEvent::StepFinish {
                        index: 0.0,
                        reason: FinishReason::ToolCalls,
                        usage: None,
                        provider_metadata: None,
                    }),
                    Ok(LlmEvent::Finish {
                        reason: FinishReason::ToolCalls,
                        usage: None,
                        provider_metadata: None,
                    }),
                ]
            })
            .collect();
        script.push(text_stream("done"));
        let e = engine(name, script);
        std::fs::create_dir_all(e.worktree.join("src")).unwrap();
        let session = create_engine_session(&e);
        let answer = prompt_again(&e, &session.id, "write the files").await;
        assert_eq!(text_of(&answer), "done");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        e.services.storage.with_connection(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT type, COUNT(*), SUM(LENGTH(data)), MAX(LENGTH(data)) FROM event
                     WHERE aggregate_id = ?1 GROUP BY type ORDER BY 3 DESC",
                )
                .unwrap();
            let rows = stmt
                .query_map([&session.id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, u64>(1)?,
                        row.get::<_, u64>(2)?,
                        row.get::<_, u64>(3)?,
                    ))
                })
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            rows
        })
    }

    #[tokio::test]
    async fn a_long_tool_turn_keeps_the_event_log_small() {
        let log = event_log_after_writes("event-diet", 36).await;
        let rows: u64 = log.iter().map(|row| row.1).sum();
        let bytes: u64 = log.iter().map(|row| row.2).sum();
        eprintln!("36-step turn: {rows} event rows, {bytes} bytes");
        for (kind, count, total, largest) in &log {
            eprintln!("  {kind}: {count} rows, {total} bytes, largest {largest}");
        }
        // v0.3.0 logged the turn's whole diff, patches included, with the
        // user message after every step: `message.updated` rows of 30 KB
        // and a megabyte for a turn like this. The log keeps the files and
        // their counts and leaves the patches to the message row.
        let updated = log
            .iter()
            .find(|row| row.0.starts_with("message.updated"))
            .expect("message.updated events");
        assert!(updated.3 < 4_000, "largest message.updated: {}", updated.3);
        // Twelve 2 KB files written three times each, plus bookkeeping.
        assert!(bytes < 500_000, "{bytes}");
    }
}

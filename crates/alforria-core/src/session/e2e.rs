//! M5.8 — Mock-LLM full-loop E2E (spec §M5.8). Every scenario drives the
//! production `SessionPrompt` facade over the real stores, bus, tools and
//! permission service; only the LLM (a scripted [`DispatchLlm`]) and the
//! permission *answerer* are scripted (spec §6.6).
//!
//! Golden event sequences live in `tests/golden/session/`. Scenarios that
//! assert on live request shapes (model messages, LLM inputs) are verified
//! structurally instead — normalizing those would mean pinning the whole
//! message renderer.
//!
//! The golden capture drops `session.updated` / `session.diff` events: TS
//! forks `SessionSummary.summarize` (`Effect.forkIn`, summary.ts:102-127)
//! so their publish order is nondeterministic — the same is true of the
//! Rust `tokio::spawn` fork.

use std::collections::BTreeMap;

use alforria_llm::schema::errors::{HttpContext, LlmError, LlmErrorReason};
use alforria_llm::schema::events::LlmEvent;
use alforria_llm::schema::ids::FinishReason;
use alforria_schema::session_v1::{OutputFormat, V1Message, V1Part, V1ToolState};
use serde_json::{json, Value};

use crate::event::definition::Payload;
use crate::session::message::WithParts;
use crate::session::prompt_input::{PromptInput, PromptPartInput};
use crate::session::test_support::{
    create_engine_session, engine, engine_hanging_read, engine_with_compaction_script, text_stream,
    Engine,
};

const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// `StepStart` — makes the processor track the pre-step snapshot
/// (processor.ts:474-489), which the revert scenario needs for its Patch
/// part.
fn step_start() -> LlmEvent {
    LlmEvent::StepStart { index: 0.0 }
}

fn tool_call_step(calls: Vec<LlmEvent>) -> Vec<Result<LlmEvent, LlmError>> {
    let mut events = vec![Ok(step_start())];
    events.extend(calls.into_iter().map(Ok));
    events.extend(tool_calls_finish());
    events
}

/// `StepFinish` + `Finish` both with the tool-calls reason (the step
/// terminator of a tool loop, prompt.ts:1120-1123).
fn tool_calls_finish() -> Vec<Result<LlmEvent, LlmError>> {
    vec![
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
}

fn tool_call_event(id: &str, name: &str, args: Value) -> LlmEvent {
    LlmEvent::ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        input: args,
        provider_executed: None,
        provider_metadata: None,
    }
}

fn text_part_input(text: &str) -> PromptPartInput {
    PromptPartInput::Text {
        id: None,
        text: text.to_string(),
        synthetic: None,
        ignored: None,
        time: None,
        metadata: None,
    }
}

fn prompt_input(session_id: &str, body: &str) -> PromptInput {
    prompt_input_for("msg_user1", session_id, body)
}

fn prompt_input_for(message_id: &str, session_id: &str, body: &str) -> PromptInput {
    PromptInput {
        session_id: session_id.to_string(),
        message_id: Some(message_id.to_string()),
        model: Some(crate::session::prompt_input::ModelRef {
            provider_id: "anthropic".to_string(),
            model_id: "claude".to_string(),
        }),
        agent: Some("build".to_string()),
        no_reply: None,
        tools: None,
        format: None,
        system: None,
        variant: None,
        parts: vec![text_part_input(body)],
    }
}

async fn run_prompt(e: &Engine, input: PromptInput) -> WithParts {
    tokio::time::timeout(TIMEOUT, e.prompt.prompt(input))
        .await
        .expect("prompt runs within the timeout")
        .expect("prompt succeeds")
}

/// Drain a broadcast subscription into (type, data) pairs, dropping the
/// forked-summary event types (see the module docs): `summarize` is the
/// only writer of session summaries, so any message carrying one belongs
/// to the fork.
fn drain(mut rx: tokio::sync::broadcast::Receiver<Payload>) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    while let Ok(payload) = rx.try_recv() {
        if payload.r#type == "session.updated"
            || payload.r#type == "session.diff"
            || (payload.r#type == "message.updated"
                && payload
                    .data
                    .get("info")
                    .and_then(|info| info.get("summary"))
                    .is_some())
        {
            continue;
        }
        out.push((payload.r#type, payload.data));
    }
    out
}

// ---------------------------------------------------------------------------
// Golden normalization + comparison
// ---------------------------------------------------------------------------

fn id_regex() -> &'static regex::Regex {
    static ID: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    ID.get_or_init(|| {
        regex::Regex::new(r"(ses|msg|prt|per|que)_[0-9A-HJKMNP-TV-Z]{26}").expect("id pattern")
    })
}

/// Replace per-run values (generated ids, the tempdir path) with stable
/// placeholders. Ids are indexed globally in first-seen order across the
/// whole sequence.
fn normalize_all(e: &Engine, events: &[(String, Value)]) -> Value {
    let mut ids: BTreeMap<String, String> = BTreeMap::new();
    let mut items = Vec::with_capacity(events.len());
    for (type_, data) in events {
        let mut data = data.clone();
        walk(&mut data, e, &mut ids);
        items.push(json!({ "type": type_, "data": data }));
    }
    Value::Array(items)
}

fn walk(value: &mut Value, e: &Engine, ids: &mut BTreeMap<String, String>) {
    match value {
        Value::String(s) => {
            let mut out = s.replace(e.temp.path().to_str().expect("utf8 path"), "<tmp>");
            out = id_regex()
                .replace_all(&out, |caps: &regex::Captures| {
                    let id = caps.get(0).expect("whole match").as_str();
                    let next = format!("{}{}", &id[0..4], ids.len());
                    ids.entry(id.to_string()).or_insert_with(|| next).clone()
                })
                .to_string();
            *s = out;
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                walk(item, e, ids);
            }
        }
        Value::Object(map) => {
            for (_, item) in map.iter_mut() {
                walk(item, e, ids);
            }
        }
        _ => {}
    }
}
/// Golden event sequence: `{comment, events}` where `events` is a list of
/// `{type, data}` pairs (ids + the tempdir normalized). A missing golden
/// is written on first run (and must then be committed); an existing
/// golden is compared for equality.
fn assert_golden(name: &str, e: &Engine, events: &[(String, Value)], comment: &str) {
    let path = format!(
        "{}/tests/golden/session/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let actual = normalize_all(e, events);
    if !std::path::Path::new(&path).exists() {
        let golden = json!({ "comment": comment, "events": actual });
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&golden).expect("serializable"),
        )
        .expect("write golden");
        panic!("golden {name}.json written — inspect and commit it");
    }
    let golden: Value = serde_json::from_str(&std::fs::read_to_string(&path).expect("golden"))
        .expect("golden is valid JSON");
    assert_eq!(
        golden.get("events"),
        Some(&actual),
        "golden divergence for {name}"
    );
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::test_support::PermissionAnswerer;
    use alforria_llm::schema::errors::{HttpRequestDetails, HttpResponseDetails};
    use alforria_schema::permission_v1::PermissionV1Reply;

    // ------------------------------------------------------------------
    // 1. Plain text turn — golden event sequence
    //
    // Pins the TS event sequence of a text-only turn
    // (message.updated/part churn, session.status busy) with ids
    // normalized (prompt.ts:1052-1071 → processor.ts:609-645).
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn plain_text_turn_golden() {
        let e = engine("e2e-plain-text", vec![text_stream("hello world")]);
        let session = create_engine_session(&e);
        let rx = e.services.events.all();

        let assistant = run_prompt(&e, prompt_input(&session.id, "say hi")).await;
        let V1Message::Assistant {
            finish,
            error,
            time,
            ..
        } = &assistant.info
        else {
            panic!("expected an assistant message");
        };
        assert_eq!(finish.as_deref(), Some("stop"));
        assert!(error.is_none());
        assert!(time.completed.is_some());

        let events = drain(rx);
        assert_golden(
            "plain_text_turn",
            &e,
            &events,
            "TS: one text-only assistant turn through prompt()/runLoop — \
             message.updated churn, part lifecycle (message.part.updated / \
             message.part.delta), session.status busy + idle \
             (prompt.ts:1052-1071, processor.ts:609-645)",
        );
    }

    // ------------------------------------------------------------------
    // 2. Tool loop — read runs, completes, the result feeds back
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn tool_loop_runs_and_feeds_result_back() {
        let e = engine(
            "e2e-tool-loop",
            vec![
                vec![Ok(tool_call_event(
                    "cal_1",
                    "read",
                    json!({ "filePath": "a.txt" }),
                ))]
                .into_iter()
                .chain(tool_calls_finish())
                .collect(),
                text_stream("all done"),
            ],
        );
        std::fs::write(e.worktree.join("a.txt"), "file content").unwrap();
        let session = create_engine_session(&e);
        let rx = e.services.events.all();

        let assistant = run_prompt(&e, prompt_input(&session.id, "read the file")).await;

        // Two model turns: the tool step and the text finish.
        assert_eq!(e.llm.calls(), 2);
        let V1Message::Assistant { finish, .. } = &assistant.info else {
            panic!("expected an assistant message");
        };
        assert_eq!(finish.as_deref(), Some("stop"));

        // The tool part went pending → running → completed (the returned
        // message is the *last* assistant — the text step).
        let all = e.services.messages.stream(&session.id).unwrap();
        let tool_part = all
            .iter()
            .flat_map(|msg| msg.parts.iter())
            .find(|part| matches!(part, V1Part::Tool { .. }))
            .expect("tool part");
        let V1Part::Tool { state, .. } = tool_part else {
            unreachable!("checked the part kind above")
        };
        let V1ToolState::Completed { output, .. } = state else {
            panic!("expected a completed tool part, got {state:?}")
        };
        assert!(output.contains("file content"), "got {output}");

        // The model received the tool result in the second request
        // (prompt.ts:1081-1112 keeps the loop running on tool-calls).
        let request = e.llm.input(1);
        assert!(
            serde_json::to_string(&request.messages)
                .unwrap()
                .contains("file content"),
            "tool result missing from the model messages"
        );

        let events = drain(rx);
        assert_golden(
            "tool_loop",
            &e,
            &events,
            "TS: one tool-call step against the real read tool — part \
             running/completed churn and the tool-result feeding the \
             next model request (prompt.ts:1081-1112, tools.ts:41-138)",
        );
    }

    // ------------------------------------------------------------------
    // 3. Permission gate — once / always / reject
    // ------------------------------------------------------------------

    /// One turn with two `read` calls of a `*.env` file (the default agent
    /// ruleset asks for those, agent.ts:119-136).
    async fn permission_scenario(name: &str) -> Engine {
        let e = engine(
            name,
            vec![
                vec![
                    Ok(tool_call_event(
                        "cal_1",
                        "read",
                        json!({ "filePath": "secret.env" }),
                    )),
                    Ok(tool_call_event(
                        "cal_2",
                        "read",
                        json!({ "filePath": "secret.env" }),
                    )),
                ]
                .into_iter()
                .chain(tool_calls_finish())
                .collect(),
                text_stream("done reading"),
            ],
        );
        std::fs::write(e.worktree.join("secret.env"), "TOKEN=1\n").unwrap();
        e
    }

    #[tokio::test]
    async fn permission_once_asks_for_every_call() {
        let e = permission_scenario("e2e-permission-once").await;
        let session = create_engine_session(&e);
        let answerer =
            PermissionAnswerer::new(vec![PermissionV1Reply::Once, PermissionV1Reply::Once]);
        let asked = e.services.events.subscribe("permission.asked");
        tokio::spawn(
            answerer
                .clone()
                .serve(e.services.permission.clone(), asked, 2),
        );

        run_prompt(&e, prompt_input(&session.id, "read twice")).await;

        let asks = answerer.asks();
        assert_eq!(asks.len(), 2, "once must ask for every call");
        assert_eq!(asks[0].permission, "read");
        assert_eq!(asks[0].patterns, vec!["secret.env".to_string()]);
        assert_eq!(e.llm.calls(), 2);
    }

    #[tokio::test]
    async fn permission_always_skips_the_second_ask() {
        let e = permission_scenario("e2e-permission-always").await;
        let session = create_engine_session(&e);
        // "always" only fires for one ask; the second must auto-approve.
        let answerer = PermissionAnswerer::new(vec![PermissionV1Reply::Always]);
        let asked = e.services.events.subscribe("permission.asked");
        tokio::spawn(
            answerer
                .clone()
                .serve(e.services.permission.clone(), asked, 1),
        );

        run_prompt(&e, prompt_input(&session.id, "read twice")).await;

        // One ask: the approved `read` allow rule satisfies the second
        // call without publishing another ask (permission/index.ts:143-166).
        assert_eq!(answerer.asks().len(), 1);
        assert_eq!(e.llm.calls(), 2);
    }

    #[tokio::test]
    async fn permission_reject_breaks_the_loop() {
        let e = permission_scenario("e2e-permission-reject").await;
        let session = create_engine_session(&e);
        let answerer =
            PermissionAnswerer::new(vec![PermissionV1Reply::Reject, PermissionV1Reply::Reject]);
        let asked = e.services.events.subscribe("permission.asked");
        tokio::spawn(
            answerer
                .clone()
                .serve(e.services.permission.clone(), asked, 2),
        );

        let assistant = run_prompt(&e, prompt_input(&session.id, "read twice")).await;

        // The rejection blocks the loop (processor.ts:200-201): no second
        // model call, the assistant completes without further tool calls.
        assert_eq!(e.llm.calls(), 1);
        let V1Part::Tool { state, .. } = assistant
            .parts
            .iter()
            .find(|part| matches!(part, V1Part::Tool { .. }))
            .expect("tool part")
        else {
            unreachable!("checked the part kind above")
        };
        let V1ToolState::Error { error, .. } = state else {
            panic!("expected an error tool part, got {state:?}")
        };
        assert!(
            error.contains("The user rejected permission"),
            "got {error}"
        );
    }

    // ------------------------------------------------------------------
    // 4. Compaction — overflow → summary turn → session.compacted
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn compaction_summarizes_and_continues_with_compacted_history() {
        // A "stop" finish exits the loop before the overflow check
        // (prompt.ts:1114), so overflow rides a tool-calls step.
        let mut overflow = tool_call_step(vec![tool_call_event(
            "cal_1",
            "read",
            json!({ "filePath": "a.txt" }),
        )]);
        for item in overflow.iter_mut() {
            if let Ok(LlmEvent::StepFinish { usage, .. }) = item {
                *usage = Some(alforria_llm::schema::events::Usage {
                    input_tokens: Some(500_000.0),
                    output_tokens: Some(10.0),
                    ..alforria_llm::schema::events::Usage::default()
                });
            }
        }
        // The compaction service runs its own summary turn (its own
        // scripted LLM), then the loop continues on the compacted history.
        let e = engine_with_compaction_script(
            "e2e-compaction",
            vec![overflow, text_stream("after compaction")],
            vec![text_stream("Summary: the user asked about a big file.")],
        );
        std::fs::write(e.worktree.join("a.txt"), "x\n").unwrap();
        let session = create_engine_session(&e);
        let rx = e.services.events.all();

        // Message ordering falls back to id comparison when `created`
        // collides (fixed clock), so the user message must sort before
        // the generated continuation ids (message-v2.ts:578-604).
        run_prompt(
            &e,
            prompt_input_for(
                "msg_00000000000000000000000000",
                &session.id,
                "trigger overflow",
            ),
        )
        .await;

        // session.compacted published (compaction.ts:319-557).
        let events = drain(rx);

        // The loop continued after compaction: step 1 overflowed, step 2
        // answered from the compacted history.
        assert_eq!(e.llm.calls(), 2);
        assert!(
            events.iter().any(|(type_, _)| type_ == "session.compacted"),
            "expected a session.compacted event"
        );

        // The continued request carries the summary, not the pruned tail
        // (filterCompacted, message-v2.ts:578-581).
        let body = serde_json::to_string(&e.llm.input(1).messages).unwrap();
        assert!(body.contains("Summary: the user asked"), "{body}");
        assert!(!body.contains("trigger overflow"), "{body}");
    }

    // ------------------------------------------------------------------
    // 5. Revert — tool edits roll back through the snapshot
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn revert_rolls_back_tool_edits() {
        let e = engine(
            "e2e-revert",
            vec![
                tool_call_step(vec![tool_call_event(
                    "cal_1",
                    "write",
                    json!({ "filePath": "a.txt", "content": "v2\n" }),
                )]),
                text_stream("edited"),
            ],
        );
        std::fs::write(e.worktree.join("a.txt"), "v1\n").unwrap();
        let session = create_engine_session(&e);
        // The message store orders by `(created, id)`; with the fixed
        // clock all timestamps collide, so the user message must sort
        // before the generated ULID ids (message-v2.ts:578-581).
        let user_id = "msg_00000000000000000000000000";
        run_prompt(&e, prompt_input_for(user_id, &session.id, "edit the file")).await;
        assert_eq!(
            std::fs::read_to_string(e.worktree.join("a.txt")).unwrap(),
            "v2\n"
        );

        // Revert at the user message: the edit rolls back through the
        // in-memory snapshot and the session records the revert state
        // (revert.ts:38-89).
        let reverted = e
            .revert
            .revert(crate::session::revert::RevertInput {
                session_id: session.id.clone(),
                message_id: user_id.to_string(),
                part_id: None,
            })
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(e.worktree.join("a.txt")).unwrap(),
            "v1\n"
        );
        let revert = reverted.revert.expect("revert state recorded");
        let summary = reverted.summary.as_ref().expect("diff summary recorded");
        assert_eq!(summary.files, 1.0, "one file changed in the turn");
        assert_eq!(summary.additions, 1.0);
        assert_eq!(summary.deletions, 1.0);
        let _ = revert;
    }

    // ------------------------------------------------------------------
    // 6. Subagent — the task tool spawns a child session
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn subagent_task_tool_spawns_child_session() {
        let e = engine(
            "e2e-subagent",
            vec![
                vec![Ok(tool_call_event(
                    "cal_1",
                    "task",
                    json!({
                        "description": "Research",
                        "prompt": "do the research",
                        "subagent_type": "general",
                    }),
                ))]
                .into_iter()
                .chain(tool_calls_finish())
                .collect(),
                text_stream("subagent answer"),
                text_stream("parent continues"),
            ],
        );
        let session = create_engine_session(&e);

        run_prompt(&e, prompt_input(&session.id, "spawn a subagent")).await;

        // Parent step, child turn, parent continuation.
        assert_eq!(e.llm.calls(), 3);
        assert_ne!(e.llm.input(1).session_id, session.id);

        // The task tool part carries parentSessionId metadata and the
        // wrapped result XML (task tool, tools.ts). The returned message
        // is the last assistant — the parent continuation step.
        let all_messages = e.services.messages.stream(&session.id).unwrap();
        let task_part = all_messages
            .iter()
            .flat_map(|msg| msg.parts.iter())
            .find(|part| matches!(part, V1Part::Tool { tool, .. } if tool == "task"))
            .expect("task tool part");
        let V1Part::Tool { state, .. } = task_part else {
            unreachable!("checked the part kind above")
        };
        // ctx.metadata updates ride the running/completed state, not the
        // part-level metadata (tools.ts:71-92).
        let V1ToolState::Completed {
            metadata, output, ..
        } = state
        else {
            panic!("expected a completed task part, got {state:?}")
        };
        assert_eq!(metadata["parentSessionId"], json!(session.id));
        let child_id = metadata["sessionId"].as_str().unwrap().to_string();
        assert!(
            output.contains(&format!("<task id=\"{child_id}\" state=\"completed\">")),
            "got {output}"
        );
        assert!(output.contains("subagent answer"), "got {output}");

        // The child session row exists with the parent link.
        let child = e.services.sessions.get(&child_id).unwrap();
        assert_eq!(child.parent_id.as_deref(), Some(session.id.as_str()));
        assert_eq!(child.agent.as_deref(), Some("general"));
    }

    // ------------------------------------------------------------------
    // 7. Retry — 429 with retry-after, then success; exhaustion errors
    // ------------------------------------------------------------------

    /// A 429 whose `retry-after` header pins the delay to an exact value
    /// (retry.ts:85-96 — no jitter on the header path).
    fn rate_limit_after(seconds: &str) -> LlmError {
        LlmError {
            module: "ProviderShared".to_string(),
            method: "request".to_string(),
            reason: LlmErrorReason::RateLimit {
                message: "Too many requests, please slow down".to_string(),
                retry_after_ms: None,
                rate_limit: None,
                provider_metadata: None,
                http: Some(HttpContext {
                    request: HttpRequestDetails {
                        method: "POST".to_string(),
                        url: "https://api.example.com/v1/messages".to_string(),
                        headers: Default::default(),
                    },
                    response: Some(HttpResponseDetails {
                        status: 429.0,
                        headers: BTreeMap::from([("retry-after".to_string(), seconds.to_string())]),
                    }),
                    body: None,
                    body_truncated: None,
                    request_id: None,
                    rate_limit: None,
                }),
            },
        }
    }

    /// Pause tokio's clock so the retry sleeps auto-advance — no real
    /// sleeping in tests (spec S2).
    #[tokio::test(start_paused = true)]
    async fn retry_observes_the_exact_retry_after_delay() {
        let e = engine(
            "e2e-retry",
            vec![vec![Err(rate_limit_after("3"))], text_stream("recovered")],
        );
        let session = create_engine_session(&e);
        let mut statuses = e.services.events.subscribe("session.status");

        run_prompt(&e, prompt_input(&session.id, "retry me")).await;

        // Success on attempt 2.
        assert_eq!(e.llm.calls(), 2);

        // The retry status event: attempt 1, retrying after exactly
        // 3 seconds from the fixed clock (1_761_000_000_000).
        let mut retry_seen = false;
        while let Ok(payload) = statuses.try_recv() {
            let status = &payload.data["status"];
            if status["type"] == json!("retry") {
                retry_seen = true;
                assert_eq!(status["attempt"], json!(1));
                assert_eq!(status["next"], json!(1_761_000_000_000i64 + 3000));
            }
        }
        assert!(retry_seen, "expected a retry status event");
    }

    #[tokio::test(start_paused = true)]
    async fn retry_exhaustion_errors_the_message() {
        // Attempts 1..=5 retry (RETRY_MAX_RETRIES, retry.ts:16-23); the
        // sixth error is final.
        let script: Vec<Vec<Result<LlmEvent, LlmError>>> =
            (0..6).map(|_| vec![Err(rate_limit_after("0"))]).collect();
        let e = engine("e2e-retry-exhaustion", script);
        let session = create_engine_session(&e);

        let assistant = run_prompt(&e, prompt_input(&session.id, "retry me")).await;
        assert_eq!(e.llm.calls(), 6);
        let V1Message::Assistant { error, .. } = &assistant.info else {
            panic!("expected an assistant message");
        };
        assert!(
            error.is_some(),
            "exhaustion must error the assistant message"
        );
    }

    // ------------------------------------------------------------------
    // 8. Doom loop — three identical tool calls ask
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn doom_loop_asks_on_three_identical_calls() {
        let e = engine(
            "e2e-doom-loop",
            vec![
                vec![
                    Ok(tool_call_event(
                        "cal_1",
                        "read",
                        json!({ "filePath": "a.txt" }),
                    )),
                    Ok(tool_call_event(
                        "cal_2",
                        "read",
                        json!({ "filePath": "a.txt" }),
                    )),
                    Ok(tool_call_event(
                        "cal_3",
                        "read",
                        json!({ "filePath": "a.txt" }),
                    )),
                ]
                .into_iter()
                .chain(tool_calls_finish())
                .collect(),
                text_stream("recovered"),
            ],
        );
        std::fs::write(e.worktree.join("a.txt"), "x\n").unwrap();
        let session = create_engine_session(&e);
        let answerer = PermissionAnswerer::new(vec![PermissionV1Reply::Once]);
        let asked = e.services.events.subscribe("permission.asked");
        tokio::spawn(
            answerer
                .clone()
                .serve(e.services.permission.clone(), asked, 1),
        );

        let assistant = run_prompt(&e, prompt_input(&session.id, "loop the tool")).await;

        // One doom_loop ask, answered "once" — the three calls still ran.
        let asks = answerer.asks();
        assert_eq!(asks.len(), 1);
        assert_eq!(asks[0].permission, "doom_loop");
        assert_eq!(asks[0].patterns, vec!["read".to_string()]);
        // "always" sticks to the tool name, not the input
        // (processor.ts:372-379).
        assert_eq!(asks[0].always, vec!["read".to_string()]);
        // All three calls ran (the returned message is the last assistant —
        // the recovery text step).
        let tool_parts = e
            .services
            .messages
            .stream(&session.id)
            .unwrap()
            .iter()
            .flat_map(|msg| msg.parts.iter())
            .filter(|part| matches!(part, V1Part::Tool { .. }))
            .count();
        assert_eq!(tool_parts, 3);
        assert!(assistant
            .parts
            .iter()
            .any(|part| matches!(part, V1Part::Text { .. })));
    }

    // ------------------------------------------------------------------
    // 9. Structured output
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn structured_output_records_the_tool_payload() {
        let e = engine(
            "e2e-structured",
            vec![vec![
                Ok(tool_call_event(
                    "cal_1",
                    "StructuredOutput",
                    json!({ "answer": 42 }),
                )),
                Ok(LlmEvent::Finish {
                    reason: FinishReason::ToolCalls,
                    usage: None,
                    provider_metadata: None,
                }),
            ]],
        );
        let session = create_engine_session(&e);
        let mut input = prompt_input(&session.id, "answer in json");
        input.format = Some(OutputFormat::JsonSchema {
            schema: json!({
                "type": "object",
                "properties": { "answer": { "type": "number" } },
            })
            .as_object()
            .unwrap()
            .clone(),
            retry_count: None,
        });

        let assistant = run_prompt(&e, input).await;

        let V1Message::Assistant {
            structured, finish, ..
        } = &assistant.info
        else {
            panic!("expected an assistant message");
        };
        assert_eq!(structured.as_ref(), Some(&json!({ "answer": 42 })));
        assert_eq!(finish.as_deref(), Some("stop"));
    }

    #[tokio::test]
    async fn structured_output_error_without_a_tool_call() {
        let e = engine("e2e-structured-error", vec![text_stream("plain text")]);
        let session = create_engine_session(&e);
        let mut input = prompt_input(&session.id, "answer in json");
        input.format = Some(OutputFormat::JsonSchema {
            schema: json!({
                "type": "object",
                "properties": { "answer": { "type": "number" } },
            })
            .as_object()
            .unwrap()
            .clone(),
            retry_count: None,
        });

        let assistant = run_prompt(&e, input).await;

        let V1Message::Assistant { error, .. } = &assistant.info else {
            panic!("expected an assistant message");
        };
        let Some(alforria_schema::session_v1::AssistantError::StructuredOutput { message, .. }) =
            error
        else {
            panic!("expected a StructuredOutput error, got {error:?}")
        };
        assert_eq!(message, "Model did not produce structured output");
    }

    // ------------------------------------------------------------------
    // 10. Abort mid-stream — interrupted parts, then re-prompt continues
    // ------------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abort_marks_interrupted_and_reprompt_continues() {
        let e = engine_hanging_read(
            "e2e-abort",
            vec![
                vec![Ok(tool_call_event(
                    "cal_1",
                    "read",
                    json!({ "filePath": "a.txt" }),
                ))],
                text_stream("resumed"),
            ],
        );
        std::fs::write(e.worktree.join("a.txt"), "x\n").unwrap();
        let session = create_engine_session(&e);

        // Prompt in-flight; the read tool never resolves. Cancel mid-loop
        // via the run state (prompt.ts:152-155).
        let prompt = e.prompt.clone();
        let running = {
            let input = prompt_input(&session.id, "go");
            tokio::spawn(async move { prompt.prompt(input).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        e.prompt.cancel(&session.id).await.unwrap();
        // Cancel resolves the prompt through `onInterrupt` — the last
        // assistant message, not an error (prompt.ts:1346).
        let result = tokio::time::timeout(TIMEOUT, running)
            .await
            .expect("cancelling resolves the prompt")
            .unwrap()
            .expect("onInterrupt resolves the interrupted assistant");
        let _ = &result;

        // The interrupted assistant is retained: abort error + completed
        // time + the in-flight tool part marked interrupted
        // (prompt.ts:1206-1212, 1337-1338). The prompt resolves through
        // `onInterrupt` while the loop's interrupt handlers still run —
        // poll until the finalization lands.
        let mut polls = 0;
        let aborted = loop {
            let aborted = e
                .services
                .messages
                .stream(&session.id)
                .unwrap()
                .into_iter()
                .rev()
                .find(|msg| matches!(msg.info, V1Message::Assistant { .. }))
                .expect("aborted assistant");
            let done = matches!(
                &aborted.info,
                V1Message::Assistant { error, time, .. }
                if error.is_some() && time.completed.is_some()
            );
            if done {
                break aborted;
            }
            polls += 1;
            assert!(polls < 500, "aborted assistant never finalized");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };
        let V1Message::Assistant { error, time, .. } = &aborted.info else {
            unreachable!("checked the message kind above")
        };
        assert!(
            matches!(
                error,
                Some(alforria_schema::session_v1::AssistantError::Aborted { .. })
            ),
            "expected an abort error, got {error:?}"
        );
        assert!(time.completed.is_some(), "abort completes the message");
        let interrupted = aborted.parts.iter().any(|part| match part {
            V1Part::Tool { state, .. } => matches!(
                state,
                V1ToolState::Error { metadata, .. }
                    if metadata.as_ref().and_then(|m| m.get("interrupted"))
                        == Some(&json!(true))
            ),
            _ => false,
        });
        assert!(interrupted, "tool part must be marked interrupted");

        // A re-prompt continues the session normally.
        let resumed = run_prompt(&e, prompt_input_for("msg_user2", &session.id, "go again")).await;
        assert!(
            resumed
                .parts
                .iter()
                .any(|part| matches!(part, V1Part::Text { text, .. } if text == "resumed")),
            "expected the resumed answer text"
        );
    }

    // ------------------------------------------------------------------
    // 11. Provider HTTP errors — TS's default ai-sdk runtime throws an
    // `APICallError` for every non-2xx response, which `fromError` turns
    // into `APIError` (message-v2.ts:687-714, provider/error.ts:166-190)
    // ------------------------------------------------------------------

    const API_URL: &str = "https://api.example.com/v1/messages";

    /// A failed response classified by the real executor
    /// (`status_reason`), as `RequestExecutor.execute` raises it.
    fn http_failure(status: u16, body: &str) -> LlmError {
        use alforria_llm::route::executor::{status_reason, StatusReasonInput};
        let http = HttpContext {
            request: HttpRequestDetails {
                method: "POST".to_string(),
                url: API_URL.to_string(),
                headers: Default::default(),
            },
            response: Some(HttpResponseDetails {
                status: f64::from(status),
                headers: BTreeMap::from([(
                    "content-type".to_string(),
                    "application/json".to_string(),
                )]),
            }),
            body: Some(body.to_string()),
            body_truncated: None,
            request_id: None,
            rate_limit: None,
        };
        LlmError {
            module: "RequestExecutor".to_string(),
            method: "execute".to_string(),
            reason: status_reason(StatusReasonInput {
                status,
                message: format!("Provider request failed with HTTP {status}: {body}"),
                retry_after_ms: None,
                rate_limit: None,
                http,
            }),
        }
    }

    /// [`http_failure`] with `retry-after: 0`, so the retry schedule
    /// (retry.ts:50-66) does not outlast the test timeout.
    fn immediate_retry(mut error: LlmError) -> LlmError {
        let http = match &mut error.reason {
            LlmErrorReason::RateLimit { http, .. }
            | LlmErrorReason::ProviderInternal { http, .. } => http.as_mut(),
            _ => None,
        };
        let response = http
            .and_then(|http| http.response.as_mut())
            .expect("a retryable HTTP failure");
        response
            .headers
            .insert("retry-after".to_string(), "0".to_string());
        error
    }

    /// Drive one prompt whose every attempt fails with `error`; returns
    /// the assistant's wire error, the `session.error` payload errors and
    /// the `retry` status messages.
    async fn failing_turn(
        name: &str,
        error: LlmError,
        attempts: usize,
    ) -> (Value, Vec<Value>, Vec<Value>) {
        let script = (0..attempts).map(|_| vec![Err(error.clone())]).collect();
        let e = engine(name, script);
        let session = create_engine_session(&e);
        let errors = e.services.events.subscribe("session.error");
        let statuses = e.services.events.subscribe("session.status");

        // Paused clock: the header-less backoff (retry.ts:77-78, up to 30s
        // per attempt) elapses virtually, past `run_prompt`'s timeout.
        let assistant = tokio::time::timeout(
            std::time::Duration::from_secs(600),
            e.prompt.prompt(prompt_input(&session.id, "hi")),
        )
        .await
        .expect("prompt runs within the virtual timeout")
        .expect("prompt succeeds");
        assert_eq!(e.llm.calls(), attempts);
        let V1Message::Assistant { error, .. } = &assistant.info else {
            panic!("expected an assistant message");
        };
        let wire = serde_json::to_value(error.as_ref().expect("errored")).unwrap();
        let published = drain(errors)
            .into_iter()
            .map(|(_, data)| data["error"].clone())
            .collect();
        let retries = drain(statuses)
            .into_iter()
            .filter(|(_, data)| data["status"]["type"] == json!("retry"))
            .map(|(_, data)| data["status"]["message"].clone())
            .collect();
        (wire, published, retries)
    }

    #[tokio::test(start_paused = true)]
    async fn http_400_is_a_non_retryable_api_error() {
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"model: claude-nope"}}"#;
        let (wire, published, retries) =
            failing_turn("e2e-http-400", http_failure(400, body), 1).await;
        let expected = json!({
            "name": "APIError",
            "data": {
                "message": "model: claude-nope",
                "statusCode": 400,
                "isRetryable": false,
                "responseHeaders": { "content-type": "application/json" },
                "responseBody": body,
                "metadata": { "url": API_URL },
            },
        });
        assert_eq!(wire, expected);
        assert_eq!(published, vec![expected]);
        assert!(retries.is_empty(), "a 400 is not retried: {retries:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn http_401_is_an_api_error_not_a_provider_auth_error() {
        // ai-sdk raises `APICallError` for a rejected key; only a missing
        // key (`LoadAPIKeyError`) becomes `ProviderAuthError`.
        let body = r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
        let (wire, _, retries) = failing_turn("e2e-http-401", http_failure(401, body), 1).await;
        assert_eq!(wire["name"], json!("APIError"));
        assert_eq!(wire["data"]["message"], json!("invalid x-api-key"));
        assert_eq!(wire["data"]["statusCode"], json!(401));
        assert_eq!(wire["data"]["isRetryable"], json!(false));
        assert!(retries.is_empty(), "a 401 is not retried: {retries:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn missing_key_is_a_provider_auth_error() {
        let error = LlmError {
            module: "Auth".to_string(),
            method: "apply".to_string(),
            reason: LlmErrorReason::Authentication {
                message: "Missing API key".to_string(),
                kind: alforria_llm::schema::errors::AuthKind::Missing,
                provider_metadata: None,
                http: None,
            },
        };
        let (wire, _, _) = failing_turn("e2e-missing-key", error, 1).await;
        assert_eq!(
            wire,
            json!({
                "name": "ProviderAuthError",
                "data": { "providerID": "anthropic", "message": "Missing API key" },
            })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn http_429_and_5xx_are_retryable_api_errors() {
        // RETRY_MAX_RETRIES (retry.ts:31) retries, then the final error.
        let body = r#"{"error":{"message":"Rate limited, slow down","type":"rate_limit_error"}}"#;
        let (wire, _, retries) =
            failing_turn("e2e-http-429", immediate_retry(http_failure(429, body)), 6).await;
        assert_eq!(wire["name"], json!("APIError"));
        assert_eq!(wire["data"]["statusCode"], json!(429));
        assert_eq!(wire["data"]["isRetryable"], json!(true));
        assert_eq!(retries, vec![json!("Rate limited, slow down"); 5]);

        let body = r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
        let (wire, _, retries) =
            failing_turn("e2e-http-529", immediate_retry(http_failure(529, body)), 6).await;
        assert_eq!(wire["name"], json!("APIError"));
        assert_eq!(wire["data"]["statusCode"], json!(529));
        assert_eq!(wire["data"]["isRetryable"], json!(true));
        // retry.ts:137 — "Overloaded" messages are normalized.
        assert_eq!(retries, vec![json!("Provider is overloaded"); 5]);

        // A non-JSON gateway page: ai-sdk falls back to the status text.
        let (wire, _, _) = failing_turn(
            "e2e-http-502",
            immediate_retry(http_failure(502, "<html>upstream</html>")),
            6,
        )
        .await;
        assert_eq!(wire["data"]["message"], json!("Bad Gateway"));
        assert_eq!(wire["data"]["statusCode"], json!(502));
        assert_eq!(wire["data"]["isRetryable"], json!(true));
    }

    /// A request that never got a response, as `RequestExecutor.send`
    /// raises it (kind from reqwest, message from the cause chain).
    fn connection_refused() -> LlmError {
        LlmError {
            module: "RequestExecutor".to_string(),
            method: "execute".to_string(),
            reason: LlmErrorReason::Transport {
                message:
                    "client error (Connect): tcp connect error: Connection refused (os error 111)"
                        .to_string(),
                kind: Some("ConnectError".to_string()),
                url: Some(API_URL.to_string()),
                http: Some(HttpContext {
                    request: HttpRequestDetails {
                        method: "POST".to_string(),
                        url: API_URL.to_string(),
                        headers: Default::default(),
                    },
                    response: None,
                    body: None,
                    body_truncated: None,
                    request_id: None,
                    rate_limit: None,
                }),
            },
        }
    }

    #[tokio::test(start_paused = true)]
    async fn connection_refused_retries_then_is_a_retryable_api_error() {
        // provider-utils `handleFetchError`: a failed fetch becomes a
        // status-less, retryable `APICallError("Cannot connect to API: …")`.
        let (wire, published, retries) = failing_turn("e2e-refused", connection_refused(), 6).await;
        let message = "Cannot connect to API: client error (Connect): tcp connect error: \
                       Connection refused (os error 111)";
        let expected = json!({
            "name": "APIError",
            "data": {
                "message": message,
                "isRetryable": true,
                "metadata": { "url": API_URL },
            },
        });
        assert_eq!(wire, expected);
        assert_eq!(published, vec![expected]);
        assert_eq!(retries, vec![json!(message); 5]);
    }

    #[tokio::test(start_paused = true)]
    async fn unbuildable_request_is_an_unknown_error() {
        // `handleFetchError` rethrows non-network failures untouched.
        let mut error = connection_refused();
        if let LlmErrorReason::Transport { message, kind, .. } = &mut error.reason {
            *message = "builder error".to_string();
            *kind = Some(alforria_llm::route::executor::ENCODE_ERROR.to_string());
        }
        let (wire, _, retries) = failing_turn("e2e-unbuildable", error, 1).await;
        assert_eq!(
            wire,
            json!({
                "name": "UnknownError",
                "data": { "message": "RequestExecutor.execute: builder error" },
            })
        );
        assert!(retries.is_empty(), "{retries:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn body_dropped_mid_stream_is_a_connection_reset() {
        // Bun rejects the body read with `code: "ECONNRESET"`, which
        // `fromError` maps before any ai-sdk wrapping (message-v2.ts:629).
        let cause = "error reading a body from connection: unexpected EOF during chunk size line";
        let error = LlmError {
            module: "Route".to_string(),
            method: "stream".to_string(),
            reason: LlmErrorReason::Transport {
                message: cause.to_string(),
                kind: Some(alforria_llm::route::client::BODY_READ_ERROR.to_string()),
                url: None,
                http: None,
            },
        };
        let (wire, _, retries) = failing_turn("e2e-mid-stream-reset", error, 6).await;
        assert_eq!(
            wire,
            json!({
                "name": "APIError",
                "data": {
                    "message": "Connection reset by server",
                    "isRetryable": true,
                    "metadata": { "code": "ECONNRESET", "syscall": "", "message": cause },
                },
            })
        );
        assert_eq!(retries, vec![json!("Connection reset by server"); 5]);
    }

    #[tokio::test(start_paused = true)]
    async fn context_overflow_body_is_a_context_overflow_error() {
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 250000 tokens > 200000 maximum"}}"#;
        let config = json!({ "compaction": { "auto": false } });
        let e = crate::session::test_support::engine_with_config(
            "e2e-http-overflow",
            vec![vec![Err(http_failure(400, body))]],
            config,
        );
        let session = create_engine_session(&e);
        let assistant = run_prompt(&e, prompt_input(&session.id, "hi")).await;
        let V1Message::Assistant { error, .. } = &assistant.info else {
            panic!("expected an assistant message");
        };
        assert_eq!(
            serde_json::to_value(error.as_ref().expect("errored")).unwrap(),
            json!({
                "name": "ContextOverflowError",
                "data": {
                    "message": "prompt is too long: 250000 tokens > 200000 maximum",
                    "responseBody": body,
                },
            })
        );
    }

    // ------------------------------------------------------------------
    // 12. Final event order of a turn (processor.ts:571-648, run-state.ts:
    // 60-63): the completed `message.updated` precedes the runner's idle;
    // on a stream failure `halt` publishes error + idle *before* cleanup
    // completes the message, and the runner goes idle again after.
    // ------------------------------------------------------------------

    /// The tail-relevant events: assistant `message.updated` (tagged with
    /// whether it is completed), `session.error`, `session.status`
    /// (tagged with its type) and `session.idle`.
    fn turn_markers(events: &[(String, Value)]) -> Vec<String> {
        events
            .iter()
            .filter_map(|(type_, data)| match type_.as_str() {
                "message.updated" if data["info"]["role"] == json!("assistant") => {
                    Some(if data["info"]["time"]["completed"].is_null() {
                        "message.updated".to_string()
                    } else {
                        "message.updated:completed".to_string()
                    })
                }
                "session.status" => Some(format!(
                    "session.status:{}",
                    data["status"]["type"].as_str().unwrap_or_default()
                )),
                "session.error" | "session.idle" => Some(type_.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn completed_message_precedes_idle() {
        let e = engine("e2e-order-ok", vec![text_stream("hello")]);
        let session = create_engine_session(&e);
        let rx = e.services.events.all();
        run_prompt(&e, prompt_input(&session.id, "hi")).await;

        let markers = turn_markers(&drain(rx));
        assert_eq!(
            markers[markers.len() - 4..],
            [
                "message.updated:completed",
                "session.status:busy",
                "session.status:idle",
                "session.idle",
            ],
            "{markers:?}"
        );
        assert_eq!(
            markers
                .iter()
                .filter(|marker| *marker == "session.idle")
                .count(),
            1,
            "{markers:?}"
        );
    }

    #[tokio::test]
    async fn failed_turn_halts_idle_before_cleanup_completes_the_message() {
        let body = r#"{"error":{"message":"bad request"}}"#;
        let e = engine("e2e-order-error", vec![vec![Err(http_failure(400, body))]]);
        let session = create_engine_session(&e);
        let rx = e.services.events.all();
        run_prompt(&e, prompt_input(&session.id, "hi")).await;

        let markers = turn_markers(&drain(rx));
        assert_eq!(
            markers,
            [
                "session.status:busy",
                "message.updated",
                "session.status:busy",
                "session.error",
                "session.status:idle",
                "session.idle",
                "message.updated:completed",
                "session.status:idle",
                "session.idle",
            ],
        );
    }
}

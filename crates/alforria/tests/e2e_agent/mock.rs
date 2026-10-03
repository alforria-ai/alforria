//! Deterministic scenarios (always run): the scripted wire backend driving
//! the real `opencode` binary end-to-end.

mod e2e_agent {

    use serde_json::json;

    use crate::backend::{LlmBackend, MockBackend};
    use crate::scenarios;

    #[test]
    fn a1_file_mutation_round_trip() {
        let backend = MockBackend::new(scenarios::A1_FILE_MUTATION);
        scenarios::a1_file_mutation(&backend);

        let requests = backend.requests();
        assert_eq!(requests.len(), 2, "one model request per step");
        let first = serde_json::to_string(&requests[0]).expect("request body");
        assert!(
            first.contains("create notes.md for me"),
            "request 1 must carry the prompt: {first}"
        );
        // The tool result fed back into request 2 (the tool-call round-trip).
        let results = tool_results(backend.request(1));
        assert_eq!(results.len(), 1, "one tool result: {results:?}");
        assert_eq!(results[0]["tool_call_id"], json!("cal_1"));
        let content = results[0]["content"].as_str().expect("tool content");
        assert!(content.contains("Wrote file successfully"), "{content}");
    }

    #[test]
    fn a2_multi_step_loop_with_parallel_calls() {
        let backend = MockBackend::new(scenarios::A2_MULTI_STEP);
        scenarios::a2_multi_step(&backend);

        let requests = backend.requests();
        assert_eq!(requests.len(), 3, "one model request per step");
        let results = tool_results(backend.request(1));
        assert_eq!(results.len(), 1, "step 2 sees one result: {results:?}");
        assert_eq!(results[0]["tool_call_id"], json!("cal_1"));
        assert!(results[0]["content"].as_str().unwrap().contains("alpha"));

        let results = tool_results(backend.request(2));
        assert_eq!(results.len(), 3, "step 3 sees all results: {results:?}");
        let contents = results
            .iter()
            .map(|result| {
                (
                    result["tool_call_id"].as_str().expect("call id"),
                    result["content"].as_str().expect("content"),
                )
            })
            .collect::<Vec<_>>();
        assert!(
            contents
                .iter()
                .any(|(id, content)| *id == "cal_2" && content.contains("alpha")),
            "{contents:?}"
        );
        assert!(
            contents
                .iter()
                .any(|(id, content)| *id == "cal_3" && content.contains("beta")),
            "{contents:?}"
        );
    }

    /// The `role: "tool"` messages of one recorded request body.
    fn tool_results(request: serde_json::Value) -> Vec<serde_json::Value> {
        request["messages"]
            .as_array()
            .map(|messages| {
                messages
                    .iter()
                    .filter(|message| message["role"] == "tool")
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }
    // -------------------------------------------------------------------
    // A3 — permission gate over the wire (once / always / reject)
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn a3_permission_once_asks_for_every_call() {
        let backend = MockBackend::new(scenarios::A3_PERMISSION_GATE);
        let asks = scenarios::a3_permission_gate(&backend, scenarios::Reply::Once).await;

        assert_eq!(asks.len(), 2);
        assert_eq!(backend.requests().len(), 3, "one request per step");
        let second = tool_results(backend.request(1));
        assert_eq!(second.len(), 1, "step 2 sees one result: {second:?}");
        assert!(
            second[0]["content"]
                .as_str()
                .unwrap_or_default()
                .contains("TOKEN=1"),
            "{second:?}"
        );
        let third = tool_results(backend.request(2));
        assert_eq!(third.len(), 2, "step 3 sees both results: {third:?}");
    }

    #[tokio::test]
    async fn a3_permission_always_skips_the_second_ask() {
        let backend = MockBackend::new(scenarios::A3_PERMISSION_GATE);
        let asks = scenarios::a3_permission_gate(&backend, scenarios::Reply::Always).await;

        assert_eq!(asks.len(), 1, "always must skip the second ask");
        assert_eq!(backend.requests().len(), 3, "one request per step");
        let third = tool_results(backend.request(2));
        assert_eq!(third.len(), 2, "step 3 sees both results: {third:?}");
    }

    #[tokio::test]
    async fn a3_permission_reject_breaks_the_loop() {
        let backend = MockBackend::new(scenarios::A3_PERMISSION_GATE);
        let asks = scenarios::a3_permission_gate(&backend, scenarios::Reply::Reject).await;

        // The rejection blocks the loop (processor.ts:200-201): no second
        // model request.
        assert!(!asks.is_empty());
        assert_eq!(backend.requests().len(), 1, "the loop must break");
    }

    // -------------------------------------------------------------------
    // A4 — subagent over the wire
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn a4_subagent_spawns_child_session() {
        let backend = MockBackend::new(scenarios::A4_SUBAGENT);
        let (_, child_id) = scenarios::a4_subagent(&backend).await;

        // Parent step, child turn, parent continuation.
        let requests = backend.requests();
        assert_eq!(requests.len(), 3, "one request per loop turn");
        let child = serde_json::to_string(&backend.request(1)).expect("request body");
        assert!(
            child.contains("do the research"),
            "the child prompt must reach the model: {child}"
        );
        let results = tool_results(backend.request(2));
        assert_eq!(results.len(), 1, "one task result: {results:?}");
        assert_eq!(results[0]["tool_call_id"], json!("cal_1"));
        assert!(
            results[0]["content"]
                .as_str()
                .unwrap_or_default()
                .contains(&format!("<task id=\"{child_id}\" state=\"completed\">")),
            "the parent continuation must carry the task result: {results:?}"
        );
    }

    // -------------------------------------------------------------------
    // A5 — doom-loop over the wire
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn a5_doom_loop_asks_once_and_all_parts_run() {
        let backend = MockBackend::new(scenarios::A5_DOOM_LOOP);
        let asks = scenarios::a5_doom_loop(&backend).await;

        assert_eq!(asks.len(), 1);
        assert_eq!(backend.requests().len(), 2, "one request per step");
        let results = tool_results(backend.request(1));
        assert_eq!(results.len(), 3, "three read results: {results:?}");
    }

    // -------------------------------------------------------------------
    // A6 — compaction over the wire
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn a6_compaction_continues_on_the_compacted_history() {
        let backend = MockBackend::new(scenarios::A6_COMPACTION);
        scenarios::a6_compaction(&backend).await;

        // The overflowing step and the compacted continuation; the
        // compaction fork consumed the transcript's summary turn.
        assert_eq!(backend.requests().len(), 2, "one request per loop turn");
        let summary = backend
            .transcript(scenarios::A6_COMPACTION)
            .expect("transcript")
            .turn_text(1);
        let continued = serde_json::to_string(&backend.request(1)).expect("request body");
        assert!(continued.contains(&summary), "{continued}");
        assert!(
            !continued.contains("trigger overflow"),
            "the pruned tail must not reach the model: {continued}"
        );
    }

    // -------------------------------------------------------------------
    // A7 — revert/unrevert over the wire
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn a7_revert_unrevert_with_git_worktree() {
        let backend = MockBackend::new(scenarios::A7_REVERT);
        scenarios::a7_revert(&backend).await;

        assert_eq!(backend.requests().len(), 2, "one model request per step");
    }

    // -------------------------------------------------------------------
    // A11 — session diff + file events over the wire
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn a11_edit_populates_turn_diff_and_file_events() {
        let backend = MockBackend::new(scenarios::A11_SESSION_DIFF);
        scenarios::a11_session_diff(&backend).await;

        assert_eq!(backend.requests().len(), 2, "one model request per step");
    }

    // -------------------------------------------------------------------
    // A8 — cancel mid-stream
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn a8_cancel_mid_stream_interrupts_and_reprompt_continues() {
        let backend = MockBackend::new(scenarios::A8_CANCEL_MID_STREAM);
        let messages = scenarios::a8_cancel_mid_stream(&backend).await;

        // Two model requests: the aborted stream and the re-prompt.
        assert_eq!(backend.requests().len(), 2, "{messages:?}");
        let serialized = serde_json::to_string(&backend.request(1)).expect("request body");
        assert!(
            serialized.contains("go again"),
            "the re-prompt must reach the model: {serialized}"
        );
    }

    // -------------------------------------------------------------------
    // A9 — structured output over the wire
    // -------------------------------------------------------------------

    /// The last assistant message of the store.
    fn last_assistant(messages: &[serde_json::Value]) -> &serde_json::Value {
        messages
            .iter()
            .rev()
            .find(|message| message["info"]["role"] == json!("assistant"))
            .expect("assistant message")
    }

    #[tokio::test]
    async fn a9_structured_output_captures_the_tool_payload() {
        let backend = MockBackend::new(scenarios::A9_STRUCTURED_OUTPUT);
        let messages = scenarios::a9_structured_output(&backend).await;

        let assistant = last_assistant(&messages);
        // The step already set finish to tool-calls; the capture keeps it
        // (the loop only defaults finish when unset, prompt.ts:1282-1289).
        assert_eq!(
            assistant["info"]["structured"],
            json!({ "answer": 42 }),
            "{assistant}"
        );
        assert_eq!(
            assistant["info"]["finish"],
            json!("tool-calls"),
            "{assistant}"
        );

        // The StructuredOutput tool rode the request with tool_choice
        // required (prompt.ts:1276-1281).
        assert_eq!(backend.requests().len(), 1);
        let request = backend.request(0);
        let tools = request["tools"].as_array().cloned().unwrap_or_default();
        let names = tools
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str())
            .collect::<Vec<_>>();
        assert!(
            names.contains(&"StructuredOutput"),
            "no StructuredOutput tool\n{request}"
        );
        assert_eq!(request["tool_choice"], json!("required"), "{request}");
    }

    #[tokio::test]
    async fn a9_structured_output_error_without_a_tool_call() {
        let backend = MockBackend::new(scenarios::A9_STRUCTURED_ERROR);
        let messages = scenarios::a9_structured_error(&backend).await;

        let assistant = last_assistant(&messages);
        assert_eq!(
            assistant["info"]["error"]["name"],
            json!("StructuredOutputError"),
            "{assistant}"
        );
        assert!(
            assistant["info"]["structured"].is_null(),
            "no structured payload on the error path: {assistant}"
        );
    }

    // -------------------------------------------------------------------
    // A10 + CLI gap #4 — stream golden, --auto reply path
    // -------------------------------------------------------------------

    #[test]
    fn a10_cli_stream_matches_the_committed_golden() {
        let backend = MockBackend::new(scenarios::A1_FILE_MUTATION);
        scenarios::a10_cli_stream(&backend);
    }

    #[test]
    fn cli_auto_replies_permission_asks() {
        let backend = MockBackend::new(scenarios::CLI_AUTO_REPLY);
        scenarios::cli_permission_auto(&backend, true);

        assert_eq!(backend.requests().len(), 3, "one request per step");
        let second = tool_results(backend.request(1));
        let third = tool_results(backend.request(2));
        assert_eq!(second.len(), 1, "step 2 sees one result: {second:?}");
        assert!(
            second[0]["content"]
                .as_str()
                .unwrap_or_default()
                .contains("TOKEN=1"),
            "{second:?}"
        );
        assert_eq!(third.len(), 2, "step 3 sees both results: {third:?}");
    }

    #[test]
    fn cli_permission_asks_auto_reject_without_auto() {
        let backend = MockBackend::new(scenarios::CLI_AUTO_REPLY);
        scenarios::cli_permission_auto(&backend, false);

        assert_eq!(backend.requests().len(), 1, "the loop must break");
    }
}

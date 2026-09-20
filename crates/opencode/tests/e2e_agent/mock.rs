//! Deterministic scenarios (always run): the scripted wire backend driving
//! the real `opencode` binary end-to-end.

mod e2e_agent {

    use serde_json::json;

    use crate::backend::MockBackend;
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
}

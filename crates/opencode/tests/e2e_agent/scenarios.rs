//! Backend-agnostic scenario drivers (spec E2E §2.4): prompt → pump →
//! assert. Deterministic assertions relax into outcome assertions for
//! live backends (`backend.live()`).

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::backend::LlmBackend;
use crate::harness::{Env, ProcOutput};

pub const A1_FILE_MUTATION: &str = "a1_file_mutation";
pub const A2_MULTI_STEP: &str = "a2_multi_step";

/// One driven scenario: the process output plus the parsed `--format
/// json` event stream. Keeps the `Env` (and its tempdir) alive so the
/// project directory survives for assertions.
pub struct ScenarioRun {
    pub _env: Env,
    pub output: ProcOutput,
    pub project_dir: PathBuf,
    pub events: Vec<Value>,
}

impl ScenarioRun {
    /// The `tool_use` events for the given tool, in stream order.
    fn tool_parts(&self, tool: &str) -> Vec<&Value> {
        self.events
            .iter()
            .filter(|event| {
                event["type"] == json!("tool_use") && event["part"]["tool"] == json!(tool)
            })
            .collect()
    }

    fn assert_exit_zero(&self) {
        assert_eq!(
            self.output.code,
            Some(0),
            "run failed\nstdout={}\nstderr={}",
            self.output.stdout,
            self.output.stderr
        );
    }
}

fn run_scenario(
    backend: &impl LlmBackend,
    scenario: &str,
    prompt: &str,
    setup: impl FnOnce(&Path),
) -> ScenarioRun {
    let env = Env::new(scenario);
    env.write_provider_config(
        &backend.base_url(),
        scenario,
        &backend.api_key(),
        backend.provider_id(),
        backend.model_id(),
        backend.context_limit(),
    );
    let project = env.project_dir();
    std::fs::create_dir_all(&project).expect("create project");
    setup(&project);
    let model = format!("{}/{}", backend.provider_id(), backend.model_id());
    let output = env.run(&["run", "--format", "json", "--model", &model, prompt]);
    let events = output
        .stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("valid JSON event"))
        .collect();
    ScenarioRun {
        _env: env,
        output,
        project_dir: project,
        events,
    }
}

/// The filtered event-type sequence (types + part tags, ids normalized):
/// the golden shape of a `--format json` run. Consecutive parallel tool
/// calls settle in nondeterministic order, so each `tool_use` run is
/// sorted.
fn event_sequence(events: &[Value]) -> Vec<String> {
    let mut out = Vec::new();
    let mut run: Vec<String> = Vec::new();
    for event in events {
        let tag = match event["type"].as_str().unwrap_or_default() {
            "tool_use" => format!(
                "tool_use:{}",
                event["part"]["tool"].as_str().unwrap_or_default()
            ),
            kind => kind.to_string(),
        };
        if tag.starts_with("tool_use:") {
            run.push(tag);
        } else {
            run.sort();
            out.append(&mut run);
            out.push(tag);
        }
    }
    run.sort();
    out.append(&mut run);
    out
}

/// A1 — file mutation round-trip: a `write` tool call over the wire, the
/// file lands on disk, the tool result feeds the follow-up request, and
/// the final text answer closes the loop.
pub fn a1_file_mutation(backend: &impl LlmBackend) -> ScenarioRun {
    let run = run_scenario(backend, A1_FILE_MUTATION, "create notes.md for me", |_| {});
    run.assert_exit_zero();

    let content = std::fs::read_to_string(run.project_dir.join("notes.md"))
        .unwrap_or_else(|err| panic!("notes.md must exist: {err}"));
    if let Some(transcript) = backend.transcript(A1_FILE_MUTATION) {
        assert_eq!(
            content,
            transcript
                .tool_argument("cal_1", "content")
                .expect("scripted content"),
            "file content must match the scripted tool call"
        );
    } else {
        assert!(!content.trim().is_empty(), "model-created file is empty");
    }

    let tools = run.tool_parts("write");
    assert_eq!(tools.len(), 1, "one write tool part\n{}", run.output.stdout);
    assert_eq!(tools[0]["part"]["state"]["status"], json!("completed"));
    assert!(tools[0]["part"]["state"]["time"].get("start").is_some());
    assert!(tools[0]["part"]["state"]["time"].get("end").is_some());

    if !backend.live() {
        assert_eq!(
            event_sequence(&run.events),
            [
                // The step finishes (tool-calls) before the forked tool
                // settlement lands on the part.
                "step_start",
                "step_finish",
                "tool_use:write",
                "step_start",
                "text",
                "step_finish",
            ]
        );
    }
    assert!(
        run.events
            .iter()
            .any(|event| event["type"] == json!("text")),
        "no text answer in the stream\n{}",
        run.output.stdout
    );
    run
}

/// A2 — multi-step loop: three model steps, the middle one issuing two
/// parallel `read` calls in one assistant turn.
pub fn a2_multi_step(backend: &impl LlmBackend) -> ScenarioRun {
    let run = run_scenario(
        backend,
        A2_MULTI_STEP,
        "read the two files and answer",
        |project| {
            std::fs::write(project.join("a.txt"), "alpha\n").expect("seed a.txt");
            std::fs::write(project.join("b.txt"), "beta\n").expect("seed b.txt");
        },
    );
    run.assert_exit_zero();

    let reads = run.tool_parts("read");
    assert_eq!(reads.len(), 3, "three read parts across the steps");
    for part in &reads {
        assert_eq!(part["part"]["state"]["status"], json!("completed"));
    }

    if !backend.live() {
        assert_eq!(
            event_sequence(&run.events),
            [
                "step_start",
                "step_finish",
                "tool_use:read",
                "step_start",
                "step_finish",
                "tool_use:read",
                "tool_use:read",
                "step_start",
                "text",
                "step_finish",
            ]
        );
    }
    run
}

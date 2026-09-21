//! Scenario parity tests (gated): the dual-binary drivers compared under
//! normalization. P1–P6/P8/P9 — the chunk-2 HTTP scenario matrix (spec
//! PARITY §5); the strict SSE stream diff is chunk 3's first-class
//! section, so only the event-type multiset is compared here.

use serde::Deserialize;
use serde_json::{json, Value};

use crate::differ::{diff, Deviation, Kind};
use crate::normal::{
    drop_control_plane, event_types, normalize_events, normalize_requests, sort_events, Normalizer,
};
use crate::parity_harness::{gated, TsSource};
use crate::report::Report;
use crate::scenarios::{self, Capture, Named};
use crate::transcript::Transcript;

/// The committed, justified deviation list (spec PARITY §6.2). Structural
/// deviations are never allowlisted; an allowlisted entry that stops
/// matching fails the run (stale entries are pruned, not rotted).
#[derive(Deserialize)]
pub struct AllowEntry {
    pub check: String,
    pub pattern: String,
    pub reason: String,
    pub milestone: u32,
}

fn triage(check: &str, deviations: Vec<Deviation>) -> Vec<Deviation> {
    let text = include_str!("allowlist.json");
    let allowlist: Vec<AllowEntry> =
        serde_json::from_str(text).unwrap_or_else(|err| panic!("allowlist.json: {err}"));
    for entry in &allowlist {
        assert!(
            !entry.reason.trim().is_empty() && entry.milestone > 0,
            "allowlist entry ({} {}) needs a justification and an expiry milestone",
            entry.check,
            entry.pattern
        );
    }
    let mut used = vec![false; allowlist.len()];
    let triaged = deviations
        .into_iter()
        .filter(|deviation| {
            if deviation.kind == Kind::Structural {
                return true;
            }
            if let Some(index) = allowlist
                .iter()
                .position(|entry| entry.check == check && deviation.path.contains(&entry.pattern))
            {
                used[index] = true;
                return false;
            }
            true
        })
        .collect::<Vec<_>>();
    for (entry, used) in allowlist.iter().zip(used.iter()) {
        if entry.check == check {
            assert!(
                *used,
                "stale allowlist entry ({} {}): the deviation disappeared",
                entry.check, entry.pattern
            );
        }
    }
    triaged
}

/// One side's captures, normalized with a per-side `Normalizer` in a
/// fixed order (named captures in driving order, then requests, then
/// events) so id/timestamp counters align across the two sides.
struct Normalized {
    named: Named,
    requests: Value,
    events: Vec<Value>,
    types: Vec<String>,
    control_plane: std::collections::BTreeMap<String, usize>,
}

fn normalize_side(capture: &Capture) -> Normalized {
    let mut normalizer = Normalizer::new();
    let named = capture
        .named
        .iter()
        .map(|(key, value)| (*key, normalizer.normalize(&capture.project, value)))
        .collect();
    let requests = normalize_requests(&mut normalizer, &capture.project, &capture.requests);
    let events = normalize_events(&mut normalizer, &capture.project, &capture.events);
    let types = event_types(&sort_events(drop_control_plane(&events)));
    let control_plane = events
        .iter()
        .filter_map(|event| {
            let kind = event["type"].as_str().unwrap_or_default().to_string();
            crate::normal::INSTANCE_CONTROL_PLANE
                .contains(&kind.as_str())
                .then_some(kind)
        })
        .fold(std::collections::BTreeMap::new(), |mut acc, kind| {
            *acc.entry(kind).or_insert(0) += 1;
            acc
        });
    Normalized {
        named,
        requests,
        events,
        types,
        control_plane,
    }
}

/// Normalize both sides, diff every named capture (plus the recorded
/// mock-LLM request bodies where the scenario's parity assertion needs
/// them), triage value deviations, write the report, and assert the
/// matrix clean. Returns the report for scenario-specific findings.
fn assert_parity(check: &str, diff_requests: bool, ts: &Capture, rs: &Capture) -> Report {
    let report = Report::open(check);
    for (side, capture) in [("ts", ts), ("rs", rs)] {
        for (name, value) in &capture.named {
            report.write_raw(side, name, value);
        }
        report.write_raw(side, "requests", &json!(capture.requests));
        report.write_raw(side, "events", &json!(capture.events));
    }

    let ts = normalize_side(ts);
    let rs = normalize_side(rs);
    report.write_events("ts", &ts.events);
    report.write_events("rs", &rs.events);

    let mut captures: Vec<(&str, Vec<Deviation>)> = Vec::new();
    for ((name, ts_value), (rs_name, rs_value)) in ts.named.iter().zip(rs.named.iter()) {
        assert_eq!(name, rs_name, "capture names diverged between the sides");
        let path = format!("$.{name}");
        captures.push((name, triage(check, diff(&path, ts_value, rs_value))));
    }
    if diff_requests {
        captures.push((
            "requests",
            triage(check, diff("$.requests", &ts.requests, &rs.requests)),
        ));
    }
    let all = report.write_scenario(check, &captures);
    assert!(
        all.is_empty(),
        "{} un-allowlisted deviation(s) in {check}: {all:#?}",
        all.len()
    );

    // Event-type multiset (chunk 3 tightens this into a full stream
    // diff). Instance control-plane startup events that only one side
    // publishes are chunk-3 triage findings, not assertion failures.
    assert_eq!(ts.types, rs.types, "event-type streams diverged in {check}");
    if !ts.control_plane.is_empty() || !rs.control_plane.is_empty() {
        report.append_findings(&format!(
            "## Instance control-plane events (chunk-3 triage)\n\n- ts: {:?}\n- rs: {:?}\n",
            ts.control_plane, rs.control_plane
        ));
    }
    report
}

/// One named capture of a side.
fn named<'a>(capture: &'a Capture, key: &str) -> &'a Value {
    &capture
        .named
        .iter()
        .find(|(name, _)| *name == key)
        .unwrap_or_else(|| panic!("no capture named {key}"))
        .1
}

fn setup_none() -> impl Fn(&std::path::Path) {
    |_| {}
}

#[tokio::test]
async fn p1_file_mutation_parity() {
    if !gated() {
        return;
    }
    let ts_source = TsSource::resolve();
    let (ts, rs) = scenarios::scenario(
        "p1",
        scenarios::A1_FILE_MUTATION,
        &ts_source,
        setup_none(),
        |backend, env, port, log, _| Box::pin(scenarios::drive_p1(backend, env, port, log)),
    )
    .await;
    assert_parity("p1_file_mutation", false, &ts, &rs);

    // The final file bytes must match the scripted tool call, and both
    // sides drive the scripted two-step agent loop.
    let scripted = Transcript::load(scenarios::A1_FILE_MUTATION)
        .tool_argument("cal_1", "content")
        .expect("scripted content");
    for (side, capture) in [("ts", &ts), ("rs", &rs)] {
        assert_eq!(
            named(capture, "file"),
            &json!(scripted),
            "{side} notes.md must match the scripted bytes"
        );
        assert_eq!(capture.requests.len(), 2, "{side} agent requests");
        assert!(
            capture.requests[0]
                .to_string()
                .contains("create notes.md for me"),
            "{side} prompt must reach the mock backend"
        );
    }
}

#[tokio::test]
async fn p2_multi_step_parity() {
    if !gated() {
        return;
    }
    let ts_source = TsSource::resolve();
    let setup = |project: &std::path::Path| {
        std::fs::write(project.join("a.txt"), "alpha\n").expect("seed a.txt");
        std::fs::write(project.join("b.txt"), "beta\n").expect("seed b.txt");
    };
    let (ts, rs) = scenarios::scenario(
        "p2",
        scenarios::A2_MULTI_STEP,
        &ts_source,
        setup,
        |backend, env, port, log, _| Box::pin(scenarios::drive_p2(backend, env, port, log)),
    )
    .await;
    assert_parity("p2_multi_step", true, &ts, &rs);
}

async fn p3_parity(check: &str, reply: scenarios::Reply) {
    if !gated() {
        return;
    }
    let ts_source = TsSource::resolve();
    let setup = |project: &std::path::Path| {
        std::fs::write(project.join("secret.env"), "TOKEN=1\n").expect("seed secret.env");
    };
    let (ts, rs) = scenarios::scenario(
        "p3",
        scenarios::A3_PERMISSION_GATE,
        &ts_source,
        setup,
        |backend, env, port, log, _| Box::pin(scenarios::drive_p3(backend, env, port, log, reply)),
    )
    .await;
    assert_parity(check, false, &ts, &rs);
    let ask_count = |capture: &Capture| named(capture, "asks").as_array().map(|a| a.len());
    assert_eq!(
        ask_count(&ts),
        ask_count(&rs),
        "ask counts diverged in {check}"
    );
}

#[tokio::test]
async fn p3_permission_gate_once_parity() {
    p3_parity("p3_permission_gate_once", scenarios::Reply::Once).await;
}

#[tokio::test]
async fn p3_permission_gate_always_parity() {
    p3_parity("p3_permission_gate_always", scenarios::Reply::Always).await;
}

#[tokio::test]
async fn p3_permission_gate_reject_parity() {
    p3_parity("p3_permission_gate_reject", scenarios::Reply::Reject).await;
}

#[tokio::test]
async fn p4_subagent_parity() {
    if !gated() {
        return;
    }
    let ts_source = TsSource::resolve();
    let (ts, rs) = scenarios::scenario(
        "p4",
        scenarios::A4_SUBAGENT,
        &ts_source,
        setup_none(),
        |backend, env, port, log, _| Box::pin(scenarios::drive_p4(backend, env, port, log)),
    )
    .await;
    assert_parity("p4_subagent", false, &ts, &rs);

    // The child session is listed with the parent link.
    for (side, capture) in [("ts", &ts), ("rs", &rs)] {
        let parent = named(capture, "session")["id"].as_str().expect("parent id");
        let list = named(capture, "session_list");
        let child = list
            .as_array()
            .expect("session list")
            .iter()
            .find(|item| item["id"] != json!(parent))
            .unwrap_or_else(|| panic!("{side} child session listed"));
        assert_eq!(child["parentID"], json!(parent), "{side} parent link");
    }
}

#[tokio::test]
async fn p5_compaction_parity() {
    if !gated() {
        return;
    }
    let ts_source = TsSource::resolve();
    let setup = |project: &std::path::Path| {
        std::fs::write(project.join("a.txt"), "x\n").expect("seed a.txt");
    };
    let (ts, rs) = scenarios::scenario(
        "p5",
        scenarios::A6_COMPACTION,
        &ts_source,
        setup,
        |backend, env, port, log, _| Box::pin(scenarios::drive_p5(backend, env, port, log)),
    )
    .await;
    assert_parity("p5_compaction", true, &ts, &rs);

    // The compaction fork published `session.compacted` on both sides.
    for (side, capture) in [("ts", &ts), ("rs", &rs)] {
        assert!(
            capture
                .events
                .iter()
                .any(|event| event["type"] == json!("session.compacted")),
            "{side} no session.compacted event"
        );
    }
}

fn git(project: &std::path::Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(project)
        .status()
        .expect("git spawns");
    assert!(status.success(), "git {args:?} failed");
}

#[tokio::test]
async fn p6_revert_unrevert_parity() {
    if !gated() {
        return;
    }
    let ts_source = TsSource::resolve();
    let setup = |project: &std::path::Path| {
        git(project, &["init", "--quiet"]);
        git(project, &["config", "user.email", "e2e@opencode.test"]);
        git(project, &["config", "user.name", "E2E"]);
        std::fs::write(project.join("a.txt"), "v1\n").expect("seed a.txt");
        git(project, &["add", "-A"]);
        git(project, &["commit", "--quiet", "-m", "init"]);
    };
    let (ts, rs) = scenarios::scenario(
        "p6",
        scenarios::A7_REVERT,
        &ts_source,
        setup,
        |backend, env, port, log, _| Box::pin(scenarios::drive_p6(backend, env, port, log)),
    )
    .await;
    assert_parity("p6_revert_unrevert", false, &ts, &rs);

    // Revert rolled the edit back on disk; unrevert restored it.
    for (side, capture) in [("ts", &ts), ("rs", &rs)] {
        assert_eq!(named(capture, "a_txt_reverted"), &json!("v1\n"), "{side}");
        assert_eq!(named(capture, "a_txt_unreverted"), &json!("v2\n"), "{side}");
        let summary = &named(capture, "revert")["summary"];
        assert_eq!(summary["files"].as_f64(), Some(1.0), "{side} {summary}");
        assert_eq!(summary["additions"].as_f64(), Some(1.0), "{side} {summary}");
        assert_eq!(summary["deletions"].as_f64(), Some(1.0), "{side} {summary}");
    }
}

#[tokio::test]
async fn p8_structured_output_parity() {
    if !gated() {
        return;
    }
    let ts_source = TsSource::resolve();
    let (ts, rs) = scenarios::scenario(
        "p8",
        scenarios::A9_STRUCTURED_OUTPUT,
        &ts_source,
        setup_none(),
        |backend, env, port, log, _| Box::pin(scenarios::drive_p8(backend, env, port, log)),
    )
    .await;
    assert_parity("p8_structured_output", false, &ts, &rs);

    // The structured payload landed on the final assistant message.
    for (side, capture) in [("ts", &ts), ("rs", &rs)] {
        assert_eq!(
            named(capture, "message")["info"]["structured"],
            json!({ "answer": 42 }),
            "{side} structured payload"
        );
    }
}

#[tokio::test]
async fn p9_session_lifecycle_parity() {
    if !gated() {
        return;
    }
    let ts_source = TsSource::resolve();
    let (ts, rs) = scenarios::scenario(
        "p9",
        scenarios::A1_FILE_MUTATION,
        &ts_source,
        setup_none(),
        |backend, env, port, log, cli| Box::pin(scenarios::drive_p9(backend, env, port, log, cli)),
    )
    .await;
    assert_parity("p9_session_lifecycle", false, &ts, &rs);

    // The export → import round-trip is byte-equal on both sides, modulo
    // the volatile `info.time` (late async patches — title, tokens —
    // can bump `time.updated` between export and re-export).
    for (side, capture) in [("ts", &ts), ("rs", &rs)] {
        let strip = |value: &Value| {
            let mut value = value.clone();
            if let Some(info) = value.get_mut("info") {
                info.as_object_mut().unwrap().remove("time");
            }
            value
        };
        let export = strip(named(capture, "export"));
        let reexport = strip(named(capture, "reexport"));
        assert_eq!(export, reexport, "{side} re-export differs");
    }
}

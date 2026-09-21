//! Scenario parity tests (gated): the dual-binary drivers compared under
//! normalization. P1 — the file-mutation round-trip (spec PARITY §5).

use serde::Deserialize;

use crate::backend::{LlmBackend, MockBackend};
use crate::differ::{diff, Deviation, Kind};
use crate::normal::{drop_control_plane, event_types, normalize_events, sort_events, Normalizer};
use crate::parity_harness::{gated, TsSource};
use crate::report::Report;
use crate::scenarios::{self, Capture};

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
            let matched = allowlist
                .iter()
                .position(|entry| entry.check == check && deviation.path.contains(&entry.pattern));
            if let Some(index) = matched {
                used[index] = true;
                return false;
            }
            true
        })
        .collect::<Vec<_>>();
    for (entry, used) in allowlist.iter().zip(used) {
        assert!(
            used,
            "stale allowlist entry ({} {}): the deviation disappeared",
            entry.check, entry.pattern
        );
    }
    triaged
}

/// One side's captures, normalized with a per-side `Normalizer` in a fixed
/// order so id/timestamp counters align across the two sides.
struct Normalized {
    session: serde_json::Value,
    store: serde_json::Value,
    events: Vec<serde_json::Value>,
    types: Vec<String>,
    control_plane: std::collections::BTreeMap<String, usize>,
}

fn normalize_side(capture: &Capture) -> Normalized {
    let mut normalizer = Normalizer::new();
    let session = normalizer.normalize(&capture.project, &capture.session);
    let store = normalizer.normalize(&capture.project, &capture.store);
    let events = normalize_events(&mut normalizer, &capture.project, &capture.events);
    let scoped = drop_control_plane(&events);
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
    let types = event_types(&sort_events(scoped));
    Normalized {
        session,
        store,
        events,
        types,
        control_plane,
    }
}

#[tokio::test]
async fn p1_file_mutation_parity() {
    if !gated() {
        return;
    }
    let ts_source = TsSource::resolve();
    let backend_rs = MockBackend::new(scenarios::A1_FILE_MUTATION);
    let backend_ts = MockBackend::new(scenarios::A1_FILE_MUTATION);
    let rs_capture = scenarios::a1_rust(&backend_rs).await;
    let ts_capture = scenarios::a1_ts(&backend_ts, &ts_source).await;

    let report = Report::open("p1_file_mutation");
    for (side, capture) in [("ts", &ts_capture), ("rs", &rs_capture)] {
        report.write_raw(side, "session", &capture.session);
        report.write_raw(side, "store", &capture.store);
        report.write_raw(side, "requests", &serde_json::json!(capture.requests));
        report.write_raw(side, "events", &serde_json::json!(capture.events));
    }

    let ts = normalize_side(&ts_capture);
    let rs = normalize_side(&rs_capture);
    report.write_events("ts", &ts.events);
    report.write_events("rs", &rs.events);

    // The driving-call responses must diff clean under triage.
    let session = triage(
        "p1_file_mutation",
        diff("$.session", &ts.session, &rs.session),
    );
    let store = triage("p1_file_mutation", diff("$.store", &ts.store, &rs.store));
    let all = report.write_scenario(
        "p1_file_mutation",
        &[("session", session), ("store", store)],
    );
    assert!(
        all.is_empty(),
        "{} un-allowlisted deviation(s): {all:#?}",
        all.len()
    );

    // The final file bytes must match the scripted tool call, and both
    // sides drive the scripted two-step agent loop.
    let scripted = backend_ts
        .transcript(scenarios::A1_FILE_MUTATION)
        .expect("mock transcript")
        .tool_argument("cal_1", "content")
        .expect("scripted content");
    for (side, capture) in [("ts", &ts_capture), ("rs", &rs_capture)] {
        let content = serde_json::Value::String(capture.file.clone().unwrap_or_default());
        assert_eq!(
            content, scripted,
            "{side} notes.md must match the scripted bytes"
        );
    }
    assert!(backend_rs
        .request(0)
        .to_string()
        .contains("create notes.md for me"));
    assert!(backend_ts
        .request(0)
        .to_string()
        .contains("create notes.md for me"));
    assert_eq!(rs_capture.requests.len(), 2, "rs agent requests");
    assert_eq!(ts_capture.requests.len(), 2, "ts agent requests");

    // The SSE stream: the session-scoped event-type multiset must agree
    // (chunk 3 adds the strict arrival-order comparison). Instance
    // control-plane startup events that only one side publishes are
    // recorded in the report as chunk-3 triage findings.
    if !ts.control_plane.is_empty() || !rs.control_plane.is_empty() {
        report.append_findings(&format!(
            "## Instance control-plane events (chunk-3 triage)\n\n- ts: {:?}\n- rs: {:?}\n",
            ts.control_plane, rs.control_plane
        ));
    }
    assert_eq!(ts.types, rs.types, "event-type streams diverged");
}

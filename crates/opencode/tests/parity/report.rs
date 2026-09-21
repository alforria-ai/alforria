//! Parity report writer (spec PARITY §6.1) — chunk-1 skeleton: one
//! directory per test run under `target/parity/` (redirect with
//! `OPENCODE_PARITY_REPORT`), with `parity.json` (machine) + `parity.md`
//! (human) plus the raw pre-normalization captures of both sides.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::differ::Deviation;
use crate::normal::sort_events;

pub struct Report {
    dir: PathBuf,
}

/// One deviation record trimmed for the JSON report.
fn record(deviation: &Deviation) -> Value {
    let trim = |value: &Value| -> Value {
        let text = serde_json::to_string(value).unwrap_or_default();
        if text.len() <= 300 {
            value.clone()
        } else {
            json!(format!("{}…", text.chars().take(300).collect::<String>()))
        }
    };
    json!({
        "kind": if deviation.kind == crate::differ::Kind::Structural {
            "structural"
        } else {
            "value"
        },
        "path": deviation.path,
        "ts": trim(&deviation.ts),
        "rs": trim(&deviation.rs),
    })
}

impl Report {
    pub fn open(name: &str) -> Report {
        let workspace = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
        let base = std::env::var("OPENCODE_PARITY_REPORT")
            .unwrap_or_else(|_| format!("{workspace}/target/parity"));
        let run = std::env::var("OPENCODE_PARITY_RUN").unwrap_or_else(|_| run_id());
        let dir = PathBuf::from(base).join(run).join(name);
        std::fs::create_dir_all(&dir).expect("create report dir");
        Report { dir }
    }

    /// Write a raw (pre-normalization) capture artifact for one side.
    pub fn write_raw(&self, side: &str, name: &str, value: &Value) {
        let dir = self.dir.join(side);
        std::fs::create_dir_all(&dir).expect("create side dir");
        let path = dir.join(format!("{name}.json"));
        std::fs::write(
            &path,
            serde_json::to_string_pretty(value).expect("serialize capture"),
        )
        .expect("write capture");
    }

    /// Write normalized event streams (post-normalization) for diffing by
    /// hand.
    pub fn write_events(&self, side: &str, events: &[Value]) {
        self.write_raw(
            "normalized",
            &format!("{side}_events"),
            &Value::Array(sort_events(events.to_vec())),
        );
    }

    /// Append a findings section to the human report.
    pub fn append_findings(&self, section: &str) {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(self.dir.join("parity.md"))
            .expect("open parity.md");
        writeln!(file, "\n{section}").expect("append findings");
    }

    /// Write `parity.json` + `parity.md` for one scenario comparison.
    pub fn write_scenario(
        &self,
        name: &str,
        captures: &[(&'static str, Vec<Deviation>)],
    ) -> Vec<Deviation> {
        let mut all = Vec::new();
        let mut summary = Vec::new();
        for (capture, deviations) in captures {
            all.extend(deviations.iter().cloned());
            summary.push(json!({
                "capture": capture,
                "structural": deviations
                    .iter()
                    .filter(|d| d.kind == crate::differ::Kind::Structural)
                    .count(),
                "value": deviations
                    .iter()
                    .filter(|d| d.kind == crate::differ::Kind::Value)
                    .count(),
            }));
        }
        let json_path = self.dir.join("parity.json");
        std::fs::write(
            &json_path,
            serde_json::to_string_pretty(&json!({
                "scenario": name,
                "captures": summary,
                "deviations": all.iter().map(record).collect::<Vec<_>>(),
            }))
            .expect("serialize report"),
        )
        .expect("write parity.json");

        let mut md = format!(
            "# Parity report — {name}\n\n| capture | structural | value |\n|---|---|---|\n"
        );
        for (capture, deviations) in captures {
            let structural = deviations
                .iter()
                .filter(|d| d.kind == crate::differ::Kind::Structural)
                .count();
            let value = deviations
                .iter()
                .filter(|d| d.kind == crate::differ::Kind::Value)
                .count();
            md.push_str(&format!("| {capture} | {structural} | {value} |\n"));
        }
        for deviation in &all {
            md.push_str(&format!(
                "\n## `{kind}` {path}\n\n- ts: `{ts}`\n- rs: `{rs}`\n",
                kind = if deviation.kind == crate::differ::Kind::Structural {
                    "structural"
                } else {
                    "value"
                },
                path = deviation.path,
                ts = serde_json::to_string(&deviation.ts).unwrap_or_default(),
                rs = serde_json::to_string(&deviation.rs).unwrap_or_default(),
            ));
        }
        std::fs::write(self.dir.join("parity.md"), md).expect("write parity.md");
        all
    }
}

fn run_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_millis();
    format!("{millis}-{}", std::process::id())
}

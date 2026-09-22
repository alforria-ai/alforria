//! API-surface extraction from the live served docs (spec PARITY §4):
//! route inventory, event-type inventory, and the OpenAPI documents both
//! binaries serve.
//!
//! - v1 (`GET /doc`): served by both binaries.
//! - v2 (`GET /openapi.json` on Rust): TS's `opencode serve` does not
//!   serve the v2 document over HTTP (it mounts the UI catch-all there;
//!   only the embedded web server exposes it at `/openapi.json`), so the
//!   TS side is derived live from the same source the embedded server
//!   uses — `OpenApi.fromApi(Api)` (`packages/server/src/routes.ts:54`)
//!   — via `bun -e`.

use std::collections::BTreeSet;
use std::process::Command;

use serde_json::Value;

use crate::parity_harness::TsSource;

const METHODS: &[&str] = &["get", "post", "put", "delete", "patch"];

/// `GET <path>` — one HTTP fetch parsed as JSON.
pub async fn fetch_json(port: u16, path: &str) -> Value {
    let url = format!("http://127.0.0.1:{port}{path}");
    let response = reqwest::get(&url)
        .await
        .unwrap_or_else(|err| panic!("GET {url} failed: {err}"));
    assert!(
        response.status().is_success(),
        "GET {url} failed: {}",
        response.status()
    );
    response
        .json()
        .await
        .unwrap_or_else(|err| panic!("GET {url} body: {err}"))
}

/// The `METHOD path` inventory of an OpenAPI document.
pub fn route_inventory(doc: &Value) -> BTreeSet<String> {
    let mut routes = BTreeSet::new();
    for (path, item) in doc["paths"].as_object().expect("paths") {
        for (method, _) in item.as_object().expect("operations") {
            if METHODS.contains(&method.as_str()) {
                routes.insert(format!("{} {}", method.to_uppercase(), path));
            }
        }
    }
    routes
}

/// The event `type` strings of one event union component (`Event` — 89
/// legacy types, `V2Event` — 88 V2 types) of an OpenAPI document.
pub fn event_types(doc: &Value, union: &str) -> BTreeSet<String> {
    let components = doc["components"]["schemas"]
        .as_object()
        .unwrap_or_else(|| panic!("components in {union} doc"));
    let schema = components
        .get(union)
        .unwrap_or_else(|| panic!("union {union} missing"))
        .as_object()
        .expect("union schema");
    let refs = schema
        .get("anyOf")
        .and_then(|any| any.as_array())
        .unwrap_or_else(|| panic!("{union} anyOf"));
    refs.iter()
        .filter_map(|member| {
            let name = member["$ref"].as_str()?.rsplit('/').next()?.to_string();
            let schema = components.get(&name)?;
            Some(
                schema["properties"]["type"]["enum"][0]
                    .as_str()
                    .expect("event type")
                    .to_string(),
            )
        })
        .collect()
}

/// Dump the TS v2 OpenAPI document live from the pinned source. The
/// document is written to a file, not stdout: bun truncates large
/// single `console.log` writes piped to a parent process at the 64 KiB
/// pipe buffer.
pub fn ts_v2_doc(ts: &TsSource) -> Value {
    let out = tempfile::NamedTempFile::new().expect("temp file for the v2 doc dump");
    let script = r#"const {Api} = await import("@opencode-ai/server/api");
const {OpenApi} = await import("effect/unstable/httpapi");
const fs = await import("node:fs");
fs.writeFileSync(process.env.OUT_PATH, JSON.stringify(OpenApi.fromApi(Api)));"#;
    let cwd = ts.root.join("packages/opencode");
    let output = Command::new("bun")
        .arg("-e")
        .arg(script)
        .env("OUT_PATH", out.path())
        .current_dir(&cwd)
        .output()
        .expect("spawn bun -e");
    assert!(
        output.status.success(),
        "bun -e v2 doc dump failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_reader(std::io::BufReader::new(out)).expect("v2 doc dump body")
}

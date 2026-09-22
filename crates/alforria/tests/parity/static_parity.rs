//! P0 — static surface parity (spec PARITY §4): route inventory,
//! OpenAPI schema shape, and event-type inventory, live TS vs the Rust
//! binary. Also the fast-fail gate for the more expensive scenario work.

use crate::catalog;
use crate::differ::Deviation;
use crate::harness::Env;
use crate::parity_harness::{gated, golden_mode, TsServe, TsSource};
use crate::report::Report;

use crate::harness::Serve;

struct Surface {
    ts_v1: serde_json::Value,
    ts_v2: serde_json::Value,
    rs_v1: serde_json::Value,
    rs_v2: serde_json::Value,
}

/// Live docs from both binaries: TS `GET /doc` + the TS v2 document
/// derived from the pinned source, Rust `GET /doc` + `GET /openapi.json`.
async fn surface() -> Surface {
    let ts_source = TsSource::resolve();
    let ts_env = Env::new("p0-ts");
    let ts_serve = TsServe::spawn(&ts_source, &ts_env);
    let rs_env = Env::new("p0-rs");
    let rs_serve = Serve::spawn(&rs_env);
    Surface {
        ts_v1: catalog::fetch_json(ts_serve.port, "/doc").await,
        ts_v2: catalog::ts_v2_doc(&ts_source),
        rs_v1: catalog::fetch_json(rs_serve.port, "/doc").await,
        rs_v2: catalog::fetch_json(rs_serve.port, "/openapi.json").await,
    }
}

#[tokio::test]
async fn p0_route_inventory_parity() {
    // P0 compares live TS docs: no golden replay exists for it, so the
    // golden mode (no TS clone) skips like the un-gated default run.
    if !gated() || golden_mode() {
        return;
    }
    let surface = surface().await;
    let ts_v1 = catalog::route_inventory(&surface.ts_v1);
    let rs_v1 = catalog::route_inventory(&surface.rs_v1);
    assert_eq!(ts_v1, rs_v1, "v1 route inventory diverged");

    let ts_v2 = catalog::route_inventory(&surface.ts_v2);
    let rs_v2 = catalog::route_inventory(&surface.rs_v2);
    assert_eq!(ts_v2, rs_v2, "v2 route inventory diverged");
}

#[tokio::test]
async fn p0_openapi_schema_shape_parity() {
    // P0 compares live TS docs: no golden replay exists for it, so the
    // golden mode (no TS clone) skips like the un-gated default run.
    if !gated() || golden_mode() {
        return;
    }
    let report = Report::open("p0_openapi_schema_shape");
    let surface = surface().await;

    let v1 = crate::differ::diff("$.v1", &surface.ts_v1, &surface.rs_v1);
    let v2 = crate::differ::diff("$.v2", &surface.ts_v2, &surface.rs_v2);
    let captures: Vec<(&str, Vec<Deviation>)> = vec![("v1", v1), ("v2", v2)];
    let all = report.write_scenario("p0_openapi_schema_shape", &captures);
    assert!(
        all.is_empty(),
        "{} schema-shape deviation(s): {all:#?}",
        all.len()
    );
}

#[tokio::test]
async fn p0_event_type_inventory_parity() {
    // P0 compares live TS docs: no golden replay exists for it, so the
    // golden mode (no TS clone) skips like the un-gated default run.
    if !gated() || golden_mode() {
        return;
    }
    let surface = surface().await;
    for (union, expected) in [("Event", 89), ("V2Event", 88)] {
        let ts = catalog::event_types(&surface.ts_v1, union);
        let rs = catalog::event_types(&surface.rs_v1, union);
        assert_eq!(
            ts.len(),
            expected,
            "{union} event-type inventory has unexpected size"
        );
        assert_eq!(
            ts, rs,
            "{union} event-type inventory diverged:\nts={ts:?}\nrs={rs:?}"
        );
    }
}

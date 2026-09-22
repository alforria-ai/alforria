//! Wire-conformance suite for the OpenAPI export (spec M6.9).
//!
//! Locks `GET /doc` and `GET /openapi.json` to the frozen TS documents and
//! cross-checks both route tables against the documents' operation sets:
//!
//! - endpoint bytes must equal the captured goldens under `tests/golden/`;
//! - the `/doc` golden must equal the frozen fixture
//!   (`fixtures/openapi/openapi.json`) minus the SDK generator's
//!   `x-codeSamples` annotations;
//! - every registered route must be documented and vice versa.

use std::collections::HashSet;
use std::path::Path;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use alforria_server::routes;
use alforria_server::state::ServerContext;

const METHODS: &[&str] = &["get", "post", "put", "delete", "patch"];

fn router() -> axum::Router {
    routes::build_router(Arc::new(ServerContext::for_tests()))
}

async fn body(response: axum::http::Response<Body>) -> Vec<u8> {
    axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec()
}

fn read(rel: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read(&path).unwrap_or_else(|e| panic!("failed to read {path:?}: {e}"))
}

fn golden_doc() -> Vec<u8> {
    read("tests/golden/doc.json")
}

fn golden_openapi_json() -> Vec<u8> {
    read("tests/golden/openapi.json")
}

fn parse(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).expect("the frozen document is valid JSON")
}

/// The docs spell the filesystem wildcard `*` where the axum route table
/// uses a trailing `{*path}` parameter (`/api/fs/read/*`).
fn doc_spelling(path: &str) -> String {
    path.replace("/{*path}", "/*")
}

/// Every `(METHOD, path)` operation a document carries.
fn operations(doc: &Value) -> HashSet<(String, String)> {
    let mut ops = HashSet::new();
    for (path, item) in doc["paths"].as_object().expect("paths object") {
        for method in METHODS {
            if item.get(method).is_some() {
                ops.insert((method.to_uppercase(), path.clone()));
            }
        }
    }
    ops
}

/// Every registered `(METHOD, path)` pair across the given route tables,
/// doc-spelled.
fn registered(tables: &[&[(&'static str, &'static str)]]) -> HashSet<(String, String)> {
    tables
        .iter()
        .flat_map(|table| table.iter())
        .map(|(method, path)| (method.to_string(), doc_spelling(path)))
        .collect()
}

#[tokio::test]
async fn doc_endpoint_serves_the_frozen_ts_document() {
    let response = router()
        .oneshot(Request::builder().uri("/doc").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-type"],
        "application/json",
        "`HttpServerResponse.jsonUnsafe` content type"
    );
    let served = body(response).await;
    let golden = golden_doc();
    assert_eq!(
        served.len(),
        golden.len(),
        "GET /doc must serve the frozen document byte for byte"
    );
    assert_eq!(served, golden);
}

#[tokio::test]
async fn openapi_json_endpoint_serves_the_frozen_ts_document() {
    let response = router()
        .oneshot(
            Request::builder()
                .uri("/openapi.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "application/json");
    let served = body(response).await;
    let golden = golden_openapi_json();
    assert_eq!(
        served.len(),
        golden.len(),
        "GET /openapi.json must serve the frozen document byte for byte"
    );
    assert_eq!(served, golden);
}

#[test]
fn doc_conforms_to_the_frozen_fixture() {
    // `fixtures/openapi/openapi.json` is the SDK generator's output: the
    // `/doc` response plus `x-codeSamples` injected per operation
    // (`cli/cmd/generate.ts:16-31`).
    let mut fixture = parse(&read("../../fixtures/openapi/openapi.json"));
    if let Some(paths) = fixture
        .get_mut("paths")
        .and_then(|paths| paths.as_object_mut())
    {
        for item in paths.values_mut() {
            for method in METHODS {
                if let Some(operation) = item.get_mut(*method).and_then(Value::as_object_mut) {
                    operation.remove("x-codeSamples");
                }
            }
        }
    }
    let golden = parse(&golden_doc());
    assert_eq!(
        golden, fixture,
        "the /doc golden drifted from the frozen fixture"
    );
}

fn v1_doc() -> Value {
    parse(&golden_doc())
}

fn v2_doc() -> Value {
    parse(&golden_openapi_json())
}

#[test]
fn every_v1_route_is_documented() {
    let doc = v1_doc();
    let documented = operations(&doc);
    for (method, path) in routes::v1::ROUTES {
        let found = documented.contains(&((*method).to_string(), doc_spelling(path)));
        assert!(
            found,
            "v1 route {method} {path} is missing from the /doc document"
        );
    }
}

#[test]
fn every_v2_route_is_documented() {
    let doc = v2_doc();
    let documented = operations(&doc);
    for (method, path) in routes::v2::ROUTES {
        let found = documented.contains(&((*method).to_string(), doc_spelling(path)));
        assert!(
            found,
            "v2 route {method} {path} is missing from the /openapi.json document"
        );
    }
}

#[test]
fn every_documented_v1_operation_is_registered() {
    let doc = v1_doc();
    // The v1 document carries the unprefixed project-copy operations in the
    // v2 route table (spec §2.1) and a `/api` subset of the v2 surface.
    let registered = registered(&[routes::v1::ROUTES, routes::v2::ROUTES]);
    let documented = operations(&doc);
    assert_eq!(documented.len(), 188);
    for (method, path) in documented {
        let found = registered.contains(&(method.clone(), path.clone()));
        assert!(
            found,
            "documented v1 operation {method} {path} is not registered"
        );
    }
}

#[test]
fn every_documented_v2_operation_is_registered() {
    let doc = v2_doc();
    let registered = registered(&[routes::v2::ROUTES]);
    let documented = operations(&doc);
    assert_eq!(documented.len(), 61);
    for (method, path) in documented {
        let found = registered.contains(&(method.clone(), path.clone()));
        assert!(
            found,
            "documented v2 operation {method} {path} is not registered"
        );
    }
}

#[test]
fn operation_ids_are_unique() {
    for (name, doc) in [("v1", v1_doc()), ("v2", v2_doc())] {
        let mut ids = HashSet::new();
        for (path, item) in doc["paths"].as_object().expect("paths object") {
            for method in METHODS {
                let Some(operation) = item.get(method) else {
                    continue;
                };
                let id = operation
                    .get("operationId")
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| panic!("{name} doc {method} {path} lacks an operationId"));
                assert!(
                    ids.insert(id.to_string()),
                    "duplicate operationId {id} in the {name} doc"
                );
            }
        }
    }
}

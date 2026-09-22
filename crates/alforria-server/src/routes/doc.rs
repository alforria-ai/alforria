//! `GET /doc` (v1 OpenAPI) and `GET /openapi.json` (v2 OpenAPI).

use axum::body::Body;
use axum::http::{header, StatusCode};
use axum::response::Response;

/// 200 with `application/json`, mirroring `HttpServerResponse.jsonUnsafe`.
fn serve(body: &'static str) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("static response parts are valid")
}

/// The v1 document: `OpenApi.fromApi(PublicApi)` with the legacy
/// normalization transforms (`httpapi/server.ts:188-192`).
pub async fn doc() -> Response {
    serve(crate::openapi::V1_DOC)
}

/// The v2 document: `OpenApi.fromApi(Api)` registered bare on the router
/// (`HttpApiBuilder.ts:100-103`) — outside any auth middleware.
pub async fn openapi_json() -> Response {
    serve(crate::openapi::V2_DOC)
}

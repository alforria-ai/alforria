//! Frozen OpenAPI documents served by `GET /doc` and `GET /openapi.json`
//! (spec M6.9).
//!
//! Both TS documents are pure functions of the pinned commit's route tables
//! (`OpenApi.fromApi`), so the exact wire bytes — `JSON.stringify` output,
//! minified, no trailing newline — were captured from the reference:
//!
//! - `/doc`: `OpenApi.fromApi(PublicApi)` including the legacy normalization
//!   transforms (`httpapi/server.ts:188-192`, `httpapi/public.ts:530-537`).
//! - `/openapi.json`: `OpenApi.fromApi(Api)` from the v2 builder
//!   (`packages/server/src/routes.ts:54`, `HttpApiBuilder.ts:100-103`).
//!
//! The byte-identical goldens under `tests/golden/` are locked against the
//! frozen fixture by the conformance suite (`tests/openapi_conformance.rs`).

pub const V1_DOC: &str = include_str!("openapi/v1-doc.json");
pub const V2_DOC: &str = include_str!("openapi/v2-doc.json");

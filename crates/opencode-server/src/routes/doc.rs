//! `GET /doc` (v1 OpenAPI) and `GET /openapi.json` (v2 OpenAPI).

// TODO(M6.9): serve the legacy `PublicApi` document here
// (`OpenApi.fromApi(PublicApi)` with the `matchLegacyOpenApi` transforms,
// `httpapi/public.ts:530-537`) and the v2 document from
// `HttpApiBuilder.layer(Api, {openapiPath: "/openapi.json"})`.
pub async fn doc() -> axum::response::Response {
    crate::error::defect_response()
}

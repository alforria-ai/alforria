//! CORS origin allowlist + `vary` handling — port of `packages/server/src/cors.ts`,
//! effect's `HttpMiddleware.cors` (as configured by `server.ts:121-128`) and
//! the opencode `corsVaryFix` (`middleware/cors-vary.ts`).
//!
//! opencode configures effect's cors middleware with a predicate
//! `allowedOrigins`, `maxAge: 86_400`, and every other option left at its
//! default (`allowedMethods: ["GET", "HEAD", "PUT", "PATCH", "POST", "DELETE"]`,
//! `credentials: false`, `allowedHeaders: []`).

use std::sync::LazyLock;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::{HeaderValue, Method, Response};
use futures::future::BoxFuture;
use regex::Regex;
use tower::Service;

static OPENCODE_ORIGIN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^https://([a-z0-9-]+\.)*opencode\.ai$").unwrap());

/// `isAllowedCorsOrigin` (`cors.ts:11-19`).
pub fn is_allowed_cors_origin(input: Option<&str>, opts: &[String]) -> bool {
    let Some(input) = input else { return true };
    if input.starts_with("http://localhost:") {
        return true;
    }
    if input.starts_with("http://127.0.0.1:") {
        return true;
    }
    if input.starts_with("oc://renderer") {
        return true;
    }
    if input == "tauri://localhost"
        || input == "http://tauri.localhost"
        || input == "https://tauri.localhost"
    {
        return true;
    }
    if OPENCODE_ORIGIN.is_match(input) {
        return true;
    }
    opts.iter().any(|origin| origin == input)
}

/// `isAllowedRequestOrigin` (`cors.ts:22-31`): a same-host origin passes
/// even when not allowlisted.
pub fn is_allowed_request_origin(input: Option<&str>, host: Option<&str>, opts: &[String]) -> bool {
    // TS `if (!input) return true` — an empty Origin header is falsy too.
    let Some(input) = input.filter(|input| !input.is_empty()) else {
        return true;
    };
    if let Some(host) = host {
        if same_host(input, host) {
            return true;
        }
    }
    is_allowed_cors_origin(Some(input), opts)
}

/// `sameHost` (`cors.ts:33-38`) — `new URL(origin).host === host`. The port
/// must be present on both sides; `URL.host` keeps an explicit port and
/// drops a default one.
fn same_host(origin: &str, host: &str) -> bool {
    let Some(rest) = origin.split_once("://") else {
        return false;
    };
    if rest.0.is_empty() || rest.0.contains(' ') {
        return false;
    }
    let Some(authority) = rest.1.split(['/', '?', '#']).next() else {
        return false;
    };
    let origin_host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    origin_host == host
}

/// Request-header form of [`is_allowed_request_origin`].
pub fn request_origin_allowed(headers: &axum::http::HeaderMap, opts: &[String]) -> bool {
    let origin = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|value| value.to_str().ok());
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok());
    is_allowed_request_origin(origin, host, opts)
}

/// effect `compressionInternal.varyWith` (`internal/compression.ts:9-15`).
pub fn vary_with(vary: Option<&HeaderValue>, dimension: &str) -> String {
    match vary {
        None => dimension.to_string(),
        Some(vary) => {
            let vary = vary.to_str().unwrap_or_default();
            let members: Vec<String> = vary.split(',').map(|m| m.trim().to_lowercase()).collect();
            if members
                .iter()
                .any(|m| m == "*" || m.eq_ignore_ascii_case(dimension))
            {
                vary.to_string()
            } else {
                format!("{vary}, {dimension}")
            }
        }
    }
}

/// `vary: Origin` merging for outgoing responses.
fn merge_vary_origin(headers: &mut axum::http::HeaderMap) {
    let merged = vary_with(headers.get("vary"), "Origin");
    if let Ok(value) = HeaderValue::from_str(&merged) {
        headers.insert(axum::http::header::VARY, value);
    }
}

/// The effect cors middleware with opencode's options. Non-`OPTIONS` requests
/// get `access-control-allow-origin` (dynamic echo) plus merged `vary: Origin`
/// headers; `OPTIONS` requests short-circuit into an empty 204 preflight
/// response.
#[derive(Clone)]
pub struct CorsLayer {
    allowed_origins: Vec<String>,
}

impl CorsLayer {
    pub fn new(allowed_origins: Vec<String>) -> CorsLayer {
        CorsLayer { allowed_origins }
    }
}

impl<S> tower::Layer<S> for CorsLayer {
    type Service = CorsService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        CorsService {
            inner,
            allowed_origins: self.allowed_origins.clone(),
        }
    }
}

#[derive(Clone)]
pub struct CorsService<S> {
    inner: S,
    allowed_origins: Vec<String>,
}

impl<S> Service<axum::http::Request<Body>> for CorsService<S>
where
    S: Service<axum::http::Request<Body>, Response = Response<Body>> + Clone + Send + 'static,
    S::Future: Send,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: axum::http::Request<Body>) -> Self::Future {
        let origin = req
            .headers()
            .get("origin")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.to_string());
        let allowed = is_allowed_cors_origin(origin.as_deref(), &self.allowed_origins);

        if req.method() == Method::OPTIONS {
            let mut response = Response::builder()
                .status(204)
                .body(Body::empty())
                .expect("static response parts are valid");
            let headers = response.headers_mut();
            if let Some(origin) = &origin {
                if allowed {
                    headers.insert(
                        "access-control-allow-origin",
                        HeaderValue::from_str(origin).expect("origin header is ascii"),
                    );
                }
            }
            headers.insert(
                "access-control-allow-methods",
                HeaderValue::from_static("GET, HEAD, PUT, PATCH, POST, DELETE"),
            );
            headers.insert("access-control-max-age", HeaderValue::from_static("86400"));
            if let Some(acrh) = req
                .headers()
                .get("access-control-request-headers")
                .and_then(|v| v.to_str().ok())
            {
                let vary = vary_with(
                    Some(&HeaderValue::from_str("Origin").expect("constant header value is valid")),
                    "Access-Control-Request-Headers",
                );
                headers.insert("vary", HeaderValue::from_str(&vary).unwrap());
                headers.insert(
                    "access-control-allow-headers",
                    HeaderValue::from_str(acrh).expect("request header values are ascii"),
                );
            } else {
                headers.insert("vary", HeaderValue::from_static("Origin"));
            }
            return Box::pin(async move { Ok(response) });
        }

        // Cross-site request forgery guard. Browsers attach `Origin` to every
        // cross-site POST/PUT/PATCH/DELETE, including "simple" form posts that
        // skip preflight, and the server parses bodies as JSON whatever their
        // content type. Without a password such a request would reach
        // state-changing routes (`POST /pty` spawns a process). Same-host,
        // localhost and allowlisted origins pass, as do clients that send no
        // `Origin` at all (CLI, TUI, SDKs).
        if !matches!(*req.method(), Method::GET | Method::HEAD)
            && !request_origin_allowed(req.headers(), &self.allowed_origins)
        {
            use axum::response::IntoResponse;
            let response = crate::error::ServerError::from(crate::error::ApiError::Forbidden {
                message: "Cross-origin request refused".to_string(),
            })
            .into_response();
            return Box::pin(async move { Ok(response) });
        }

        let mut inner = self.inner.clone();
        Box::pin(async move {
            let mut response = inner.call(req).await?;
            let headers = response.headers_mut();
            if let Some(origin) = &origin {
                if allowed {
                    headers.insert(
                        "access-control-allow-origin",
                        HeaderValue::from_str(origin).expect("origin header is ascii"),
                    );
                }
            }
            merge_vary_origin(headers);
            Ok(response)
        })
    }
}

/// The opencode `corsVaryFix` middleware (`middleware/cors-vary.ts:13-29`):
/// when a dynamic (non-`*`) `access-control-allow-origin` is set, guarantee
/// `vary` includes `Origin`.
#[derive(Clone, Default)]
pub struct CorsVaryLayer;

impl<S> tower::Layer<S> for CorsVaryLayer {
    type Service = CorsVaryService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        CorsVaryService { inner }
    }
}

#[derive(Clone)]
pub struct CorsVaryService<S> {
    inner: S,
}

impl<S> Service<axum::http::Request<Body>> for CorsVaryService<S>
where
    S: Service<axum::http::Request<Body>, Response = Response<Body>> + Clone + Send + 'static,
    S::Future: Send,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: axum::http::Request<Body>) -> Self::Future {
        let mut inner = self.inner.clone();
        Box::pin(async move {
            let mut response = inner.call(req).await?;
            let allow_origin = response
                .headers()
                .get("access-control-allow-origin")
                .and_then(|v| v.to_str().ok())
                .map(|v| v.to_string());
            if allow_origin.as_deref() == Some("*") || allow_origin.is_none() {
                return Ok(response);
            }
            let vary = response
                .headers()
                .get("vary")
                .and_then(|v| v.to_str().ok())
                .map(|v| v.to_string());
            match vary {
                None => {
                    response
                        .headers_mut()
                        .insert("vary", HeaderValue::from_static("Origin"));
                }
                Some(vary) => {
                    let tokens: Vec<String> =
                        vary.split(',').map(|s| s.trim().to_lowercase()).collect();
                    if tokens.iter().any(|t| t == "origin") || tokens.iter().any(|t| t == "*") {
                        return Ok(response);
                    }
                    response.headers_mut().insert(
                        "vary",
                        HeaderValue::from_str(&format!("{vary}, Origin")).unwrap(),
                    );
                }
            }
            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::Layer;
    use tower::ServiceExt;

    /// A state-changing request from a foreign site never reaches the route;
    /// reads, same-host, localhost and origin-less clients do.
    #[tokio::test]
    async fn cross_site_writes_are_refused() {
        let reached = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = reached.clone();
        let inner = tower::service_fn(move |_req: axum::http::Request<Body>| {
            let seen = seen.clone();
            async move {
                seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok::<_, std::convert::Infallible>(Response::new(Body::empty()))
            }
        });
        let svc = CorsLayer::new(vec![]).layer(inner);
        let send = |method: &str, origin: Option<&str>| {
            let mut req = axum::http::Request::builder()
                .method(method)
                .uri("/pty")
                .header("host", "127.0.0.1:4096");
            if let Some(origin) = origin {
                req = req.header("origin", origin);
            }
            svc.clone().oneshot(req.body(Body::empty()).unwrap())
        };
        let status = |r: Response<Body>| r.status().as_u16();

        assert_eq!(
            status(send("POST", Some("https://evil.example")).await.unwrap()),
            403
        );
        assert_eq!(status(send("DELETE", Some("null")).await.unwrap()), 403);
        assert_eq!(reached.load(std::sync::atomic::Ordering::SeqCst), 0);

        assert_eq!(status(send("POST", None).await.unwrap()), 200);
        assert_eq!(
            status(send("POST", Some("http://127.0.0.1:4096")).await.unwrap()),
            200
        );
        assert_eq!(
            status(send("PATCH", Some("http://localhost:5173")).await.unwrap()),
            200
        );
        assert_eq!(
            status(send("GET", Some("https://evil.example")).await.unwrap()),
            200
        );
        assert_eq!(reached.load(std::sync::atomic::Ordering::SeqCst), 4);
    }

    #[test]
    fn allowed_origin_matrix() {
        let none: Option<&str> = None;
        assert!(is_allowed_cors_origin(none, &[]));
        assert!(is_allowed_cors_origin(Some("http://localhost:3000"), &[]));
        assert!(is_allowed_cors_origin(Some("http://127.0.0.1:8080"), &[]));
        assert!(is_allowed_cors_origin(Some("oc://renderer"), &[]));
        assert!(is_allowed_cors_origin(Some("tauri://localhost"), &[]));
        assert!(is_allowed_cors_origin(Some("http://tauri.localhost"), &[]));
        assert!(is_allowed_cors_origin(Some("https://tauri.localhost"), &[]));
        assert!(is_allowed_cors_origin(Some("https://opencode.ai"), &[]));
        assert!(is_allowed_cors_origin(Some("https://foo.opencode.ai"), &[]));
        assert!(is_allowed_cors_origin(Some("https://app.opencode.ai"), &[]));
        assert!(!is_allowed_cors_origin(Some("https://evil.com"), &[]));
        assert!(!is_allowed_cors_origin(
            Some("https://opencode.ai.evil.com"),
            &[]
        ));
        assert!(!is_allowed_cors_origin(
            Some("https://opencode-ai.com"),
            &[]
        ));
        assert!(is_allowed_cors_origin(
            Some("https://evil.com"),
            &["https://evil.com".to_string()]
        ));
    }

    #[tokio::test]
    async fn non_options_sets_allow_origin_and_vary() {
        let service = CorsLayer::new(vec![]).layer(tower::service_fn(|_| async {
            Ok::<_, std::convert::Infallible>(Response::new(Body::empty()))
        }));
        let response = service
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/session")
                    .header("origin", "http://localhost:3000")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.headers()["access-control-allow-origin"],
            "http://localhost:3000"
        );
        assert_eq!(response.headers()["vary"], "Origin");
    }

    #[tokio::test]
    async fn disallowed_origin_gets_no_allow_origin() {
        let service = CorsLayer::new(vec![]).layer(tower::service_fn(|_| async {
            Ok::<_, std::convert::Infallible>(Response::new(Body::empty()))
        }));
        let response = service
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/session")
                    .header("origin", "https://evil.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(response
            .headers()
            .get("access-control-allow-origin")
            .is_none());
        assert_eq!(response.headers()["vary"], "Origin");
    }

    #[tokio::test]
    async fn preflight_short_circuits_with_204() {
        let service = CorsLayer::new(vec![]).layer(tower::service_fn(|_| async {
            Ok::<_, std::convert::Infallible>(Response::new(Body::empty()))
        }));
        let response = service
            .oneshot(
                axum::http::Request::builder()
                    .method("OPTIONS")
                    .uri("/session")
                    .header("origin", "http://localhost:3000")
                    .header("access-control-request-method", "GET")
                    .header("access-control-request-headers", "content-type")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 204);
        let (parts, body) = response.into_parts();
        assert!(axum::body::to_bytes(body, usize::MAX)
            .await
            .unwrap()
            .is_empty());
        let response = axum::http::Response::from_parts(parts, Body::empty());
        assert_eq!(
            response.headers()["access-control-allow-origin"],
            "http://localhost:3000"
        );
        assert_eq!(
            response.headers()["access-control-allow-methods"],
            "GET, HEAD, PUT, PATCH, POST, DELETE"
        );
        assert_eq!(response.headers()["access-control-max-age"], "86400");
        assert_eq!(
            response.headers()["access-control-allow-headers"],
            "content-type"
        );
        assert_eq!(
            response.headers()["vary"],
            "Origin, Access-Control-Request-Headers"
        );
    }

    #[tokio::test]
    async fn preflight_without_request_headers_has_plain_vary() {
        let service = CorsLayer::new(vec![]).layer(tower::service_fn(|_| async {
            Ok::<_, std::convert::Infallible>(Response::new(Body::empty()))
        }));
        let response = service
            .oneshot(
                axum::http::Request::builder()
                    .method("OPTIONS")
                    .uri("/session")
                    .header("origin", "http://localhost:3000")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 204);
        assert_eq!(response.headers()["vary"], "Origin");
    }

    #[tokio::test]
    async fn cors_vary_fix_appends_origin() {
        let service = CorsVaryLayer.layer(tower::service_fn(|_| async {
            let mut response = Response::new(Body::empty());
            response.headers_mut().insert(
                "access-control-allow-origin",
                HeaderValue::from_static("http://localhost:3000"),
            );
            response
                .headers_mut()
                .insert("vary", HeaderValue::from_static("Accept-Encoding"));
            Ok::<_, std::convert::Infallible>(response)
        }));
        let response = service
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/session")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.headers()["vary"], "Accept-Encoding, Origin");
    }

    #[tokio::test]
    async fn cors_vary_fix_keeps_existing_origin_token() {
        let service = CorsVaryLayer.layer(tower::service_fn(|_| async {
            let mut response = Response::new(Body::empty());
            response.headers_mut().insert(
                "access-control-allow-origin",
                HeaderValue::from_static("http://localhost:3000"),
            );
            response
                .headers_mut()
                .insert("vary", HeaderValue::from_static("origin, Accept-Encoding"));
            Ok::<_, std::convert::Infallible>(response)
        }));
        let response = service
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/session")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.headers()["vary"], "origin, Accept-Encoding");
    }

    #[tokio::test]
    async fn cors_vary_fix_ignores_wildcard_origin() {
        let service = CorsVaryLayer.layer(tower::service_fn(|_| async {
            let mut response = Response::new(Body::empty());
            response
                .headers_mut()
                .insert("access-control-allow-origin", HeaderValue::from_static("*"));
            Ok::<_, std::convert::Infallible>(response)
        }));
        let response = service
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/session")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(response.headers().get("vary").is_none());
    }
}

//! UI catch-all — port of `shared/ui.ts` (empty embedded map branch) and the
//! public manifest paths (`shared/public-ui.ts`).

use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::{HeaderValue, Response};
use futures::future::BoxFuture;
use tower::Service;

use crate::state::ServerContext;

/// Static UI assets browsers fetch without credentials; these bypass auth
/// (`shared/public-ui.ts:4-8`).
pub const PUBLIC_UI_PATHS: [&str; 3] = [
    "/site.webmanifest",
    "/web-app-manifest-192x192.png",
    "/web-app-manifest-512x512.png",
];

fn not_found() -> Response<Body> {
    // `HttpServerResponse.jsonUnsafe({error: "Not Found"}, {status: 404})`
    // (shared/ui.ts:51-53).
    Response::builder()
        .status(404)
        .header("content-type", "application/json")
        .body(Body::from("{\"error\":\"Not Found\"}"))
        .expect("static response parts are valid")
}

/// Serve the embedded UI for a request path: map lookup, then the
/// `index.html` fallback, then the 404 JSON envelope (`shared/ui.ts:55-79`).
pub fn serve_ui(ctx: &Arc<ServerContext>, path: &str) -> Response<Body> {
    // `embeddedWebUI[requestPath.replace(/^\//, "")]` (ui.ts:60) — the
    // leading slash never keys the map.
    let path = path.strip_prefix('/').unwrap_or(path);
    if let Some(file) = ctx.ui.get(path).or_else(|| ctx.ui.index()) {
        let mut response = Response::builder()
            .status(200)
            .body(Body::from(file.bytes.clone()))
            .expect("static response parts are valid");
        if let Ok(value) = HeaderValue::from_str(&file.mime) {
            response.headers_mut().insert("content-type", value);
        }
        if file.mime.starts_with("text/html") {
            // `cspForHtml` (ui.ts:12-16) — the theme-preload script hash
            // inlines into the CSP.
            if let Ok(value) =
                HeaderValue::from_str(&csp_for_html(&String::from_utf8_lossy(&file.bytes)))
            {
                response
                    .headers_mut()
                    .insert("content-security-policy", value);
            }
        }
        return response;
    }
    not_found()
}

/// `themePreloadHash` + `csp` (ui.ts:14-19, 26-27).
fn csp_for_html(body: &str) -> String {
    let hash = theme_preload_script(body)
        .map(|script| {
            use base64::Engine as _;
            use sha2::{Digest, Sha256};
            let digest = Sha256::digest(script.as_bytes());
            base64::engine::general_purpose::STANDARD.encode(digest)
        })
        .unwrap_or_default();
    csp(&hash)
}

fn csp(hash: &str) -> String {
    if hash.is_empty() {
        "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' data: https: blob:; font-src 'self' data:; media-src 'self' data:; connect-src * data: blob:"
            .to_string()
    } else {
        format!("default-src 'self'; script-src 'self' 'wasm-unsafe-eval' 'sha256-{hash}'; style-src 'self' 'unsafe-inline'; img-src 'self' data: https: blob:; font-src 'self' data:; media-src 'self' data:; connect-src * data: blob:")
    }
}

/// `<script id="oc-theme-preload-script">…</script>` — the captured script
/// element's content feeds the CSP hash (ui.ts:18-19).
fn theme_preload_script(body: &str) -> Option<String> {
    let start = body
        .match_indices("<script")
        .find(|(index, _)| {
            body[*index..]
                .split_once('>')
                .map(|(tag, _)| {
                    tag.contains("id=")
                        && tag.contains("oc-theme-preload-script")
                        && !tag.contains("src=")
                })
                .unwrap_or(false)
        })
        .map(|(index, _)| index)?;
    let rest = &body[start..];
    let open_end = rest.find('>')?;
    let close = rest[open_end..].find("</script>")?;
    Some(rest[open_end + 1..open_end + close].to_string())
}

/// Axum fallback handler: TS `router.add("*", "/*", serveUI)`
/// (`httpapi/server.ts:194-203`).
pub async fn serve_ui_handler(
    axum::extract::State(ctx): axum::extract::State<Arc<ServerContext>>,
    req: axum::http::Request<Body>,
) -> Response<Body> {
    serve_ui(&ctx, req.uri().path())
}

/// TS registers the UI as `router.add("*", "/*", serveUI)`: effect's
/// per-method radix trees hold the catch-all for *every* method, so a request
/// whose method misses a registered path falls through to the UI instead of
/// producing a 405. Axum's router answers 405 first — this layer rewrites
/// those responses into the UI fallback to keep TS parity.
#[derive(Clone)]
pub struct UiFallbackLayer {
    ctx: Arc<ServerContext>,
}

impl UiFallbackLayer {
    pub fn new(ctx: Arc<ServerContext>) -> UiFallbackLayer {
        UiFallbackLayer { ctx }
    }
}

impl<S> tower::Layer<S> for UiFallbackLayer {
    type Service = UiFallbackService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        UiFallbackService {
            inner,
            ctx: self.ctx.clone(),
        }
    }
}

#[derive(Clone)]
pub struct UiFallbackService<S> {
    inner: S,
    ctx: Arc<ServerContext>,
}

impl<S> Service<axum::http::Request<Body>> for UiFallbackService<S>
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
        let ctx = self.ctx.clone();
        let path = req.uri().path().to_string();
        let mut inner = self.inner.clone();
        Box::pin(async move {
            let response = inner.call(req).await?;
            if response.status() != 405 {
                return Ok(response);
            }
            Ok(serve_ui(&ctx, &path))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unregistered_paths_hit_the_ui_404_envelope() {
        let ctx = std::sync::Arc::new(crate::state::ServerContext::for_tests());
        let response = serve_ui(&ctx, "/does-not-exist");
        assert_eq!(response.status(), 404);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"{\"error\":\"Not Found\"}");
    }

    #[tokio::test]
    async fn embedded_backend_serves_index_with_csp() {
        let ctx = std::sync::Arc::new(crate::state::ServerContext::for_tests_with_ui(
            crate::state::AuthConfig::new("alforria", None),
            std::sync::Arc::new(crate::state::EmbeddedUiBackend),
        ));
        // The root maps to `index.html` via the SPA fallback.
        let response = serve_ui(&ctx, "/");
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers().get("content-type").unwrap(), "text/html");
        // `index.html` carries the inline theme-preload script, so the CSP
        // must include its sha256 hash (`cspForHtml`, ui.ts:12-19).
        let csp = response
            .headers()
            .get("content-security-policy")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(csp.contains("sha256-"), "csp: {csp}");
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8_lossy(&bytes);
        assert!(html.contains("id=\"root\""), "index.html not served");
    }

    #[tokio::test]
    async fn embedded_backend_falls_back_to_index_for_deep_links() {
        let ctx = std::sync::Arc::new(crate::state::ServerContext::for_tests_with_ui(
            crate::state::AuthConfig::new("alforria", None),
            std::sync::Arc::new(crate::state::EmbeddedUiBackend),
        ));
        let response = serve_ui(&ctx, "/session/does-not-exist");
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers().get("content-type").unwrap(), "text/html");
    }

    #[tokio::test]
    async fn embedded_backend_serves_known_asset_with_correct_mime() {
        let ctx = std::sync::Arc::new(crate::state::ServerContext::for_tests_with_ui(
            crate::state::AuthConfig::new("alforria", None),
            std::sync::Arc::new(crate::state::EmbeddedUiBackend),
        ));
        // `site.webmanifest` ships at the UI root and must carry the manifest
        // MIME (`mime-types` `lookup` behavior, `fs-util.ts:224-226`).
        let response = serve_ui(&ctx, "/site.webmanifest");
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "application/manifest+json"
        );
    }

    #[tokio::test]
    async fn embedded_backend_falls_back_to_index_for_missing_assets() {
        let ctx = std::sync::Arc::new(crate::state::ServerContext::for_tests_with_ui(
            crate::state::AuthConfig::new("alforria", None),
            std::sync::Arc::new(crate::state::EmbeddedUiBackend),
        ));
        // TS `serveEmbeddedUIEffect` does `embeddedWebUI[path] ?? index.html`
        // (`shared/ui.ts:60`), so an unknown asset path still yields the SPA
        // shell rather than a hard 404.
        let response = serve_ui(&ctx, "/assets/does-not-exist.js");
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers().get("content-type").unwrap(), "text/html");
    }

    #[test]
    fn csp_defaults_without_the_theme_preload_script() {
        let csp = csp_for_html("<html><body></body></html>");
        assert!(csp.contains("wasm-unsafe-eval"));
        assert!(!csp.contains("sha256-"));
    }

    #[test]
    fn csp_hashes_the_theme_preload_script() {
        let html =
            r#"<html><script id="oc-theme-preload-script">window.__theme = "dark"</script></html>"#;
        let csp = csp_for_html(html);
        assert!(csp.contains("sha256-"));
    }
}

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
/// `index.html` fallback, then the 404 JSON envelope (`shared/ui.ts:69-70`).
pub fn serve_ui(ctx: &Arc<ServerContext>, path: &str) -> Response<Body> {
    if let Some(file) = ctx.ui.get(path).or_else(|| ctx.ui.index()) {
        let mut response = Response::builder()
            .status(200)
            .body(Body::from(file.bytes))
            .expect("static response parts are valid");
        if let Ok(value) = HeaderValue::from_str(&file.mime) {
            response.headers_mut().insert("content-type", value);
        }
        return response;
    }
    not_found()
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
}

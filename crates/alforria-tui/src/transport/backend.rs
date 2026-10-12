//! The byte-level transport under [`crate::transport::api`] and
//! [`crate::transport::events`].
//!
//! Upstream runs the server in a worker and hands the TUI a custom
//! `fetch` plus an RPC event source (`cli/cmd/tui.ts`: `createWorkerFetch`,
//! `createEventSource`); only `--port`/`--hostname`/`--mdns` and `attach`
//! go over the network. The port keeps the same split one level lower:
//! both clients build the same requests (URL, query, headers, JSON body)
//! and hand them to a [`Backend`]:
//!
//! * [`ReqwestBackend`] — real HTTP, for `attach` and the network flags;
//! * [`ServiceBackend`] — calls a tower `Service` (the server's axum
//!   router) in memory. No socket exists, so nothing else on the machine
//!   (a browser, another user) can reach the server. SSE works unchanged:
//!   the response body is polled frame by frame like a socket would be.

use std::convert::Infallible;
use std::fmt;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::StreamExt;
use http_body_util::{BodyExt, Full};
use reqwest::Method;

/// Base URL the in-memory backend is addressed with. Only the path and
/// query reach the router; the authority becomes the `Host` header, a
/// loopback name so host checks treat it as local.
pub const IN_PROCESS_BASE_URL: &str = "http://localhost";

/// One request, already fully built by the API or SSE client.
#[derive(Debug, Clone)]
pub struct BackendRequest {
    pub method: Method,
    /// Absolute URL (`{base_url}{path}?{query}`).
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// JSON body; sets `content-type: application/json`.
    pub json: Option<Vec<u8>>,
}

/// A response: status plus the body as a byte stream (SSE reads it
/// incrementally; everything else collects it).
pub struct BackendResponse {
    pub status: u16,
    pub body: BoxStream<'static, Result<Bytes>>,
}

impl fmt::Debug for BackendResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BackendResponse")
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl BackendResponse {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Collect the body as UTF-8 text (lossy, like `Response.text()`).
    pub async fn text(mut self) -> Result<String> {
        let mut buffer = Vec::new();
        while let Some(chunk) = self.body.next().await {
            buffer.extend_from_slice(&chunk?);
        }
        Ok(String::from_utf8_lossy(&buffer).into_owned())
    }
}

/// The transport seam.
#[async_trait]
pub trait Backend: Send + Sync {
    async fn send(&self, request: BackendRequest) -> Result<BackendResponse>;
}

/// A cloneable, debuggable handle on a backend (the [`crate::TuiInput`]
/// field).
#[derive(Clone)]
pub struct SharedBackend(pub Arc<dyn Backend>);

impl fmt::Debug for SharedBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SharedBackend(..)")
    }
}

/// Real HTTP.
#[derive(Debug, Clone)]
pub struct ReqwestBackend {
    client: reqwest::Client,
}

impl ReqwestBackend {
    pub fn new() -> Result<ReqwestBackend> {
        Ok(ReqwestBackend {
            client: reqwest::Client::builder().build()?,
        })
    }
}

#[async_trait]
impl Backend for ReqwestBackend {
    async fn send(&self, request: BackendRequest) -> Result<BackendResponse> {
        let mut builder = self.client.request(request.method.clone(), &request.url);
        for (name, value) in &request.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        if let Some(json) = request.json {
            builder = builder
                .header("content-type", "application/json")
                .body(json);
        }
        let response = builder.send().await?;
        let status = response.status().as_u16();
        let body = response
            .bytes_stream()
            .map(|chunk| chunk.map_err(anyhow::Error::from))
            .boxed();
        Ok(BackendResponse { status, body })
    }
}

type Call =
    dyn Fn(http::Request<Full<Bytes>>) -> BoxFuture<'static, Result<BackendResponse>> + Send + Sync;

/// In-memory: every request is a `oneshot` call on a clone of a tower
/// service (the server's axum router).
#[derive(Clone)]
pub struct ServiceBackend {
    call: Arc<Call>,
}

impl fmt::Debug for ServiceBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ServiceBackend(..)")
    }
}

impl ServiceBackend {
    pub fn new<S, B>(service: S) -> ServiceBackend
    where
        S: tower::Service<
                http::Request<Full<Bytes>>,
                Response = http::Response<B>,
                Error = Infallible,
            > + Clone
            + Send
            + Sync
            + 'static,
        S::Future: Send + 'static,
        B: http_body::Body<Data = Bytes> + Send + 'static,
        B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        let call = move |request: http::Request<Full<Bytes>>| {
            let service = service.clone();
            Box::pin(async move {
                let response = match tower::ServiceExt::oneshot(service, request).await {
                    Ok(response) => response,
                    Err(never) => match never {},
                };
                let status = response.status().as_u16();
                let body = response
                    .into_body()
                    .into_data_stream()
                    .map(|chunk| chunk.map_err(|error| anyhow!(error.into())))
                    .boxed();
                Ok(BackendResponse { status, body })
            }) as BoxFuture<'static, Result<BackendResponse>>
        };
        ServiceBackend {
            call: Arc::new(call),
        }
    }
}

/// `scheme://authority/path?query` → (`authority`, `/path?query`).
fn split_url(url: &str) -> (&str, &str) {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => match rest.find('?') {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, "/"),
        },
    }
}

#[async_trait]
impl Backend for ServiceBackend {
    async fn send(&self, request: BackendRequest) -> Result<BackendResponse> {
        let (authority, path) = split_url(&request.url);
        let mut path = path.to_string();
        if path.starts_with('?') {
            path.insert(0, '/');
        }
        let mut builder = http::Request::builder()
            .method(request.method.as_str())
            .uri(path.as_str())
            .header("host", authority);
        for (name, value) in &request.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        let body = match request.json {
            Some(json) => {
                builder = builder.header("content-type", "application/json");
                Full::new(Bytes::from(json))
            }
            None => Full::new(Bytes::new()),
        };
        let request = builder
            .body(body)
            .with_context(|| format!("invalid in-process request {path}"))?;
        (self.call)(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::split_url;

    #[test]
    fn split_url_separates_authority_and_path() {
        assert_eq!(
            split_url("http://localhost/session?x=1"),
            ("localhost", "/session?x=1")
        );
        assert_eq!(split_url("http://localhost"), ("localhost", "/"));
        assert_eq!(split_url("http://h:1?x"), ("h:1", "?x"));
    }
}

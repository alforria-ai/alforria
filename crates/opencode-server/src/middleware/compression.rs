//! Response compression — verbatim port of `httpapi/middleware/compression.ts`.
//!
//! gzip/deflate for compressible content-types > 1024 bytes, gzip preferred;
//! skip `HEAD`, skip responses that already carry `content-encoding` /
//! `transfer-encoding`, skip the SSE streaming paths, skip
//! `cache-control: no-transform`.

use std::io::Write;
use std::sync::LazyLock;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::{header, Method, Response};
use futures::future::BoxFuture;
use tower::Service;

// compression.ts:6-7 — keep the compressible content-type set stable. The
// TS regex uses a negative lookahead (`text/(?!event-stream...)`), which the
// `regex` crate does not support; the `text/` branch captures its media type
// and [`is_compressible`] rejects `text/event-stream` after the match, which
// is equivalent because `[^;\s]+` plus the terminator consumes the whole
// media type.
const COMPRESSIBLE_CONTENT_TYPE_REGEX: &str = concat!(
    r"(?i)^\s*(",
    r"text/[^;\s]+",
    r"|application/(?:javascript|json|xml|xml-dtd|ecmascript|dart|postscript|rtf|tar|toml|vnd\.dart|vnd\.ms-fontobject|vnd\.ms-opentype|wasm|x-httpd-php|x-javascript|x-ns-proxy-autoconfig|x-sh|x-tar|x-www-form-urlencoded)",
    r"|font/(?:otf|ttf)",
    r"|image/(?:bmp|vnd\.adobe\.photoshop|vnd\.microsoft\.icon|vnd\.ms-dds|x-icon|x-ms-bmp)",
    r"|message/rfc822",
    r"|model/gltf-binary",
    r"|x-shader/x-fragment",
    r"|x-shader/x-vertex",
    r"|[^;\s]+?\+(?:json|text|xml|yaml)",
    r")(?:[;\s]|$)",
);

const NO_TRANSFORM_REGEX: &str = r"(?i)(?:^|,)\s*?no-transform\s*?(?:,|$)";

static COMPRESSIBLE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(COMPRESSIBLE_CONTENT_TYPE_REGEX).unwrap());
static NO_TRANSFORM: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(NO_TRANSFORM_REGEX).unwrap());

/// `COMPRESSIBLE_CONTENT_TYPE_REGEX.test(contentType)` including the
/// `text/event-stream` exclusion (see the regex note above).
fn is_compressible(content_type: &str) -> bool {
    match COMPRESSIBLE.captures(content_type) {
        Some(captures) => {
            captures
                .get(1)
                .map(|m| m.as_str().to_ascii_lowercase())
                .as_deref()
                != Some("text/event-stream")
        }
        None => false,
    }
}

const STREAMING_PATHS: &[&str] = &["/event", "/global/event"];
const STREAMING_POST_REGEX: &str = r"^/session/[^/]+/(?:message|prompt_async)$";

const THRESHOLD_BYTES: usize = 1024;

/// Bodies above this cap pass through uncompressed (TS only compresses
/// already-buffered `Uint8Array` bodies; streaming responses flow through
/// untouched).
const COLLECT_LIMIT: usize = 16 * 1024 * 1024;

/// `pickEncoding` (compression.ts:18-24): gzip preferred over deflate.
fn pick_encoding(accept_encoding: Option<&str>) -> Option<&'static str> {
    let accept_encoding = accept_encoding?;
    if accept_encoding.to_lowercase().contains("gzip") {
        return Some("gzip");
    }
    if accept_encoding.to_lowercase().contains("deflate") {
        return Some("deflate");
    }
    None
}

/// `pathOf` (compression.ts:26-29): strip the query string.
fn path_of(url: &str) -> &str {
    match url.find('?') {
        Some(index) => &url[..index],
        None => url,
    }
}

#[derive(Debug)]
enum Encoding {
    Gzip,
    Deflate,
}

fn compress(bytes: &[u8], encoding: Encoding) -> Vec<u8> {
    match encoding {
        Encoding::Gzip => {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(bytes).expect("Vec writer cannot fail");
            encoder.finish().expect("Vec writer cannot fail")
        }
        // TS `deflateSync` emits the zlib (RFC 1950) format.
        Encoding::Deflate => {
            let mut encoder =
                flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(bytes).expect("Vec writer cannot fail");
            encoder.finish().expect("Vec writer cannot fail")
        }
    }
}

/// The `compressionLayer` middleware (compression.ts:31-63).
#[derive(Clone, Default)]
pub struct CompressionLayer;

impl<S> tower::Layer<S> for CompressionLayer {
    type Service = CompressionService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        CompressionService { inner }
    }
}

#[derive(Clone)]
pub struct CompressionService<S> {
    inner: S,
}

impl<S> Service<axum::http::Request<Body>> for CompressionService<S>
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
        let method = req.method().clone();
        let path = req.uri().path().to_string();
        let full_url = req
            .uri()
            .path_and_query()
            .map(|pq| pq.as_str().to_string())
            .unwrap_or_else(|| path.clone());
        let accept_encoding = req
            .headers()
            .get(header::ACCEPT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.to_string());
        let mut inner = self.inner.clone();
        Box::pin(async move {
            let response = inner.call(req).await?;
            Ok(
                compress_if_eligible(response, &method, &full_url, accept_encoding.as_deref())
                    .await,
            )
        })
    }
}

async fn compress_if_eligible(
    response: Response<Body>,
    method: &Method,
    full_url: &str,
    accept_encoding: Option<&str>,
) -> Response<Body> {
    // compression.ts:36
    if method == Method::HEAD {
        return response;
    }
    // compression.ts:37-38
    if response.headers().contains_key(header::CONTENT_ENCODING)
        || response.headers().contains_key(header::TRANSFER_ENCODING)
    {
        return response;
    }

    let (mut parts, body) = response.into_parts();
    let path = path_of(full_url);
    // compression.ts:47-49
    if STREAMING_PATHS.contains(&path) {
        return Response::from_parts(parts, body);
    }
    if method == Method::POST && is_streaming_post(path) {
        return Response::from_parts(parts, body);
    }

    //compression.ts:44-45
    let cache_control = parts.headers.get(header::CACHE_CONTROL);
    if let Some(cache_control) = cache_control {
        if let Ok(value) = cache_control.to_str() {
            if NO_TRANSFORM.is_match(value) {
                return Response::from_parts(parts, body);
            }
        }
    }

    let bytes = match axum::body::to_bytes(body, COLLECT_LIMIT).await {
        Ok(bytes) => bytes,
        // Errors can only come from an aborted in-flight body; the partial
        // response is the closest thing to TS's untouched passthrough.
        Err(_) => return Response::from_parts(parts, Body::empty()),
    };
    let bytes = bytes.to_vec();
    // compression.ts:42
    if bytes.len() < THRESHOLD_BYTES {
        return Response::from_parts(parts, Body::from(bytes));
    }

    // compression.ts:51-52
    let content_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !is_compressible(content_type) {
        return Response::from_parts(parts, Body::from(bytes));
    }

    // compression.ts:54-55
    let Some(encoding) = pick_encoding(accept_encoding) else {
        return Response::from_parts(parts, Body::from(bytes));
    };

    let encoding = match encoding {
        "gzip" => Encoding::Gzip,
        _ => Encoding::Deflate,
    };
    let name = encoding_name(&encoding);
    let compressed = compress(&bytes, encoding);
    parts.headers.insert(
        header::CONTENT_ENCODING,
        header::HeaderValue::from_str(name).expect("static header value"),
    );
    parts.headers.insert(
        header::CONTENT_LENGTH,
        header::HeaderValue::from_str(&compressed.len().to_string()).expect("length is ascii"),
    );
    Response::from_parts(parts, Body::from(compressed))
}

fn encoding_name(encoding: &Encoding) -> &'static str {
    match encoding {
        Encoding::Gzip => "gzip",
        Encoding::Deflate => "deflate",
    }
}

static STREAMING_POST: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(STREAMING_POST_REGEX).unwrap());

fn is_streaming_post(path: &str) -> bool {
    STREAMING_POST.is_match(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compressible_content_type_regex_matrix() {
        assert!(is_compressible("application/json"));
        assert!(is_compressible("text/html; charset=utf-8"));
        assert!(is_compressible("application/json; charset=utf-8"));
        assert!(!is_compressible("text/event-stream"));
        assert!(!is_compressible("text/event-stream; charset=utf-8"));
        assert!(is_compressible("application/xml"));
        assert!(is_compressible("font/otf"));
        assert!(is_compressible("image/bmp"));
        assert!(is_compressible("application/vnd.docker+json"));
        assert!(!is_compressible("image/png"));
        assert!(!is_compressible("application/octet-stream"));
        // `text/plain; charset=utf-8` — parameter must be allowed.
        assert!(is_compressible("text/plain; charset=utf-8"));
    }

    #[test]
    fn picks_gzip_over_deflate() {
        assert_eq!(pick_encoding(Some("gzip, deflate")), Some("gzip"));
        assert_eq!(pick_encoding(Some("deflate, br")), Some("deflate"));
        assert_eq!(pick_encoding(Some("br")), None);
        assert_eq!(pick_encoding(None), None);
        assert_eq!(pick_encoding(Some("GZIP")), Some("gzip"));
    }

    #[test]
    fn no_transform_regex_matches() {
        assert!(NO_TRANSFORM.is_match("no-transform"));
        assert!(NO_TRANSFORM.is_match("no-cache, no-transform"));
        assert!(NO_TRANSFORM.is_match("NO-TRANSFORM"));
    }
}

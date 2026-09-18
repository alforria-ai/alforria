//! M6.1 acceptance: compression/CORS/fence middleware quirks, exercised
//! end-to-end through tower oneshot requests.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, Response, StatusCode};
use opencode_server::middleware::{compression, cors, fence};
use tower::{Layer, ServiceExt};

// ---------------------------------------------------------------- compression

fn big_json_response() -> Response<Body> {
    let body = "{\"pad\":\"".to_string() + &"x".repeat(2048) + "\"}";
    let mut response = Response::new(Body::from(body));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    response
}

#[tokio::test]
async fn compresses_big_json_with_gzip() {
    let service =
        compression::CompressionLayer.layer(tower::service_fn(|_: Request<Body>| async {
            Ok::<_, std::convert::Infallible>(big_json_response())
        }));
    let response = service
        .oneshot(
            Request::builder()
                .header(header::ACCEPT_ENCODING, "gzip")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.headers()[header::CONTENT_ENCODING], "gzip");
    let compressed = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let mut decoder = flate2::read::GzDecoder::new(&compressed[..]);
    let mut plain = String::new();
    std::io::Read::read_to_string(&mut decoder, &mut plain).unwrap();
    assert!(plain.starts_with("{\"pad\":"));
    assert_eq!(plain.len(), 2048 + "{\"pad\":\"\"}".len());
}

#[tokio::test]
async fn skips_bodies_below_the_threshold() {
    let service =
        compression::CompressionLayer.layer(tower::service_fn(|_: Request<Body>| async {
            let mut response = Response::new(Body::from("tiny body"));
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/json"),
            );
            Ok::<_, std::convert::Infallible>(response)
        }));
    let response = service
        .oneshot(
            Request::builder()
                .header(header::ACCEPT_ENCODING, "gzip")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(response.headers().get(header::CONTENT_ENCODING).is_none());
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&bytes[..], b"tiny body");
}

#[tokio::test]
async fn deflate_used_when_gzip_not_accepted() {
    let service =
        compression::CompressionLayer.layer(tower::service_fn(|_: Request<Body>| async {
            Ok::<_, std::convert::Infallible>(big_json_response())
        }));
    let response = service
        .oneshot(
            Request::builder()
                .header(header::ACCEPT_ENCODING, "deflate")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.headers()[header::CONTENT_ENCODING], "deflate");
}

#[tokio::test]
async fn skips_sse_paths() {
    for path in ["/event", "/global/event"] {
        let service =
            compression::CompressionLayer.layer(tower::service_fn(move |_: Request<Body>| async {
                Ok::<_, std::convert::Infallible>(big_json_response())
            }));
        let response = service
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header(header::ACCEPT_ENCODING, "gzip")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            response.headers().get(header::CONTENT_ENCODING).is_none(),
            "{path} responses must never compress"
        );
    }
}

#[tokio::test]
async fn skips_streaming_post_paths() {
    let service = compression::CompressionLayer.layer(tower::service_fn(|req: Request<Body>| {
        let path = req.uri().path().to_string();
        async move {
            let mut response = big_json_response();
            response
                .headers_mut()
                .insert("x-test-path", header::HeaderValue::from_str(&path).unwrap());
            Ok::<_, std::convert::Infallible>(response)
        }
    }));
    for path in ["/session/ses_123/message", "/session/ses_123/prompt_async"] {
        let response = service
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header(header::ACCEPT_ENCODING, "gzip")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            response.headers().get(header::CONTENT_ENCODING).is_none(),
            "POST {path} responses must never compress"
        );
    }
    // Non-streaming POST bodies still compress.
    let response = service
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/session/ses_123/abort")
                .header(header::ACCEPT_ENCODING, "gzip")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.headers()[header::CONTENT_ENCODING], "gzip");
}

#[tokio::test]
async fn skips_no_transform_responses() {
    let service =
        compression::CompressionLayer.layer(tower::service_fn(|_: Request<Body>| async {
            let mut response = big_json_response();
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                header::HeaderValue::from_static("no-cache, no-transform"),
            );
            Ok::<_, std::convert::Infallible>(response)
        }));
    let response = service
        .oneshot(
            Request::builder()
                .header(header::ACCEPT_ENCODING, "gzip")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(response.headers().get(header::CONTENT_ENCODING).is_none());
}

#[tokio::test]
async fn skips_head_and_precompressed_responses() {
    let service = compression::CompressionLayer.layer(tower::service_fn(|req: Request<Body>| {
        let is_head = req.method() == axum::http::Method::HEAD;
        async move {
            let mut response = big_json_response();
            if !is_head {
                response.headers_mut().insert(
                    header::CONTENT_ENCODING,
                    header::HeaderValue::from_static("br"),
                );
            }
            Ok::<_, std::convert::Infallible>(response)
        }
    }));

    let head = service
        .clone()
        .oneshot(
            Request::builder()
                .method("HEAD")
                .header(header::ACCEPT_ENCODING, "gzip")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(head.headers().get(header::CONTENT_ENCODING).is_none());

    let precompressed = service
        .oneshot(
            Request::builder()
                .header(header::ACCEPT_ENCODING, "gzip")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(precompressed.headers()[header::CONTENT_ENCODING], "br");
}

// ---------------------------------------------------------------- CORS

#[tokio::test]
async fn explicit_cors_list_allows_extra_origins() {
    let service = cors::CorsLayer::new(vec!["https://partner.example".to_string()]).layer(
        tower::service_fn(|_: Request<Body>| async {
            Ok::<_, std::convert::Infallible>(Response::new(Body::empty()))
        }),
    );
    let response = service
        .oneshot(
            Request::builder()
                .uri("/session")
                .header("origin", "https://partner.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.headers()["access-control-allow-origin"],
        "https://partner.example"
    );
}

#[tokio::test]
async fn cors_vary_fix_runs_after_cors() {
    let service = cors::CorsVaryLayer.layer(cors::CorsLayer::new(vec![]).layer(tower::service_fn(
        |_: Request<Body>| async {
            Ok::<_, std::convert::Infallible>(Response::new(Body::empty()))
        },
    )));
    let response = service
        .oneshot(
            Request::builder()
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

// ---------------------------------------------------------------- fence

fn fenced_storage() -> Arc<opencode_core::Storage> {
    Arc::new(opencode_core::Storage::open_in_memory().unwrap())
}

#[tokio::test]
async fn fence_inactive_without_workspace_id() {
    let storage = fenced_storage();
    let service =
        fence::FenceLayer::new(storage, false).layer(tower::service_fn(|_: Request<Body>| async {
            Ok::<_, std::convert::Infallible>(Response::new(Body::empty()))
        }));
    let response = service
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/anything")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(response.headers().get("x-opencode-sync").is_none());
}

#[tokio::test]
async fn fence_sets_sync_header_for_mutating_requests() {
    use opencode_core::event::sql::upsert_event_sequence;
    let storage = fenced_storage();
    let handler_storage = storage.clone();
    let service =
        fence::FenceLayer::new(storage, true).layer(tower::service_fn(move |_: Request<Body>| {
            let storage = handler_storage.clone();
            async move {
                storage
                    .with_connection(|conn| upsert_event_sequence(conn, "ses_x", 7, None, false))
                    .unwrap();
                Ok::<_, std::convert::Infallible>(Response::new(Body::empty()))
            }
        }));
    let response = service
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/anything")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.headers()["x-opencode-sync"],
        "{\"ses_x\":7}",
        "changed sequences are reported"
    );
}

#[tokio::test]
async fn fence_ignores_get_requests() {
    let storage = fenced_storage();
    let handler_storage = storage.clone();
    let service =
        fence::FenceLayer::new(storage, true).layer(tower::service_fn(move |_: Request<Body>| {
            let storage = handler_storage.clone();
            async move {
                use opencode_core::event::sql::upsert_event_sequence;
                storage
                    .with_connection(|conn| upsert_event_sequence(conn, "ses_x", 7, None, false))
                    .unwrap();
                Ok::<_, std::convert::Infallible>(Response::new(Body::empty()))
            }
        }));
    let response = service
        .oneshot(
            Request::builder()
                .uri("/anything")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(response.headers().get("x-opencode-sync").is_none());
    assert_eq!(response.status(), StatusCode::OK);
}

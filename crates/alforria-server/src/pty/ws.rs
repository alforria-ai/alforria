//! PTY connect WebSocket — port of the `pty.connect` raw handlers
//! (`httpapi/handlers/pty.ts:163-273` for v1,
//! `packages/server/src/handlers/pty.ts:140-221` for v2).
//!
//! Handshake order (v1): 404 (missing or not running) → 400 (`CursorQuery`
//! decode) → 403 (bad ticket or origin) → upgrade. v2 drops the 400 step.
//! After the upgrade: replay bytes, then the meta frame, then live output
//! through a single outbox queue; inbound frames are UTF-8 text/binary,
//! invalid UTF-8 is dropped.

use std::sync::Arc;

use alforria_schema::pty::PtyStatus;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::FromRequestParts;
use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use futures::{SinkExt, StreamExt};

use crate::middleware::auth::query_param;
use crate::middleware::cors::request_origin_allowed;
use crate::middleware::location::LocationContext;
use crate::state::ServerContext;

use crate::pty::protocol;
use crate::pty::routes::{require_pty_id_v1, require_pty_id_v2, ticket_scope};
use crate::pty::{PtyError, PtyService};

/// One queued outbound frame — replay chunk, meta frame or close event.
enum OutFrame {
    Text(String),
    Binary(Vec<u8>),
    Close(u16, String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Surface {
    V1,
    V2,
}

fn empty(status: StatusCode) -> Response {
    Response::builder()
        .status(status)
        .body(axum::body::Body::empty())
        .expect("static response parts are valid")
}

/// `Number()` — JS number coercion for the `cursor` query param
/// (`httpapi/handlers/pty.ts:202-206`). Returns NaN as `None`-like
/// `f64::NAN` so the safe-integer gate rejects it.
fn js_number(input: &str) -> f64 {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return 0.0;
    }
    let (sign, rest) = match trimmed.as_bytes().first() {
        Some(b'+') => (1.0, &trimmed[1..]),
        Some(b'-') => (-1.0, &trimmed[1..]),
        _ => (1.0, trimmed),
    };
    if let Some(hex) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
        return match u64::from_str_radix(hex, 16) {
            Ok(value) => sign * value as f64,
            Err(_) => f64::NAN,
        };
    }
    trimmed.parse::<f64>().unwrap_or(f64::NAN)
}

/// `parsedCursor` → `cursor` (`handlers/pty.ts:202-206`): integer ≥ -1,
/// anything else is ignored.
fn parsed_cursor(query: Option<&str>) -> Option<i64> {
    let raw = query_param(query, "cursor")?;
    let value = js_number(&raw);
    if value.fract() == 0.0 && (-1.0..=9_007_199_254_740_992.0).contains(&value) {
        return Some(value as i64);
    }
    None
}

/// The v1 `CursorQuery` decode (`handlers/pty.ts:193-194`): repeated
/// `directory` / `workspace` / `cursor` params surface as arrays and fail
/// `Schema.String`.
fn cursor_query_valid(uri: &Uri) -> bool {
    let query = uri.query().unwrap_or_default();
    for name in ["directory", "workspace", "cursor"] {
        let count = query
            .split('&')
            .filter(|pair| !pair.is_empty())
            .filter(|pair| {
                let key = pair.split_once('=').map_or(*pair, |(key, _)| key);
                key == name
            })
            .count();
        if count > 1 {
            return false;
        }
    }
    true
}

/// `GET /pty/{ptyID}/connect` (`httpapi/handlers/pty.ts:181-271`).
pub async fn connect_v1(
    State(ctx): State<Arc<ServerContext>>,
    Path(pty_id): Path<String>,
    request: Request,
) -> Response {
    if let Err(err) = require_pty_id_v1(&pty_id) {
        return err.into_response();
    }
    let Some(location) = request.extensions().get::<LocationContext>().cloned() else {
        return crate::error::defect_response();
    };
    let service = ctx.ptys.resolve(&location);
    let uri = request.uri().clone();
    let running = matches!(
        service.get(&pty_id),
        Ok(info) if matches!(info.status, PtyStatus::Running)
    );
    if !running {
        return empty(StatusCode::NOT_FOUND);
    }
    if !cursor_query_valid(&uri) {
        return empty(StatusCode::BAD_REQUEST);
    }
    if !ticket_check(&ctx, &request, &pty_id, &location) {
        return empty(StatusCode::FORBIDDEN);
    }
    let cursor = parsed_cursor(uri.query());
    upgrade(ctx, service, pty_id, cursor, request, Surface::V1).await
}

/// `GET /api/pty/{ptyID}/connect` (`packages/server/src/handlers/pty.ts:142-220`).
pub async fn connect_v2(
    State(ctx): State<Arc<ServerContext>>,
    Path(pty_id): Path<String>,
    request: Request,
) -> Response {
    if let Err(err) = require_pty_id_v2(&pty_id) {
        return err.into_response();
    }
    let Some(location) = request.extensions().get::<LocationContext>().cloned() else {
        return crate::error::defect_response();
    };
    let service = ctx.ptys.resolve(&location);
    if service.get(&pty_id).is_err() {
        return empty(StatusCode::NOT_FOUND);
    }
    if !ticket_check(&ctx, &request, &pty_id, &location) {
        return empty(StatusCode::FORBIDDEN);
    }
    let cursor = parsed_cursor(request.uri().query());
    upgrade(ctx, service, pty_id, cursor, request, Surface::V2).await
}

/// The ticket consumption shared by both handlers (`:196-201`,
/// `packages/.../handlers/pty.ts:152-157`).
fn ticket_check(
    ctx: &Arc<ServerContext>,
    request: &Request,
    pty_id: &str,
    location: &LocationContext,
) -> bool {
    let Some(ticket) = query_param(request.uri().query(), "ticket").filter(|t| !t.is_empty())
    else {
        return true;
    };
    let valid = request_origin_allowed(request.headers(), &ctx.cors)
        && ctx
            .pty_tickets
            .consume(&ticket, &ticket_scope(pty_id, location));
    valid
}

async fn upgrade(
    ctx: Arc<ServerContext>,
    service: Arc<PtyService>,
    pty_id: String,
    cursor: Option<i64>,
    request: Request,
    surface: Surface,
) -> Response {
    let (mut parts, _) = request.into_parts();
    match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Err(_) => crate::error::defect_response(),
        Ok(upgrade) => upgrade.on_upgrade(move |socket| async move {
            run_connect(ctx, service, pty_id, cursor, socket, surface).await
        }),
    }
}

async fn run_connect(
    ctx: Arc<ServerContext>,
    service: Arc<PtyService>,
    pty_id: String,
    cursor: Option<i64>,
    socket: WebSocket,
    surface: Surface,
) {
    let (outbox_tx, mut outbox_rx) = tokio::sync::mpsc::unbounded_channel::<OutFrame>();
    let mut registered: Option<u64> = None;
    if surface == Surface::V1 {
        // v1 registers the socket in the websocket tracker
        // (`handlers/pty.ts:217-221`); a rejected registration means the
        // server is closing.
        let tracker_tx = outbox_tx.clone();
        registered = match ctx.websockets.register(Box::new(move || {
            let _ = tracker_tx.send(OutFrame::Close(1001, "server closing".to_string()));
        })) {
            Some(id) => Some(id),
            None => {
                send_close(socket, 1001, "server closing").await;
                return;
            }
        };
    }
    let attachment = service.attach(
        &pty_id,
        cursor,
        Box::new({
            let outbox = outbox_tx.clone();
            move |chunk| {
                let _ = outbox.send(OutFrame::Text(chunk.to_string()));
            }
        }),
        Box::new({
            let outbox = outbox_tx.clone();
            move |_| {
                let _ = outbox.send(OutFrame::Close(1000, String::new()));
            }
        }),
    );
    let attachment = match attachment {
        Ok(attachment) => attachment,
        Err(PtyError::NotFound) => {
            send_close(socket, 4404, "session not found").await;
            if let Some(id) = registered {
                ctx.websockets.unregister(id);
            }
            return;
        }
        Err(PtyError::Exited) => {
            let reason = match surface {
                Surface::V1 => "session not found",
                Surface::V2 => "session exited",
            };
            send_close(socket, 4404, reason).await;
            if let Some(id) = registered {
                ctx.websockets.unregister(id);
            }
            return;
        }
    };
    for chunk in protocol::chunks(&attachment.replay) {
        let _ = outbox_tx.send(OutFrame::Text(chunk));
    }
    let _ = outbox_tx.send(OutFrame::Binary(protocol::meta_frame(attachment.cursor)));
    attachment.activate();

    let (mut sink, stream) = socket.split();
    let drain = async {
        while let Some(frame) = outbox_rx.recv().await {
            let message = match frame {
                OutFrame::Text(text) => Message::Text(text.into()),
                OutFrame::Binary(bytes) => Message::Binary(bytes.into()),
                OutFrame::Close(code, reason) => {
                    if sink
                        .send(Message::Close(Some(CloseFrame {
                            code,
                            reason: reason.into(),
                        })))
                        .await
                        .is_err()
                    {
                        break;
                    }
                    return;
                }
            };
            if sink.send(message).await.is_err() {
                break;
            }
        }
    };
    let read = async {
        let mut stream = stream;
        while let Some(Ok(message)) = stream.next().await {
            match message {
                Message::Text(text) => {
                    let _ = service.write(&pty_id, &text);
                }
                Message::Binary(bytes) => {
                    if let Some(text) = protocol::decode_input(&bytes) {
                        let _ = service.write(&pty_id, &text);
                    }
                }
                _ => {}
            }
        }
    };
    let _ = futures::future::select(Box::pin(drain), Box::pin(read)).await;
    attachment.detach();
    if let Some(id) = registered {
        ctx.websockets.unregister(id);
    }
}

async fn send_close(mut socket: WebSocket, code: u16, reason: &str) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.to_string().into(),
        })))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_number_matrix() {
        assert_eq!(js_number(""), 0.0);
        assert_eq!(js_number("12"), 12.0);
        assert_eq!(js_number(" 12 "), 12.0);
        assert_eq!(js_number("1e2"), 100.0);
        assert_eq!(js_number("0x10"), 16.0);
        assert_eq!(js_number("+0x10"), 16.0);
        assert_eq!(js_number("-0x10"), -16.0);
        assert!(js_number("abc").is_nan());
    }

    #[test]
    fn cursor_gate() {
        assert_eq!(parsed_cursor(Some("cursor=5")), Some(5));
        assert_eq!(parsed_cursor(Some("cursor=-1")), Some(-1));
        assert_eq!(parsed_cursor(Some("cursor=1e2")), Some(100));
        assert_eq!(parsed_cursor(Some("cursor=0x10")), Some(16));
        assert_eq!(parsed_cursor(Some("cursor=-5")), None);
        assert_eq!(parsed_cursor(Some("cursor=1.5")), None);
        assert_eq!(parsed_cursor(Some("cursor=abc")), None);
        assert_eq!(parsed_cursor(None), None);
        assert_eq!(parsed_cursor(Some("other=1")), None);
    }

    #[test]
    fn repeated_query_params_fail_the_cursor_query() {
        assert!(cursor_query_valid(&Uri::from_static("/x?cursor=1")));
        assert!(cursor_query_valid(&Uri::from_static(
            "/x?cursor=1&directory=/repo"
        )));
        assert!(!cursor_query_valid(&Uri::from_static(
            "/x?cursor=a&cursor=b"
        )));
        assert!(!cursor_query_valid(&Uri::from_static(
            "/x?directory=a&directory=b&cursor=1"
        )));
        assert!(!cursor_query_valid(&Uri::from_static(
            "/x?workspace=a&workspace=b"
        )));
    }
}

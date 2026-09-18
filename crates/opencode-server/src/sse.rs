//! SSE event streams — M6.4 port of `handlers/event.ts` (v1 `/event`),
//! `handlers/global.ts:25-58` (`/global/event`),
//! `packages/server/src/handlers/event.ts` (v2 `/api/event`) and the v2
//! session-scoped durable stream (`protocol/groups/session.ts:327-343`),
//! plus the `GlobalBus` bridge (`bus/global.ts`, `event-v2-bridge.ts`).

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use opencode_core::{new_event_id, versioned_type, EventBus, Payload, Subscription};
use opencode_schema::event_manifest::SERVER_HEARTBEAT_TYPE;
use serde::Serialize;
use tokio::sync::{broadcast, mpsc};

use crate::error::ApiError;
use crate::middleware::auth::query_param;
use crate::middleware::location::LocationContext;
use crate::state::{resolve_directory, ServerContext};

/// v1 `server.connected` (`handlers/event.ts:70`).
pub const SERVER_CONNECTED_TYPE: &str = "server.connected";
/// `server.instance.disposed` (`server/event.ts:1-10`).
pub const INSTANCE_DISPOSED_TYPE: &str = "server.instance.disposed";
/// `EventV2.allBounded(events, 256)` (`packages/server/src/handlers/event.ts:9`).
pub const SUBSCRIBER_CAPACITY: usize = 256;

// ------------------------------------------------------------ GlobalBus

/// The `syncEvent` payload of the bridge's durable mirror frame
/// (`event-v2-bridge.ts:42-54`). Field order matches the TS object literal.
#[derive(Debug, Clone, Serialize)]
pub struct SyncEvent {
    pub id: String,
    pub r#type: String,
    pub seq: i64,
    #[serde(rename = "aggregateID")]
    pub aggregate_id: String,
    pub data: serde_json::Value,
}

/// One `GlobalBus` payload. The three layouts are the exact wire key orders
/// TS produces — `JSON.stringify` emits the literal's keys in insertion
/// order, and the emitter appends a missing `id` *last*
/// (`bus/global.ts:9-17`).
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum GlobalPayload {
    /// `{ id, type, properties }` — bridge regular frame
    /// (`event-v2-bridge.ts:37-41`) and the v1 connected/heartbeat frames
    /// (`handlers/event.ts:70`, `:65`).
    Event {
        id: String,
        r#type: String,
        properties: serde_json::Value,
    },
    /// `{ type, syncEvent, id }` — the bridge's durable mirror; the emitter
    /// assigns `payload.id = syncEvent.id`.
    Sync {
        r#type: String,
        #[serde(rename = "syncEvent")]
        sync_event: SyncEvent,
        id: String,
    },
    /// `{ type, properties, id }` — server-injected frames
    /// (`instance-store.ts:79-93`, `global-lifecycle.ts:6-13`); the emitter
    /// assigns a fresh id after the literal.
    Injected {
        r#type: String,
        properties: serde_json::Value,
        id: String,
    },
}

/// One `GlobalBus` event (`bus/global.ts:3-9`).
#[derive(Debug, Clone, Serialize)]
pub struct GlobalEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    pub payload: GlobalPayload,
}

impl GlobalEvent {
    /// A server-injected `{type, properties}` event; the emitter assigns the
    /// id (`GlobalBusEmitter.emit`, `bus/global.ts:9-17`).
    pub fn injected(
        directory: Option<String>,
        project: Option<String>,
        workspace: Option<String>,
        payload_type: &str,
        properties: serde_json::Value,
    ) -> GlobalEvent {
        GlobalEvent {
            directory,
            project,
            workspace,
            payload: GlobalPayload::Injected {
                r#type: payload_type.to_string(),
                properties,
                id: new_event_id(),
            },
        }
    }
}

/// The process-wide event bus feeding `/global/event` and the v1 `/event`
/// disposal terminator — TS `bus/global.ts` (an `EventEmitter` singleton)
/// fed by the `EventV2Bridge` listener (`event-v2-bridge.ts:26-62`).
#[derive(Clone)]
pub struct GlobalBus {
    tx: broadcast::Sender<GlobalEvent>,
    /// Keeps the core-bus bridge listener alive while any handle exists.
    bridge: Option<Arc<Subscription>>,
    /// Retained subscriptions of chained per-instance buses (M7.1).
    chains: Arc<std::sync::Mutex<Vec<Arc<Subscription>>>>,
}

impl Default for GlobalBus {
    fn default() -> Self {
        Self::new()
    }
}

impl GlobalBus {
    pub fn new() -> GlobalBus {
        GlobalBus {
            tx: broadcast::channel(opencode_core::event::bus::CHANNEL_CAPACITY).0,
            bridge: None,
            chains: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// A bus bridged to the core EventV2 bus: every published event is
    /// mirrored as a regular global frame, and every durable event
    /// additionally as a `sync` frame (`event-v2-bridge.ts:26-62`).
    pub fn bridged(bus: Arc<EventBus>) -> GlobalBus {
        let mut global = GlobalBus::new();
        let bridged = global.clone();
        let subscription = bus.listen(Arc::new(move |event| {
            for frame in bridge_frames(event) {
                bridged.emit(frame);
            }
        }));
        global.bridge = Some(Arc::new(subscription));
        global
    }

    /// Bridge an additional (per-instance) EventV2 bus into the global bus
    /// — every instance publishes onto its own bus in the Rust port, and
    /// the bridge listener is the TS process-wide EventV2's listener.
    pub fn chain(&self, bus: Arc<EventBus>) {
        let chained = self.clone();
        let subscription = bus.listen(Arc::new(move |event| {
            for frame in bridge_frames(event) {
                chained.emit(frame);
            }
        }));
        self.chains
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(Arc::new(subscription));
    }

    /// `GlobalBus.emit` (global.ts:9-17) — ids are assigned at construction
    /// (see `GlobalPayload`), so this only fans out.
    pub fn emit(&self, event: GlobalEvent) {
        let _ = self.tx.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<GlobalEvent> {
        self.tx.subscribe()
    }

    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

/// The `EventV2Bridge` listener body (`event-v2-bridge.ts:26-62`): the
/// directory comes from the event location (TS falls back to the ambient
/// instance context — Rust instances always inject the ambient location at
/// publish time), the project from the location's project field.
fn bridge_frames(event: &Payload) -> Vec<GlobalEvent> {
    let directory = event.location.as_ref().map(|l| l.directory.clone());
    let workspace = event.location.as_ref().and_then(|l| l.workspace_id.clone());
    let project = event
        .location
        .as_ref()
        .and_then(|l| l.project.as_ref().map(|p| p.id.clone()));
    let mut out = vec![GlobalEvent {
        directory: directory.clone(),
        project,
        workspace: workspace.clone(),
        payload: GlobalPayload::Event {
            id: event.id.clone(),
            r#type: event.r#type.clone(),
            properties: event.data.clone(),
        },
    }];
    if let Some(durable) = &event.durable {
        let sync = SyncEvent {
            id: event.id.clone(),
            r#type: versioned_type(&event.r#type, durable.version as u32),
            seq: durable.seq,
            aggregate_id: durable.aggregate_id.clone(),
            data: event.data.clone(),
        };
        out.push(GlobalEvent {
            directory,
            project: event
                .location
                .as_ref()
                .and_then(|l| l.project.as_ref().map(|p| p.id.clone())),
            workspace,
            payload: GlobalPayload::Sync {
                r#type: "sync".to_string(),
                sync_event: sync.clone(),
                id: sync.id,
            },
        });
    }
    out
}

// ------------------------------------------------------------ SSE framing

/// One SSE frame as encoded by effect's `Sse.encoder.write`
/// (`encoding/Sse.ts:556-572`): no `id:` line (ids are `undefined`), no
/// `event:` line (the encoder omits `event: message`), embedded newlines
/// continued with `data: ` prefixes.
fn sse_frame(data: &str) -> String {
    if data.is_empty() {
        return "\n".to_string();
    }
    let mut out = String::from("data: ");
    out.push_str(&data.replace('\n', "\ndata: "));
    out.push_str("\n\n");
    out
}

/// `{ id, type, properties }` — the legacy event wire shape
/// (`eventData`, handlers/event.ts:12-19).
fn legacy_frame(id: &str, event_type: &str, properties: serde_json::Value) -> String {
    let payload = GlobalPayload::Event {
        id: id.to_string(),
        r#type: event_type.to_string(),
        properties,
    };
    serde_json::to_string(&payload).expect("payload serialization cannot fail")
}

/// `HttpServerResponse.stream(…, { contentType: "text/event-stream", headers })`
/// (handlers/event.ts:77-84, global.ts:48-55,
/// packages/server/src/handlers/event.ts:40-47).
fn sse_response(rx: mpsc::Receiver<Bytes>) -> Response {
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|bytes| (Ok::<Bytes, std::convert::Infallible>(bytes), rx))
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache, no-transform")
        .header("x-accel-buffering", "no")
        .header("x-content-type-options", "nosniff")
        .body(Body::from_stream(stream))
        .expect("static response parts are valid")
}

// ------------------------------------------------------------ v1 /event

/// The v1 `/event` filter (handlers/event.ts:36-41): the event location
/// must match the instance directory, and an event workspaceID must match
/// the resolved workspace.
fn v1_event_matches(event: &GlobalEvent, directory: &str, workspace_id: &Option<String>) -> bool {
    if event.directory.as_deref() != Some(directory) {
        return false;
    }
    match &event.workspace {
        None => true,
        Some(workspace) => Some(workspace) == workspace_id.as_ref(),
    }
}

/// The `/event` disposal terminator (handlers/event.ts:42-62): a
/// `server.instance.disposed` global event for this directory ends the
/// stream after being framed.
fn v1_disposed_frame(event: &GlobalEvent, directory: &str) -> Option<String> {
    if event.directory.as_deref() != Some(directory) {
        return None;
    }
    let (id, payload_type, properties) = match &event.payload {
        GlobalPayload::Event {
            id,
            r#type,
            properties,
        } => (id, r#type, properties),
        GlobalPayload::Injected {
            id,
            r#type,
            properties,
        } => (id, r#type, properties),
        GlobalPayload::Sync { .. } => return None,
    };
    if payload_type != INSTANCE_DISPOSED_TYPE {
        return None;
    }
    Some(legacy_frame(id, INSTANCE_DISPOSED_TYPE, properties.clone()))
}

/// `GET /event` (handlers/event.ts:25-87).
pub async fn v1_event(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Response {
    // Listener registration is eager, so events published after this point
    // cannot be lost while the HTTP body fiber is starting or emitting
    // server.connected (handlers/event.ts:30-33).
    //
    // The stream consumes the global bus, which mirrors every EventV2
    // publish — the server bus and each per-instance bus — with
    // `directory` and `project` filled from the publish location (M7.1).
    let mut events = ctx.global_bus.subscribe();
    let directory = resolve_directory(&location.directory).display().to_string();
    let workspace_id = location.workspace_id;
    let interval = ctx.heartbeat.v1;

    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        let connected = sse_frame(&legacy_frame(
            &new_event_id(),
            SERVER_CONNECTED_TYPE,
            serde_json::json!({}),
        ));
        if tx.send(connected.into()).await.is_err() {
            return;
        }

        // `Stream.tick("10 seconds").pipe(Stream.drop(1))` — the first tick
        // fires immediately, so heartbeats start one full interval in.
        let mut heartbeat = Box::pin(tokio::time::sleep(interval));
        loop {
            tokio::select! {
                received = events.recv() => match received {
                    Ok(event) => {
                        if !v1_event_matches(&event, &directory, &workspace_id) {
                            continue;
                        }
                        if let GlobalPayload::Event {
                            id,
                            r#type,
                            properties,
                        } = &event.payload
                        {
                            let frame = sse_frame(&legacy_frame(id, r#type, properties.clone()));
                            if tx.send(frame.into()).await.is_err() {
                                return;
                            }
                        }
                        if let Some(frame) = v1_disposed_frame(&event, &directory) {
                            if tx.send(sse_frame(&frame).into()).await.is_err() {
                                return;
                            }
                            // `Stream.takeUntil` — the stream ends after the
                            // disposed frame (handlers/event.ts:61).
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return,
                },
                _ = &mut heartbeat => {
                    let frame = sse_frame(&legacy_frame(
                        &new_event_id(),
                        SERVER_HEARTBEAT_TYPE,
                        serde_json::json!({}),
                    ));
                    if tx.send(frame.into()).await.is_err() {
                        return;
                    }
                    heartbeat = Box::pin(tokio::time::sleep(interval));
                }
            }
        }
    });
    sse_response(rx)
}

// ------------------------------------------------------ v1 /global/event

/// `GET /global/event` (handlers/global.ts:25-58).
pub async fn global_event(State(ctx): State<Arc<ServerContext>>) -> Response {
    let mut events = ctx.global_bus.subscribe();
    let interval = ctx.heartbeat.v1;

    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        let connected = GlobalEvent {
            directory: None,
            project: None,
            workspace: None,
            payload: GlobalPayload::Event {
                id: new_event_id(),
                r#type: SERVER_CONNECTED_TYPE.to_string(),
                properties: serde_json::json!({}),
            },
        };
        if tx
            .send(sse_frame(&serde_json::to_string(&connected).unwrap()).into())
            .await
            .is_err()
        {
            return;
        }

        let mut heartbeat = Box::pin(tokio::time::sleep(interval));
        loop {
            tokio::select! {
                received = events.recv() => match received {
                    Ok(event) => {
                        let frame = sse_frame(&serde_json::to_string(&event).unwrap());
                        if tx.send(frame.into()).await.is_err() {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return,
                },
                _ = &mut heartbeat => {
                    let heartbeat_event = GlobalEvent {
                        directory: None,
                        project: None,
                        workspace: None,
                        payload: GlobalPayload::Event {
                            id: new_event_id(),
                            r#type: SERVER_HEARTBEAT_TYPE.to_string(),
                            properties: serde_json::json!({}),
                        },
                    };
                    let frame = sse_frame(&serde_json::to_string(&heartbeat_event).unwrap());
                    if tx.send(frame.into()).await.is_err() {
                        return;
                    }
                    heartbeat = Box::pin(tokio::time::sleep(interval));
                }
            }
        }
    });
    sse_response(rx)
}

// ------------------------------------------------------------ v2 /api/event

/// `GET /api/event` (packages/server/src/handlers/event.ts).
pub async fn api_event(State(ctx): State<Arc<ServerContext>>) -> Response {
    // `EventV2.allBounded(events, subscriberCapacity)` (event.ts:152-163):
    // a dropping queue — once it is full the queue fails permanently and the
    // stream ends. Acquiring the bounded stream installs its listener
    // before readiness is observable (handlers/event.ts:32-35).
    let (event_tx, mut events) = mpsc::channel::<Payload>(SUBSCRIBER_CAPACITY);
    let event_tx = Arc::new(std::sync::Mutex::new(Some(event_tx)));
    let listener_tx = Arc::clone(&event_tx);
    let subscription = ctx.bus.listen(Arc::new(move |event| {
        let mut sender = listener_tx.lock().unwrap_or_else(|p| p.into_inner());
        let failed = match sender.as_ref() {
            Some(sender) => sender.try_send(event.clone()).is_err(),
            None => false,
        };
        if failed {
            *sender = None;
        }
    }));

    let interval = ctx.heartbeat.v2;
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        // Hold the subscription (and the listener's sender) for the
        // lifetime of the stream — dropping them on disconnect
        // unsubscribes.
        let _subscription = subscription;
        let _listener_sender = event_tx;

        let connected = Payload {
            id: new_event_id(),
            metadata: None,
            r#type: SERVER_CONNECTED_TYPE.to_string(),
            durable: None,
            location: None,
            data: serde_json::json!({}),
        };
        if tx
            .send(sse_frame(&serde_json::to_string(&connected).unwrap()).into())
            .await
            .is_err()
        {
            return;
        }
        // `Stream.tick("15 seconds")` without a drop — the first tick fires
        // immediately (packages/server/src/handlers/event.ts:37).
        if tx.send(": heartbeat\n\n".into()).await.is_err() {
            return;
        }

        let mut heartbeat = Box::pin(tokio::time::sleep(interval));
        loop {
            tokio::select! {
                event = events.recv() => match event {
                    Some(event) => {
                        let frame = sse_frame(&serde_json::to_string(&event).unwrap());
                        if tx.send(frame.into()).await.is_err() {
                            return;
                        }
                    }
                    None => return, // subscriber overflow — the queue failed
                },
                _ = &mut heartbeat => {
                    if tx.send(": heartbeat\n\n".into()).await.is_err() {
                        return;
                    }
                    heartbeat = Box::pin(tokio::time::sleep(interval));
                }
            }
        }
    });
    sse_response(rx)
}

// --------------------------------------------- v2 /api/session/:sessionID/event

/// `?after=` query — `NumberFromString` decoded to a non-negative integer
/// (`protocol/groups/session.ts:330-333`).
fn parse_after(query: Option<&str>) -> Result<Option<i64>, String> {
    match query_param(query, "after") {
        None => Ok(None),
        Some(raw) => match raw.parse::<i64>() {
            Ok(after) if after >= 0 => Ok(Some(after)),
            // TODO(M6.9): pin the exact Effect issue-message format against
            // a captured TS response.
            _ => Err(format!("Expected a non-negative integer, but got {raw}")),
        },
    }
}

/// `GET /api/session/:sessionID/event` (`protocol/groups/session.ts:327-343`):
/// replay durable session events with `seq > after`, then continue live —
/// no `server.connected`, no heartbeat.
pub async fn api_session_event(
    State(ctx): State<Arc<ServerContext>>,
    Path(session_id): Path<String>,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
) -> Response {
    let after = match parse_after(uri.query()) {
        Ok(after) => after,
        Err(message) => {
            return ApiError::InvalidRequest {
                message,
                kind: Some("Query".to_string()),
                field: None,
            }
            .into_response()
        }
    };
    let mut events = ctx.bus.durable_stream(&session_id, after);
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        loop {
            match events.next().await {
                Ok(Some(event)) => {
                    let frame = sse_frame(&serde_json::to_string(&event).unwrap());
                    if tx.send(frame.into()).await.is_err() {
                        return;
                    }
                }
                Ok(None) | Err(_) => return,
            }
        }
    });
    sse_response(rx)
}

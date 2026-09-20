//! SSE event source — `transport::events` (M8.1).
//!
//! Port of `context/sdk.tsx:48-117` (the SSE reconnect loop, the 16 ms
//! event coalescer and the retry backoff) plus `context/event.ts`
//! (`sync`-frame filtering and `{ directory, workspace }` metadata).
//!
//! One delivered batch = one `Msg::Bus(Vec<BusEvent>)`: a whole coalescer
//! flush applies in a single `update()` pass, the TS `batch()` equivalent
//! (`sdk.tsx:60-66`).

use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

use opencode_schema::event::LegacyEnvelope;
use opencode_schema::event_manifest::Event;

use crate::transport::api::{percent_encode, HttpClientConfig, Location};

/// `sdk.tsx:68-80` — if the last flush was < 16 ms ago, events batch on a
/// 16 ms timer; otherwise they flush immediately.
pub const COALESCE_WINDOW_MS: u64 = 16;
/// `sdk.tsx:51` (`retryDelay`).
pub const RETRY_DELAY_MS: u64 = 1000;
/// `sdk.tsx:52` (`maxRetryDelay`).
pub const MAX_RETRY_DELAY_MS: u64 = 30000;

/// `EventMetadata` (`event.ts:4-7`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventMetadata {
    pub directory: Option<String>,
    pub workspace: Option<String>,
}

/// One event delivered to the store — `event.ts:12-19`: a non-`sync` bus
/// event plus the `{directory, workspace}` metadata of its frame.
#[derive(Debug, Clone, PartialEq)]
pub struct BusEvent {
    pub event: Event,
    pub metadata: EventMetadata,
}

/// Parse one `/global/event` SSE data frame.
///
/// `sync` frames (the durable-event mirror, `event.ts:13-17`) and frames
/// whose payload is not in the `Event` union — e.g. the wire-only
/// `server.heartbeat` (`M1 event_manifest::SERVER_HEARTBEAT_TYPE`) or
/// unknown future types — yield `None`: in TS they flow through the bus
/// and reach no handler.
pub fn parse_global_frame(data: &str) -> Option<BusEvent> {
    let frame: Value = serde_json::from_str(data).ok()?;
    let payload = frame.get("payload")?;
    if payload.get("type").and_then(Value::as_str) == Some("sync") {
        return None;
    }
    let envelope: LegacyEnvelope<Event> = serde_json::from_value(payload.clone()).ok()?;
    Some(BusEvent {
        event: envelope.event,
        metadata: EventMetadata {
            directory: frame
                .get("directory")
                .and_then(Value::as_str)
                .map(str::to_string),
            workspace: frame
                .get("workspace")
                .and_then(Value::as_str)
                .map(str::to_string),
        },
    })
}

/// `Math.min(retryDelay * 2 ** (attempt - 1), maxRetryDelay)`
/// (`sdk.tsx:113`) — 1s, 2s, 4s, … capped at 30 s.
pub fn backoff_delay(attempt: u32) -> u64 {
    let factor = 2u64.saturating_pow(attempt.max(1) - 1);
    RETRY_DELAY_MS
        .saturating_mul(factor)
        .min(MAX_RETRY_DELAY_MS)
}

/// The outcome of [`Coalescer::push`].
#[derive(Debug, PartialEq)]
pub enum Push {
    /// The batch flushed inside the push.
    Delivered(Vec<BusEvent>),
    /// The event queued; the timer deadline is [`Coalescer::deadline_ms`].
    Queued,
}

/// The 16 ms coalescing queue (`sdk.tsx:54-80`). Pure state machine over
/// injected timestamps, so the boundary tests are synchronous.
#[derive(Debug, Default)]
pub struct Coalescer {
    queue: Vec<BusEvent>,
    deadline_ms: Option<u64>,
    last_flush_ms: u64,
}

impl Coalescer {
    pub fn new() -> Coalescer {
        Coalescer::default()
    }

    /// `handleEvent` (`sdk.tsx:68-80`). `now_ms` is the injected
    /// `Date.now()`.
    pub fn push(&mut self, event: BusEvent, now_ms: u64) -> Push {
        self.queue.push(event);
        if self.deadline_ms.is_some() {
            return Push::Queued;
        }
        let elapsed = now_ms.saturating_sub(self.last_flush_ms);
        if elapsed < COALESCE_WINDOW_MS {
            self.deadline_ms = Some(now_ms + COALESCE_WINDOW_MS);
            Push::Queued
        } else {
            Push::Delivered(self.flush(now_ms))
        }
    }

    /// `flush` (`sdk.tsx:54-66`): take the queue, clear the timer, stamp
    /// the flush time. Empty batches do not restamp (the TS early return).
    pub fn flush(&mut self, now_ms: u64) -> Vec<BusEvent> {
        self.deadline_ms = None;
        if self.queue.is_empty() {
            return Vec::new();
        }
        self.last_flush_ms = now_ms;
        std::mem::take(&mut self.queue)
    }

    /// The pending `setTimeout(flush, 16)` deadline, if any.
    pub fn deadline_ms(&self) -> Option<u64> {
        self.deadline_ms
    }
}

/// Injectable time source for the event driver — production is tokio
/// time; tests drive a manual clock.
#[async_trait]
pub trait Clock: Send + Sync {
    /// Milliseconds since an arbitrary epoch (TS `Date.now()`).
    fn now_ms(&self) -> u64;
    /// TS `setTimeout` sleep.
    async fn sleep(&self, ms: u64);
}

/// Production [`Clock`] — tokio time.
pub struct TokioClock {
    start: Instant,
}

impl TokioClock {
    pub fn new() -> TokioClock {
        TokioClock {
            start: Instant::now(),
        }
    }
}

impl Default for TokioClock {
    fn default() -> Self {
        TokioClock::new()
    }
}

#[async_trait]
impl Clock for TokioClock {
    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    async fn sleep(&self, ms: u64) {
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
    }
}

/// `Flag.OPENCODE_EXPERIMENTAL_WORKSPACES` (`core/flag/flag.ts`): read at
/// construction, like the TS module-load read.
pub fn experimental_workspaces_from_env() -> bool {
    fn truthy(key: &str) -> bool {
        let value = std::env::var(key).unwrap_or_default().to_lowercase();
        value == "true" || value == "1"
    }
    match std::env::var("OPENCODE_EXPERIMENTAL_WORKSPACES") {
        Ok(_) => truthy("OPENCODE_EXPERIMENTAL_WORKSPACES"),
        Err(_) => truthy("OPENCODE_EXPERIMENTAL"),
    }
}

/// The event source seam — `sdk.global.event({…})` for the SSE transport
/// (`EventSource` in the spec §2.3 seam table). Tests feed a channel-backed
/// double instead of a live connection.
#[async_trait]
pub trait EventSource: Send + Sync {
    /// One `/global/event` connection. Events flow into `tx` until the
    /// stream ends (its sender drops). Resolves once the connection is
    /// established; errors mean the connection failed — the SSE gen client
    /// with `sseMaxRetryAttempts: 0` ends the stream on connect errors
    /// the same way (`serverSentEvents.gen.ts:211-223`).
    async fn stream(&self, tx: UnboundedSender<BusEvent>) -> Result<()>;
    /// Best-effort `sdk.sync.start()` (`sdk.tsx:96-100`).
    async fn sync_start(&self) -> Result<()>;
    /// `Flag.OPENCODE_EXPERIMENTAL_WORKSPACES`, read at construction.
    fn experimental_workspaces(&self) -> bool;
}

/// Production [`EventSource`]: one GET per connection to
/// `{base_url}/global/event`, framed by the `eventsource-stream` crate.
pub struct SseEventSource {
    client: reqwest::Client,
    config: HttpClientConfig,
    experimental_workspaces: bool,
}

impl SseEventSource {
    pub fn new(config: HttpClientConfig) -> Result<SseEventSource> {
        Ok(SseEventSource {
            client: reqwest::Client::builder().build()?,
            experimental_workspaces: experimental_workspaces_from_env(),
            config,
        })
    }

    fn request(&self, path: &str) -> Result<reqwest::RequestBuilder> {
        let url = crate::transport::api::build_url(
            &self.config,
            path,
            &reqwest::Method::GET,
            &Location::default(),
            &[],
        );
        let mut request = self.client.get(url);
        for (name, value) in &self.config.headers {
            request = request.header(name.as_str(), value.as_str());
        }
        Ok(request)
    }
}

#[async_trait]
impl EventSource for SseEventSource {
    async fn stream(&self, tx: UnboundedSender<BusEvent>) -> Result<()> {
        let response = self
            .request("/global/event")?
            .send()
            .await?
            .error_for_status()?;
        let stream = response.bytes_stream().eventsource();
        tokio::spawn(async move {
            let mut stream = stream;
            while let Some(event) = stream.next().await {
                match event {
                    Ok(event) => {
                        if let Some(bus_event) = parse_global_frame(&event.data) {
                            if tx.send(bus_event).is_err() {
                                return;
                            }
                        }
                    }
                    Err(_) => return,
                }
            }
        });
        Ok(())
    }

    async fn sync_start(&self) -> Result<()> {
        let url = crate::transport::api::build_url(
            &self.config,
            "/sync/start",
            &reqwest::Method::POST,
            &Location::default(),
            &[],
        );
        let mut request = self.client.post(url);
        if let Some(directory) = self
            .config
            .directory
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            request = request.header("x-opencode-directory", percent_encode(directory));
        }
        for (name, value) in &self.config.headers {
            request = request.header(name.as_str(), value.as_str());
        }
        request
            .send()
            .await?
            .error_for_status()
            .map(|_| ())
            .map_err(Into::into)
    }

    fn experimental_workspaces(&self) -> bool {
        self.experimental_workspaces
    }
}

/// The reconnect loop (`startSSE`, `sdk.tsx:82-117`): stream end →
/// coalescer flush → backoff → reconnect. One attempt counter for the
/// process lifetime (TS never resets it). Stops when the sink closes
/// (the app shut down).
async fn run_event_loop(
    source: Arc<dyn EventSource>,
    clock: Arc<dyn Clock>,
    sink: UnboundedSender<Vec<BusEvent>>,
) {
    let mut attempt: u32 = 0;
    loop {
        pump_connection(&source, &clock, &sink).await;
        if sink.is_closed() {
            return;
        }
        attempt += 1;
        let backoff = backoff_delay(attempt);
        clock.sleep(backoff).await;
    }
}

/// One connection lifetime: connect → best-effort `sync.start` → the
/// coalescing pump until the stream ends.
async fn pump_connection(
    source: &Arc<dyn EventSource>,
    clock: &Arc<dyn Clock>,
    sink: &UnboundedSender<Vec<BusEvent>>,
) {
    let (tx, mut rx) = unbounded_channel::<BusEvent>();
    if source.stream(tx).await.is_err() {
        return;
    }
    if source.experimental_workspaces() {
        let _ = source.sync_start().await;
    }
    let mut coalescer = Coalescer::new();
    loop {
        let deadline = coalescer.deadline_ms();
        let timer = async {
            match deadline {
                Some(deadline) => clock.sleep(deadline.saturating_sub(clock.now_ms())).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            biased;
            () = timer => {
                let batch = coalescer.flush(clock.now_ms());
                if !batch.is_empty() && sink.send(batch).is_err() {
                    return;
                }
            }
            event = rx.recv() => match event {
                Some(event) => match coalescer.push(event, clock.now_ms()) {
                    Push::Delivered(batch) => {
                        if sink.send(batch).is_err() {
                            return;
                        }
                    }
                    Push::Queued => {}
                },
                // Stream ended (`sdk.tsx:107-108`): flush, then back off.
                None => {
                    let batch = coalescer.flush(clock.now_ms());
                    if !batch.is_empty() {
                        let _ = sink.send(batch);
                    }
                    return;
                }
            }
        }
    }
}

/// Spawn the SSE task (`transport::events`, spec §2.2). Dropping the
/// returned handle detaches; abort it on shutdown (TS tears the loop down
/// by aborting its controllers).
pub fn spawn_event_loop(
    source: Arc<dyn EventSource>,
    clock: Arc<dyn Clock>,
    sink: UnboundedSender<Vec<BusEvent>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run_event_loop(source, clock, sink))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn connected_frame() -> String {
        json!({
            "payload": {
                "id": "evt_1",
                "type": "server.connected",
                "properties": {},
            },
        })
        .to_string()
    }

    #[test]
    fn parses_event_and_metadata() {
        let frame = json!({
            "directory": "/repo",
            "workspace": "wrk_1",
            "payload": {
                "id": "evt_1",
                "type": "models-dev.refreshed",
                "properties": {},
            },
        })
        .to_string();
        let event = parse_global_frame(&frame).expect("parses");
        assert_eq!(
            event.metadata,
            EventMetadata {
                directory: Some("/repo".to_string()),
                workspace: Some("wrk_1".to_string()),
            }
        );
        assert!(matches!(event.event, Event::ModelsDevRefreshed(_)));
    }

    #[test]
    fn sync_frames_are_filtered() {
        // The durable mirror frame from `event-v2-bridge.ts` (event.ts:13-17).
        let frame = json!({
            "payload": {
                "type": "sync",
                "syncEvent": {
                    "id": "evt_1",
                    "type": "session.created.1",
                    "seq": 0,
                    "aggregateID": "ses_1",
                    "data": {},
                },
                "id": "evt_1",
            },
        })
        .to_string();
        assert!(parse_global_frame(&frame).is_none());
    }

    #[test]
    fn heartbeat_and_unknown_frames_are_dropped() {
        for payload in [
            json!({"id": "evt_1", "type": "server.heartbeat", "properties": {}}),
            json!({"id": "evt_1", "type": "future.event", "properties": {}}),
        ] {
            let frame = json!({"payload": payload}).to_string();
            assert!(parse_global_frame(&frame).is_none());
        }
        let malformed = "not json at all";
        assert!(parse_global_frame(malformed).is_none());
    }

    #[test]
    fn coalescer_immediately_flushes_after_the_window() {
        let mut coalescer = Coalescer::new();
        let event = || parse_global_frame(&connected_frame()).expect("frame");

        // last_flush starts at 0 — the first event at t>=16 flushes.
        let push = coalescer.push(event(), 100);
        assert!(matches!(push, Push::Delivered(batch) if batch.len() == 1));
        // The flush restamps: t=101 is within 16 ms of t=100.
        match coalescer.push(event(), 101) {
            Push::Queued => {}
            _ => panic!("expected deferral within the window"),
        }
        assert_eq!(coalescer.deadline_ms(), Some(101 + COALESCE_WINDOW_MS));
    }

    #[test]
    fn coalescer_batches_until_the_deadline_and_keeps_it() {
        let mut coalescer = Coalescer::new();
        let event = || parse_global_frame(&connected_frame()).expect("frame");

        assert!(matches!(coalescer.push(event(), 0), Push::Queued));
        assert_eq!(coalescer.deadline_ms(), Some(16));
        // A second event before the deadline does not extend it.
        assert!(matches!(coalescer.push(event(), 5), Push::Queued));
        assert_eq!(coalescer.deadline_ms(), Some(16));

        let batch = coalescer.flush(16);
        assert_eq!(batch.len(), 2);
        assert_eq!(coalescer.deadline_ms(), None);

        // The next event is < 16 ms after the last flush (sdk.tsx:75).
        assert!(matches!(coalescer.push(event(), 20), Push::Queued));
        let batch = coalescer.flush(36);
        assert_eq!(batch.len(), 1);
    }

    #[test]
    fn coalescer_empty_flush_does_not_restamp() {
        let mut coalescer = Coalescer::new();
        assert!(coalescer.flush(5000).is_empty());
        // last_flush was not restamped, so t=10 is still < 16 ms after 0.
        assert!(matches!(
            coalescer.push(parse_global_frame(&connected_frame()).expect("frame"), 10),
            Push::Queued
        ));
    }

    #[test]
    fn backoff_sequence_matches_ts() {
        let expected = [
            1000u64, 2000, 4000, 8000, 16000, 30000, // capped from 32000
            30000, 30000,
        ];
        for (attempt, delay) in expected.into_iter().enumerate() {
            assert_eq!(backoff_delay(attempt as u32 + 1), delay);
        }
        assert_eq!(backoff_delay(u32::MAX), 30000);
    }
}

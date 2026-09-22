//! `transport::events` acceptance checks (spec M8.1):
//! * SSE parse of a recorded M6 `/global/event` stream → ordered events;
//! * 16 ms coalescing boundary (injected clock);
//! * reconnect backoff 1s/2s/4s…30s (injected sleeper);
//! * `sync`-frame filtering and `{directory, workspace}` metadata.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::response::Response;
use serde_json::json;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

use alforria_schema::event_manifest::Event;
use alforria_tui::transport::api::HttpClientConfig;
use alforria_tui::transport::events::{
    parse_global_frame, spawn_event_loop, BusEvent, Clock, EventSource, SseEventSource,
};

// -------------------------------------------------------------- clock

#[derive(Default)]
struct ManualClock {
    state: Mutex<ManualState>,
}

#[derive(Default)]
struct ManualState {
    now_ms: u64,
    sleeps: Vec<u64>,
    sleepers: Vec<(u64, Arc<tokio::sync::Notify>)>,
}

impl ManualClock {
    fn advance(&self, ms: u64) {
        let mut state = self.state.lock().unwrap();
        state.now_ms += ms;
        let now = state.now_ms;
        state.sleepers.retain(|(target, notify)| {
            if target <= &now {
                notify.notify_one();
                false
            } else {
                true
            }
        });
    }

    fn sleeps(&self) -> Vec<u64> {
        self.state.lock().unwrap().sleeps.clone()
    }
}

#[async_trait]
impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.state.lock().unwrap().now_ms
    }

    async fn sleep(&self, ms: u64) {
        self.state.lock().unwrap().sleeps.push(ms);
        if ms == 0 {
            return;
        }
        let notify = {
            let mut state = self.state.lock().unwrap();
            let target = state.now_ms + ms;
            let notify = Arc::new(tokio::sync::Notify::new());
            state.sleepers.push((target, Arc::clone(&notify)));
            notify
        };
        notify.notified().await;
    }
}

// -------------------------------------------------------------- fake

/// Channel-fed fake: each connection consumes one script entry.
struct FakeSource {
    script: Mutex<VecDeque<Vec<String>>>,
    end_streams: bool,
    workspaces: bool,
    connects: AtomicUsize,
    sync_starts: AtomicUsize,
}

impl FakeSource {
    fn new(script: Vec<Vec<String>>, end_streams: bool, workspaces: bool) -> FakeSource {
        FakeSource {
            script: Mutex::new(script.into()),
            end_streams,
            workspaces,
            connects: AtomicUsize::new(0),
            sync_starts: AtomicUsize::new(0),
        }
    }

    fn connects(&self) -> usize {
        self.connects.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl EventSource for FakeSource {
    async fn stream(&self, tx: UnboundedSender<BusEvent>) -> anyhow::Result<()> {
        self.connects.fetch_add(1, Ordering::SeqCst);
        let frames = self.script.lock().unwrap().pop_front().unwrap_or_default();
        let end = self.end_streams;
        tokio::spawn(async move {
            for frame in frames {
                if let Some(event) = parse_global_frame(&frame) {
                    if tx.send(event).is_err() {
                        return;
                    }
                }
            }
            if !end {
                // Keep the sender alive — the stream stays open.
                std::future::pending::<()>().await;
            }
        });
        Ok(())
    }

    async fn sync_start(&self) -> anyhow::Result<()> {
        self.sync_starts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn experimental_workspaces(&self) -> bool {
        self.workspaces
    }
}

async fn until(mut ready: impl FnMut() -> bool) {
    for _ in 0..100_000 {
        if ready() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("condition never became ready");
}

async fn until_connects(source: &FakeSource, count: usize) {
    until(move || source.connects() >= count).await;
}

// ------------------------------------------------- recorded M6 stream parse

const RECORDED_GLOBAL_EVENT_STREAM: &str = concat!(
    "data: {\"payload\":{\"id\":\"evt_1\",\"type\":\"server.connected\",\"properties\":{}}}\n\n",
    "data: {\"payload\":{\"id\":\"evt_2\",\"type\":\"models-dev.refreshed\",\"properties\":{}}}\n\n",
    "data: {\"payload\":{\"id\":\"evt_3\",\"type\":\"models-dev.refreshed\",\"properties\":{}}}\n\n",
    "data: {\"payload\":{\"type\":\"sync\",\"syncEvent\":{\"id\":\"evt_3\",\"type\":\"models-dev.refreshed.1\",\"seq\":0,\"aggregateID\":\"ses_1\",\"data\":{}},\"id\":\"evt_3\"}}\n\n",
    "data: {\"payload\":{\"id\":\"evt_4\",\"type\":\"server.heartbeat\",\"properties\":{}}}\n\n",
    "data: {\"directory\":\"/repo\",\"payload\":{\"type\":\"server.instance.disposed\",\"properties\":{\"directory\":\"/repo\"},\"id\":\"evt_5\"}}\n\n",
    ": heartbeat\n\n",
);

#[tokio::test]
async fn recorded_m6_stream_parses_into_ordered_events() {
    let seen_query = Arc::new(Mutex::new(String::new()));
    let seen_query_handler = Arc::clone(&seen_query);
    let app = axum::Router::new().route(
        "/global/event",
        axum::routing::get(move |request: axum::extract::Request| {
            let seen_query = Arc::clone(&seen_query_handler);
            async move {
                *seen_query.lock().unwrap() = request.uri().query().unwrap_or_default().to_string();
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Body::from(RECORDED_GLOBAL_EVENT_STREAM))
                    .unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind port 0");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("server");
    });

    let source = SseEventSource::new(HttpClientConfig {
        base_url: format!("http://{addr}"),
        directory: Some("/the repo".to_string()),
        headers: Vec::new(),
    })
    .expect("source");

    let (tx, mut rx) = unbounded_channel();
    source.stream(tx).await.expect("stream connects");
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }

    assert_eq!(events.len(), 4, "sync and heartbeat frames are dropped");
    assert!(matches!(events[0].event, Event::ServerConnected(_)));
    assert!(matches!(events[1].event, Event::ModelsDevRefreshed(_)));
    assert!(matches!(events[2].event, Event::ModelsDevRefreshed(_)));
    assert!(matches!(events[3].event, Event::ServerInstanceDisposed(_)));
    assert_eq!(events[0].metadata.directory, None);
    assert_eq!(
        events[3].metadata.directory,
        Some("/repo".to_string()),
        "directory metadata is attached to every delivered event"
    );

    let query = seen_query.lock().unwrap().clone();
    assert!(
        query.contains("directory=%2Fthe%20repo"),
        "config directory rides as an encoded query param on the SSE GET: {query}"
    );
}

// ------------------------------------------------------ driver (clock)

#[tokio::test]
async fn driver_coalesces_on_the_16ms_boundary() {
    let frame = || {
        json!({
            "payload": {"id": "evt_1", "type": "models-dev.refreshed", "properties": {}}
        })
        .to_string()
    };
    let source = Arc::new(FakeSource::new(vec![vec![frame(), frame()]], false, false));
    let clock = Arc::new(ManualClock::default());
    let (tx, mut rx) = unbounded_channel();

    spawn_event_loop(
        Arc::clone(&source) as Arc<dyn EventSource>,
        Arc::clone(&clock) as Arc<dyn Clock>,
        tx,
    );

    until_connects(&source, 1).await;
    // No batch before the coalescing deadline fires.
    until(|| rx.is_empty()).await;
    clock.advance(16);
    until(|| !rx.is_empty()).await;

    let batch = rx.recv().await.expect("coalesced batch");
    assert_eq!(batch.len(), 2, "both deferred events flush in one batch");
    assert!(matches!(batch[0].event, Event::ModelsDevRefreshed(_)));

    // No further batches until more events arrive.
    until(|| rx.is_empty()).await;
    assert_eq!(rx.len(), 0);
    assert!(
        clock.sleeps().iter().all(|delay| *delay == 16),
        "only coalescing timers run: {:?}",
        clock.sleeps()
    );
}

#[tokio::test]
async fn driver_reconnects_with_ts_backoff() {
    let source = Arc::new(FakeSource::new(Vec::new(), true, false));
    let clock = Arc::new(ManualClock::default());
    let (tx, _rx) = unbounded_channel();

    spawn_event_loop(
        Arc::clone(&source) as Arc<dyn EventSource>,
        Arc::clone(&clock) as Arc<dyn Clock>,
        tx,
    );

    until_connects(&source, 1).await;

    let backoff = [1000u64, 2000, 4000, 8000, 16000, 30000, 30000];
    for (attempt, delay) in backoff.into_iter().enumerate() {
        clock.advance(delay);
        until_connects(&source, attempt + 2).await;
    }
    assert!(
        clock.sleeps().len() >= backoff.len() && clock.sleeps()[..backoff.len()] == backoff[..],
        "backoff sequence is min(1000 * 2^(n-1), 30000) (sdk.tsx:113): {:?}",
        clock.sleeps()
    );
}

#[tokio::test]
async fn sync_start_is_gated_on_experimental_workspaces() {
    let source = Arc::new(FakeSource::new(Vec::new(), true, true));
    let (tx, _rx) = unbounded_channel();

    spawn_event_loop(
        Arc::clone(&source) as Arc<dyn EventSource>,
        Arc::new(ManualClock::default()) as Arc<dyn Clock>,
        tx,
    );

    until_connects(&source, 1).await;
    let source_after = Arc::clone(&source);
    until(move || source_after.sync_starts.load(Ordering::SeqCst) >= 1).await;
    assert_eq!(source.sync_starts.load(Ordering::SeqCst), 1);
}

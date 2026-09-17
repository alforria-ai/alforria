//! `EventBus` — the EventV2 in-process bus: tokio broadcast PubSub + durable
//! SQLite event log (per-aggregate sequence numbers, aggregate replay, owner
//! claim). Port of `packages/core/src/event.ts`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::broadcast;

use opencode_schema::event::DurableRef;
use opencode_schema::location::LocationRef;
use opencode_schema::schema::JsonMap;

use crate::event::definition::{
    new_event_id, versioned_type, Definition, DurableManifest, InvalidDurableEventError, Payload,
    SerializedEvent, MODELS_DEV_REFRESHED,
};
use crate::event::sql;
use crate::storage::Storage;
use crate::CoreError;

use std::collections::HashMap;

/// Broadcast capacity for the per-type and all-events channels.
///
/// TS uses Effect `PubSub.unbounded` — tokio broadcast is bounded, so the
/// capacity is generous and a slow subscriber observes `RecvError::Lagged`
/// where TS would have silently buffered. Consumers are expected to treat
/// `Lagged` as "subscriber too slow": log + continue.
pub const CHANNEL_CAPACITY: usize = 1024;

/// Capacity for the per-aggregate durable wake channels (TS `PubSub.sliding(1)`).
pub const WAKE_CAPACITY: usize = 32;

/// In-memory listener callback (`Subscriber` in `event.ts:18`).
pub type Listener = Arc<dyn Fn(&Payload) + Send + Sync>;

/// Projector callback — runs inside the commit transaction
/// (`event.ts:615-619`).
pub type Projector = Arc<dyn Fn(&Payload) -> Result<(), CoreError> + Send + Sync>;

/// Local operational projection committed atomically with a durable event
/// (`event.ts:118-123`). Runs inside the transaction, before the INSERT.
/// A failure rolls the transaction back.
///
/// Must not call back into the bus — `publish`/`replay` re-enter the
/// durable-commit lock.
pub type CommitHook = Box<dyn FnOnce(i64) -> Result<(), CoreError> + Send>;

/// [`EventBus::publish`] options (`event.ts:118-123`).
#[derive(Default)]
pub struct PublishOptions {
    /// Explicit event ID (default: a fresh `evt_…`).
    pub id: Option<String>,
    pub metadata: Option<JsonMap>,
    /// Caller-supplied location. The TS bus falls back to the ambient
    /// `Location.Service`; there is no ambient service in Rust, callers
    /// pass the location explicitly.
    pub location: Option<LocationRef>,
    /// Runs inside the commit transaction, before the event INSERT.
    pub commit: Option<CommitHook>,
}

/// [`EventBus::replay`] / [`EventBus::replay_all`] options
/// (`event.ts:138-141`).
#[derive(Debug, Clone, Default)]
pub struct ReplayOpts {
    /// Notify in-memory subscribers after the (re-)commit.
    pub publish: bool,
    /// Owner recorded with the aggregate's sequence row.
    pub owner_id: Option<String>,
    /// Reject replays whose aggregate is owned by a different owner.
    pub strict_owner: bool,
}

/// In-memory bus state. Everything is created lazily except `all`.
struct BusState {
    all: broadcast::Sender<Payload>,
    /// Per-type channels, keyed by unversioned type.
    typed: HashMap<String, broadcast::Sender<Payload>>,
    /// Per-aggregate wake channels for durable streams.
    durable: HashMap<String, broadcast::Sender<()>>,
    listeners: Vec<Listener>,
    /// Keyed by unversioned type.
    projectors: HashMap<String, Vec<Projector>>,
}

/// The EventV2 bus over a [`Storage`] connection.
pub struct EventBus {
    storage: Arc<Storage>,
    manifest: Arc<dyn DurableManifest>,
    state: Arc<Mutex<BusState>>,
    /// Serializes the durable commit + notify path so subscribers observe
    /// durable events of one aggregate in ascending sequence order
    /// (§M3.6 / §9.4).
    publish_lock: Mutex<()>,
}

impl EventBus {
    /// Create the bus over a storage connection.
    ///
    /// `manifest` supplies the known durable definitions (used by replay and
    /// the durable streams); `None` means no definitions are known — M3
    /// ships the generic machinery, concrete definitions land in M4.
    pub fn new(storage: Storage, manifest: Option<Arc<dyn DurableManifest>>) -> EventBus {
        EventBus {
            storage: Arc::new(storage),
            manifest: manifest.unwrap_or_else(|| Arc::new(crate::event::definition::EmptyManifest)),
            state: Arc::new(Mutex::new(BusState {
                all: broadcast::channel(CHANNEL_CAPACITY).0,
                typed: HashMap::new(),
                durable: HashMap::new(),
                listeners: Vec::new(),
                projectors: HashMap::new(),
            })),
            publish_lock: Mutex::new(()),
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, BusState> {
        // A poisoned lock only means another thread panicked while holding
        // it; the state itself is still consistent, so recover.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `latestSequence` (`event.ts:21-32`): the aggregate's last committed
    /// sequence number, `-1` when the aggregate is unknown.
    pub fn latest_sequence(&self, aggregate_id: &str) -> Result<i64, CoreError> {
        self.storage
            .with_connection(|conn| sql::latest_sequence(conn, aggregate_id))
    }

    /// Publish an event (`event.ts:419-439`).
    ///
    /// Durable definitions commit the event + sequence allocation in a
    /// single transaction, then wake durable streams and notify
    /// subscribers. The returned payload carries the assigned `durable`
    /// info.
    pub fn publish(
        &self,
        definition: &Definition,
        data: serde_json::Value,
        opts: PublishOptions,
    ) -> Result<Payload, CoreError> {
        let mut event = Payload {
            id: opts.id.unwrap_or_else(new_event_id),
            metadata: opts.metadata,
            r#type: definition.r#type.to_string(),
            durable: None,
            location: opts.location,
            data,
        };
        if definition.durable.is_none() {
            if opts.commit.is_some() {
                return Err(InvalidDurableEventError::new(
                    definition.r#type,
                    "Local commit hooks require a durable event",
                )
                .into());
            }
            self.notify(&event);
            return Ok(event);
        }
        let _ordering = self.publish_lock.lock().unwrap_or_else(|p| p.into_inner());
        let committed = self.commit_durable_event(definition, &event, None, opts.commit)?;
        if let Some((aggregate_id, seq)) = committed {
            event.durable = Some(DurableRef {
                aggregate_id,
                seq,
                version: definition
                    .durable
                    .map(|d| i64::from(d.version))
                    .unwrap_or_default(),
            });
            self.notify(&event);
        }
        Ok(event)
    }

    /// Stream of one type's events (unversioned type).
    pub fn subscribe(&self, type_: &str) -> broadcast::Receiver<Payload> {
        let mut state = self.lock_state();
        Self::typed_channel(&mut state, type_).subscribe()
    }

    /// Stream of all events (`event.ts:539`).
    pub fn all(&self) -> broadcast::Receiver<Payload> {
        let state = self.lock_state();
        state.all.subscribe()
    }

    /// Register a synchronous listener invoked on every event; returns an
    /// unsubscribe guard (`event.ts:606-613`). Listener failures never
    /// propagate (TS `observe`, `event.ts:398-404`).
    pub fn listen(&self, listener: Listener) -> Subscription {
        {
            let mut state = self.lock_state();
            state.listeners.push(listener.clone());
        }
        Subscription {
            state: Arc::clone(&self.state),
            listener,
        }
    }

    /// Register a projector for a definition; it runs inside the commit
    /// transaction, before the INSERT (`event.ts:615-619`). A projector
    /// failure aborts the transaction. Projectors are never unregistered
    /// (TS parity).
    pub fn project(&self, definition: &Definition, projector: Projector) {
        let mut state = self.lock_state();
        state
            .projectors
            .entry(definition.r#type.to_string())
            .or_default()
            .push(projector);
    }

    /// Durable aggregate stream: initial read of `seq > after`, then a live
    /// tail (`event.ts:565-604`). `after` defaults to `-1` (all events).
    pub fn durable_stream(&self, aggregate_id: &str, after: Option<i64>) -> DurableStream {
        let wake = {
            let mut state = self.lock_state();
            state.durable.retain(|_, tx| tx.receiver_count() > 0);
            let tx = state
                .durable
                .entry(aggregate_id.to_string())
                .or_insert_with(|| broadcast::channel(WAKE_CAPACITY).0);
            tx.subscribe()
        };
        DurableStream {
            storage: Arc::clone(&self.storage),
            manifest: Arc::clone(&self.manifest),
            aggregate_id: aggregate_id.to_string(),
            last_seq: after.unwrap_or(-1),
            buffer: VecDeque::new(),
            wake,
        }
    }

    /// Replay a serialized event into the durable log (`event.ts:441-478`).
    ///
    /// Stored-event idempotence: an event whose id, versioned type and data
    /// deep-equal the already-stored row at the same sequence is a no-op;
    /// anything else at the same sequence is a divergence error.
    pub fn replay(&self, event: SerializedEvent, opts: ReplayOpts) -> Result<(), CoreError> {
        let definition = self
            .manifest
            .get(&event.r#type)
            .filter(|d| d.durable.is_some())
            .ok_or_else(|| {
                InvalidDurableEventError::new(
                    &event.r#type,
                    format!("Unknown durable event type {}", event.r#type),
                )
            })?;
        self.manifest.validate(definition, &event.data)?;
        let payload = Payload {
            id: event.id.clone(),
            metadata: None,
            r#type: definition.r#type.to_string(),
            durable: None,
            location: None,
            data: event.data.clone(),
        };
        let _ordering = self.publish_lock.lock().unwrap_or_else(|p| p.into_inner());
        let committed = self.commit_durable_event(
            definition,
            &payload,
            Some(ReplayInput {
                seq: event.seq,
                aggregate_id: &event.aggregate_id,
                owner_id: opts.owner_id.as_deref(),
                strict_owner: opts.strict_owner,
            }),
            None,
        )?;
        if opts.publish {
            if let Some((aggregate_id, seq)) = committed {
                let mut published = payload;
                published.durable = Some(DurableRef {
                    aggregate_id,
                    seq,
                    version: definition
                        .durable
                        .map(|d| i64::from(d.version))
                        .unwrap_or_default(),
                });
                self.notify(&published);
            }
        }
        Ok(())
    }

    /// Replay a batch belonging to one aggregate, validating contiguous
    /// sequence numbers starting at the first event's seq
    /// (`event.ts:480-512`). Returns the aggregate ID, or `None` for an
    /// empty batch.
    pub fn replay_all(
        &self,
        events: Vec<SerializedEvent>,
        opts: ReplayOpts,
    ) -> Result<Option<String>, CoreError> {
        let Some(first) = events.first() else {
            return Ok(None);
        };
        let source = first.aggregate_id.clone();
        if events.iter().any(|event| event.aggregate_id != source) {
            return Err(InvalidDurableEventError::new(
                first.r#type.as_str(),
                "Replay events must belong to the same aggregate",
            )
            .into());
        }
        let start = first.seq;
        for (index, event) in events.iter().enumerate() {
            let seq = start + index as i64;
            if event.seq != seq {
                return Err(InvalidDurableEventError::new(
                    &event.r#type,
                    format!(
                        "Replay sequence mismatch at index {index}: expected {seq}, got {}",
                        event.seq
                    ),
                )
                .into());
            }
        }
        for event in events {
            self.replay(event, opts.clone())?;
        }
        Ok(Some(source))
    }

    /// Remove an aggregate and all its events (`event.ts:514-523`).
    pub fn remove(&self, aggregate_id: &str) -> Result<(), CoreError> {
        self.storage.with_connection_mut(|conn| {
            let tx = conn.transaction()?;
            sql::delete_event_sequence(&tx, aggregate_id)?;
            sql::delete_events(&tx, aggregate_id)?;
            tx.commit()?;
            Ok(())
        })
    }

    /// Set the owner of an aggregate (`event.ts:525-532`).
    pub fn claim(&self, aggregate_id: &str, owner_id: &str) -> Result<(), CoreError> {
        self.storage
            .with_connection(|conn| sql::update_owner(conn, aggregate_id, owner_id))
    }

    fn typed_channel<'a>(
        state: &'a mut BusState,
        type_: &'a str,
    ) -> &'a broadcast::Sender<Payload> {
        state
            .typed
            .entry(type_.to_string())
            .or_insert_with(|| broadcast::channel(CHANNEL_CAPACITY).0)
    }

    /// `commitDurableEvent` (`event.ts:205-367`): allocate a sequence
    /// number and insert the event in one transaction. Returns
    /// `Some((aggregate_id, seq))` when an event was committed, `None` for
    /// the no-op paths (stored-event idempotence, foreign-owner skip).
    fn commit_durable_event(
        &self,
        definition: &Definition,
        event: &Payload,
        input: Option<ReplayInput<'_>>,
        commit: Option<CommitHook>,
    ) -> Result<Option<(String, i64)>, CoreError> {
        let durable = definition.durable;
        let Some(durable) = durable else {
            return Ok(None);
        };
        let aggregate_id = event
            .data
            .get(durable.aggregate)
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                InvalidDurableEventError::new(
                    definition.r#type,
                    format!("Expected string aggregate field {}", durable.aggregate),
                )
            })?;
        if let Some(input) = &input {
            if input.aggregate_id != aggregate_id {
                return Err(InvalidDurableEventError::new(
                    definition.r#type,
                    format!(
                        "Aggregate mismatch: expected {}, got {}",
                        input.aggregate_id, aggregate_id
                    ),
                )
                .into());
            }
        }

        // Clone projectors out of the bus state before taking the storage
        // lock (never nest the two mutexes).
        let projectors = {
            let state = self.lock_state();
            state
                .projectors
                .get(event.r#type.as_str())
                .cloned()
                .unwrap_or_default()
        };

        let committed = self.storage.with_connection_mut(
            |conn| -> Result<Option<(String, i64)>, CoreError> {
            let tx = conn.transaction()?;
            let row = sql::get_event_sequence(&tx, aggregate_id)?;
            let latest = row.as_ref().map(|row| row.seq).unwrap_or(-1);
            let stored_type = versioned_type(definition.r#type, durable.version);

            if input.map(|i| i.strict_owner).unwrap_or(false) {
                if let Some(owner_id) = row.as_ref().and_then(|r| r.owner_id.as_deref()) {
                    let replay_owner = input.and_then(|i| i.owner_id).unwrap_or("none");
                    if owner_id != replay_owner {
                        return Err(InvalidDurableEventError::new(
                            definition.r#type,
                            format!(
                                "Replay owner mismatch for aggregate {aggregate_id}: expected {owner_id}, got {replay_owner}",
                            ),
                        )
                        .into());
                    }
                }
            }

            if let Some(input) = input {
                if input.seq <= latest {
                    let stored = sql::get_event_by_seq(&tx, aggregate_id, input.seq)?;
                    let equal = stored
                        .as_ref()
                        .map(|stored| {
                            stored.id == event.id
                                && stored.r#type == stored_type
                                && stored.data == event.data
                        })
                        .unwrap_or(false);
                    if !equal {
                        return Err(InvalidDurableEventError::new(
                            definition.r#type,
                            format!(
                                "Replay diverged at aggregate {aggregate_id} sequence {}",
                                input.seq
                            ),
                        )
                        .into());
                    }
                    if let Some(owner_id) = input.owner_id {
                        if row.as_ref().and_then(|r| r.owner_id.as_deref()).is_none() {
                            sql::update_owner(&tx, aggregate_id, owner_id)?;
                        }
                    }
                    tx.commit()?;
                    return Ok(None);
                }
            }

            // Owned by a different owner: skip silently (`event.ts:291-293`).
            if let Some(input) = input {
                if let Some(owner_id) = row.as_ref().and_then(|r| r.owner_id.as_deref()) {
                    if Some(owner_id) != input.owner_id {
                        tx.commit()?;
                        return Ok(None);
                    }
                }
            }

            let seq = input.map(|i| i.seq).unwrap_or(latest + 1);
            if input.is_some() && seq != latest + 1 {
                return Err(InvalidDurableEventError::new(
                    definition.r#type,
                    format!(
                        "Sequence mismatch for aggregate {aggregate_id}: expected {}, got {seq}",
                        latest + 1
                    ),
                )
                .into());
            }

            if let Some(stored) = sql::get_event_by_id(&tx, &event.id)? {
                return Err(InvalidDurableEventError::new(
                    definition.r#type,
                    format!(
                        "Event {} already exists at aggregate {} sequence {}",
                        event.id, stored.aggregate_id, stored.seq
                    ),
                )
                .into());
            }

            let committed = Payload {
                id: event.id.clone(),
                metadata: event.metadata.clone(),
                r#type: event.r#type.clone(),
                durable: Some(DurableRef {
                    aggregate_id: aggregate_id.to_string(),
                    seq,
                    version: i64::from(durable.version),
                }),
                location: event.location.clone(),
                data: event.data.clone(),
            };
            for projector in &projectors {
                projector(&committed)?;
            }
            if let Some(commit) = commit {
                commit(seq)?;
            }
            sql::upsert_event_sequence(
                &tx,
                aggregate_id,
                seq,
                input.and_then(|i| i.owner_id),
                input.map(|i| i.owner_id.is_some()).unwrap_or(false)
                    && row.as_ref().and_then(|r| r.owner_id.as_deref()).is_none(),
            )?;
            sql::insert_event(
                &tx,
                &sql::EventRow {
                    id: event.id.clone(),
                    aggregate_id: aggregate_id.to_string(),
                    seq,
                    r#type: stored_type,
                    data: event.data.clone(),
                },
            )?;
            tx.commit()?;
            Ok(Some((aggregate_id.to_string(), seq)))
        })?;

        if committed.is_some() {
            // Wake durable streams for this aggregate (`event.ts:354-360`).
            let mut state = self.lock_state();
            if let Some(wake) = state.durable.get(aggregate_id) {
                let _ = wake.send(());
            }
            state.durable.retain(|_, tx| tx.receiver_count() > 0);
        }
        Ok(committed)
    }

    /// `notify` (`event.ts:406-417`): listeners first, then the typed
    /// channel, then the all-events channel.
    fn notify(&self, event: &Payload) {
        let (listeners, typed, all) = {
            let state = self.lock_state();
            (
                state.listeners.clone(),
                state.typed.get(event.r#type.as_str()).cloned(),
                state.all.clone(),
            )
        };
        for listener in &listeners {
            listener(event);
        }
        if let Some(typed) = typed {
            let _ = typed.send(event.clone());
        }
        let _ = all.send(event.clone());
    }
}

/// Inputs of a replay pass (`event.ts:208-213`).
#[derive(Clone, Copy)]
struct ReplayInput<'a> {
    seq: i64,
    aggregate_id: &'a str,
    owner_id: Option<&'a str>,
    strict_owner: bool,
}

/// Unsubscribe guard returned by [`EventBus::listen`].
pub struct Subscription {
    state: Arc<Mutex<BusState>>,
    listener: Listener,
}

impl Subscription {
    /// Explicitly unsubscribe (equivalent to dropping the guard).
    pub fn unsubscribe(self) {}
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let listener = &self.listener;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state
            .listeners
            .retain(|candidate| !Arc::ptr_eq(candidate, listener));
    }
}

/// Durable aggregate stream: initial read of `seq > after` followed by a
/// live tail woken by publishes. Use [`DurableStream::next`] to advance.
pub struct DurableStream {
    storage: Arc<Storage>,
    manifest: Arc<dyn DurableManifest>,
    aggregate_id: String,
    last_seq: i64,
    buffer: VecDeque<Payload>,
    wake: broadcast::Receiver<()>,
}

impl DurableStream {
    /// Next event, or `None` once the wake channel has closed (no bus
    /// instance left). A lagged wake channel is retried; the re-read is a
    /// no-op, matching the sliding-wake semantics of `event.ts:565-604`.
    pub async fn next(&mut self) -> Result<Option<Payload>, CoreError> {
        loop {
            if let Some(event) = self.buffer.pop_front() {
                return Ok(Some(event));
            }
            let events = self.read_after()?;
            if events.is_empty() {
                match self.wake.recv().await {
                    Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return Ok(None),
                }
            } else {
                if let Some(last) = events.last() {
                    self.last_seq = last
                        .durable
                        .as_ref()
                        .map(|d| d.seq)
                        .unwrap_or(self.last_seq);
                }
                self.buffer.extend(events);
            }
        }
    }

    fn read_after(&self) -> Result<Vec<Payload>, CoreError> {
        let rows = self.storage.with_connection(|conn| {
            sql::list_events_after(conn, &self.aggregate_id, self.last_seq)
        })?;
        rows.iter()
            .map(|row| {
                decode_serialized_event(
                    self.manifest.as_ref(),
                    SerializedEvent {
                        id: row.id.clone(),
                        r#type: row.r#type.clone(),
                        seq: row.seq,
                        aggregate_id: row.aggregate_id.clone(),
                        data: row.data.clone(),
                    },
                )
            })
            .collect()
    }
}

/// `decodeSerializedEvent` (`event.ts:50-61`): turn a stored row back into
/// a payload via the durable manifest.
fn decode_serialized_event(
    manifest: &dyn DurableManifest,
    event: SerializedEvent,
) -> Result<Payload, CoreError> {
    let definition = manifest
        .get(&event.r#type)
        .filter(|d| d.durable.is_some())
        .ok_or_else(|| {
            InvalidDurableEventError::new(
                &event.r#type,
                format!("Unknown durable event type {}", event.r#type),
            )
        })?;
    manifest.validate(definition, &event.data)?;
    Ok(Payload {
        id: event.id,
        metadata: None,
        r#type: definition.r#type.to_string(),
        durable: Some(DurableRef {
            aggregate_id: event.aggregate_id,
            seq: event.seq,
            version: definition
                .durable
                .map(|d| i64::from(d.version))
                .unwrap_or_default(),
        }),
        location: None,
        data: event.data,
    })
}

/// Bridge the M3.4 catalog refresh seam to the bus: on every successful
/// catalog refresh, publish `models-dev.refreshed` — the TS catalog service
/// publishes this event through the EventV2 bus (`models-dev.ts:237-253`).
///
/// ```
/// use std::sync::Arc;
///
/// use opencode_core::{catalog_refresh_listener, EventBus, Storage};
///
/// # fn main() -> Result<(), opencode_core::CoreError> {
/// let storage = Storage::open_in_memory()?;
/// let bus = Arc::new(EventBus::new(storage, None));
/// let listener = catalog_refresh_listener(Arc::clone(&bus));
/// // catalog_service.add_refresh_listener(listener);
/// # let _ = listener;
/// # Ok(())
/// # }
/// ```
pub fn catalog_refresh_listener(bus: Arc<EventBus>) -> crate::catalog::RefreshListener {
    Arc::new(move || {
        let result = bus.publish(
            &MODELS_DEV_REFRESHED,
            serde_json::json!({}),
            PublishOptions::default(),
        );
        if let Err(error) = result {
            tracing::warn!("failed to publish models-dev.refreshed: {error}");
        }
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::super::definition::DurableSpec;
    use super::*;
    use crate::catalog::{CatalogConfig, Clock, DEFAULT_MODELS_SOURCE};
    use crate::storage::test_support::TempDir;

    fn durable_def() -> Definition {
        Definition {
            r#type: "test.event",
            durable: Some(DurableSpec {
                aggregate: "sessionID",
                version: 1,
            }),
        }
    }

    /// Manifest that knows `test.event` at version 1.
    struct TestManifest {
        defs: HashMap<String, Definition>,
    }

    impl TestManifest {
        fn new() -> TestManifest {
            let mut defs = HashMap::new();
            defs.insert(versioned_type("test.event", 1), durable_def());
            TestManifest { defs }
        }
    }

    impl DurableManifest for TestManifest {
        fn get(&self, versioned_type: &str) -> Option<&Definition> {
            self.defs.get(versioned_type)
        }

        fn validate(
            &self,
            _definition: &Definition,
            _data: &serde_json::Value,
        ) -> Result<(), CoreError> {
            Ok(())
        }
    }

    fn bus_with_manifest() -> (EventBus, TempDir) {
        let dir = TempDir::new("event-bus");
        let storage = Storage::open(dir.path().join("db.sqlite")).unwrap();
        (
            EventBus::new(storage, Some(Arc::new(TestManifest::new()))),
            dir,
        )
    }

    fn durable_data(aggregate: &str) -> serde_json::Value {
        serde_json::json!({ "sessionID": aggregate })
    }

    fn serialized(seq: i64, aggregate: &str, id: &str) -> SerializedEvent {
        SerializedEvent {
            id: id.to_string(),
            r#type: versioned_type("test.event", 1),
            seq,
            aggregate_id: aggregate.to_string(),
            data: durable_data(aggregate),
        }
    }

    #[test]
    fn publish_assigns_seq_and_notifies() {
        let (bus, _dir) = bus_with_manifest();
        let mut rx = bus.subscribe("test.event");

        let event = bus
            .publish(
                &durable_def(),
                durable_data("ses_1"),
                PublishOptions::default(),
            )
            .unwrap();
        assert_eq!(event.durable().unwrap().seq, 0);
        assert!(event.id.starts_with("evt_"));

        let received = rx.try_recv().unwrap();
        assert_eq!(received.id, event.id);
        assert_eq!(received.durable().unwrap().aggregate_id, "ses_1");
    }

    #[test]
    fn publish_stamps_metadata_and_location() {
        let (bus, _dir) = bus_with_manifest();
        let mut metadata = serde_json::Map::new();
        metadata.insert("origin".to_string(), serde_json::json!("test"));
        let event = bus
            .publish(
                &durable_def(),
                durable_data("ses_1"),
                PublishOptions {
                    metadata: Some(metadata),
                    location: Some(LocationRef {
                        directory: "/repo".to_string(),
                        workspace_id: None,
                    }),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            event.metadata.as_ref().unwrap().get("origin"),
            Some(&serde_json::json!("test"))
        );
        assert_eq!(event.location.unwrap().directory, "/repo");
    }

    #[test]
    fn non_durable_publishes_do_not_touch_storage() {
        let (bus, _dir) = bus_with_manifest();
        let mut all = bus.all();

        let event = bus
            .publish(
                &MODELS_DEV_REFRESHED,
                serde_json::json!({}),
                PublishOptions::default(),
            )
            .unwrap();
        assert!(event.durable.is_none());
        assert_eq!(bus.latest_sequence("models-dev").unwrap(), -1);
        assert_eq!(all.try_recv().unwrap().r#type, "models-dev.refreshed");
    }

    #[test]
    fn commit_hooks_require_a_durable_event() {
        let (bus, _dir) = bus_with_manifest();
        let err = bus
            .publish(
                &MODELS_DEV_REFRESHED,
                serde_json::json!({}),
                PublishOptions {
                    commit: Some(Box::new(|_seq| Ok(()))),
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("Local commit hooks require a durable event"),
            "{err}"
        );
    }

    #[test]
    fn commit_hook_runs_inside_the_transaction_with_the_seq() {
        let (bus, _dir) = bus_with_manifest();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_hook = Arc::clone(&seen);
        bus.publish(
            &durable_def(),
            durable_data("ses_1"),
            PublishOptions {
                commit: Some(Box::new(move |seq| {
                    seen_hook.lock().unwrap().push(seq);
                    Ok(())
                })),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(*seen.lock().unwrap(), vec![0]);
    }

    #[test]
    fn failed_commit_hook_rolls_back() {
        let (bus, _dir) = bus_with_manifest();
        let result = bus.publish(
            &durable_def(),
            durable_data("ses_1"),
            PublishOptions {
                commit: Some(Box::new(|_seq| {
                    Err(CoreError::Storage("hook failed".to_string()))
                })),
                ..Default::default()
            },
        );
        assert!(result.is_err());
        assert_eq!(bus.latest_sequence("ses_1").unwrap(), -1);
    }

    #[test]
    fn projectors_run_inside_the_commit_and_can_abort() {
        let (bus, _dir) = bus_with_manifest();

        let observed = Arc::new(AtomicUsize::new(0));
        let observed_projector = Arc::clone(&observed);
        bus.project(
            &durable_def(),
            Arc::new(move |_event| {
                observed_projector.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
        );
        bus.publish(
            &durable_def(),
            durable_data("ses_1"),
            PublishOptions::default(),
        )
        .unwrap();
        assert_eq!(observed.load(Ordering::SeqCst), 1);

        bus.project(
            &durable_def(),
            Arc::new(|_event| Err(CoreError::Storage("projector failed".to_string()))),
        );
        let result = bus.publish(
            &durable_def(),
            durable_data("ses_1"),
            PublishOptions::default(),
        );
        assert!(result.is_err(), "failing projector aborts the commit");
        assert_eq!(
            bus.latest_sequence("ses_1").unwrap(),
            0,
            "sequence must not advance on projector failure"
        );
    }

    #[test]
    fn duplicate_event_id_is_rejected() {
        let (bus, _dir) = bus_with_manifest();
        let id = new_event_id();
        bus.publish(
            &durable_def(),
            durable_data("ses_1"),
            PublishOptions {
                id: Some(id.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        let err = bus
            .publish(
                &durable_def(),
                durable_data("ses_1"),
                PublishOptions {
                    id: Some(id),
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
    }

    #[test]
    fn missing_aggregate_field_is_rejected() {
        let (bus, _dir) = bus_with_manifest();
        let err = bus
            .publish(
                &durable_def(),
                serde_json::json!({"other": true}),
                PublishOptions::default(),
            )
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("Expected string aggregate field sessionID"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn hundred_concurrent_publishes_are_gapless() {
        let (bus, _dir) = bus_with_manifest();
        let bus = Arc::new(bus);
        let mut rx = bus.subscribe("test.event");

        let mut tasks = Vec::new();
        for _ in 0..100 {
            let bus = Arc::clone(&bus);
            tasks.push(tokio::task::spawn_blocking(move || {
                bus.publish(
                    &durable_def(),
                    durable_data("ses_1"),
                    PublishOptions::default(),
                )
            }));
        }
        let mut seqs = Vec::new();
        for task in tasks {
            seqs.push(task.await.unwrap().unwrap().durable().unwrap().seq);
        }
        seqs.sort_unstable();
        assert_eq!(seqs, (0..100).collect::<Vec<i64>>(), "gapless 0..=99");

        let mut observed = Vec::new();
        while let Ok(event) = rx.try_recv() {
            observed.push(event.durable().unwrap().seq);
        }
        assert_eq!(observed, (0..100).collect::<Vec<i64>>(), "ascending order");
    }

    #[tokio::test]
    async fn events_persist_across_bus_instances() {
        let dir = TempDir::new("event-bus-durable");
        {
            let bus = EventBus::new(
                Storage::open(dir.path().join("db.sqlite")).unwrap(),
                Some(Arc::new(TestManifest::new())),
            );
            for _ in 0..5 {
                bus.publish(
                    &durable_def(),
                    durable_data("ses_1"),
                    PublishOptions::default(),
                )
                .unwrap();
            }
        }

        let reopened = EventBus::new(
            Storage::open(dir.path().join("db.sqlite")).unwrap(),
            Some(Arc::new(TestManifest::new())),
        );
        assert_eq!(reopened.latest_sequence("ses_1").unwrap(), 4);
        let mut stream = reopened.durable_stream("ses_1", None);
        for expected in 0..5 {
            let event = stream.next().await.unwrap().unwrap();
            assert_eq!(event.durable().unwrap().seq, expected);
        }
    }

    #[tokio::test]
    async fn durable_stream_replays_history_and_tails_live() {
        let (bus, _dir) = bus_with_manifest();
        let bus = Arc::new(bus);
        for _ in 0..3 {
            bus.publish(
                &durable_def(),
                durable_data("ses_1"),
                PublishOptions::default(),
            )
            .unwrap();
        }

        let mut stream = bus.durable_stream("ses_1", None);
        for expected in 0..3 {
            let event = stream.next().await.unwrap().unwrap();
            assert_eq!(event.durable().unwrap().seq, expected);
        }

        // History exhausted; the stream parks on the wake channel. Publish
        // from another thread after a delay.
        let publisher_bus = Arc::clone(&bus);
        let publisher = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            publisher_bus
                .publish(
                    &durable_def(),
                    durable_data("ses_1"),
                    PublishOptions::default(),
                )
                .unwrap();
        });
        let event = stream.next().await.unwrap().unwrap();
        assert_eq!(event.durable().unwrap().seq, 3);
        publisher.join().unwrap();
    }

    #[tokio::test]
    async fn durable_stream_honors_cursor_and_unknown_type() {
        let (bus, _dir) = bus_with_manifest();
        for _ in 0..3 {
            bus.publish(
                &durable_def(),
                durable_data("ses_1"),
                PublishOptions::default(),
            )
            .unwrap();
        }
        let mut stream = bus.durable_stream("ses_1", Some(1));
        let event = stream.next().await.unwrap().unwrap();
        assert_eq!(event.durable().unwrap().seq, 2);

        // A bus whose manifest does not know the stored type fails the read.
        let dir = TempDir::new("event-bus-unknown");
        let manifestless =
            EventBus::new(Storage::open(dir.path().join("db.sqlite")).unwrap(), None);
        manifestless
            .publish(
                &durable_def(),
                durable_data("ses_9"),
                PublishOptions::default(),
            )
            .unwrap();
        let mut stream = manifestless.durable_stream("ses_9", None);
        let err = stream.next().await.unwrap_err();
        assert!(
            err.to_string().contains("Unknown durable event type"),
            "{err}"
        );
    }

    #[test]
    fn replay_is_idempotent_and_detects_divergence() {
        let (bus, _dir) = bus_with_manifest();
        let event = bus
            .publish(
                &durable_def(),
                durable_data("ses_1"),
                PublishOptions::default(),
            )
            .unwrap();

        // Same event replayed twice: no-op both times.
        bus.replay(serialized(0, "ses_1", &event.id), ReplayOpts::default())
            .unwrap();
        bus.replay(serialized(0, "ses_1", &event.id), ReplayOpts::default())
            .unwrap();
        assert_eq!(bus.latest_sequence("ses_1").unwrap(), 0);

        // Same seq, different data: divergence.
        let mut diverged = serialized(0, "ses_1", &event.id);
        diverged.data = serde_json::json!({"sessionID": "ses_1", "changed": true});
        let err = bus.replay(diverged, ReplayOpts::default()).unwrap_err();
        assert!(err.to_string().contains("Replay diverged"), "{err}");
    }

    #[test]
    fn replay_validates_seq_continuity() {
        let (bus, _dir) = bus_with_manifest();
        bus.publish(
            &durable_def(),
            durable_data("ses_1"),
            PublishOptions::default(),
        )
        .unwrap();
        let err = bus
            .replay(
                serialized(5, "ses_1", &new_event_id()),
                ReplayOpts::default(),
            )
            .unwrap_err();
        assert!(err.to_string().contains("Sequence mismatch"), "{err}");
    }

    #[test]
    fn replay_rejects_unknown_types_and_aggregate_mismatch() {
        let (bus, _dir) = bus_with_manifest();
        let mut unknown = serialized(0, "ses_1", &new_event_id());
        unknown.r#type = "unknown.event.1".to_string();
        let err = bus.replay(unknown, ReplayOpts::default()).unwrap_err();
        assert!(
            err.to_string().contains("Unknown durable event type"),
            "{err}"
        );

        let err = bus
            .replay(
                SerializedEvent {
                    id: new_event_id(),
                    r#type: versioned_type("test.event", 1),
                    seq: 0,
                    aggregate_id: "ses_other".to_string(),
                    data: durable_data("ses_1"),
                },
                ReplayOpts::default(),
            )
            .unwrap_err();
        assert!(err.to_string().contains("Aggregate mismatch"), "{err}");
    }

    #[test]
    fn replay_owner_semantics() {
        let (bus, _dir) = bus_with_manifest();
        bus.publish(
            &durable_def(),
            durable_data("ses_1"),
            PublishOptions::default(),
        )
        .unwrap();
        bus.claim("ses_1", "own_1").unwrap();

        // Strict owner mismatch.
        let err = bus
            .replay(
                serialized(1, "ses_1", &new_event_id()),
                ReplayOpts {
                    owner_id: Some("own_2".to_string()),
                    strict_owner: true,
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(err.to_string().contains("Replay owner mismatch"), "{err}");

        // Non-strict foreign owner: silently skipped, nothing committed.
        bus.replay(
            serialized(1, "ses_1", &new_event_id()),
            ReplayOpts {
                owner_id: Some("own_2".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(bus.latest_sequence("ses_1").unwrap(), 0);
    }

    #[test]
    fn replay_all_validates_contiguity_and_aggregate() {
        let (bus, _dir) = bus_with_manifest();
        let events = vec![
            serialized(0, "ses_1", &new_event_id()),
            serialized(1, "ses_1", &new_event_id()),
        ];
        let source = bus.replay_all(events, ReplayOpts::default()).unwrap();
        assert_eq!(source.as_deref(), Some("ses_1"));
        assert_eq!(bus.latest_sequence("ses_1").unwrap(), 1);

        let mixed = vec![
            serialized(2, "ses_1", &new_event_id()),
            serialized(3, "ses_2", &new_event_id()),
        ];
        let err = bus.replay_all(mixed, ReplayOpts::default()).unwrap_err();
        assert!(
            err.to_string()
                .contains("must belong to the same aggregate"),
            "{err}"
        );

        let gap = vec![
            serialized(2, "ses_1", &new_event_id()),
            serialized(9, "ses_1", &new_event_id()),
        ];
        let err = bus.replay_all(gap, ReplayOpts::default()).unwrap_err();
        assert!(
            err.to_string().contains("Replay sequence mismatch"),
            "{err}"
        );

        assert_eq!(
            bus.replay_all(Vec::new(), ReplayOpts::default()).unwrap(),
            None
        );
    }

    #[test]
    fn remove_deletes_aggregate_and_events() {
        let (bus, _dir) = bus_with_manifest();
        bus.publish(
            &durable_def(),
            durable_data("ses_1"),
            PublishOptions::default(),
        )
        .unwrap();
        bus.remove("ses_1").unwrap();
        assert_eq!(bus.latest_sequence("ses_1").unwrap(), -1);
        let count = bus.storage.with_connection(|conn| {
            conn.query_row("SELECT COUNT(*) FROM event", [], |row| row.get::<_, i64>(0))
        });
        assert_eq!(count.unwrap(), 0);
    }

    #[test]
    fn listen_notifies_and_unsubscribes() {
        let (bus, _dir) = bus_with_manifest();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_listener = Arc::clone(&calls);
        let subscription = bus.listen(Arc::new(move |_event| {
            calls_listener.fetch_add(1, Ordering::SeqCst);
        }));
        bus.publish(
            &MODELS_DEV_REFRESHED,
            serde_json::json!({}),
            PublishOptions::default(),
        )
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        subscription.unsubscribe();
        bus.publish(
            &MODELS_DEV_REFRESHED,
            serde_json::json!({}),
            PublishOptions::default(),
        )
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "listener unsubscribed");
    }

    /// A catalog refresh (via the M3.4 `add_refresh_listener` seam) bridges
    /// onto the bus as a `models-dev.refreshed` event.
    #[tokio::test]
    async fn catalog_refresh_listener_bridges_to_bus() {
        struct AlwaysClock;
        impl Clock for AlwaysClock {
            fn now_ms(&self) -> u64 {
                0
            }
        }

        struct StubFetch;
        impl crate::catalog::Fetcher for StubFetch {
            fn get(
                &self,
                _url: &str,
                _user_agent: &str,
            ) -> Result<String, crate::catalog::FetchError> {
                Ok(
                    r#"{"anthropic":{"name":"Anthropic","id":"anthropic","env":[],"models":{}}}"#
                        .to_string(),
                )
            }
        }

        let (bus, _dir) = bus_with_manifest();
        let bus = Arc::new(bus);
        let mut all = bus.all();

        let dir = TempDir::new("event-bus-catalog-bridge");
        let cfg = CatalogConfig {
            source: DEFAULT_MODELS_SOURCE.to_string(),
            models_path_override: None,
            disable_fetch: false,
            user_agent: "opencode/local/local/cli".to_string(),
            ttl: crate::catalog::TTL,
            refresh_interval: crate::catalog::REFRESH_INTERVAL,
            retry_backoff_base: std::time::Duration::from_secs(0),
            snapshot: None,
        };
        let catalog = crate::catalog::CatalogService::new(
            dir.path(),
            cfg,
            Arc::new(AlwaysClock),
            Arc::new(StubFetch),
        );
        catalog.add_refresh_listener(catalog_refresh_listener(Arc::clone(&bus)));

        catalog.refresh(true);
        let event = all.try_recv().unwrap();
        assert_eq!(event.r#type, "models-dev.refreshed");
        assert_eq!(event.data, serde_json::json!({}));
    }
}

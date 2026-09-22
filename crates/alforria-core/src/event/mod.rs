//! EventV2: typed pub-sub + durable SQLite event log.
//!
//! * [`definition`] (M3.6) — Definition/Payload types + DurableManifest trait.
//! * [`bus`] (M3.6) — EventBus: publish/subscribe/durable/replay.
//! * [`sql`] (M3.6) — event/event_sequence persistence.

pub mod bus;
pub mod definition;
pub mod sql;

pub use bus::{
    catalog_refresh_listener, CommitHook, DurableStream, EventBus, Listener, Projector,
    PublishOptions, ReplayOpts, Subscription, CHANNEL_CAPACITY, WAKE_CAPACITY,
};
pub use definition::{
    new_event_id, versioned_type, AnyValue, Definition, DurableInfo, DurableManifest, DurableSpec,
    EmptyManifest, InvalidDurableEventError, Payload, SerializedEvent, Validate, ID_PREFIX,
    MODELS_DEV_REFRESHED,
};

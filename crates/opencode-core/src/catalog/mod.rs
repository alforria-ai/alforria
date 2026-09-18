//! models.dev catalog: fetch, disk cache, TTL, hourly refresh.
//!
//! * [`types`] (M3.4) — Provider/Model/Cost wire types.
//! * [`service`] (M3.4) — TTL cache + fetch + refresh.

pub mod service;
pub mod types;

pub use service::{
    CatalogConfig, CatalogService, Clock, FetchError, Fetcher, RefreshListener, SystemClock,
    DEFAULT_MODELS_SOURCE, FETCH_RETRIES, INSTALLATION_CHANNEL, INSTALLATION_VERSION,
    REFRESH_INTERVAL, RETRY_BACKOFF_BASE, TTL,
};
pub use types::{
    Catalog, CatalogModelStatus, ContextOver200k, ContextTierType, Cost, CostTier, CostTierType,
    Experimental, ExperimentalMode, ExperimentalProvider, Interleaved, InterleavedField,
    Modalities, Modality, Model, ModelLimit, Provider, ProviderInfo, Providers, ReasoningOption,
};

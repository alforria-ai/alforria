//! models.dev catalog service — port of `packages/core/src/models-dev.ts:135-262`.
//!
//! Caching semantics (TS `ModelsDev` service):
//!
//! * source URL: `OPENCODE_MODELS_URL` or `https://models.opencode.ai` (no
//!   trailing-slash handling — TS does `||` only);
//! * cache file: `<cache>/models.json` for the default URL, else
//!   `<cache>/models-{fnv1a(source)}.json`;
//! * freshness: `now - mtime < 5 min`;
//! * populate order: disk → compile-time snapshot stub → `{}` when fetch
//!   disabled → fetch under a cross-process `flock` (fs4), atomic write
//!   (temp file + rename), parse;
//! * refresh: skip when fresh unless `force`; re-check freshness under the
//!   flock (another process may have refreshed); on success invalidate the
//!   in-memory cache and notify listeners (the TS code publishes
//!   `models_dev.refreshed` through the EventV2 bus — the Rust port exposes a
//!   listener seam, see [`CatalogService::add_refresh_listener`]);
//! * hourly repeating refresh, spawned only when fetch is enabled.
//!
//! The TS reference performs HTTP through Effect's `HttpClient` with
//! `retryTransient(times: 2, exponential(200) + jitter)` and a 10s request
//! timeout. This crate has no HTTP client dependency; [`Fetcher`] is the
//! injection seam used by tests. Production wiring (reqwest, 10s timeout,
//! transient classification) lives with the server crate.

use std::collections::BTreeMap;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::CoreError;
use fs4::fs_std::FileExt as Fs4FileExt;

use super::types::{Catalog, Providers};

/// Default models.dev source (`models-dev.ts:160`).
pub const DEFAULT_MODELS_SOURCE: &str = "https://models.opencode.ai";

/// Catalog cache TTL: 5 minutes (`models-dev.ts:165`).
pub const TTL: Duration = Duration::from_secs(5 * 60);

/// Hourly refresh interval (`models-dev.ts:257`).
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// TS `InstallationChannel`/`InstallationVersion` fallbacks
/// (`installation/version.ts`): build-time injected constants, `"local"`
/// when unset.
pub const INSTALLATION_CHANNEL: &str = "local";
pub const INSTALLATION_VERSION: &str = "local";

/// Number of retries on transient fetch errors (TS `retryTransient(times: 2)`).
pub const FETCH_RETRIES: u32 = 2;

/// Base delay for the jittered exponential fetch backoff (TS `exponential(200)`).
pub const RETRY_BACKOFF_BASE: Duration = Duration::from_millis(200);

/// Wall clock seam (unix epoch milliseconds).
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}

/// Real wall clock.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// HTTP failure classification for [`Fetcher`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// Retryable (network errors, timeouts, 5xx…).
    Transient(String),
    /// Not retried (4xx, non-response body…).
    Permanent(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Transient(cause) => write!(f, "transient: {cause}"),
            FetchError::Permanent(cause) => write!(f, "permanent: {cause}"),
        }
    }
}

/// HTTP seam for the catalog fetch (`GET {source}/api.json`).
///
/// Implementations must send the `User-Agent` header they are handed and are
/// expected to enforce the TS 10s request timeout; the production reqwest
/// implementation lives outside this crate.
pub trait Fetcher: Send + Sync {
    fn get(&self, url: &str, user_agent: &str) -> Result<String, FetchError>;
}

/// Handler invoked after every successful refresh (the `models_dev.refreshed`
/// event seam).
pub type RefreshListener = Arc<dyn Fn() + Send + Sync>;

/// Static catalog configuration (env-flag mirrors of `flag/flag.ts`).
#[derive(Debug, Clone)]
pub struct CatalogConfig {
    /// `OPENCODE_MODELS_URL` — empty is falsy, falls back to the default.
    pub source: String,
    /// `OPENCODE_MODELS_PATH` — read-path override (write path unaffected).
    pub models_path_override: Option<PathBuf>,
    /// `OPENCODE_DISABLE_MODELS_FETCH` (truthy: "1"/"true", case-insensitive).
    pub disable_fetch: bool,
    /// `User-Agent` — `opencode/{channel}/{version}/{client}`.
    pub user_agent: String,
    /// Freshness window for the on-disk cache.
    pub ttl: Duration,
    /// Repeating refresh period.
    pub refresh_interval: Duration,
    /// Base delay for the fetch backoff.
    pub retry_backoff_base: Duration,
    /// Compile-time catalog stub (TS `OPENCODE_MODELS_DEV`); wire: skip if none.
    pub snapshot: Option<Providers>,
}

/// `truthy()` — `flag.ts:4-6`: "true"/"1", lowercased.
fn env_truthy(value: Option<&str>) -> bool {
    matches!(value.map(str::to_lowercase).as_deref(), Some("true" | "1"))
}

impl CatalogConfig {
    /// Gather the configuration from the process environment.
    pub fn from_env() -> CatalogConfig {
        let client = env::var("OPENCODE_CLIENT").unwrap_or_else(|_| "cli".to_string());
        CatalogConfig {
            source: env::var("OPENCODE_MODELS_URL")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_MODELS_SOURCE.to_string()),
            models_path_override: env::var_os("OPENCODE_MODELS_PATH").map(PathBuf::from),
            disable_fetch: env_truthy(env::var("OPENCODE_DISABLE_MODELS_FETCH").ok().as_deref()),
            user_agent: format!("opencode/{INSTALLATION_CHANNEL}/{INSTALLATION_VERSION}/{client}"),
            ttl: TTL,
            refresh_interval: REFRESH_INTERVAL,
            retry_backoff_base: RETRY_BACKOFF_BASE,
            snapshot: None,
        }
    }
}

/// models.dev catalog: TTL disk cache + fetch + hourly refresh.
pub struct CatalogService {
    cfg: CatalogConfig,
    cache_file: PathBuf,
    clock: Arc<dyn Clock>,
    fetcher: Arc<dyn Fetcher>,
    /// In-memory memoization of `populate` (TS `cachedInvalidateWithTTL`).
    state: Mutex<Option<Providers>>,
    /// Single-flight lock for `populate` — TS suspends co-running `get`s.
    populate_lock: Mutex<()>,
    listeners: Mutex<Vec<RefreshListener>>,
}

impl CatalogService {
    /// Build the service over an explicit cache directory (`Global.Path.cache`).
    pub fn new(
        cache_dir: impl AsRef<Path>,
        cfg: CatalogConfig,
        clock: Arc<dyn Clock>,
        fetcher: Arc<dyn Fetcher>,
    ) -> CatalogService {
        CatalogService {
            cache_file: cache_dir.as_ref().join(cache_file_name(&cfg.source)),
            cfg,
            clock,
            fetcher,
            state: Mutex::new(None),
            populate_lock: Mutex::new(()),
            listeners: Mutex::new(Vec::new()),
        }
    }

    /// Cache file path (`models-dev.ts:161-164`).
    pub fn cache_file(&self) -> &Path {
        &self.cache_file
    }

    fn read_path(&self) -> PathBuf {
        self.cfg
            .models_path_override
            .clone()
            .unwrap_or_else(|| self.cache_file.clone())
    }

    /// `fresh()` — `models-dev.ts:168-173`: file exists and
    /// `now - mtime < ttl` (a missing mtime counts as epoch).
    pub fn fresh(&self) -> bool {
        let Ok(meta) = fs::metadata(&self.cache_file) else {
            return false;
        };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.clock.now_ms().saturating_sub(mtime) < self.cfg.ttl.as_millis() as u64
    }

    /// Current catalog: disk → snapshot stub → fetch. In-process concurrent
    /// `get`s share one populate (TS `cachedInvalidateWithTTL` single-flight).
    pub fn get(&self) -> Result<Providers, CoreError> {
        if let Some(cached) = self.state.lock().unwrap().as_ref() {
            return Ok(cached.clone());
        }
        let _single_flight = self.populate_lock.lock().unwrap();
        // Re-check under the single-flight lock: a racing populate may have
        // completed while we waited.
        if let Some(cached) = self.state.lock().unwrap().as_ref() {
            return Ok(cached.clone());
        }
        let populated = self.populate()?;
        *self.state.lock().unwrap() = Some(populated.clone());
        Ok(populated)
    }

    /// Drop the in-memory cache (TS `invalidate`).
    pub fn invalidate(&self) {
        *self.state.lock().unwrap() = None;
    }

    /// `populate` — `models-dev.ts:217-231`.
    fn populate(&self) -> Result<Providers, CoreError> {
        if let Some(from_disk) = self.load_from_disk() {
            return Ok(from_disk);
        }
        if let Some(snapshot) = &self.cfg.snapshot {
            return Ok(snapshot.clone());
        }
        if self.cfg.disable_fetch {
            return Ok(BTreeMap::new());
        }
        // Flock is cross-process: concurrent opencode CLIs can race on this
        // cache file.
        let _flock = self.acquire_flock()?;
        let text = self
            .fetch_and_write()
            .map_err(|e| CoreError::Catalog(e.to_string()))?;
        Catalog::parse(&text).map(|c| c.providers).map_err(|e| {
            CoreError::Catalog(format!(
                "models.dev cache {} is not valid JSON: {e}",
                self.cache_file.display()
            ))
        })
    }

    /// `loadFromDisk` — `models-dev.ts:184-196`: read `OPENCODE_MODELS_PATH`
    /// ?? filepath; on failure, delete the normal cache file (not the
    /// override) so the next fetch replaces it. Returns `None` on any error.
    fn load_from_disk(&self) -> Option<Providers> {
        let read_path = self.read_path();
        let text = fs::read_to_string(&read_path).ok()?;
        match Catalog::parse(&text) {
            Ok(catalog) => Some(catalog.providers),
            Err(_) => {
                if self.cfg.models_path_override.is_none() {
                    let _ = fs::remove_file(&self.cache_file);
                }
                None
            }
        }
    }

    /// `fetchAndWrite` — `models-dev.ts:202-215`: GET, write
    /// `{filepath}.{pid}.{millis}.tmp`, rename into place, remove the temp
    /// file on error.
    fn fetch_and_write(&self) -> Result<String, FetchError> {
        let text = self.fetch_api()?;
        let tempfile = self.cache_file.with_file_name(format!(
            "{}.{}.{}.tmp",
            self.cache_file
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            std::process::id(),
            self.clock.now_ms(),
        ));
        let write = fs::create_dir_all(self.cache_file.parent().unwrap_or(Path::new(".")))
            .and_then(|_| fs::write(&tempfile, &text))
            .and_then(|_| fs::rename(&tempfile, &self.cache_file));
        if let Err(error) = write {
            let _ = fs::remove_file(&tempfile);
            return Err(FetchError::Permanent(format!(
                "failed to write cache file {}: {error}",
                self.cache_file.display()
            )));
        }
        Ok(text)
    }

    /// `fetchApi` — `models-dev.ts:175-182`: GET `{source}/api.json` with the
    /// `User-Agent` header, retried on transient errors
    /// (`retryTransient(times: 2, jittered exponential(200))`).
    fn fetch_api(&self) -> Result<String, FetchError> {
        let url = format!("{}/api.json", self.cfg.source);
        let mut attempt = 0;
        loop {
            match self.fetcher.get(&url, &self.cfg.user_agent) {
                Ok(text) => return Ok(text),
                Err(error @ FetchError::Permanent(_)) => return Err(error),
                Err(error) if attempt >= FETCH_RETRIES => return Err(error),
                Err(_) => {
                    std::thread::sleep(self.backoff(attempt));
                    attempt += 1;
                }
            }
        }
    }

    /// Jittered exponential backoff: 200ms, 400ms, … ±30% derived from the
    /// injected clock (TS `Schedule.exponential(200).pipe(Schedule.jittered)`).
    fn backoff(&self, attempt: u32) -> Duration {
        let base = self.cfg.retry_backoff_base.as_millis() as u64;
        let exp = base.saturating_mul(1u64 << attempt.min(FETCH_RETRIES));
        let jitter = (self.clock.now_ms() % 7) as i64 - 3; // −3..=+3 → ±30%
        let delayed = exp as i64 + exp as i64 * jitter / 10;
        Duration::from_millis(delayed.max(0) as u64)
    }

    /// Blocking cross-process lock on `<cache_file>.lock` (fs4 flock).
    fn acquire_flock(&self) -> Result<File, CoreError> {
        let lockfile = self.cache_file.with_file_name(format!(
            "{}.lock",
            self.cache_file
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        ));
        let open = || -> std::io::Result<File> {
            if let Some(parent) = lockfile.parent() {
                fs::create_dir_all(parent)?;
            }
            let file = OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(&lockfile)?;
            Fs4FileExt::lock_exclusive(&file)?;
            Ok(file)
        };
        open().map_err(|error| {
            CoreError::Catalog(format!(
                "failed to lock models.dev cache {}: {error}",
                lockfile.display()
            ))
        })
    }

    /// Register a handler invoked after every successful refresh (the
    /// `models_dev.refreshed` event seam).
    pub fn add_refresh_listener(&self, listener: RefreshListener) {
        self.listeners.lock().unwrap().push(listener);
    }

    fn notify_refreshed(&self) {
        let listeners = self.listeners.lock().unwrap();
        for listener in listeners.iter() {
            listener();
        }
    }

    /// `refresh` — `models-dev.ts:237-253`. Errors are logged and swallowed
    /// (TS `tapCause → logError → ignore`).
    pub fn refresh(&self, force: bool) {
        if !force && self.fresh() {
            return;
        }
        let Ok(flock) = self.acquire_flock() else {
            tracing::warn!("Failed to fetch models.dev: could not acquire lock");
            return;
        };
        // Re-check under the lock: another process may have refreshed between
        // our outer check and lock acquisition.
        if !force && self.fresh() {
            return;
        }
        match self.fetch_and_write() {
            Ok(_) => {
                self.invalidate();
                self.notify_refreshed();
            }
            Err(error) => tracing::warn!("Failed to fetch models.dev: {error}"),
        }
        drop(flock);
    }

    /// Hourly repeating refresh (TS `models-dev.ts:255-258`), spawned only
    /// when fetch is enabled — mirrors `Schedule.spaced("60 minutes")`:
    /// refresh once, then wait between completions.
    pub fn spawn_refresh_task(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let svc = Arc::clone(self);
        tokio::spawn(async move {
            if svc.cfg.disable_fetch {
                return;
            }
            loop {
                let s = Arc::clone(&svc);
                let _ = tokio::task::spawn_blocking(move || s.refresh(false)).await;
                tokio::time::sleep(svc.cfg.refresh_interval).await;
            }
        })
    }
}

/// `models-{hash}.json` cache filename for non-default sources
/// (`models-dev.ts:161-164`; TS `Hash.fast` is sha1 — the Rust port uses
/// FNV-1a, the filename only needs cross-process self-consistency).
fn cache_file_name(source: &str) -> String {
    if source == DEFAULT_MODELS_SOURCE {
        "models.json".to_string()
    } else {
        format!("models-{}.json", fnv1a_hex(source))
    }
}

/// FNV-1a of the source URL, hex-encoded.
fn fnv1a_hex(input: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("{hash:x}")
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    use super::super::types::Provider;
    use super::*;

    struct MockClock(AtomicU64);

    impl MockClock {
        fn new(now_ms: u64) -> Arc<MockClock> {
            Arc::new(MockClock(AtomicU64::new(now_ms)))
        }
    }

    impl Clock for MockClock {
        fn now_ms(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    struct FakeFetcher {
        hits: AtomicUsize,
        responses: Mutex<VecDeque<Result<String, FetchError>>>,
        /// Repeated once the response queue is drained.
        default: Result<String, FetchError>,
    }

    impl FakeFetcher {
        fn new(responses: Vec<Result<String, FetchError>>) -> Arc<FakeFetcher> {
            Arc::new(FakeFetcher {
                hits: AtomicUsize::new(0),
                default: Err(FetchError::Permanent("not configured".to_string())),
                responses: Mutex::new(responses.into()),
            })
        }

        fn repeating(response: String) -> Arc<FakeFetcher> {
            Arc::new(FakeFetcher {
                hits: AtomicUsize::new(0),
                default: Ok(response),
                responses: Mutex::new(VecDeque::new()),
            })
        }

        fn hit_count(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }
    }

    impl Fetcher for FakeFetcher {
        fn get(&self, _url: &str, _user_agent: &str) -> Result<String, FetchError> {
            self.hits.fetch_add(1, Ordering::SeqCst);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| self.default.clone())
        }
    }

    /// Unique scratch directory under the OS temp dir (std-only `tempfile`
    /// replacement; S8 forbids new deps).
    fn temp_dir() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = env::temp_dir().join(format!(
            "alforria-core-m34-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The default cache file path inside `cache_dir`.
    fn cache_path(cache_dir: &Path) -> PathBuf {
        cache_dir.join("models.json")
    }

    fn provider_json() -> String {
        r#"{"anthropic":{"name":"Anthropic","id":"anthropic","env":[],"models":{}}}"#.to_string()
    }

    fn service(
        cache_dir: &Path,
        fetcher: Arc<FakeFetcher>,
        clock: Arc<MockClock>,
        tweak: impl FnOnce(&mut CatalogConfig),
    ) -> Arc<CatalogService> {
        let mut cfg = CatalogConfig {
            source: DEFAULT_MODELS_SOURCE.to_string(),
            models_path_override: None,
            disable_fetch: false,
            user_agent: "opencode/local/local/cli".to_string(),
            ttl: TTL,
            refresh_interval: REFRESH_INTERVAL,
            retry_backoff_base: Duration::from_secs(0),
            snapshot: None,
        };
        tweak(&mut cfg);
        Arc::new(CatalogService::new(
            cache_dir,
            cfg,
            clock,
            fetcher as Arc<dyn Fetcher>,
        ))
    }

    /// Real wall-clock now, for combining real file mtimes with the mock clock.
    fn real_now_ms() -> u64 {
        SystemClock.now_ms()
    }

    fn set_mtime(path: &Path, time: SystemTime) {
        let file = OpenOptions::new().write(true).open(path).unwrap();
        file.set_times(
            std::fs::FileTimes::new()
                .set_modified(time)
                .set_accessed(time),
        )
        .unwrap();
    }

    #[test]
    fn cache_file_name_is_models_json_for_default_source() {
        assert_eq!(cache_file_name(DEFAULT_MODELS_SOURCE), "models.json");
        assert_eq!(
            cache_file_name("https://example.com"),
            format!("models-{}.json", fnv1a_hex("https://example.com"))
        );
        assert_ne!(
            cache_file_name("https://example.com"),
            cache_file_name("https://example.org")
        );
    }

    #[test]
    fn env_truthy_matches_ts_flag_semantics() {
        assert!(env_truthy(Some("true")));
        assert!(env_truthy(Some("TRUE")));
        assert!(env_truthy(Some("1")));
        assert!(!env_truthy(Some("0")));
        assert!(!env_truthy(Some("yes")));
        assert!(!env_truthy(Some("")));
        assert!(!env_truthy(None));
    }

    #[test]
    fn get_fetches_and_caches_in_memory_when_disk_empty() {
        let dir = temp_dir();
        let fetcher = FakeFetcher::new(vec![Ok(provider_json())]);
        let clock = MockClock::new(real_now_ms());
        let svc = service(&dir, fetcher.clone(), clock, |_| {});

        let providers = svc.get().unwrap();
        assert_eq!(providers["anthropic"].name, "Anthropic");
        assert_eq!(fetcher.hit_count(), 1);

        // In-memory cache: no second HTTP hit.
        let again = svc.get().unwrap();
        assert_eq!(again["anthropic"].id, "anthropic");
        assert_eq!(fetcher.hit_count(), 1);
        // The fetched catalog was written to the disk cache.
        assert!(svc.cache_file().exists());
    }

    #[test]
    fn get_reads_from_disk_without_fetch() {
        let dir = temp_dir();
        fs::write(cache_path(&dir), provider_json()).unwrap();
        let fetcher = FakeFetcher::new(vec![]);
        let clock = MockClock::new(real_now_ms());
        let svc = service(&dir, fetcher.clone(), clock, |_| {});

        assert_eq!(svc.get().unwrap()["anthropic"].name, "Anthropic");
        assert_eq!(fetcher.hit_count(), 0);
    }

    #[test]
    fn snapshot_used_when_disk_missing() {
        let dir = temp_dir();
        let fetcher = FakeFetcher::new(vec![]);
        let clock = MockClock::new(real_now_ms());
        let svc = service(&dir, fetcher.clone(), clock, |cfg| {
            let mut providers = BTreeMap::new();
            providers.insert(
                "stub".to_string(),
                Provider {
                    api: None,
                    name: "Stub".to_string(),
                    env: vec![],
                    id: "stub".to_string(),
                    npm: None,
                    models: BTreeMap::new(),
                },
            );
            cfg.snapshot = Some(providers);
        });
        assert_eq!(svc.get().unwrap()["stub"].name, "Stub");
        assert_eq!(fetcher.hit_count(), 0);
    }

    #[test]
    fn disable_fetch_returns_empty_map() {
        let dir = temp_dir();
        let fetcher = FakeFetcher::new(vec![]);
        let clock = MockClock::new(real_now_ms());
        let svc = service(&dir, fetcher.clone(), clock, |cfg| {
            cfg.disable_fetch = true;
        });
        assert!(svc.get().unwrap().is_empty());
        assert_eq!(fetcher.hit_count(), 0);
    }

    #[test]
    fn corrupt_normal_cache_file_is_deleted_and_refetched() {
        let dir = temp_dir();
        fs::write(cache_path(&dir), "not json").unwrap();
        let fetcher = FakeFetcher::new(vec![
            Err(FetchError::Transient("boom".to_string())),
            Ok(provider_json()),
        ]);
        let clock = MockClock::new(real_now_ms());
        let svc = service(&dir, fetcher.clone(), clock, |_| {});

        assert_eq!(svc.get().unwrap()["anthropic"].name, "Anthropic");
        // The corrupt cache file was replaced by the fetched catalog.
        let text = fs::read_to_string(svc.cache_file()).unwrap();
        assert_eq!(
            Catalog::parse(&text).unwrap().providers["anthropic"].name,
            "Anthropic"
        );
        // 2 hits: one transient failure, one success.
        assert_eq!(fetcher.hit_count(), 2);
    }

    #[test]
    fn corrupt_cache_file_deleted_even_when_fetch_fails() {
        let dir = temp_dir();
        fs::write(cache_path(&dir), "not json").unwrap();
        let fetcher = FakeFetcher::new(vec![Err(FetchError::Permanent("down".to_string()))]);
        let clock = MockClock::new(real_now_ms());
        let svc = service(&dir, fetcher, clock, |_| {});

        assert!(svc.get().is_err());
        assert!(
            !svc.cache_file().exists(),
            "unreadable cache must be deleted"
        );
    }

    #[test]
    fn path_override_reads_override_and_is_never_deleted() {
        let dir = temp_dir();
        let override_path = dir.join("my-models.json");
        fs::write(&override_path, "corrupt").unwrap();
        fs::write(cache_path(&dir), "corrupt").unwrap();
        let fetcher = FakeFetcher::new(vec![Err(FetchError::Permanent("down".to_string()))]);
        let clock = MockClock::new(real_now_ms());
        let svc = service(&dir, fetcher, clock, |cfg| {
            cfg.models_path_override = Some(override_path.clone());
        });

        let _ = svc.get();
        // With an override set nothing is ever deleted (models-dev.ts:187).
        assert!(override_path.exists());
        assert!(svc.cache_file().exists());
    }

    #[test]
    fn path_override_content_wins() {
        let dir = temp_dir();
        fs::write(cache_path(&dir), provider_json()).unwrap();
        let override_path = dir.join("my-models.json");
        fs::write(
            &override_path,
            r#"{"openai":{"name":"OpenAI","id":"openai","env":[],"models":{}}}"#,
        )
        .unwrap();
        let fetcher = FakeFetcher::new(vec![]);
        let clock = MockClock::new(real_now_ms());
        let svc = service(&dir, fetcher, clock, |cfg| {
            cfg.models_path_override = Some(override_path);
        });
        assert_eq!(svc.get().unwrap()["openai"].name, "OpenAI");
    }

    #[test]
    fn fresh_within_ttl_means_refresh_skips_fetch() {
        let dir = temp_dir();
        fs::write(cache_path(&dir), provider_json()).unwrap();
        let fetcher = FakeFetcher::new(vec![]);
        // mtime is real-now; clock matches → within the 5-minute window.
        let clock = MockClock::new(real_now_ms());
        let svc = service(&dir, fetcher.clone(), clock, |_| {});

        assert!(svc.fresh());
        svc.refresh(false);
        assert_eq!(fetcher.hit_count(), 0);
    }

    #[test]
    fn ttl_boundary_is_exclusive() {
        let dir = temp_dir();
        fs::write(cache_path(&dir), provider_json()).unwrap();
        // mtime = epoch; clock exactly ttl later → not fresh (>=, not >).
        set_mtime(&cache_path(&dir), UNIX_EPOCH);
        let fetcher = FakeFetcher::new(vec![]);
        let clock = MockClock::new(TTL.as_millis() as u64);
        let svc = service(&dir, fetcher.clone(), clock, |_| {});
        assert!(!svc.fresh());
        let clock = MockClock::new(TTL.as_millis() as u64 - 1);
        let svc = service(&dir, fetcher, clock, |_| {});
        assert!(svc.fresh());
    }

    #[test]
    fn missing_cache_file_is_not_fresh() {
        let dir = temp_dir();
        let fetcher = FakeFetcher::new(vec![]);
        let clock = MockClock::new(real_now_ms());
        let svc = service(&dir, fetcher, clock, |_| {});
        assert!(!svc.fresh());
    }

    #[test]
    fn stale_triggers_fetch_and_listener() {
        let dir = temp_dir();
        fs::write(cache_path(&dir), provider_json()).unwrap();
        // mtime = epoch; clock 6 minutes later → stale.
        set_mtime(&cache_path(&dir), UNIX_EPOCH);
        let fetcher = FakeFetcher::new(vec![Ok(provider_json())]);
        let clock = MockClock::new(6 * 60 * 1000);
        let svc = service(&dir, fetcher.clone(), clock, |_| {});
        assert!(!svc.fresh());

        let notified = Arc::new(AtomicU64::new(0));
        let notified_count = Arc::clone(&notified);
        svc.add_refresh_listener(Arc::new(move || {
            notified_count.fetch_add(1, Ordering::SeqCst);
        }));

        svc.refresh(false);
        assert_eq!(fetcher.hit_count(), 1);
        assert_eq!(notified.load(Ordering::SeqCst), 1);
        // File mtime was refreshed by the write → now fresh.
        assert!(svc.fresh());
    }

    #[test]
    fn force_bypasses_freshness() {
        let dir = temp_dir();
        fs::write(cache_path(&dir), provider_json()).unwrap();
        let fetcher = FakeFetcher::new(vec![Ok(provider_json())]);
        let clock = MockClock::new(real_now_ms());
        let svc = service(&dir, fetcher.clone(), clock, |_| {});

        assert!(svc.fresh());
        svc.refresh(true);
        assert_eq!(fetcher.hit_count(), 1);
    }

    #[test]
    fn refresh_swallows_fetch_errors() {
        let dir = temp_dir();
        fs::write(cache_path(&dir), provider_json()).unwrap();
        set_mtime(&cache_path(&dir), UNIX_EPOCH);
        let fetcher = FakeFetcher::new(vec![Err(FetchError::Permanent("down".to_string()))]);
        let clock = MockClock::new(6 * 60 * 1000);
        let svc = service(&dir, fetcher, clock, |_| {});
        svc.refresh(false); // must not panic
    }

    #[tokio::test]
    async fn contended_get_fires_exactly_one_http_hit() {
        let dir = temp_dir();
        let fetcher = FakeFetcher::new(vec![Ok(provider_json())]);
        let clock = MockClock::new(real_now_ms());
        let svc = service(&dir, fetcher.clone(), clock, |_| {});

        let mut tasks = Vec::new();
        for _ in 0..2 {
            let svc = Arc::clone(&svc);
            tasks.push(tokio::task::spawn_blocking(move || svc.get()));
        }
        for task in tasks {
            assert!(task.await.unwrap().is_ok());
        }
        assert_eq!(fetcher.hit_count(), 1);
    }

    #[tokio::test]
    async fn contended_refresh_rechecks_freshness_under_flock() {
        let dir = temp_dir();
        fs::write(cache_path(&dir), provider_json()).unwrap();
        set_mtime(&cache_path(&dir), UNIX_EPOCH);
        let fetcher = FakeFetcher::new(vec![Ok(provider_json())]);
        let clock = MockClock::new(6 * 60 * 1000);
        let svc = service(&dir, fetcher.clone(), clock, |_| {});

        let mut tasks = Vec::new();
        for _ in 0..2 {
            let svc = Arc::clone(&svc);
            tasks.push(tokio::task::spawn_blocking(move || svc.refresh(false)));
        }
        for task in tasks {
            task.await.unwrap();
        }
        // The loser of the flock race re-checks freshness and skips.
        assert_eq!(fetcher.hit_count(), 1);
    }

    #[tokio::test]
    async fn repeating_refresh_task_refreshes_and_notifies() {
        let dir = temp_dir();
        fs::write(cache_path(&dir), provider_json()).unwrap();
        set_mtime(&cache_path(&dir), UNIX_EPOCH);
        let notified = Arc::new(AtomicU64::new(0));
        let notified_count = Arc::clone(&notified);

        // Repeating fetch responses and a clock far past any real file mtime,
        // so every tick is stale and refreshes.
        let svc = service(
            &dir,
            FakeFetcher::repeating(provider_json()),
            MockClock::new(u64::MAX / 2),
            |cfg| {
                cfg.refresh_interval = Duration::from_millis(10);
            },
        );
        svc.add_refresh_listener(Arc::new(move || {
            notified_count.fetch_add(1, Ordering::SeqCst);
        }));

        let handle = svc.spawn_refresh_task();
        // First iteration refreshes immediately; wait for at least 2 ticks.
        let mut refreshed = 0;
        for _ in 0..200 {
            tokio::time::sleep(Duration::from_millis(5)).await;
            refreshed = notified.load(Ordering::SeqCst);
            if refreshed >= 2 {
                break;
            }
        }
        handle.abort();
        assert!(refreshed >= 2, "hourly task should have refreshed twice");
    }

    #[test]
    fn fetch_error_display() {
        assert_eq!(
            FetchError::Transient("x".to_string()).to_string(),
            "transient: x"
        );
        assert_eq!(
            FetchError::Permanent("y".to_string()).to_string(),
            "permanent: y"
        );
    }

    #[test]
    fn fetch_transient_retries_then_gives_up() {
        let dir = temp_dir();
        let fetcher = FakeFetcher::new(vec![
            Err(FetchError::Transient("a".to_string())),
            Err(FetchError::Transient("b".to_string())),
            Err(FetchError::Transient("c".to_string())),
        ]);
        let clock = MockClock::new(real_now_ms());
        let svc = service(&dir, fetcher.clone(), clock, |_| {});
        assert!(svc.get().is_err());
        // initial attempt + 2 retries
        assert_eq!(fetcher.hit_count(), 3);
    }

    #[test]
    fn fetch_permanent_error_not_retried() {
        let dir = temp_dir();
        let fetcher = FakeFetcher::new(vec![Err(FetchError::Permanent("no".to_string()))]);
        let clock = MockClock::new(real_now_ms());
        let svc = service(&dir, fetcher.clone(), clock, |_| {});
        assert!(svc.get().is_err());
        assert_eq!(fetcher.hit_count(), 1);
    }

    #[test]
    fn hashed_source_cache_file_name() {
        let dir = temp_dir();
        let fetcher = FakeFetcher::new(vec![]);
        let clock = MockClock::new(real_now_ms());
        let svc = service(&dir, fetcher, clock, |cfg| {
            cfg.source = "https://example.com".to_string();
        });
        let name = svc.cache_file().file_name().unwrap().to_string_lossy();
        assert_ne!(name, "models.json");
        assert!(name.starts_with("models-"));
        assert!(name.ends_with(".json"));
    }
}

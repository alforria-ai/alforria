//! Per-server state — port of `server/auth.ts` (config half), TS
//! `InstanceStore` and the `UiBackend` seam.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use opencode_core::{BackgroundJobs, EventBus, SessionServices, SessionStore, Storage};

use crate::error::ServerError;
use crate::sse::{GlobalBus, GlobalEvent, INSTANCE_DISPOSED_TYPE};

/// `OPENCODE_SERVER_PASSWORD` / `OPENCODE_SERVER_USERNAME` config
/// (`server/auth.ts:17-20`).
#[derive(Debug, Clone)]
pub struct AuthConfig {
    pub username: String,
    pub password: Option<String>,
}

impl AuthConfig {
    pub fn new(username: impl Into<String>, password: Option<String>) -> AuthConfig {
        AuthConfig {
            username: username.into(),
            password,
        }
    }

    pub fn from_env() -> AuthConfig {
        // Effect's `withDefault("opencode")` applies only when the env var is
        // absent — a set-but-empty `OPENCODE_SERVER_USERNAME` stays empty
        // (ConfigProvider.js:639-641).
        AuthConfig {
            username: std::env::var("OPENCODE_SERVER_USERNAME")
                .unwrap_or_else(|_| "opencode".to_string()),
            password: std::env::var("OPENCODE_SERVER_PASSWORD").ok(),
        }
    }

    /// Auth is enforced only when the password is present *and* non-empty
    /// (`auth.ts:24-26`).
    pub fn required(&self) -> bool {
        match &self.password {
            Some(password) => !password.is_empty(),
            None => false,
        }
    }

    /// `authorized` (`auth.ts:22-28`).
    pub fn authorized(&self, username: &str, password: &str) -> bool {
        match &self.password {
            Some(expected) => username == self.username && password == expected,
            None => false,
        }
    }

    /// Client helper: `Basic base64(user:pass)` (`auth.ts:36-42`).
    pub fn header(&self) -> Option<String> {
        let password = self.password.as_ref()?;
        if password.is_empty() {
            return None;
        }
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", self.username, password));
        Some(format!("Basic {encoded}"))
    }
}

/// Creates the per-directory session services for an instance. M6.3 wires the
/// real factory (config loading + storage bootstrap); tests inject their own.
pub type InstanceFactory =
    Arc<dyn Fn(&Path) -> Result<Arc<SessionServices>, ServerError> + Send + Sync>;

/// Cached directory → `Arc<SessionServices>` map (TS `InstanceStore`,
/// `project/instance-store.ts:17-29`).
#[derive(Clone)]
pub struct InstanceStore {
    factory: InstanceFactory,
    entries: Arc<Mutex<HashMap<PathBuf, Arc<SessionServices>>>>,
    /// Disposal emissions feed the GlobalBus (`disposeContext`,
    /// `instance-store.ts:78-93`).
    global_bus: Arc<Mutex<Option<GlobalBus>>>,
}

impl InstanceStore {
    pub fn new(factory: InstanceFactory) -> InstanceStore {
        InstanceStore {
            factory,
            entries: Arc::new(Mutex::new(HashMap::new())),
            global_bus: Arc::new(Mutex::new(None)),
        }
    }

    /// The GlobalBus to emit `server.instance.disposed` events into — wired
    /// by [`ServerContext::new`].
    pub fn set_global_bus(&self, global_bus: GlobalBus) {
        *self.global_bus.lock().unwrap_or_else(|p| p.into_inner()) = Some(global_bus);
    }

    /// `emitDisposed` (`instance-store.ts:79-93`): a
    /// `{type, properties: {directory}}` payload — the emitter assigns the
    /// id. TS also carries the instance project id and the ambient
    /// workspace, which need the project registry / workspace context
    /// (M7).
    fn emit_disposed(&self, directory: &str) {
        let global_bus = self.global_bus.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(global_bus) = global_bus.as_ref() {
            global_bus.emit(GlobalEvent::injected(
                Some(directory.to_string()),
                None,
                None,
                INSTANCE_DISPOSED_TYPE,
                serde_json::json!({ "directory": directory }),
            ));
        }
    }

    /// Load (and cache) the services for a directory. The cache is keyed by
    /// the `FSUtil.resolve`d directory (`InstanceStore.load`,
    /// `project/instance-store.ts:130-137`).
    pub fn load(&self, directory: &Path) -> Result<Arc<SessionServices>, ServerError> {
        let directory = resolve_directory(directory);
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(existing) = entries.get(&directory) {
            return Ok(existing.clone());
        }
        let services = (self.factory)(&directory)?;
        entries.insert(directory.clone(), services.clone());
        Ok(services)
    }

    /// Drop the cached instance for a directory (TS `disposeDirectory`).
    pub fn dispose_directory(&self, directory: &Path) {
        let directory = resolve_directory(directory);
        let removed = self
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&directory)
            .is_some();
        if removed {
            self.emit_disposed(&directory.display().to_string());
        }
    }

    /// Drop every cached instance (TS `disposeAll`) — each disposed instance
    /// emits its own disposed event.
    pub fn dispose_all(&self) {
        let removed: Vec<PathBuf> = self
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain()
            .map(|(directory, _)| directory)
            .collect();
        for directory in removed {
            self.emit_disposed(&directory.display().to_string());
        }
    }
}

/// `FSUtil.resolve` (`core/src/fs-util.ts:247-258`): `path.resolve` against
/// the process cwd, then realpath normalization; a directory that does not
/// exist keeps its lexically normalized form.
pub fn resolve_directory(p: &Path) -> PathBuf {
    let resolved = if p.is_absolute() {
        lexical_normalize(p)
    } else {
        lexical_normalize(&cwd().join(p))
    };
    match std::fs::canonicalize(&resolved) {
        Ok(real) => real,
        Err(_) => resolved,
    }
}

/// `process.cwd()`.
pub fn cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_default()
}

/// `path.resolve`-style lexical normalization — collapse `.`/`..` without
/// touching the filesystem; `..` above the root clamps to the root.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut parts: Vec<OsString> = Vec::new();
    let mut root = PathBuf::new();
    for component in path.components() {
        match component {
            c @ (Component::Prefix(_) | Component::RootDir) => {
                root.push(c.as_os_str());
                parts.clear();
            }
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop();
            }
            Component::Normal(part) => parts.push(part.to_os_string()),
        }
    }
    for part in parts {
        root.push(part);
    }
    root
}

/// No-op `BackgroundJob.Service` — background jobs are a per-session runner
/// concept (M5.7); the server has no job registry to expose.
pub struct NoBackgroundJobs;

impl BackgroundJobs for NoBackgroundJobs {
    fn list(&self) -> Result<Vec<opencode_core::BackgroundJobInfo>, opencode_core::CoreError> {
        Ok(Vec::new())
    }

    fn cancel(&self, _id: &str) -> Result<(), opencode_core::CoreError> {
        Ok(())
    }
}

/// One file served from the embedded web UI map.
#[derive(Debug, Clone)]
pub struct UiFile {
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// Embedded web-UI seam. M6.1 serves from an empty map — every lookup misses
/// and the catch-all falls through to the 404 JSON envelope
/// (`shared/ui.ts:51-53`). Proxying to `https://app.opencode.ai` is out of
/// scope (spec M6 S7).
pub trait UiBackend: Send + Sync {
    /// TS `embeddedWebUI[<path>]`.
    fn get(&self, _path: &str) -> Option<UiFile> {
        None
    }

    /// TS `embeddedWebUI["index.html"]` — the SPA fallback.
    fn index(&self) -> Option<UiFile> {
        None
    }
}

/// The empty default backend: no embedded UI at all.
#[derive(Debug, Clone, Default)]
pub struct EmptyUiBackend;

impl UiBackend for EmptyUiBackend {}

/// SSE heartbeat intervals. TS bakes `Stream.tick("10 seconds")` /
/// `Stream.tick("15 seconds")` into the stream handlers
/// (`handlers/event.ts:63`, `handlers/global.ts:35`,
/// `packages/server/src/handlers/event.ts:37`); the Rust port injects them
/// through the context so tests need not sleep for real intervals.
#[derive(Debug, Clone)]
pub struct HeartbeatConfig {
    /// v1 streams (`/event`, `/global/event`): a `server.heartbeat` event
    /// every 10 s.
    pub v1: Duration,
    /// `/api/event`: an SSE comment every 15 s.
    pub v2: Duration,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        HeartbeatConfig {
            v1: Duration::from_secs(10),
            v2: Duration::from_secs(15),
        }
    }
}

/// Shared per-server state (spec §2.2): the auth config, the per-directory
/// instance cache and the global bus/storage handle.
#[derive(Clone)]
pub struct ServerContext {
    pub auth: AuthConfig,
    pub instances: InstanceStore,
    /// `Session.Service` lookups for workspace routing (`/session/:id/...`
    /// resolves the session's directory; `shared/workspace-routing.ts:20-29`).
    pub sessions: SessionStore,
    pub storage: Arc<Storage>,
    pub bus: Arc<EventBus>,
    /// Additional allowed CORS origins (CLI `--cors` list).
    pub cors: Vec<String>,
    pub ui: Arc<dyn UiBackend>,
    /// The GlobalBus feeding `/global/event` + the v1 `/event` disposal
    /// terminator, bridged onto `bus` (`bus/global.ts`, `event-v2-bridge.ts`).
    pub global_bus: GlobalBus,
    pub heartbeat: HeartbeatConfig,
}

impl ServerContext {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        auth: AuthConfig,
        instances: InstanceStore,
        storage: Arc<Storage>,
        bus: Arc<EventBus>,
        cors: Vec<String>,
        ui: Arc<dyn UiBackend>,
    ) -> ServerContext {
        // TS Session.Service is a global DB-backed layer (session.ts:474,
        // :540-546) — its bridge carries the session projectors, so the
        // store both writes and reads the session table.
        opencode_core::register_projectors(&bus);
        let global_bus = GlobalBus::bridged(Arc::clone(&bus));
        instances.set_global_bus(global_bus.clone());
        let sessions = SessionStore::new(
            bus.clone(),
            storage.clone(),
            Arc::new(NoBackgroundJobs),
            Arc::new(opencode_core::catalog::SystemClock),
        );
        ServerContext {
            auth,
            instances,
            sessions,
            storage,
            bus,
            cors,
            ui,
            global_bus,
            heartbeat: HeartbeatConfig::default(),
        }
    }

    /// Context backed by an in-memory database and an instance factory that
    /// always fails — M6.1 routes never load instances; M6.3 replaces the
    /// factory.
    pub fn for_tests() -> ServerContext {
        Self::for_tests_with_auth(AuthConfig::new("opencode", None))
    }

    /// `for_tests` with an explicit auth config (M6.2 auth-middleware tests).
    pub fn for_tests_with_auth(auth: AuthConfig) -> ServerContext {
        let storage = Arc::new(Storage::open_in_memory().expect("in-memory storage"));
        ServerContext::new(
            auth,
            InstanceStore::new(Arc::new(|_directory| {
                Err(ServerError::Core(opencode_core::CoreError::Storage(
                    "instance factory not wired (M6.3)".to_string(),
                )))
            })),
            storage.clone(),
            Arc::new(EventBus::new_shared(storage, None)),
            Vec::new(),
            Arc::new(EmptyUiBackend),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_required_only_for_non_empty_password() {
        let mut config = AuthConfig::new("opencode", None);
        assert!(!config.required());
        config = AuthConfig::new("opencode", Some(String::new()));
        assert!(!config.required());
        config = AuthConfig::new("opencode", Some("hunter2".into()));
        assert!(config.required());
    }

    #[test]
    fn auth_authorized_checks_username_and_password() {
        let config = AuthConfig::new("opencode", Some("hunter2".into()));
        assert!(config.authorized("opencode", "hunter2"));
        assert!(!config.authorized("admin", "hunter2"));
        assert!(!config.authorized("opencode", "wrong"));

        let unset = AuthConfig::new("opencode", None);
        assert!(!unset.authorized("opencode", ""));
    }

    #[test]
    fn auth_header_client_helper() {
        // base64("opencode:hunter2") = b3BlbmNvZGU6aHVudGVyMg==
        let config = AuthConfig::new("opencode", Some("hunter2".into()));
        assert_eq!(
            config.header().as_deref(),
            Some("Basic b3BlbmNvZGU6aHVudGVyMg==")
        );

        assert_eq!(AuthConfig::new("opencode", None).header(), None);
        assert_eq!(
            AuthConfig::new("opencode", Some(String::new())).header(),
            None
        );
    }

    #[test]
    fn instance_store_caches_and_disposes() {
        let counter = Arc::new(Mutex::new(0));
        let counter_clone = counter.clone();
        let store = InstanceStore::new(Arc::new(move |_directory| {
            *counter_clone.lock().unwrap() += 1;
            Ok(Arc::new(test_services()))
        }));

        let dir = Path::new("/repo");
        store.load(dir).unwrap();
        store.load(dir).unwrap();
        assert_eq!(
            *counter.lock().unwrap(),
            1,
            "factory runs once per directory"
        );

        store.load(Path::new("/other")).unwrap();
        assert_eq!(*counter.lock().unwrap(), 2);

        store.dispose_directory(dir);
        store.load(dir).unwrap();
        assert_eq!(
            *counter.lock().unwrap(),
            3,
            "dispose evicts the cached instance"
        );

        store.dispose_all();
        store.load(Path::new("/other")).unwrap();
        assert_eq!(*counter.lock().unwrap(), 4);
    }

    #[test]
    fn instance_store_propagates_factory_error() {
        let store: InstanceStore = InstanceStore::new(Arc::new(|_directory| {
            Err(ServerError::Core(opencode_core::CoreError::Storage(
                "boom".to_string(),
            )))
        }));
        assert!(store.load(Path::new("/repo")).is_err());
    }

    #[test]
    fn empty_ui_backend_misses_everything() {
        let ctx = ServerContext::for_tests();
        assert!(ctx.ui.get("/").is_none());
        assert!(ctx.ui.index().is_none());
    }

    #[test]
    fn resolve_directory_matches_fsutil_resolve() {
        assert_eq!(
            resolve_directory(Path::new("/repo/../other")),
            PathBuf::from("/other")
        );
        assert_eq!(
            resolve_directory(Path::new("/repo/./x/")),
            PathBuf::from("/repo/x")
        );
        // `..` above the root clamps to the root (path.resolve).
        assert_eq!(resolve_directory(Path::new("/..")), PathBuf::from("/"));
        // Relative paths resolve against the cwd.
        assert!(resolve_directory(Path::new("some/dir")).is_absolute());
        // An existing directory resolves through its real path.
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_directory(&dir.path().join("sub/../")),
            dir.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn instance_store_keys_by_the_resolved_directory() {
        let counter = Arc::new(Mutex::new(0));
        let counter_clone = counter.clone();
        let store = InstanceStore::new(Arc::new(move |_directory| {
            *counter_clone.lock().unwrap() += 1;
            Ok(Arc::new(test_services()))
        }));

        store.load(Path::new("/repo")).unwrap();
        // Trailing slashes and `..` segments resolve to the same instance
        // (FSUtil.resolve keys the cache).
        store.load(Path::new("/repo/")).unwrap();
        store.load(Path::new("/repo/x/../")).unwrap();
        assert_eq!(*counter.lock().unwrap(), 1);

        store.dispose_directory(Path::new("/repo/./"));
        store.load(Path::new("/repo")).unwrap();
        assert_eq!(*counter.lock().unwrap(), 2);
    }

    fn test_services() -> SessionServices {
        let dir = tempfile::tempdir().unwrap();
        let worktree = dir.path().join("repo");
        std::fs::create_dir_all(&worktree).unwrap();
        let storage = Arc::new(Storage::open(dir.path().join("db.sqlite")).unwrap());
        let agent_input = opencode_core::AgentRegistryInput {
            config: serde_json::from_value(serde_json::json!({})).unwrap(),
            skill_dirs: Vec::new(),
            reference_dirs: Vec::new(),
            worktree: worktree.clone(),
            data_dir: dir.path().to_path_buf(),
            tmp_dir: dir.path().to_path_buf(),
            home: dir.path().to_path_buf(),
        };
        SessionServices::new(
            storage,
            Arc::new(NoJobs),
            Arc::new(FixedClock),
            &agent_input,
        )
    }
    struct FixedClock;
    impl opencode_core::Clock for FixedClock {
        fn now_ms(&self) -> u64 {
            0
        }
    }
    struct NoJobs;
    impl opencode_core::BackgroundJobs for NoJobs {
        fn list(&self) -> Result<Vec<opencode_core::BackgroundJobInfo>, opencode_core::CoreError> {
            Ok(Vec::new())
        }
        fn cancel(&self, _id: &str) -> Result<(), opencode_core::CoreError> {
            Ok(())
        }
    }
}

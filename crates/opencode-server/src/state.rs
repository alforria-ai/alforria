//! Per-server state — port of `server/auth.ts` (config half), TS
//! `InstanceStore` and the `UiBackend` seam.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use opencode_core::{EventBus, SessionServices, Storage};

use crate::error::ServerError;

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
}

impl InstanceStore {
    pub fn new(factory: InstanceFactory) -> InstanceStore {
        InstanceStore {
            factory,
            entries: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Load (and cache) the services for a directory.
    pub fn load(&self, directory: &Path) -> Result<Arc<SessionServices>, ServerError> {
        let mut entries = self.entries.lock().unwrap();
        if let Some(existing) = entries.get(directory) {
            return Ok(existing.clone());
        }
        let services = (self.factory)(directory)?;
        entries.insert(directory.to_path_buf(), services.clone());
        Ok(services)
    }

    /// Drop the cached instance for a directory (TS `disposeDirectory`).
    pub fn dispose_directory(&self, directory: &Path) {
        self.entries.lock().unwrap().remove(directory);
    }

    /// Drop every cached instance (TS `disposeAll`).
    pub fn dispose_all(&self) {
        self.entries.lock().unwrap().clear();
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

/// Shared per-server state (spec §2.2): the auth config, the per-directory
/// instance cache and the global bus/storage handle.
#[derive(Clone)]
pub struct ServerContext {
    pub auth: AuthConfig,
    pub instances: InstanceStore,
    pub storage: Arc<Storage>,
    pub bus: Arc<EventBus>,
    /// Additional allowed CORS origins (CLI `--cors` list).
    pub cors: Vec<String>,
    pub ui: Arc<dyn UiBackend>,
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
        ServerContext {
            auth,
            instances,
            storage,
            bus,
            cors,
            ui,
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

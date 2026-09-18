//! Per-server state — port of `server/auth.ts` (config half), TS
//! `InstanceStore` and the `UiBackend` seam.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use opencode_core::session::prompt::{CommandInput, ShellInput};
use opencode_core::session::prompt_input::{PromptError, PromptInput};
use opencode_core::session::revert::RevertInput;
use opencode_core::{
    BackgroundJobs, CoreError, EventBus, RunnerError, SessionError, SessionServices, SessionStore,
    Storage, WithParts,
};
use opencode_schema::file_diff::SnapshotFileDiff;
use opencode_schema::session_v1::V1SessionInfo;

use crate::error::ServerError;
use crate::middleware::location::LocationContext;
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

/// The per-directory prompt-engine surface the v1 session family drives
/// (TS resolves `SessionPrompt`, `SessionRevert`, `SessionSummary` and
/// `SessionShare` from Effect layers, session.ts:50-62).
///
/// M5 ships the engine pieces but not the per-directory production assembly
/// (LLM stream, instruction, tool registry, share network service) — the
/// server binds them through this seam instead. TODO(M7): production wiring.
pub trait SessionEngine: Send + Sync {
    /// `SessionPrompt.prompt` (prompt.ts:1052-1071).
    fn prompt(&self, input: PromptInput) -> BoxFuture<'static, Result<WithParts, PromptError>>;
    /// `SessionPrompt.loop` (prompt.ts:1350-1354).
    fn loop_(
        &self,
        session_id: String,
    ) -> BoxFuture<'static, Result<WithParts, RunnerError<SessionError>>>;
    /// `SessionPrompt.command` (prompt.ts:1361-1481).
    fn command(&self, input: CommandInput) -> BoxFuture<'static, Result<WithParts, PromptError>>;
    /// `SessionPrompt.shell` (prompt.ts:452-459).
    fn shell(&self, input: ShellInput) -> BoxFuture<'static, Result<WithParts, SessionError>>;
    /// `SessionRevert.revert` / `unrevert` (revert.ts:38-89, 96-99).
    fn revert(&self, input: RevertInput)
        -> BoxFuture<'static, Result<V1SessionInfo, SessionError>>;
    fn unrevert(
        &self,
        session_id: String,
    ) -> BoxFuture<'static, Result<V1SessionInfo, SessionError>>;
    /// `SessionRevert.cleanup` (revert.ts:101-124).
    fn cleanup(&self, session: &V1SessionInfo) -> Result<(), SessionError>;
    /// `SessionSummary.diff` (summary.ts:129-142).
    fn diff(
        &self,
        session_id: &str,
        message_id: Option<&str>,
    ) -> Result<Vec<SnapshotFileDiff>, SessionError>;
    /// `SessionShare.share` / `unshare` (share/session.ts:56-72) — persist
    /// the share on the session (the service owns `session.setShare`).
    fn share(&self, session: &V1SessionInfo) -> Result<(), String>;
    fn unshare(&self, session_id: &str) -> Result<(), String>;
}

/// Resolves the per-directory [`SessionEngine`]. The default (unwired)
/// factory fails every lookup with a defect-500.
pub type EngineFactory =
    Arc<dyn Fn(&LocationContext) -> Result<Arc<dyn SessionEngine>, ServerError> + Send + Sync>;

/// TODO(M7): production engine wiring (LLM stream, instruction, tool
/// registry, share service).
fn unwired_engine_factory(
    _location: &LocationContext,
) -> Result<Arc<dyn SessionEngine>, ServerError> {
    Err(ServerError::Core(CoreError::Storage(
        "session engine factory not wired (M6.5)".to_string(),
    )))
}

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

    /// The cached instances (`/api/session/active` scans every instance's
    /// status map — the process-wide approximation of the V2
    /// `SessionExecution.active` set).
    pub fn cached(&self) -> Vec<Arc<SessionServices>> {
        self.entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .cloned()
            .collect()
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

// ---------------------------------------------------------------------------
// M6.6 service seams
// ---------------------------------------------------------------------------

/// `Auth.Service` seam — auth credentials per provider. TS persists to
/// `~/.local/share/opencode/auth.json`; the M6 default keeps credentials in
/// process memory. TODO(M7): file-backed store.
pub trait AuthStore: Send + Sync {
    fn set(&self, provider_id: &str, info: serde_json::Value) -> Result<(), ServerError>;
    fn remove(&self, provider_id: &str) -> Result<(), ServerError>;
    fn has(&self, provider_id: &str) -> bool;
    fn ids(&self) -> Vec<String>;
}

/// In-memory default [`AuthStore`].
#[derive(Default)]
pub struct MemoryAuthStore {
    entries: Mutex<std::collections::BTreeMap<String, serde_json::Value>>,
}

impl AuthStore for MemoryAuthStore {
    fn set(&self, provider_id: &str, info: serde_json::Value) -> Result<(), ServerError> {
        self.entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(provider_id.to_string(), info);
        Ok(())
    }

    fn remove(&self, provider_id: &str) -> Result<(), ServerError> {
        self.entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(provider_id);
        Ok(())
    }

    fn has(&self, provider_id: &str) -> bool {
        self.entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains_key(provider_id)
    }

    fn ids(&self) -> Vec<String> {
        self.entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .keys()
            .cloned()
            .collect()
    }
}

/// `Installation.Service` seam (`installation/index.ts`). The M6 default is
/// the unknown installation method.
pub trait Installation: Send + Sync {
    /// `method()` — one of `curl|npm|yarn|pnpm|bun|brew|scoop|choco|unknown`.
    fn method(&self) -> &'static str;
    fn upgrade(&self, target: &str) -> Result<(), String>;
}

/// The unwired installation (`method === "unknown"`).
#[derive(Debug, Clone, Copy, Default)]
pub struct UnknownInstallation;

impl Installation for UnknownInstallation {
    fn method(&self) -> &'static str {
        "unknown"
    }

    fn upgrade(&self, _target: &str) -> Result<(), String> {
        Err("Unknown installation method".to_string())
    }
}

/// `Vcs.Service` seam — the instance vcs routes (`project/vcs.ts`).
pub trait VcsService: Send + Sync {
    fn info(&self, directory: &Path) -> Result<serde_json::Value, ServerError>;
    fn status(&self, directory: &Path) -> Result<Vec<serde_json::Value>, ServerError>;
    fn diff(
        &self,
        directory: &Path,
        mode: &str,
        context: Option<i64>,
    ) -> Result<Vec<serde_json::Value>, ServerError>;
    fn diff_raw(&self, directory: &Path) -> Result<String, ServerError>;
    fn apply(
        &self,
        directory: &Path,
        patch: &serde_json::Value,
    ) -> Result<serde_json::Value, ServerError>;
}

/// The unwired vcs seam — empty status shapes (`vcs.ts:235-248`).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoVcs;

impl VcsService for NoVcs {
    fn info(&self, _directory: &Path) -> Result<serde_json::Value, ServerError> {
        Ok(serde_json::json!({
            "branch": "",
            "default_branch": "",
        }))
    }

    fn status(&self, _directory: &Path) -> Result<Vec<serde_json::Value>, ServerError> {
        Ok(Vec::new())
    }

    fn diff(
        &self,
        _directory: &Path,
        _mode: &str,
        _context: Option<i64>,
    ) -> Result<Vec<serde_json::Value>, ServerError> {
        Ok(Vec::new())
    }

    fn diff_raw(&self, _directory: &Path) -> Result<String, ServerError> {
        Ok(String::new())
    }

    fn apply(
        &self,
        _directory: &Path,
        _patch: &serde_json::Value,
    ) -> Result<serde_json::Value, ServerError> {
        Err(ServerError::Core(opencode_core::CoreError::Storage(
            "vcs seam not wired (M6.6)".to_string(),
        )))
    }
}

/// `Skill.Service.all` / `LSP.Service.status` / `Format.Service.status` seams
/// — the M6 defaults are empty status lists.
pub trait StatusSeam: Send + Sync {
    fn status(&self, directory: &Path) -> Result<Vec<serde_json::Value>, ServerError>;
}

/// The empty default — no skills/LSP servers/formatters registered.
#[derive(Debug, Clone, Copy, Default)]
pub struct EmptyStatus;

impl StatusSeam for EmptyStatus {
    fn status(&self, _directory: &Path) -> Result<Vec<serde_json::Value>, ServerError> {
        Ok(Vec::new())
    }
}

/// The `tui-control` queue seam (`shared/tui-control.ts`): two unbounded
/// async queues — the server pops Tui requests (`GET /tui/control/next`) and
/// pushes control responses (`POST /tui/control/response`). M9 wires the
/// ratatui side through `push_request` / `next_response`.
///
/// The receivers sit behind a tokio mutex (not std) because the guard is
/// held across the `recv().await` — a std guard would make every handler
/// future non-Send.
#[derive(Clone)]
pub struct TuiControl {
    requests: Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>>>,
    request_tx: tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
    responses: Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>>>,
    response_tx: tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
}

impl TuiControl {
    /// `submitTuiRequest` (`tui-control.ts:23-25`).
    pub fn push_request(&self, request: serde_json::Value) {
        let _ = self.request_tx.send(request);
    }

    /// `nextTuiRequest` (`tui-control.ts:19-21`).
    pub async fn next_request(&self) -> Option<serde_json::Value> {
        self.requests.lock().await.recv().await
    }

    /// `submitTuiResponse` (`tui-control.ts:27-29`).
    pub fn push_response(&self, response: serde_json::Value) {
        let _ = self.response_tx.send(response);
    }

    /// `nextTuiResponse` (`tui-control.ts:31-33`) — the TUI side.
    pub async fn next_response(&self) -> Option<serde_json::Value> {
        self.responses.lock().await.recv().await
    }
}

impl Default for TuiControl {
    fn default() -> Self {
        Self::new()
    }
}

impl TuiControl {
    /// Create the seam with fresh queues.
    pub fn new() -> TuiControl {
        let (request_tx, requests) = tokio::sync::mpsc::unbounded_channel();
        let (response_tx, responses) = tokio::sync::mpsc::unbounded_channel();
        TuiControl {
            requests: Arc::new(tokio::sync::Mutex::new(requests)),
            request_tx,
            responses: Arc::new(tokio::sync::Mutex::new(responses)),
            response_tx,
        }
    }
}

/// `ToolRegistry.Service` access seam — the production registry wiring (which
/// builtins, code-mode describer) lands with the M7 engine wiring.
pub trait ToolRegistrySource: Send + Sync {
    fn registry(
        &self,
        location: &LocationContext,
    ) -> Result<Arc<opencode_core::tool::registry::ToolRegistry>, ServerError>;
}

struct UnwiredTools;

impl ToolRegistrySource for UnwiredTools {
    fn registry(
        &self,
        _location: &LocationContext,
    ) -> Result<Arc<opencode_core::tool::registry::ToolRegistry>, ServerError> {
        Err(ServerError::Core(opencode_core::CoreError::Storage(
            "tool registry not wired (M6.6)".to_string(),
        )))
    }
}

/// `ProviderAuth.Service` error surface mapped onto the `ProviderAuthError`
/// wire shape (`groups/provider.ts:14-32`).
#[derive(Debug, Clone)]
pub enum ProviderAuthError {
    OauthMissing { provider_id: String },
    OauthCodeMissing { provider_id: String },
    OauthCallbackFailed,
    ValidationFailed { field: String, message: String },
    BadRequest,
}

/// `ProviderAuth.Service` seam (`provider/auth.ts`) — the OAuth machinery is
/// plugin-driven in TS; M6 ships the no-plugin default (`methods` empty,
/// authorize/callback defect) until M7 wires real hooks.
pub trait ProviderAuth: Send + Sync {
    /// `methods()` (auth.ts:131-158) — `Record<providerID, Method[]>`.
    fn methods(&self) -> Result<serde_json::Value, ServerError> {
        Ok(serde_json::json!({}))
    }

    /// `authorize` (auth.ts:160-180) — `Ok(None)` resolves without a result.
    fn authorize(
        &self,
        provider_id: &str,
        method: i64,
        inputs: Option<std::collections::BTreeMap<String, String>>,
    ) -> Result<Option<serde_json::Value>, ProviderAuthError> {
        let _ = (provider_id, method, inputs);
        Err(ProviderAuthError::BadRequest)
    }

    /// `callback` (auth.ts:182-213).
    fn callback(
        &self,
        provider_id: &str,
        method: i64,
        code: Option<String>,
    ) -> Result<(), ProviderAuthError> {
        let _ = (provider_id, method, code);
        Err(ProviderAuthError::BadRequest)
    }
}

/// The unwired default — `hooks[providerID]` access without plugin hooks is
/// a defect in TS (auth.ts:166-167).
#[derive(Debug, Clone, Copy, Default)]
pub struct UnwiredProviderAuth;

impl ProviderAuth for UnwiredProviderAuth {}

/// models.dev catalog source (`ModelsDev.Service.get`). The M6 default reads
/// the disk cache and never fetches — the HTTP fetcher is an core seam M6
/// does not fill.
pub type CatalogSource = Arc<
    dyn Fn() -> Result<opencode_core::catalog::Providers, opencode_core::CoreError> + Send + Sync,
>;

struct NeverFetch;

impl opencode_core::catalog::Fetcher for NeverFetch {
    fn get(
        &self,
        _url: &str,
        _user_agent: &str,
    ) -> Result<String, opencode_core::catalog::FetchError> {
        Err(opencode_core::catalog::FetchError::Permanent(
            "catalog fetch not wired".to_string(),
        ))
    }
}

fn default_catalog() -> CatalogSource {
    let paths = opencode_core::GlobalPaths::from_env();
    let cfg = opencode_core::CatalogConfig::from_env();
    let service = opencode_core::CatalogService::new(
        paths.cache,
        opencode_core::CatalogConfig {
            disable_fetch: true,
            ..cfg
        },
        Arc::new(opencode_core::catalog::SystemClock),
        Arc::new(NeverFetch),
    );
    Arc::new(move || service.get())
}

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
    /// Per-directory [`SessionEngine`] lookup for the session route family
    /// (unwired until M7).
    pub engine_factory: EngineFactory,
    /// M6.6 seams — `Installation.Service`.
    pub installation: Arc<dyn Installation>,
    /// `Auth.Service`.
    pub auth_store: Arc<dyn AuthStore>,
    /// `Vcs.Service`.
    pub vcs: Arc<dyn VcsService>,
    /// `Skill.Service` status list.
    pub skills: Arc<dyn StatusSeam>,
    /// `LSP.Service.status`.
    pub lsp: Arc<dyn StatusSeam>,
    /// `Format.Service.status`.
    pub formatter: Arc<dyn StatusSeam>,
    /// `shared/tui-control.ts` queues.
    pub tui: TuiControl,
    /// The M4 tool registry for `/experimental/tool` (unwired until M7).
    pub tools: Arc<dyn ToolRegistrySource>,
    /// models.dev catalog (`ModelsDev.Service.get`).
    pub catalog: CatalogSource,
    /// `ProviderAuth.Service`.
    pub provider_auth: Arc<dyn ProviderAuth>,
    /// The V2 pending-permission registry (`PermissionV2.Service`). TS owns
    /// one per location node; the M6 adapter keeps it process-wide, filtered
    /// by directory (M6.7).
    pub v2_permissions: Arc<crate::routes::v2::permission::PermissionRegistry>,
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
            engine_factory: Arc::new(unwired_engine_factory),
            installation: Arc::new(UnknownInstallation),
            auth_store: Arc::new(MemoryAuthStore::default()),
            vcs: Arc::new(NoVcs),
            skills: Arc::new(EmptyStatus),
            lsp: Arc::new(EmptyStatus),
            formatter: Arc::new(EmptyStatus),
            tui: TuiControl::new(),
            tools: Arc::new(UnwiredTools),
            catalog: default_catalog(),
            provider_auth: Arc::new(UnwiredProviderAuth),
            v2_permissions: Arc::new(crate::routes::v2::permission::PermissionRegistry::default()),
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

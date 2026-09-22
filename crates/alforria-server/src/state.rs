//! Per-server state — port of `server/auth.ts` (config half), TS
//! `InstanceStore` and the `UiBackend` seam.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alforria_core::session::prompt::{CommandInput, ShellInput};
use alforria_core::session::prompt_input::{PromptError, PromptInput};
use alforria_core::session::revert::RevertInput;
use alforria_core::{
    BackgroundJobs, CoreError, EventBus, RunnerError, SessionError, SessionServices, SessionStore,
    Storage, WithParts,
};
use alforria_schema::file_diff::SnapshotFileDiff;
use alforria_schema::session_v1::V1SessionInfo;
use futures::future::BoxFuture;

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
        // Effect's `withDefault("alforria")` applies only when the env var is
        // absent — a set-but-empty `OPENCODE_SERVER_USERNAME` stays empty
        // (ConfigProvider.js:639-641).
        AuthConfig {
            username: std::env::var("OPENCODE_SERVER_USERNAME")
                .unwrap_or_else(|_| "alforria".to_string()),
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
    /// `SessionShare.share` / `unshare` (share/session.ts:26-37) — persist
    /// the share on the session (the service owns `session.setShare`).
    fn share(&self, session: &V1SessionInfo) -> Result<(), String>;
    fn unshare(&self, session_id: &str) -> Result<(), String>;
    /// `SessionShare.create`'s auto-share fork (share/session.ts:39-46):
    /// parentless sessions when `flags.autoShare || config.share == "auto"`,
    /// failures ignored.
    fn auto_share(&self, _session: &V1SessionInfo) {}
    /// `sessionBackground` (handlers/experimental.ts:178-193): promote the
    /// session's running, non-background task jobs; `true` when any
    /// promoted. `false` when background subagents are disabled.
    fn session_background<'a>(&'a self, _session_id: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async { false })
    }
    /// `projectCopy.generateName` (handlers/project-copy.ts:22-69) — the
    /// one-shot LLM copy-name stream with `Slug.create()` fallbacks.
    fn generate_copy_name<'a>(&'a self, _context: &'a str) -> BoxFuture<'a, String> {
        Box::pin(async { alforria_core::session::agents::slug_create() })
    }
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
    // TODO(M7): the entries lock is held across the factory call
    // (config I/O) — serialize on demand with per-key locks if instance
    // creation shows up in profiles.
    pub fn load(&self, directory: &Path) -> Result<Arc<SessionServices>, ServerError> {
        let directory = resolve_directory(directory);
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(existing) = entries.get(&directory) {
            return Ok(existing.clone());
        }
        let services = (self.factory)(&directory)?;
        // M7.1: the instance bus feeds the global bus so per-instance
        // events reach `/global/event` and `/event` with their location
        // (`event-v2-bridge.ts:26-62`).
        let global_bus = self.global_bus.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(global_bus) = global_bus.as_ref() {
            global_bus.chain(services.events.clone());
        }
        entries.insert(directory.clone(), services.clone());
        // M7.1: every instance boot emits `project.updated` with directory
        // "global" (`project.ts:305` → `GlobalEvents.publish`). The
        // production factory resolves the project with a no-op sink, so
        // the boot frame is emitted here, where the global bus is known.
        if let (Some(global_bus), Some(location)) =
            (global_bus.as_ref(), services.instance_location())
        {
            global_bus.emit(GlobalEvent::injected(
                Some("global".to_string()),
                Some(location.project.id.clone()),
                None,
                "project.updated",
                serde_json::to_value(&location.project).unwrap_or_default(),
            ));
        }
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
    fn list(&self) -> Result<Vec<alforria_core::BackgroundJobInfo>, alforria_core::CoreError> {
        Ok(Vec::new())
    }

    fn cancel(&self, _id: &str) -> Result<(), alforria_core::CoreError> {
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
/// `~/.local/share/opencode/auth.json` (`auth/index.ts`); the file-backed
/// [`FileAuthStore`] is the production store.
pub trait AuthStore: Send + Sync {
    fn set(&self, provider_id: &str, info: serde_json::Value) -> Result<(), ServerError>;
    fn remove(&self, provider_id: &str) -> Result<(), ServerError>;
    fn has(&self, provider_id: &str) -> bool;
    fn ids(&self) -> Vec<String>;
    /// `Auth.all()` (auth/index.ts:58-67) — the decoded `Info` entries.
    fn all(&self) -> Result<std::collections::BTreeMap<String, serde_json::Value>, ServerError>;
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

    fn all(&self) -> Result<std::collections::BTreeMap<String, serde_json::Value>, ServerError> {
        Ok(self
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone())
    }
}

/// The `Auth.Info` union (auth/index.ts:14-37) — entries that fail the
/// decode are dropped (`Record.filterMap`).
fn decode_auth_info(value: &serde_json::Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    let string = |key: &str| {
        object
            .get(key)
            .and_then(serde_json::Value::as_str)
            .is_some()
    };
    match object.get("type").and_then(serde_json::Value::as_str) {
        Some("oauth") => string("refresh") && string("access"),
        Some("api") => string("key"),
        Some("wellknown") => string("key") && string("token"),
        _ => false,
    }
}

/// File-backed `Auth.Service` (`auth/index.ts`): reads/writes
/// `<data>/auth.json` with the `OPENCODE_AUTH_CONTENT` env override
/// (auth/index.ts:52-93).
pub struct FileAuthStore {
    file: PathBuf,
    lock: Mutex<()>,
}

impl FileAuthStore {
    pub fn new(file: PathBuf) -> FileAuthStore {
        FileAuthStore {
            file,
            lock: Mutex::new(()),
        }
    }

    fn all_unlocked(
        &self,
    ) -> Result<std::collections::BTreeMap<String, serde_json::Value>, ServerError> {
        if let Ok(content) = std::env::var("OPENCODE_AUTH_CONTENT") {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) {
                return Ok(value
                    .as_object()
                    .map(|object| {
                        object
                            .iter()
                            .map(|(key, value)| (key.clone(), value.clone()))
                            .collect()
                    })
                    .unwrap_or_default());
            }
        }
        let data = std::fs::read_to_string(&self.file)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .filter(|value| value.is_object())
            .unwrap_or_else(|| serde_json::json!({}));
        let mut out = std::collections::BTreeMap::new();
        for (key, value) in data.as_object().unwrap_or(&serde_json::Map::new()) {
            if decode_auth_info(value) {
                out.insert(key.clone(), value.clone());
            }
        }
        Ok(out)
    }

    fn write(&self, data: &serde_json::Value) -> Result<(), ServerError> {
        if let Some(parent) = self.file.parent() {
            std::fs::create_dir_all(parent).map_err(|err| {
                ServerError::Core(alforria_core::CoreError::Storage(format!(
                    "Failed to write auth data: {err}"
                )))
            })?;
        }
        let text = serde_json::to_string(data).map_err(|err| {
            ServerError::Core(alforria_core::CoreError::Storage(format!(
                "Failed to write auth data: {err}"
            )))
        })?;
        std::fs::write(&self.file, text).map_err(|err| {
            ServerError::Core(alforria_core::CoreError::Storage(format!(
                "Failed to write auth data: {err}"
            )))
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.file, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }
}

impl AuthStore for FileAuthStore {
    fn set(&self, provider_id: &str, info: serde_json::Value) -> Result<(), ServerError> {
        let _guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        let norm = provider_id.trim_end_matches('/');
        let mut data = serde_json::Value::Object(
            self.all_unlocked()?
                .into_iter()
                .collect::<serde_json::Map<String, serde_json::Value>>(),
        );
        let object = data
            .as_object_mut()
            .expect("auth entries are always an object");
        if norm != provider_id {
            object.remove(provider_id);
        }
        let slash_key = format!("{norm}/");
        object.remove(&slash_key);
        object.insert(norm.to_string(), info);
        self.write(&data)
    }

    fn remove(&self, provider_id: &str) -> Result<(), ServerError> {
        let _guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        let norm = provider_id.trim_end_matches('/');
        let mut data = serde_json::Value::Object(
            self.all_unlocked()?
                .into_iter()
                .collect::<serde_json::Map<String, serde_json::Value>>(),
        );
        let object = data
            .as_object_mut()
            .expect("auth entries are always an object");
        object.remove(provider_id);
        object.remove(norm);
        self.write(&data)
    }

    fn has(&self, provider_id: &str) -> bool {
        self.all()
            .map(|all| all.contains_key(provider_id))
            .unwrap_or(false)
    }

    fn ids(&self) -> Vec<String> {
        self.all()
            .map(|all| all.keys().cloned().collect())
            .unwrap_or_default()
    }

    fn all(&self) -> Result<std::collections::BTreeMap<String, serde_json::Value>, ServerError> {
        let _guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        self.all_unlocked()
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
        Err(ServerError::Core(alforria_core::CoreError::Storage(
            "vcs seam not wired (M6.6)".to_string(),
        )))
    }
}

/// The production vcs service over the core GitCli (`project/vcs.ts`).
/// `NoVcs` stays as the unwired seam default; `production_context` installs
/// this one.
#[derive(Default)]
pub struct CoreVcs {
    inner: alforria_core::vcs::Vcs,
}

impl CoreVcs {
    fn patch_apply_error(err: alforria_core::vcs::PatchApplyError) -> ServerError {
        crate::error::ApiError::VcsApply {
            message: err.message,
            reason: err.reason,
        }
        .into()
    }
}

impl VcsService for CoreVcs {
    fn info(&self, directory: &Path) -> Result<serde_json::Value, ServerError> {
        Ok(serde_json::to_value(self.inner.info(directory)).unwrap_or_default())
    }

    fn status(&self, directory: &Path) -> Result<Vec<serde_json::Value>, ServerError> {
        Ok(self
            .inner
            .status(directory)
            .into_iter()
            .map(|row| serde_json::to_value(row).unwrap_or_default())
            .collect())
    }

    fn diff(
        &self,
        directory: &Path,
        mode: &str,
        context: Option<i64>,
    ) -> Result<Vec<serde_json::Value>, ServerError> {
        Ok(self
            .inner
            .diff(directory, mode, context)
            .into_iter()
            .map(|row| serde_json::to_value(row).unwrap_or_default())
            .collect())
    }

    fn diff_raw(&self, directory: &Path) -> Result<String, ServerError> {
        Ok(self.inner.diff_raw(directory))
    }

    fn apply(
        &self,
        directory: &Path,
        patch: &serde_json::Value,
    ) -> Result<serde_json::Value, ServerError> {
        let patch = patch
            .get("patch")
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                ServerError::Core(alforria_core::CoreError::Storage(format!(
                    "vcs.apply payload is not a string: {patch:?}"
                )))
            })?;
        let result = self
            .inner
            .apply(directory, patch)
            .map_err(Self::patch_apply_error)?;
        Ok(serde_json::to_value(result).unwrap_or_default())
    }
}

/// The server-side [`alforria_core::worktree::Deps`] — the project
/// registry, the instance store and the GlobalBus behind the worktree
/// service.
#[derive(Clone)]
pub struct WorktreeDeps {
    /// `Global.Path.data` — the worktree root base (`worktree/index.ts:208`).
    pub data_dir: PathBuf,
    pub global_bus: GlobalBus,
    pub instances: InstanceStore,
    pub projects: Arc<alforria_core::project::registry::ProjectRegistry>,
}

impl alforria_core::worktree::Deps for WorktreeDeps {
    fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    fn add_sandbox(&self, project_id: &str, directory: &Path) {
        let _ = self
            .projects
            .add_sandbox(project_id, &directory.to_string_lossy());
    }

    fn dispose_directory(&self, directory: &Path) {
        self.instances.dispose_directory(directory);
    }

    fn load_instance(&self, directory: &Path) -> Result<(), String> {
        self.instances
            .load(directory)
            .map(|_| ())
            .map_err(|err| format!("{err:?}"))
    }

    fn start_command(&self, project_id: &str) -> Option<String> {
        self.projects
            .get(project_id)
            .ok()
            .flatten()
            .and_then(|project| project.commands.and_then(|commands| commands.start))
    }

    fn emit(&self, frame: &alforria_core::worktree::Frame) {
        self.global_bus.emit(GlobalEvent::injected(
            Some(frame.directory.to_string_lossy().into_owned()),
            Some(frame.project_id.clone()),
            frame.workspace_id.clone(),
            frame.event_type,
            frame.properties.clone(),
        ));
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
    ) -> Result<Arc<alforria_core::tool::registry::ToolRegistry>, ServerError>;
}

struct UnwiredTools;

impl ToolRegistrySource for UnwiredTools {
    fn registry(
        &self,
        _location: &LocationContext,
    ) -> Result<Arc<alforria_core::tool::registry::ToolRegistry>, ServerError> {
        Err(ServerError::Core(alforria_core::CoreError::Storage(
            "tool registry not wired (M6.6)".to_string(),
        )))
    }
}

/// `MCP.Service` access seam — resolves the per-instance MCP service for
/// the `/mcp` route family and `GET /experimental/resource` (M7.6).
pub trait McpSource: Send + Sync {
    fn service(
        &self,
        location: &LocationContext,
    ) -> Result<Arc<alforria_core::mcp::McpService>, ServerError>;
}

struct UnwiredMcp;

impl McpSource for UnwiredMcp {
    fn service(
        &self,
        _location: &LocationContext,
    ) -> Result<Arc<alforria_core::mcp::McpService>, ServerError> {
        Err(ServerError::Core(alforria_core::CoreError::Storage(
            "mcp service not wired (M7.6)".to_string(),
        )))
    }
}

/// `LSP.Service.status` access seam — resolves the per-instance LSP
/// service for `GET /lsp` (M7.9).
pub trait LspSource: Send + Sync {
    fn status(&self, location: &LocationContext) -> Result<Vec<serde_json::Value>, ServerError>;
}

struct UnwiredLsp;

impl LspSource for UnwiredLsp {
    fn status(&self, _location: &LocationContext) -> Result<Vec<serde_json::Value>, ServerError> {
        Err(ServerError::Core(alforria_core::CoreError::Storage(
            "lsp service not wired (M7.9)".to_string(),
        )))
    }
}

/// `ProviderAuth.Service` error surface mapped onto the `ProviderAuthError`
/// wire shape (`groups/provider.ts:14-32`).
#[derive(Debug, Clone)]
pub enum ProviderAuthError {
    OauthMissing {
        provider_id: String,
    },
    OauthCodeMissing {
        provider_id: String,
    },
    OauthCallbackFailed,
    ValidationFailed {
        field: String,
        message: String,
    },
    BadRequest,
    /// A TS `Effect` defect — authorize on a provider without a plugin
    /// hook throws (auth.ts:166-167); it renders as the defect-500.
    Defect,
}

/// `ProviderAuth.Service` seam (`provider/auth.ts`) — the OAuth machinery is
/// plugin-driven in TS; without a plugin runtime the hook registry ships
/// empty (M7 §7.4), so `methods()` is `{}`, `authorize` defects and
/// `callback` reports the missing pending flow.
pub trait ProviderAuth: Send + Sync {
    /// `methods()` (auth.ts:131-158) — `Record<providerID, Method[]>`.
    fn methods(&self) -> Result<serde_json::Value, ServerError> {
        Ok(serde_json::json!({}))
    }

    /// `authorize` (auth.ts:160-186) — `Ok(None)` resolves without a result.
    fn authorize(
        &self,
        provider_id: &str,
        method: i64,
        inputs: Option<std::collections::BTreeMap<String, String>>,
    ) -> Result<Option<serde_json::Value>, ProviderAuthError> {
        let _ = (provider_id, method, inputs);
        Err(ProviderAuthError::BadRequest)
    }

    /// `callback` (auth.ts:188-221).
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

/// The production `ProviderAuth.Service` (provider/auth.ts:109-223) with an
/// empty plugin-hook registry: the pending-oauth map and the authorize/
/// callback error mapping.
#[derive(Default)]
pub struct ProviderAuthService {
    pending: Mutex<HashMap<String, serde_json::Value>>,
}

impl ProviderAuthService {
    pub fn new() -> ProviderAuthService {
        ProviderAuthService::default()
    }
}

impl ProviderAuth for ProviderAuthService {
    fn methods(&self) -> Result<serde_json::Value, ServerError> {
        Ok(serde_json::json!({}))
    }

    fn authorize(
        &self,
        provider_id: &str,
        _method: i64,
        _inputs: Option<std::collections::BTreeMap<String, String>>,
    ) -> Result<Option<serde_json::Value>, ProviderAuthError> {
        // `hooks[input.providerID].methods[input.method]` — the no-plugin
        // registry dereferences undefined and throws (auth.ts:166-167).
        let _ = provider_id;
        Err(ProviderAuthError::Defect)
    }

    fn callback(
        &self,
        provider_id: &str,
        _method: i64,
        _code: Option<String>,
    ) -> Result<(), ProviderAuthError> {
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !pending.contains_key(provider_id) {
            return Err(ProviderAuthError::OauthMissing {
                provider_id: provider_id.to_string(),
            });
        }
        // A registered pending flow requires a plugin hook to complete
        // (match.callback); hooks are empty, so the exchange fails.
        Err(ProviderAuthError::OauthCallbackFailed)
    }
}

/// models.dev catalog source (`ModelsDev.Service.get`). The M6 default reads
/// the disk cache and never fetches — the HTTP fetcher is an core seam M6
/// does not fill.
pub type CatalogSource = Arc<
    dyn Fn() -> Result<alforria_core::catalog::Providers, alforria_core::CoreError> + Send + Sync,
>;

struct NeverFetch;

impl alforria_core::catalog::Fetcher for NeverFetch {
    fn get(
        &self,
        _url: &str,
        _user_agent: &str,
    ) -> Result<String, alforria_core::catalog::FetchError> {
        Err(alforria_core::catalog::FetchError::Permanent(
            "catalog fetch not wired".to_string(),
        ))
    }
}

pub fn default_catalog() -> CatalogSource {
    let paths = alforria_core::GlobalPaths::from_env();
    let cfg = alforria_core::CatalogConfig::from_env();
    let service = alforria_core::CatalogService::new(
        paths.cache,
        alforria_core::CatalogConfig {
            disable_fetch: true,
            ..cfg
        },
        Arc::new(alforria_core::catalog::SystemClock),
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

/// `WebSocketTracker` (`httpapi/websocket-tracker.ts:17-45`): registered
/// PTY sockets receive the server-closing event when the server stops.
#[derive(Clone, Default)]
pub struct WebSocketTracker {
    inner: Arc<Mutex<TrackerInner>>,
}

impl std::fmt::Debug for WebSocketTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        f.debug_struct("WebSocketTracker")
            .field("closing", &inner.closing)
            .field("sockets", &inner.sockets.len())
            .finish()
    }
}

#[derive(Default)]
struct TrackerInner {
    closing: bool,
    next: u64,
    sockets: HashMap<u64, Arc<dyn Fn() + Send + Sync>>,
}

impl WebSocketTracker {
    /// `add` — `None` once the server is closing.
    pub fn register(&self, close: Box<dyn Fn() + Send + Sync>) -> Option<u64> {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if inner.closing {
            return None;
        }
        let id = inner.next;
        inner.next += 1;
        inner.sockets.insert(id, Arc::from(close));
        Some(id)
    }

    /// `remove`.
    pub fn unregister(&self, id: u64) {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .sockets
            .remove(&id);
    }

    /// `closeAll` — send `SERVER_CLOSING_EVENT` to every live socket.
    pub fn close_all(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.closing = true;
        let sockets = inner.sockets.drain().collect::<Vec<_>>();
        for (_, close) in sockets {
            close();
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
    /// The worktree service (`worktree/index.ts`) + its dependencies.
    pub worktree: alforria_core::worktree::Worktree,
    pub worktree_deps: Arc<dyn alforria_core::worktree::Deps>,
    /// `Skill.Service` status list.
    pub skills: Arc<dyn StatusSeam>,
    /// `LSP.Service.status`.
    pub lsp: Arc<dyn LspSource>,
    /// `Format.Service.status`.
    pub formatter: Arc<dyn StatusSeam>,
    /// `shared/tui-control.ts` queues.
    pub tui: TuiControl,
    /// The M4 tool registry for `/experimental/tool` (unwired until M7).
    pub tools: Arc<dyn ToolRegistrySource>,
    /// `MCP.Service` for the `/mcp` route family (unwired until M7.6).
    pub mcp: Arc<dyn McpSource>,
    /// models.dev catalog (`ModelsDev.Service.get`).
    pub catalog: CatalogSource,
    /// `ProviderAuth.Service`.
    pub provider_auth: Arc<dyn ProviderAuth>,
    /// The V2 pending-permission registry (`PermissionV2.Service`). TS owns
    /// one per location node; the M6 adapter keeps it process-wide, filtered
    /// by directory (M6.7).
    pub v2_permissions: Arc<crate::routes::v2::permission::PermissionRegistry>,
    /// `PtyTicket.Service` — the PTY connect-ticket cache (M6.8).
    pub pty_tickets: crate::pty::ticket::TicketCache,
    /// The per-location `Pty.Service` registry (M6.8).
    pub ptys: crate::pty::PtyRegistry,
    /// `WebSocketTracker` — live PTY sockets closed on server stop.
    pub websockets: WebSocketTracker,
    /// `Project.Service` (M7.1) — the project registry over the shared
    /// storage; its `project.updated` emissions feed the GlobalBus.
    pub projects: Arc<alforria_core::project::registry::ProjectRegistry>,
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
        alforria_core::register_projectors(&bus);
        let global_bus = GlobalBus::bridged(Arc::clone(&bus));
        instances.set_global_bus(global_bus.clone());
        let (global_bus_clone, instances_clone) = (global_bus.clone(), instances.clone());
        // `emitUpdated` — the registry's `project.updated` frames
        // (`project.ts:133-140`).
        let projects = Arc::new(alforria_core::project::registry::ProjectRegistry::new(
            storage.clone(),
            Arc::new(alforria_core::git::SubprocessGit),
            Arc::new(alforria_core::catalog::SystemClock),
            {
                let global_bus = global_bus.clone();
                Arc::new(move |info: &alforria_schema::project::ProjectInfo| {
                    global_bus.emit(GlobalEvent::injected(
                        Some("global".to_string()),
                        Some(info.id.clone()),
                        None,
                        "project.updated",
                        serde_json::to_value(info).unwrap_or_default(),
                    ));
                })
            },
        ));
        let sessions = SessionStore::new(
            bus.clone(),
            storage.clone(),
            Arc::new(NoBackgroundJobs),
            Arc::new(alforria_core::catalog::SystemClock),
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
            auth_store: Arc::new(FileAuthStore::new(
                alforria_core::GlobalPaths::from_env()
                    .data
                    .join("auth.json"),
            )),
            vcs: Arc::new(NoVcs),
            worktree: alforria_core::worktree::Worktree::default(),
            worktree_deps: Arc::new(WorktreeDeps {
                data_dir: alforria_core::GlobalPaths::from_env().data,
                global_bus: global_bus_clone,
                instances: instances_clone,
                projects: projects.clone(),
            }),
            skills: Arc::new(EmptyStatus),
            lsp: Arc::new(UnwiredLsp),
            formatter: Arc::new(EmptyStatus),
            tui: TuiControl::new(),
            tools: Arc::new(UnwiredTools),
            mcp: Arc::new(UnwiredMcp),
            catalog: default_catalog(),
            provider_auth: Arc::new(ProviderAuthService::new()),
            v2_permissions: Arc::new(crate::routes::v2::permission::PermissionRegistry::default()),
            pty_tickets: crate::pty::ticket::TicketCache::default(),
            ptys: crate::pty::PtyRegistry::default(),
            websockets: WebSocketTracker::default(),
            projects,
        }
    }

    /// Context backed by an in-memory database and an instance factory that
    /// always fails — M6.1 routes never load instances; M6.3 replaces the
    /// factory.
    pub fn for_tests() -> ServerContext {
        Self::for_tests_with_auth(AuthConfig::new("alforria", None))
    }

    /// `for_tests` with an explicit auth config (M6.2 auth-middleware tests).
    pub fn for_tests_with_auth(auth: AuthConfig) -> ServerContext {
        let storage = Arc::new(Storage::open_in_memory().expect("in-memory storage"));
        ServerContext::new(
            auth,
            InstanceStore::new(Arc::new(|_directory| {
                Err(ServerError::Core(alforria_core::CoreError::Storage(
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
        let mut config = AuthConfig::new("alforria", None);
        assert!(!config.required());
        config = AuthConfig::new("alforria", Some(String::new()));
        assert!(!config.required());
        config = AuthConfig::new("alforria", Some("hunter2".into()));
        assert!(config.required());
    }

    #[test]
    fn auth_authorized_checks_username_and_password() {
        let config = AuthConfig::new("alforria", Some("hunter2".into()));
        assert!(config.authorized("alforria", "hunter2"));
        assert!(!config.authorized("admin", "hunter2"));
        assert!(!config.authorized("alforria", "wrong"));

        let unset = AuthConfig::new("alforria", None);
        assert!(!unset.authorized("alforria", ""));
    }

    #[test]
    fn auth_header_client_helper() {
        // base64("alforria:hunter2") = YWxmb3JyaWE6aHVudGVyMg==
        let config = AuthConfig::new("alforria", Some("hunter2".into()));
        assert_eq!(
            config.header().as_deref(),
            Some("Basic YWxmb3JyaWE6aHVudGVyMg==")
        );

        assert_eq!(AuthConfig::new("alforria", None).header(), None);
        assert_eq!(
            AuthConfig::new("alforria", Some(String::new())).header(),
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
            Err(ServerError::Core(alforria_core::CoreError::Storage(
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
        let agent_input = alforria_core::AgentRegistryInput {
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
    impl alforria_core::Clock for FixedClock {
        fn now_ms(&self) -> u64 {
            0
        }
    }
    struct NoJobs;
    impl alforria_core::BackgroundJobs for NoJobs {
        fn list(&self) -> Result<Vec<alforria_core::BackgroundJobInfo>, alforria_core::CoreError> {
            Ok(Vec::new())
        }
        fn cancel(&self, _id: &str) -> Result<(), alforria_core::CoreError> {
            Ok(())
        }
    }
}

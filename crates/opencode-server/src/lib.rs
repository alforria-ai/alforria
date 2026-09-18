//! `opencode-server` — the HTTP surface of opencode: v1+v2 routers, the
//! tagged-error wire envelopes, the CORS/compression/fence middleware quirks
//! and the `serve` listener bootstrap.

pub mod engine;
pub mod error;
pub mod middleware;
pub mod openapi;
pub mod pty;
pub mod routes;
pub mod sse;
pub mod state;

use std::io;
use std::sync::Arc;

use opencode_core::Storage;

pub use error::{ApiError, ServerError};
pub use state::{
    AuthConfig, EngineFactory, HeartbeatConfig, InstanceStore, ServerContext, SessionEngine,
    UiBackend,
};

/// `resolveNetworkOptions` defaults (`cli/network.ts:6-19`).
pub const DEFAULT_PORT: u16 = 0;
pub const DEFAULT_HOSTNAME: &str = "127.0.0.1";

/// The port `port: 0` prefers before falling back to any free port
/// (`server/server.ts:117-122`).
pub const PORT_FALLBACK: u16 = 4096;

#[derive(Debug, Clone)]
pub struct ListenOptions {
    pub port: u16,
    pub hostname: String,
    /// Additional allowed CORS origins (CLI `--cors`).
    pub cors: Vec<String>,
}

impl Default for ListenOptions {
    fn default() -> Self {
        ListenOptions {
            port: DEFAULT_PORT,
            hostname: DEFAULT_HOSTNAME.to_string(),
            cors: Vec::new(),
        }
    }
}

/// A running listener (`server/server.ts:20-25`).
#[derive(Debug)]
pub struct Listener {
    pub hostname: String,
    pub port: u16,
    pub url: String,
    websockets: state::WebSocketTracker,
    shutdown: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<io::Result<()>>,
}

impl Listener {
    /// Graceful stop: stop accepting, close HTTP sockets and websockets
    /// (`websocket-tracker.ts:17-45` semantics; the tracker close runs
    /// before the listener shuts down so live sockets observe 1001).
    pub async fn stop(self) -> io::Result<()> {
        self.websockets.close_all();
        self.shutdown
            .send(true)
            .map_err(|_| io::Error::other("server task already stopped"))?;
        let _ = self.task.await;
        Ok(())
    }
}

async fn bind_with_port_fallback(opts: &ListenOptions) -> io::Result<tokio::net::TcpListener> {
    if opts.port != 0 {
        return tokio::net::TcpListener::bind((opts.hostname.as_str(), opts.port)).await;
    }
    // Legacy port resolution: explicit 0 prefers 4096 first, then any free
    // port (`server/server.ts:117-122`).
    match tokio::net::TcpListener::bind((opts.hostname.as_str(), PORT_FALLBACK)).await {
        Ok(listener) => Ok(listener),
        Err(_) => tokio::net::TcpListener::bind((opts.hostname.as_str(), 0)).await,
    }
}

/// Bind the server and spawn it on the current tokio runtime.
pub async fn listen_with(opts: &ListenOptions, ctx: Arc<ServerContext>) -> io::Result<Listener> {
    let listener = bind_with_port_fallback(opts).await?;
    let port = listener.local_addr()?.port();
    let url = format!("http://{}:{}", opts.hostname, port);

    let router = routes::build_router(ctx.clone());
    let (shutdown, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let serve = axum::serve(listener, router).with_graceful_shutdown(async move {
        let _ = shutdown_rx.changed().await;
    });
    let task = tokio::spawn(async move { serve.await });

    let hostname = opts.hostname.clone();
    Ok(Listener {
        hostname,
        port,
        url,
        websockets: ctx.websockets.clone(),
        shutdown,
        task,
    })
}

/// Bind a server with a fresh environment-derived context.
pub async fn listen(opts: &ListenOptions) -> io::Result<Listener> {
    let ctx = default_context(opts)?;
    listen_with(opts, ctx).await
}

fn default_context(opts: &ListenOptions) -> io::Result<Arc<ServerContext>> {
    let paths = opencode_core::GlobalPaths::from_env();
    production_context(opts, paths, engine::EngineSeams::default())
}

/// The production server context: shared storage + bus and the per-directory
/// instance factory that builds the M7.2 production engines. Tests inject
/// the engine [`engine::EngineSeams`] (mock LLM); production passes the
/// default (unwired until M7.7).
pub fn production_context(
    opts: &ListenOptions,
    paths: opencode_core::GlobalPaths,
    seams: engine::EngineSeams,
) -> io::Result<Arc<ServerContext>> {
    let storage = Storage::open_default(&paths.data)
        .map_err(|err| io::Error::other(format!("storage open failed: {err}")))?;
    let storage = Arc::new(storage);
    // The session durable manifest — required by the durable streams
    // (`/api/session/:id/event`, `/api/session/:id/history`).
    let manifest = Arc::new(opencode_core::session::event_definitions::SessionManifest::new());
    let bus = Arc::new(opencode_core::EventBus::new_shared(
        storage.clone(),
        Some(manifest),
    ));
    // M7.2: per-instance engines land in the store keyed by the services
    // the LocationContext carries; the engine seam resolves through it.
    let engines = Arc::new(engine::EngineStore::default());
    let instances =
        production_instance_factory(storage.clone(), paths.clone(), engines.clone(), seams);
    let mut ctx = ServerContext::new(
        AuthConfig::from_env(),
        instances,
        storage,
        bus,
        opts.cors.clone(),
        Arc::new(state::EmptyUiBackend),
    );
    ctx.engine_factory = engines.factory();
    ctx.tools = engines.tools();
    ctx.mcp = engines.mcp_source();
    ctx.vcs = Arc::new(state::CoreVcs::default());
    Ok(Arc::new(ctx))
}

/// The per-directory instance factory (TS `InstanceStore.boot` +
/// `InstanceBootstrap.run`, `project/instance-store.ts:46-56`): load the
/// directory's merged config, wire the agent registry and build the M5
/// service graph over the shared storage.
fn production_instance_factory(
    storage: Arc<Storage>,
    paths: opencode_core::GlobalPaths,
    engines: Arc<engine::EngineStore>,
    seams: engine::EngineSeams,
) -> InstanceStore {
    let factory: state::InstanceFactory = Arc::new(move |directory: &std::path::Path| {
        instance_for_directory(
            storage.clone(),
            paths.clone(),
            engines.clone(),
            seams.clone(),
            directory,
        )
    });
    InstanceStore::new(factory)
}

/// One instance boot: config load + project resolution + service wiring
/// (TS `InstanceStore.boot` + `InstanceBootstrap.run`,
/// `project/instance-store.ts:45-61`).
fn instance_for_directory(
    storage: Arc<Storage>,
    paths: opencode_core::GlobalPaths,
    engines: Arc<engine::EngineStore>,
    seams: engine::EngineSeams,
    directory: &std::path::Path,
) -> Result<Arc<opencode_core::SessionServices>, ServerError> {
    let params = opencode_core::LoadParams::new(directory.to_path_buf()).paths(paths.clone());
    let (config, _opencode_dirs) = opencode_core::ConfigLoader::new().load(&params)?;

    // `skill.dirs()` — directory sources from the config (`skills.paths`);
    // http(s) entries are URL sources materialized through the discovery
    // cache (`config/plugin/skill.ts:31-45`, `skill/discovery.ts`).
    let mut skill_dirs: Vec<std::path::PathBuf> = Vec::new();
    if let Some(skills) = config
        .skills
        .as_ref()
        .and_then(|skills| skills.paths.clone())
    {
        let mut items: Vec<String> = Vec::new();
        for item in skills {
            if let Some(url) = http_url(&item) {
                items.push(url);
            } else {
                skill_dirs.push(std::path::PathBuf::from(item));
            }
        }
        if !items.is_empty() {
            let discovery = opencode_core::skill::SkillDiscovery::new(
                paths.cache.clone(),
                Arc::new(opencode_core::skill::ReqwestFetcher),
            );
            for url in items {
                skill_dirs.extend(discovery.pull(&url));
            }
        }
    }

    // `reference.list()` — local sources land directly; git sources
    // materialize to the repository cache path (`reference.ts:60-96`).
    let mut reference_dirs = Vec::new();
    if let Some(references) = config.references.as_ref().or(config.reference.as_ref()) {
        for (name, entry) in references {
            if !valid_alias(name) {
                continue;
            }
            match entry {
                opencode_core::config::schema::ReferenceEntry::Local(local) => {
                    reference_dirs.push(std::path::PathBuf::from(&local.path));
                }
                opencode_core::config::schema::ReferenceEntry::Repository(repository) => {
                    // A string entry is local when it starts with `.`, `/`
                    // or `~`; anything else is a git repository
                    // (`config/plugin/reference.ts:31-38`).
                    if repository.starts_with(['.', '/', '~']) {
                        reference_dirs.push(expand_reference_path(&paths.home, repository));
                    } else if let Some(path) =
                        materialize_git_reference(&paths.data.join("repos"), repository, None)
                    {
                        reference_dirs.push(path);
                    }
                }
                opencode_core::config::schema::ReferenceEntry::Git(git_entry) => {
                    if let Some(path) = materialize_git_reference(
                        &paths.data.join("repos"),
                        &git_entry.repository,
                        git_entry.branch.as_deref(),
                    ) {
                        reference_dirs.push(path);
                    }
                }
            }
        }
    }

    // M7.1 project resolution — the instance worktree is the project
    // sandbox: the git worktree root when a repo exists, else the
    // directory (`instance-store.ts:54-57`).
    let registry = opencode_core::project::registry::ProjectRegistry::new(
        storage.clone(),
        Arc::new(opencode_core::git::SubprocessGit),
        Arc::new(opencode_core::catalog::SystemClock),
        Arc::new(|_| {}),
    );
    let (project, worktree) = registry
        .from_directory(directory)
        .map_err(|err| ServerError::Core(opencode_core::CoreError::Storage(err.to_string())))?;

    let agent_input = opencode_core::AgentRegistryInput {
        config: config.clone(),
        skill_dirs,
        reference_dirs,
        worktree: worktree.clone(),
        data_dir: paths.data.clone(),
        tmp_dir: std::env::temp_dir().join("opencode"),
        home: paths.home.clone(),
    };
    // M7.2: the background-job service backs the run-state cancel seam.
    let background =
        opencode_core::BackgroundJobService::new(Arc::new(opencode_core::catalog::SystemClock));
    let services = Arc::new(opencode_core::SessionServices::new(
        storage,
        background.clone(),
        Arc::new(opencode_core::catalog::SystemClock),
        &agent_input,
    ));
    // Stamp the instance context + the ambient publish location
    // (`event-v2-bridge.ts:19-33`).
    let instance_location = opencode_core::InstanceLocation {
        directory: directory.to_path_buf(),
        worktree: worktree.clone(),
        project,
        workspace_id: None,
    };
    services.set_instance_location(instance_location);
    // M7.2: the production engine for this instance — prompt facade,
    // revert/summary and the tool registry (`/experimental/tool`).
    engines.boot(&engine::EngineInput {
        services: services.clone(),
        background,
        config: Arc::new(config),
        config_dirs: _opencode_dirs,
        directory: directory.to_path_buf(),
        worktree: worktree.clone(),
        paths,
        seams,
    })?;
    Ok(services)
}

/// The `serve` stdout lines (`cli/cmd/serve.ts:15-20`): the password
/// warning is printed *before* the listener binds, then the listening line.
pub fn write_warning(out: &mut impl std::io::Write) {
    let _ = writeln!(
        out,
        "Warning: OPENCODE_SERVER_PASSWORD is not set; server is unsecured."
    );
}

pub fn write_listening(out: &mut impl std::io::Write, hostname: &str, port: u16) {
    let _ = writeln!(out, "opencode server listening on http://{hostname}:{port}");
}

/// `URL.canParse(item) && /^(https?:)$/.test(new URL(item).protocol)`
/// (`config/plugin/skill.ts:31-34`) — the `http:`/`https:` scheme prefix,
/// case-insensitive like the WHATWG URL parser.
fn http_url(item: &str) -> Option<String> {
    let (scheme, rest) = item.split_once(':')?;
    if (scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
        && !rest.is_empty()
    {
        return Some(item.to_string());
    }
    None
}

/// `validAlias` (`config/plugin/reference.ts:42-44`) — non-empty, without
/// `/`, whitespace, backtick or comma.
fn valid_alias(name: &str) -> bool {
    !name.is_empty() && !name.chars().any(|c| matches!(c, '/' | ' ' | '`' | ','))
}

/// `~/` expands against the home directory (`localPath`,
/// `config/plugin/reference.ts:46-49`); other shapes keep the raw value,
/// matching the `Local` entry handling.
fn expand_reference_path(home: &std::path::Path, value: &str) -> std::path::PathBuf {
    if let Some(rest) = value.strip_prefix("~/") {
        return home.join(rest);
    }
    std::path::PathBuf::from(value)
}

/// One git reference materialization — the cache path is returned
/// immediately, the tracking `ensure` runs in the background with its
/// failures logged (`reference.ts:86-96`).
fn materialize_git_reference(
    repos_dir: &std::path::Path,
    repository: &str,
    branch: Option<&str>,
) -> Option<std::path::PathBuf> {
    let reference = opencode_core::repository::parse(repository)?;
    if !opencode_core::repository::is_remote(&reference) {
        return None;
    }
    let opencode_core::repository::Reference::Remote(reference) = reference else {
        return None;
    };
    if let Some(branch) = branch {
        if opencode_core::repository::validate_branch(branch).is_err() {
            return None;
        }
    }
    let path = opencode_core::repository::cache_path(
        repos_dir,
        &opencode_core::repository::Reference::Remote(reference.clone()),
        branch,
    );
    let repos_dir = repos_dir.to_path_buf();
    let repository = repository.to_string();
    let branch = branch.map(str::to_string);
    std::thread::spawn(move || {
        let cache = opencode_core::repository::RepositoryCache::new(
            Arc::new(opencode_core::SubprocessGit),
            repos_dir,
        );
        if let Err(cause) = cache.ensure(opencode_core::repository::EnsureInput {
            reference: &reference,
            refresh: true,
            branch: branch.as_deref(),
        }) {
            tracing::warn!(repository, cause = %cause, "failed to materialize reference");
        }
    });
    Some(path)
}

/// `opencode serve` — print the banner and run until stopped.
pub async fn serve(opts: &ListenOptions) -> io::Result<Listener> {
    let password_set = std::env::var("OPENCODE_SERVER_PASSWORD")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    if !password_set {
        write_warning(&mut std::io::stdout());
    }
    let listener = listen(opts).await?;
    write_listening(&mut std::io::stdout(), &listener.hostname, listener.port);
    Ok(listener)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banner_includes_warning_without_password() {
        let mut out = Vec::new();
        write_warning(&mut out);
        write_listening(&mut out, "127.0.0.1", 4096);
        let out = String::from_utf8(out).unwrap();
        assert_eq!(
            out,
            "Warning: OPENCODE_SERVER_PASSWORD is not set; server is unsecured.\n\
             opencode server listening on http://127.0.0.1:4096\n"
        );
    }

    #[test]
    fn banner_omits_warning_with_password() {
        let mut out = Vec::new();
        write_listening(&mut out, "0.0.0.0", 1234);
        let out = String::from_utf8(out).unwrap();
        assert_eq!(out, "opencode server listening on http://0.0.0.0:1234\n");
    }

    #[test]
    fn production_factory_builds_services_and_maps_config_errors() {
        let root = tempfile::tempdir().unwrap();
        let paths = opencode_core::GlobalPaths::resolve(root.path().join("home"));
        std::fs::create_dir_all(root.path().join("data")).unwrap();
        let storage = Arc::new(Storage::open(root.path().join("data/db.sqlite")).unwrap());
        let store = production_instance_factory(
            storage.clone(),
            paths,
            Arc::new(engine::EngineStore::default()),
            engine::EngineSeams::default(),
        );

        store.load(root.path()).expect("empty directory boots");

        let bad = root.path().join("bad");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("opencode.json"), "{ not json").unwrap();
        let err = store.load(&bad).err().expect("invalid config must fail");
        assert!(matches!(
            err,
            ServerError::Core(opencode_core::CoreError::Jsonc { .. })
        ));
    }
}

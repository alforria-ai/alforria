//! `alforria-server` — the HTTP surface of opencode: v1+v2 routers, the
//! tagged-error wire envelopes, the CORS/compression/fence middleware quirks
//! and the `serve` listener bootstrap.

pub mod bearer;
pub mod confine;
pub mod engine;
pub mod error;
pub mod middleware;
pub mod openapi;
pub mod outbox;
pub mod policy;
pub mod provider;
pub mod provider_runtime;
pub mod pty;
pub mod recovery;
pub mod routes;
pub mod shutdown;
pub mod sse;
pub mod state;
pub mod ticket;
pub mod workspace;

use std::io;
use std::sync::Arc;

use alforria_core::Storage;

pub use error::{ApiError, ServerError};
pub use policy::{ServerPolicy, ShutdownOptions};
pub use shutdown::ShutdownReport;
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
    /// Extra `Host` names the server answers to besides loopback, the bound
    /// hostname and the `--cors` hosts (the mDNS name).
    pub hosts: Vec<String>,
    /// Overrides `OPENCODE_SERVER_PASSWORD` (a password `serve` generated).
    pub password: Option<String>,
    /// Run the trigger scheduler (alforria extension). On for the long-lived
    /// servers (`serve`, `web`, the TUI's own); off for one-shot commands,
    /// which would start a scheduled turn only to exit under it.
    pub schedule: bool,
    /// Run the restart sweep over what an earlier process left mid-turn
    /// before serving. On by default; a caller that only wants a router
    /// over someone else's data directory turns it off.
    pub sweep: bool,
    /// The global directories to serve. `None` resolves them from the
    /// process environment — which only a process entry point may do; a
    /// test passes its temporary home.
    pub paths: Option<alforria_core::GlobalPaths>,
}

impl Default for ListenOptions {
    fn default() -> Self {
        ListenOptions {
            port: DEFAULT_PORT,
            hostname: DEFAULT_HOSTNAME.to_string(),
            cors: Vec::new(),
            hosts: Vec::new(),
            password: None,
            schedule: false,
            sweep: true,
            paths: None,
        }
    }
}

/// A running listener (`server/server.ts:20-25`).
pub struct Listener {
    pub hostname: String,
    pub port: u16,
    pub url: String,
    /// What the operator should know about this configuration
    /// ([`startup_warnings`]); `serve` prints them.
    pub warnings: Vec<String>,
    websockets: state::WebSocketTracker,
    shutdown: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<io::Result<()>>,
    ctx: Arc<ServerContext>,
}

impl std::fmt::Debug for Listener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Listener")
            .field("hostname", &self.hostname)
            .field("port", &self.port)
            .field("url", &self.url)
            .field("websockets", &self.websockets)
            .finish_non_exhaustive()
    }
}

impl Listener {
    /// Graceful stop: stop accepting, close HTTP sockets and websockets
    /// (`websocket-tracker.ts:17-45` semantics; the tracker close runs
    /// before the listener shuts down so live sockets observe 1001).
    pub async fn stop(self) -> io::Result<()> {
        self.websockets.close_all();
        (self.ctx.stop_background)();
        self.ctx.outbox.stop();
        self.shutdown
            .send(true)
            .map_err(|_| io::Error::other("server task already stopped"))?;
        let _ = self.task.await;
        Ok(())
    }

    /// The shutdown the host config asks for (`server.shutdown`).
    pub fn shutdown_options(&self) -> ShutdownOptions {
        self.ctx.policy.shutdown
    }

    /// Stop for good, within `options.timeout` (see [`shutdown`]): unlike
    /// [`Listener::stop`] this settles the running turns, kills what they
    /// started, does not wait for event streams that never end, and leaves
    /// the database checkpointed.
    pub async fn shutdown(mut self, options: ShutdownOptions) -> ShutdownReport {
        let started = std::time::Instant::now();
        let deadline = started + options.timeout;
        // Closing connections and checkpointing come out of the same budget.
        let reserve = std::time::Duration::from_millis(500).min(options.timeout / 4);
        let mut report = ShutdownReport::default();

        // Stop accepting. In-flight requests keep running for now.
        let _ = self.shutdown.send(true);
        // Nothing starts by itself from here on: the trigger schedulers
        // stop firing (a tick, a watch and a webhook call alike) and the
        // transcript indexers end. A trigger's turn that is already running
        // is a turn like any other and is settled below, aborted or
        // drained as `server.shutdown.sessions` says; its run is then
        // closed by the scheduler as completed or failed.
        (self.ctx.stop_background)();
        shutdown::settle_sessions(
            &self.ctx,
            options,
            deadline.checked_sub(reserve).unwrap_or(deadline),
            &mut report,
        )
        .await;
        shutdown::kill_processes(&self.ctx, &mut report);
        // What the turns just published (their `session.idle`) is written
        // down before the process goes; delivery resumes on the next start.
        self.ctx.outbox.flush();
        self.ctx.outbox.stop();

        // Whatever is still connected now is a stream: close the websockets
        // with 1001, give in-flight responses the rest of the budget, then
        // drop the event streams.
        self.websockets.close_all();
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if tokio::time::timeout(remaining.min(reserve), &mut self.task)
            .await
            .is_err()
        {
            self.task.abort();
            let _ = (&mut self.task).await;
        }
        shutdown::checkpoint(&self.ctx, &mut report);
        report
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
    if let Some(error) = &ctx.policy.config_error {
        return Err(io::Error::other(format!(
            "refusing to listen: the host config could not be applied ({error})"
        )));
    }
    if let Some(reason) = startup_refusal(&ctx) {
        return Err(io::Error::other(format!("refusing to listen: {reason}")));
    }
    let listener = bind_with_port_fallback(opts).await?;
    let port = listener.local_addr()?.port();
    let url = format!("http://{}:{}", opts.hostname, port);

    let router = routes::build_router(ctx.clone());
    let (shutdown, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let serve = axum::serve(listener, router).with_graceful_shutdown(async move {
        let _ = shutdown_rx.changed().await;
    });
    let task = tokio::spawn(async move { serve.await });
    // Webhook deliveries, including whatever an earlier process left.
    tokio::spawn(ctx.outbox.clone().run());

    let warnings = startup_warnings(&ctx);
    for warning in &warnings {
        tracing::warn!("{warning}");
    }
    let hostname = opts.hostname.clone();
    Ok(Listener {
        hostname,
        port,
        url,
        warnings,
        websockets: ctx.websockets.clone(),
        shutdown,
        task,
        ctx,
    })
}

/// A configuration the server will not serve: bearer authentication with
/// a directory a token may open that is, or contains, a place the host
/// config is read from or the data directory.
///
/// The host config — who may connect, what the agent's shells see, the
/// permission ceiling — is the global config directory and `~/.opencode`
/// (unless `server.trust.host` is `"managed"`), and the data directory holds
/// `auth.json` and the database. An agent working in a directory that
/// contains one of them can write it with its ordinary tools: the project at
/// `$HOME` is the usual way to get there.
pub fn startup_refusal(ctx: &ServerContext) -> Option<String> {
    ctx.policy.bearer.as_ref()?;
    // The same protected set the write guard is built from
    // (`alforria_core::protect`), compared both ways and with both sides
    // resolved alike: a directory that contains a protected location, is
    // one, or lies inside one.
    let params = alforria_core::LoadParams::new(ctx.paths.home.clone(), ctx.paths.clone());
    let protected = alforria_core::protect::Protected::host(
        &ctx.paths,
        &alforria_core::ConfigLoader::new().managed_dir(&params),
        !ctx.policy.managed_only,
    );
    for root in ctx.policy.directories.roots() {
        if let Some((what, location)) = protected.overlap(root) {
            return Some(format!(
                "{} is in server.directories (or is the working directory) and overlaps {what} \
                 ({}): one contains the other, so the agent could rewrite the host config, read \
                 the credentials or replace what the server runs. Serve a directory apart from \
                 it; for the user's config files, moving the host config to the managed \
                 directory with server.trust.host \"managed\" lifts this.",
                root.display(),
                location.display()
            ));
        }
    }
    None
}

/// Configurations that work but probably do not mean what the operator
/// thinks.
///
/// Bearer authentication with a working directory outside
/// `server.directories`: alforria's own routes (`/trigger`, `/memory`,
/// `/workflow`, `/file/*`) fall back to the working directory when a request
/// names none, and a credential below `admin` is refused there — every such
/// request must name an allowed directory. The server still starts: the
/// working directory is simply not available to those credentials.
pub fn startup_warnings(ctx: &ServerContext) -> Vec<String> {
    let mut warnings = Vec::new();
    let cwd = state::resolve_directory(&state::cwd());
    if ctx.policy.bearer.is_some() && !ctx.policy.directories.allows(&cwd) {
        warnings.push(format!(
            "bearer authentication is configured and the working directory ({}) is not in \
             server.directories: tokens below `admin` cannot use it, and requests to /trigger, \
             /memory, /workflow and /file/* that name no directory are refused for them. Start \
             the server in an allowed directory or have clients pass ?directory=.",
            cwd.display()
        ));
    }
    warnings
}

/// Bind a server with a fresh environment-derived context.
pub async fn listen(opts: &ListenOptions) -> io::Result<Listener> {
    let ctx = default_context(opts)?;
    if opts.schedule {
        boot_scheduled(&ctx);
    }
    listen_with(opts, ctx).await
}

/// Boot the instances that have something scheduled, so triggers fire
/// without a client having to open the directory first: the server's own
/// directory (its config may declare triggers) and every directory the
/// trigger tables know. Runs in the background; a directory that fails to
/// boot is logged and skipped.
pub fn boot_scheduled(ctx: &Arc<ServerContext>) {
    let ctx = ctx.clone();
    tokio::spawn(async move {
        let mut directories = vec![state::resolve_directory(&state::cwd())];
        let known = ctx.storage.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT directory FROM \"trigger\" UNION SELECT directory FROM trigger_state",
            )?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok::<_, rusqlite::Error>(rows)
        });
        for directory in known.unwrap_or_default() {
            let directory = std::path::PathBuf::from(directory);
            if directory.is_dir() && !directories.contains(&directory) {
                directories.push(directory);
            }
        }
        for directory in directories {
            let instances = ctx.clone();
            let booted = tokio::task::spawn_blocking(move || {
                instances.instances.load(&directory).map(|_| directory)
            })
            .await;
            match booted {
                Ok(Ok(directory)) => tracing::info!("triggers ready for {}", directory.display()),
                Ok(Err(error)) => {
                    tracing::warn!("could not boot an instance for its triggers: {error:?}")
                }
                Err(error) => {
                    tracing::warn!("could not boot an instance for its triggers: {error}")
                }
            }
        }
    });
}

/// The server as a tower `Service`, with no listener: the router over a
/// fresh environment-derived context. The TUI calls it in memory (upstream
/// runs the server in a worker and talks to it over RPC, `cli/cmd/tui.ts`),
/// so plain `alforria` opens no socket.
pub fn in_process_router(opts: &ListenOptions) -> io::Result<axum::Router> {
    Ok(routes::build_router(default_context(opts)?))
}

/// The UI backend: the binary-embedded web UI unless disabled. TS
/// `flags.disableEmbeddedWebUi` (`OPENCODE_DISABLE_EMBEDDED_WEB_UI`,
/// `runtime-flags.ts:20`); when disabled the UI catch-all falls through to the
/// JSON 404 envelope.
fn ui_backend() -> Arc<dyn state::UiBackend> {
    // `truthy()` (`flag.ts:4-6`): "true"/"1", lowercased.
    let disabled = std::env::var("OPENCODE_DISABLE_EMBEDDED_WEB_UI")
        .ok()
        .map(|value| value.to_ascii_lowercase())
        .is_some_and(|value| value == "true" || value == "1");
    if disabled {
        Arc::new(state::EmptyUiBackend)
    } else {
        Arc::new(state::EmbeddedUiBackend)
    }
}

fn default_context(opts: &ListenOptions) -> io::Result<Arc<ServerContext>> {
    // The one place a server resolves the process's own directories: the
    // entry point of `serve`, `web`, `acp` and the TUI. (A test run that
    // gets here without a sandbox panics in `from_env`.)
    let paths = opts
        .paths
        .clone()
        .unwrap_or_else(alforria_core::GlobalPaths::from_env);
    // The entry point is also what turns the network on: the LibertAI
    // model list and sign-in are offline in any other context.
    let seams = engine::EngineSeams {
        libertai_catalog: Some(alforria_core::libertai::LibertaiCatalog::from_env(
            &paths.cache,
        )),
        libertai_endpoints: Some(alforria_core::libertai::auth::Endpoints::from_env_in(
            &paths,
        )),
        ..engine::EngineSeams::default()
    };
    production_context(opts, paths, seams)
}

/// The production server context: shared storage + bus and the per-directory
/// instance factory that builds the M7.2 production engines. Tests inject
/// the engine [`engine::EngineSeams`] (mock LLM); production passes the
/// default (unwired until M7.7).
pub fn production_context(
    opts: &ListenOptions,
    paths: alforria_core::GlobalPaths,
    seams: engine::EngineSeams,
) -> io::Result<Arc<ServerContext>> {
    // Every harness builds its server here: a test run over directories
    // that are not a sandbox stops before the database is opened.
    alforria_core::paths::sandbox::guard(&paths, "production_context");
    let libertai_catalog = seams
        .libertai_catalog
        .clone()
        .unwrap_or_else(alforria_core::libertai::LibertaiCatalog::offline);
    let libertai_endpoints = seams.libertai_endpoints.clone().unwrap_or_else(|| {
        alforria_core::libertai::auth::Endpoints::offline(paths.data.join("libertai-auth.json"))
    });
    let storage = Storage::open_default(&paths.data)
        .map_err(|err| io::Error::other(format!("storage open failed: {err}")))?;
    let storage = Arc::new(storage);
    let global = global_config(&paths);
    apply_synchronous(&storage, &global);
    start_maintenance(&storage, &global);
    // The session durable manifest — required by the durable streams
    // (`/api/session/:id/event`, `/api/session/:id/history`).
    let manifest = Arc::new(alforria_core::session::event_definitions::SessionManifest::new());
    let bus = Arc::new(alforria_core::EventBus::new_shared(
        storage.clone(),
        Some(manifest),
    ));
    // M7.2: per-instance engines land in the store keyed by the services
    // the LocationContext carries; the engine seam resolves through it.
    let engines = Arc::new(engine::EngineStore::default());
    // M7.7: the provider-runtime inputs — the models-dev catalog and the
    // `auth.json` store shared by the routes and every instance engine.
    let runtime = engine::EngineRuntime {
        catalog: state::catalog_in(paths.cache.clone(), libertai_catalog),
        auth: Arc::new(state::FileAuthStore::new(paths.data.join("auth.json"))),
        schedule: opts.schedule,
    };
    let instances = production_instance_factory(
        storage.clone(),
        paths.clone(),
        engines.clone(),
        runtime.clone(),
        seams,
    );
    let mut policy = ServerPolicy::load(&paths);
    // `hook.webhook`: the outbox captures from every instance's bus. A
    // hook that does not resolve is a host config error like any other.
    let outbox = match outbox::Outbox::new(storage.clone(), policy.hook.as_ref()) {
        Ok(outbox) => outbox,
        Err(error) => {
            policy.config_error.get_or_insert(error);
            outbox::Outbox::new(storage.clone(), None).map_err(io::Error::other)?
        }
    };
    let policy = Arc::new(policy);
    // The unattended policy sits on each instance's ask services.
    instances.set_on_boot({
        let (policy, outbox) = (policy.clone(), outbox.clone());
        move |services| {
            services
                .permission
                .set_unattended(policy.unattended_permission);
            services.question.set_unattended(policy.unattended_question);
            outbox.attach(&services.events);
        }
    });
    let mut auth = AuthConfig::from_env().with_bearer(policy.bearer.clone());
    if let Some(password) = &opts.password {
        auth.password = Some(password.clone());
    }
    let mut ctx = ServerContext::new(
        auth,
        instances,
        storage,
        bus,
        opts.cors.clone(),
        ui_backend(),
    );
    ctx.policy = policy;
    ctx.outbox = outbox;
    alforria_core::shell_env::install(ctx.policy.shell_env.clone());
    ctx.hosts = middleware::host::HostAllowlist::new(&opts.hostname, &opts.hosts, &opts.cors);
    ctx.engine_factory = engines.factory();
    ctx.tools = engines.tools();
    ctx.mcp = engines.mcp_source();
    ctx.lsp = engines.lsp_source();
    ctx.vcs = Arc::new(state::CoreVcs::default());
    ctx.catalog = runtime.catalog;
    ctx.auth_store = runtime.auth;
    ctx.paths = paths.clone();
    ctx.detached_paths = None;
    ctx.provider_auth = Arc::new(state::ProviderAuthService::with_libertai(
        libertai_endpoints,
    ));
    ctx.stop_background = Arc::new({
        let engines = engines.clone();
        move || engines.stop_background()
    });
    // Everything this context writes goes under the paths it was given:
    // worktrees too, which defaulted to the process's own data directory.
    ctx.worktree_deps = Arc::new(state::WorktreeDeps {
        data_dir: paths.data.clone(),
        global_bus: ctx.global_bus.clone(),
        instances: ctx.instances.clone(),
        projects: ctx.projects.clone(),
    });
    let ctx = Arc::new(ctx);
    // Before the first request: no client sees a session a dead process
    // left mid-turn.
    if opts.sweep {
        recovery::sweep(
            &ctx,
            &paths.data,
            alforria_core::session::recovery::ResumePolicy::from_config(&global),
        );
    }
    Ok(ctx)
}

/// `storage.synchronous`.
fn apply_synchronous(storage: &Storage, config: &alforria_core::config::schema::Config) {
    use alforria_core::config::schema::SynchronousMode;
    let mode = config
        .storage
        .as_ref()
        .and_then(|storage| storage.synchronous);
    if let Some(mode) = mode {
        if let Err(error) = storage.set_synchronous(mode == SynchronousMode::Full) {
            tracing::warn!("could not set storage.synchronous: {error}");
        }
    }
}

/// The merged global config, for what the server decides once rather than
/// per directory. A config that does not load counts as empty here: the
/// directory that needs it reports the error.
pub(crate) fn global_config(
    paths: &alforria_core::GlobalPaths,
) -> alforria_core::config::schema::Config {
    // `server.resume` and `storage` are host keys (`EXTENSION_KEYS`): read
    // through the host loader, which consults no project file.
    alforria_core::config::host::load_host_config(paths)
        .unwrap_or_else(|_| serde_json::from_str("{}").expect("an empty config decodes"))
}

/// Event-log retention (`storage.retention`) and write-ahead-log
/// checkpoints: now, then periodically for as long as the server runs.
/// Without a runtime to run on (the in-process server is built before the
/// TUI starts one) the startup pass still happens.
fn start_maintenance(storage: &Arc<Storage>, config: &alforria_core::config::schema::Config) {
    use alforria_core::storage::maintenance;
    let retention = maintenance::event_retention(config);
    match tokio::runtime::Handle::try_current() {
        Ok(_) => {
            maintenance::spawn(storage, retention);
        }
        Err(_) => maintenance::run_once(storage, &retention),
    }
}

/// The per-directory instance factory (TS `InstanceStore.boot` +
/// `InstanceBootstrap.run`, `project/instance-store.ts:46-56`): load the
/// directory's merged config, wire the agent registry and build the M5
/// service graph over the shared storage.
fn production_instance_factory(
    storage: Arc<Storage>,
    paths: alforria_core::GlobalPaths,
    engines: Arc<engine::EngineStore>,
    runtime: engine::EngineRuntime,
    seams: engine::EngineSeams,
) -> InstanceStore {
    let factory: state::InstanceFactory = Arc::new(move |directory: &std::path::Path| {
        instance_for_directory(
            storage.clone(),
            paths.clone(),
            engines.clone(),
            runtime.clone(),
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
    paths: alforria_core::GlobalPaths,
    engines: Arc<engine::EngineStore>,
    runtime: engine::EngineRuntime,
    seams: engine::EngineSeams,
    directory: &std::path::Path,
) -> Result<Arc<alforria_core::SessionServices>, ServerError> {
    let params = alforria_core::LoadParams::new(directory.to_path_buf(), paths.clone());
    let (config, _opencode_dirs) = alforria_core::ConfigLoader::new().load(&params)?;

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
            let discovery = alforria_core::skill::SkillDiscovery::new(
                paths.cache.clone(),
                Arc::new(alforria_core::skill::ReqwestFetcher),
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
                alforria_core::config::schema::ReferenceEntry::Local(local) => {
                    reference_dirs.push(std::path::PathBuf::from(&local.path));
                }
                alforria_core::config::schema::ReferenceEntry::Repository(repository) => {
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
                alforria_core::config::schema::ReferenceEntry::Git(git_entry) => {
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
    let registry = alforria_core::project::registry::ProjectRegistry::new(
        storage.clone(),
        Arc::new(alforria_core::git::SubprocessGit),
        Arc::new(alforria_core::catalog::SystemClock),
        Arc::new(|_| {}),
    );
    let (project, worktree) = registry
        .from_directory(directory)
        .map_err(|err| ServerError::Core(alforria_core::CoreError::Storage(err.to_string())))?;

    let agent_input = alforria_core::AgentRegistryInput {
        config: config.clone(),
        skill_dirs,
        reference_dirs,
        worktree: worktree.clone(),
        data_dir: paths.data.clone(),
        tmp_dir: std::env::temp_dir().join("alforria"),
        home: paths.home.clone(),
        // The host's own config is the ceiling of every agent's rules: a
        // project's config narrows a permission, never widens one.
        host_config: Some(alforria_core::ConfigLoader::new().load_trusted(&params)?),
        // What a restricted or outside-input session's tools may not
        // write: everything the loaders consult for this instance.
        protected: Some(Arc::new(alforria_core::protect::Protected::for_instance(
            &paths,
            &alforria_core::ConfigLoader::new().managed_dir(&params),
            &alforria_core::ConfigLoader::new().search_dirs(&params),
            &params.flags,
            &config,
            directory,
            &worktree,
        ))),
    };
    // M7.2: the background-job service backs the run-state cancel seam.
    let background =
        alforria_core::BackgroundJobService::new(Arc::new(alforria_core::catalog::SystemClock));
    let services = Arc::new(alforria_core::SessionServices::new(
        storage,
        background.clone(),
        Arc::new(alforria_core::catalog::SystemClock),
        &agent_input,
    ));
    // Stamp the instance context + the ambient publish location
    // (`event-v2-bridge.ts:19-33`).
    let instance_location = alforria_core::InstanceLocation {
        directory: directory.to_path_buf(),
        worktree: worktree.clone(),
        project,
        workspace_id: None,
    };
    services.set_instance_location(instance_location);
    // Every turn leaves word that it is running, for the process that
    // comes after one that dies mid-turn.
    services.run_state.track_runs(
        alforria_core::session::recovery::RunMarkers::new(&paths.data),
        directory.to_path_buf(),
    );
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
        runtime: runtime.clone(),
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
    let _ = writeln!(out, "alforria server listening on http://{hostname}:{port}");
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
    let reference = alforria_core::repository::parse(repository)?;
    if !alforria_core::repository::is_remote(&reference) {
        return None;
    }
    let alforria_core::repository::Reference::Remote(reference) = reference else {
        return None;
    };
    if let Some(branch) = branch {
        if alforria_core::repository::validate_branch(branch).is_err() {
            return None;
        }
    }
    let path = alforria_core::repository::cache_path(
        repos_dir,
        &alforria_core::repository::Reference::Remote(reference.clone()),
        branch,
    );
    let repos_dir = repos_dir.to_path_buf();
    let repository = repository.to_string();
    let branch = branch.map(str::to_string);
    std::thread::spawn(move || {
        let cache = alforria_core::repository::RepositoryCache::new(
            Arc::new(alforria_core::SubprocessGit),
            repos_dir,
        );
        if let Err(cause) = cache.ensure(alforria_core::repository::EnsureInput {
            reference: &reference,
            refresh: true,
            branch: branch.as_deref(),
        }) {
            tracing::warn!(repository, cause = %cause, "failed to materialize reference");
        }
    });
    Some(path)
}

/// `alforria serve` — print the banner and run until stopped.
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
             alforria server listening on http://127.0.0.1:4096\n"
        );
    }

    #[test]
    fn banner_omits_warning_with_password() {
        let mut out = Vec::new();
        write_listening(&mut out, "0.0.0.0", 1234);
        let out = String::from_utf8(out).unwrap();
        assert_eq!(out, "alforria server listening on http://0.0.0.0:1234\n");
    }

    #[test]
    fn production_factory_builds_services_and_maps_config_errors() {
        let root = tempfile::tempdir().unwrap();
        let paths = alforria_core::GlobalPaths::resolve_with(root.path().join("home"), |_| None);
        std::fs::create_dir_all(root.path().join("data")).unwrap();
        let storage = Arc::new(Storage::open(root.path().join("data/db.sqlite")).unwrap());
        let store = production_instance_factory(
            storage.clone(),
            paths,
            Arc::new(engine::EngineStore::default()),
            engine::EngineRuntime {
                catalog: crate::state::catalog_in(
                    root.path().join("cache"),
                    alforria_core::libertai::LibertaiCatalog::offline(),
                ),
                auth: Arc::new(crate::state::MemoryAuthStore::default()),
                schedule: false,
            },
            engine::EngineSeams::default(),
        );

        store.load(root.path()).expect("empty directory boots");

        let bad = root.path().join("bad");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("opencode.json"), "{ not json").unwrap();
        let err = store.load(&bad).err().expect("invalid config must fail");
        assert!(matches!(
            err,
            ServerError::Core(alforria_core::CoreError::Jsonc { .. })
        ));
    }
}

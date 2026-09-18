//! `opencode-server` — the HTTP surface of opencode: v1+v2 routers, the
//! tagged-error wire envelopes, the CORS/compression/fence middleware quirks
//! and the `serve` listener bootstrap.

pub mod error;
pub mod middleware;
pub mod routes;
pub mod sse;
pub mod state;

use std::io;
use std::sync::Arc;

use opencode_core::Storage;

pub use error::{ApiError, ServerError};
pub use state::{AuthConfig, HeartbeatConfig, InstanceStore, ServerContext, UiBackend};

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
    shutdown: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<io::Result<()>>,
}

impl Listener {
    /// Graceful stop: stop accepting, close HTTP sockets and websockets
    /// (`websocket-tracker.ts:17-45` semantics; no PTY websockets until
    /// M6.8).
    pub async fn stop(self) -> io::Result<()> {
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

    let router = routes::build_router(ctx);
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
    let instances = production_instance_factory(storage.clone(), paths.clone());
    Ok(Arc::new(ServerContext::new(
        AuthConfig::from_env(),
        instances,
        storage,
        bus,
        opts.cors.clone(),
        Arc::new(state::EmptyUiBackend),
    )))
}

/// The per-directory instance factory (TS `InstanceStore.boot` +
/// `InstanceBootstrap.run`, `project/instance-store.ts:46-56`): load the
/// directory's merged config, wire the agent registry and build the M5
/// service graph over the shared storage.
fn production_instance_factory(
    storage: Arc<Storage>,
    paths: opencode_core::GlobalPaths,
) -> InstanceStore {
    let factory: state::InstanceFactory = Arc::new(move |directory: &std::path::Path| {
        instance_for_directory(storage.clone(), paths.clone(), directory)
    });
    InstanceStore::new(factory)
}

/// One instance boot: config load + service wiring.
fn instance_for_directory(
    storage: Arc<Storage>,
    paths: opencode_core::GlobalPaths,
    directory: &std::path::Path,
) -> Result<Arc<opencode_core::SessionServices>, ServerError> {
    let params = opencode_core::LoadParams::new(directory.to_path_buf()).paths(paths.clone());
    let (config, _opencode_dirs) = opencode_core::ConfigLoader::new().load(&params)?;

    // `skill.dirs()` — directory sources from the config (`skills.paths`);
    // URL sources are plugin machinery (M7).
    let skill_dirs = config
        .skills
        .as_ref()
        .and_then(|skills| skills.paths.clone())
        .unwrap_or_default()
        .into_iter()
        .map(std::path::PathBuf::from)
        .collect();

    // `reference.list()` — local sources; git sources need the repository
    // cache (M7).
    let mut reference_dirs = Vec::new();
    if let Some(references) = config.references.as_ref().or(config.reference.as_ref()) {
        for entry in references.values() {
            if let opencode_core::config::schema::ReferenceEntry::Local(local) = entry {
                reference_dirs.push(std::path::PathBuf::from(&local.path));
            }
        }
    }

    let agent_input = opencode_core::AgentRegistryInput {
        config,
        skill_dirs,
        reference_dirs,
        // TODO(M7): project sandbox detection — the worktree is the
        // directory itself until git worktree support lands.
        worktree: directory.to_path_buf(),
        data_dir: paths.data.clone(),
        tmp_dir: std::env::temp_dir().join("opencode"),
        home: paths.home.clone(),
    };
    Ok(Arc::new(opencode_core::SessionServices::new(
        storage,
        Arc::new(state::NoBackgroundJobs),
        Arc::new(opencode_core::catalog::SystemClock),
        &agent_input,
    )))
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
        let store = production_instance_factory(storage.clone(), paths);

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

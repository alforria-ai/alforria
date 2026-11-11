//! Instance boot — the CLI slice of `InstanceStore.boot` +
//! `InstanceBootstrap.run` (`project/instance-store.ts:45-61`): load the
//! merged config, resolve the project, wire the agent registry and the M5
//! `SessionServices` graph.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use alforria_core::session::InstanceLocation;
use alforria_core::skill::SkillDiscovery;
use alforria_core::{
    BackgroundJobService, ConfigLoader, LoadParams, ProjectRegistry, SessionServices, Storage,
};

use crate::error::{core_error, TypedError};

/// A booted instance: config + the M5 service graph.
pub struct Instance {
    pub paths: alforria_core::GlobalPaths,
    pub config: alforria_core::config::schema::Config,
    /// `config.directories()` — the dirs skill/agent discovery scanned.
    pub config_dirs: Vec<PathBuf>,
    pub directory: PathBuf,
    pub worktree: PathBuf,
    pub location: InstanceLocation,
    pub services: Arc<SessionServices>,
}

/// `URL.canParse(item) && /^(https?:)$/.test(new URL(item).protocol)`
/// (`config/plugin/skill.ts:31-34`) — the `http:`/`https:` scheme prefix.
fn http_url(item: &str) -> Option<String> {
    let (scheme, rest) = item.split_once(':')?;
    if (scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
        && !rest.is_empty()
    {
        return Some(item.to_string());
    }
    None
}

/// `skill.dirs()` — directory sources from `skills.paths`; http(s) entries
/// are URL sources materialized through the discovery cache
/// (`config/plugin/skill.ts:31-45`, `skill/discovery.ts`).
fn skill_dirs(config: &alforria_core::config::schema::Config, cache_dir: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let Some(paths) = config
        .skills
        .as_ref()
        .and_then(|skills| skills.paths.clone())
    else {
        return dirs;
    };
    let mut urls = Vec::new();
    for item in paths {
        match http_url(&item) {
            Some(url) => urls.push(url),
            None => dirs.push(PathBuf::from(item)),
        }
    }
    if !urls.is_empty() {
        let discovery = SkillDiscovery::new(
            cache_dir.to_path_buf(),
            Arc::new(alforria_core::skill::ReqwestFetcher),
        );
        for url in urls {
            dirs.extend(discovery.pull(&url));
        }
    }
    dirs
}

/// Boot the instance for `directory` (or the current directory).
pub fn boot(directory: Option<&Path>) -> Result<Instance, TypedError> {
    let directory = match directory {
        Some(directory) => directory.to_path_buf(),
        None => std::env::current_dir().map_err(|err| TypedError::Unknown {
            raw: err.to_string(),
        })?,
    };
    // The one-shot commands' entry point (`run`, `debug`, `mcp`, …): the
    // process's own directories, handed to the instance built below.
    boot_in(directory, alforria_core::GlobalPaths::from_env())
}

/// [`boot`] with the global directories given.
fn boot_in(directory: PathBuf, paths: alforria_core::GlobalPaths) -> Result<Instance, TypedError> {
    let params = LoadParams::new(directory.clone(), paths.clone());
    let (config, config_dirs) = ConfigLoader::new().load(&params).map_err(core_error)?;

    let storage = Arc::new(Storage::open_default(&paths.data).map_err(core_error)?);
    // Project resolution — the instance worktree is the git worktree root
    // when a repo exists, else the directory (instance-store.ts:54-57).
    let registry = ProjectRegistry::new(
        storage.clone(),
        Arc::new(alforria_core::git::SubprocessGit),
        Arc::new(alforria_core::catalog::SystemClock),
        Arc::new(|_| {}),
    );
    let (project, worktree) = registry
        .from_directory(&directory)
        .map_err(|err| TypedError::Cli(crate::error::CliError::new(err.to_string())))?;

    let agent_input = alforria_core::AgentRegistryInput {
        config: config.clone(),
        skill_dirs: skill_dirs(&config, &paths.cache),
        reference_dirs: Vec::new(),
        worktree: worktree.clone(),
        data_dir: paths.data.clone(),
        tmp_dir: std::env::temp_dir().join("alforria"),
        home: paths.home.clone(),
        // The host's own config caps every agent's rules, as in a server.
        host_config: Some(
            ConfigLoader::new()
                .load_trusted(&LoadParams::new(directory.clone(), paths.clone()))
                .map_err(core_error)?,
        ),
        // What a restricted or outside-input session's tools may not
        // write, built by the constructor the server's instances use: a
        // guarded session continued with `alforria run --session` is held
        // to the same set, resolved the same way, as on the server.
        protected: Some(Arc::new(alforria_core::protect::Protected::for_loaded(
            &params, &config, &worktree,
        ))),
    };
    let background = BackgroundJobService::new(Arc::new(alforria_core::catalog::SystemClock));
    let services = Arc::new(SessionServices::new(
        storage,
        background,
        Arc::new(alforria_core::catalog::SystemClock),
        &agent_input,
    ));
    let location = InstanceLocation {
        directory: directory.clone(),
        worktree: worktree.clone(),
        project,
        workspace_id: None,
    };
    services.set_instance_location(location.clone());
    Ok(Instance {
        paths,
        config,
        config_dirs,
        directory,
        worktree,
        location,
        services,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A guarded session run through the one-shot instance (`alforria run
    /// --session` on a restricted or outside-started session) is held to
    /// the resolver-based set the server's instances get: a host location
    /// that carries no loader name — the global config directory — reached
    /// through a symlink in the project is refused. The one-shot instance
    /// used to have no set of its own, which left the name check alone.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_one_shot_instance_guards_loader_locations_through_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let (home, project) = (root.join("home"), root.join("project"));
        std::fs::create_dir_all(&project).unwrap();
        let paths = alforria_core::GlobalPaths::resolve_with(home.clone(), |_| None);
        std::fs::create_dir_all(paths.config.join("agent")).unwrap();
        std::os::unix::fs::symlink(&paths.config, project.join("notes")).unwrap();
        std::os::unix::fs::symlink(&paths.data, project.join("cache-link")).unwrap();

        let instance = boot_in(project.clone(), paths.clone()).unwrap();
        // The guard as the session loop builds it for a guarded session.
        let guarded = alforria_core::tool::def::InstanceContext {
            directory: instance.directory.clone(),
            worktree: instance.worktree.clone(),
            write_guard: Some(Arc::new(alforria_core::protect::WriteGuard {
                protected: instance.services.agents.protected(),
                policy: Default::default(),
                worktree: instance.worktree.clone(),
            })),
        };
        for target in ["notes/agent/helper.md", "cache-link/auth.json"] {
            let refused = guarded
                .write_file(&project.join(target), b"mine")
                .await
                .expect_err(target);
            assert!(
                matches!(refused, alforria_core::tool::def::ModifyError::Refused(_)),
                "{target}: {refused:?}"
            );
        }
        assert!(!paths.config.join("agent/helper.md").exists());
        assert!(!paths.data.join("auth.json").exists());
        // An ordinary file of the project is written.
        guarded
            .write_file(&project.join("src/main.rs"), b"fn main() {}")
            .await
            .unwrap();
        assert!(project.join("src/main.rs").is_file());
    }
}

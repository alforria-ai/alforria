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
    let paths = alforria_core::GlobalPaths::from_env();
    let (config, config_dirs) = ConfigLoader::new()
        .load(&LoadParams::new(directory.clone()).paths(paths.clone()))
        .map_err(core_error)?;

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

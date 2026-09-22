//! v2 reference routes — port of
//! `packages/protocol/src/groups/reference.ts` +
//! `packages/server/src/handlers/reference.ts`.

use std::sync::Arc;

use axum::response::Response;
use axum::routing::get;

use crate::error::ServerError;
use crate::middleware::location::LocationContext;
use crate::state::ServerContext;

use super::util::envelope;
use alforria_schema::reference::{ReferenceInfo, ReferenceSource};

type Router = axum::Router<Arc<ServerContext>>;

pub fn register(router: Router, method: &str, path: &'static str) -> (Router, bool) {
    let router = match (method, path) {
        ("GET", "/api/reference") => router.route(path, get(list)),
        _ => return (router, false),
    };
    (router, true)
}

/// `reference.list` (`handlers/reference.ts` +
/// `core/src/reference.ts:70-113`) — materialize the configured references.
///
/// Local sources pass through; git sources resolve through the repository
/// cache (`Repository.cachePath` under `data/repos`), and the background
/// `cache.ensure` clone is forked like the TS `forkIn(scope)` — failures
/// are logged and swallowed.
async fn list(
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    let mut infos: Vec<ReferenceInfo> = Vec::new();
    let params = alforria_core::LoadParams::new(location.directory.clone())
        .paths(alforria_core::GlobalPaths::from_env());
    let (config, _) = match alforria_core::ConfigLoader::new().load(&params) {
        Ok(loaded) => loaded,
        // Config load failures degrade to an empty list.
        Err(_) => return envelope(&location, infos),
    };
    for (name, entry) in config.references.unwrap_or_default() {
        let info = match entry {
            alforria_core::config::schema::ReferenceEntry::Repository(_repository) => {
                // Bare repository strings carry no source type; the
                // `git`/`local` shapes below are the materializable ones.
                continue;
            }
            alforria_core::config::schema::ReferenceEntry::Git(git) => {
                let Some(repository) = alforria_core::repository::parse(&git.repository) else {
                    continue;
                };
                if !alforria_core::repository::is_remote(&repository) {
                    continue;
                }
                if let Some(branch) = git.branch.as_deref() {
                    if alforria_core::repository::validate_branch(branch).is_err() {
                        continue;
                    }
                }
                let paths = alforria_core::GlobalPaths::from_env();
                let path = alforria_core::repository::cache_path(
                    &paths.data.join("repos"),
                    &repository,
                    git.branch.as_deref(),
                );
                let branch = git.branch.clone();
                let reference = git.repository.clone();
                // Fork the clone like the TS `forkIn(scope)`.
                if let alforria_core::repository::Reference::Remote(remote) = repository.clone() {
                    tokio::spawn(async move {
                        let cache = alforria_core::repository::RepositoryCache::new(
                            Arc::new(alforria_core::git::SubprocessGit),
                            paths.data.join("repos"),
                        );
                        let _ = cache.ensure(alforria_core::repository::EnsureInput {
                            reference: &remote,
                            refresh: true,
                            branch: branch.as_deref(),
                        });
                    });
                }
                ReferenceInfo {
                    name: name.clone(),
                    path: path.to_string_lossy().into_owned(),
                    description: git.description.clone(),
                    hidden: git.hidden,
                    source: ReferenceSource::Git {
                        repository: reference,
                        branch: git.branch,
                        description: git.description,
                        hidden: git.hidden,
                    },
                }
            }
            alforria_core::config::schema::ReferenceEntry::Local(local) => ReferenceInfo {
                name: name.clone(),
                path: local.path.clone(),
                description: local.description.clone(),
                hidden: local.hidden,
                source: ReferenceSource::Local {
                    path: local.path,
                    description: local.description,
                    hidden: local.hidden,
                },
            },
        };
        infos.push(info);
    }
    envelope(&location, infos)
}

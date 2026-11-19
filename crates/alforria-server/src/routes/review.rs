//! Review completion (alforria extension, outside the frozen v1/v2 route tables).

use std::sync::Arc;

use alforria_schema::vcs::CommitInfo;
use axum::routing::get;
use axum::{Extension, Json, Router};

use crate::error::ServerError;
use crate::middleware::location::LocationContext;
use crate::state::ServerContext;

pub fn register(router: Router<Arc<ServerContext>>) -> Router<Arc<ServerContext>> {
    router.route("/experimental/vcs/commits", get(commits))
}

async fn commits(
    Extension(location): Extension<LocationContext>,
) -> Result<Json<Vec<CommitInfo>>, ServerError> {
    let commits = tokio::task::spawn_blocking(move || {
        alforria_core::vcs::GitCli.commits(&location.directory, 50)
    })
    .await
    .map_err(|error| alforria_core::CoreError::Storage(error.to_string()))?;
    Ok(Json(commits))
}

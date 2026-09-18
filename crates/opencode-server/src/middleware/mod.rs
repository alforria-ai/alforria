//! Middleware stack — TS order (`httpapi/server.ts:276-297`):
//! `errorLayer → compressionLayer → corsVaryFix → fenceLayer → cors → route
//! handlers`. Auth and location layers land with M6.2/M6.3.

pub mod compression;
pub mod cors;
pub mod fence;

use std::sync::Arc;

use axum::Router;

use crate::state::ServerContext;

/// Wrap the router in the global middleware stack.
///
/// `Router::layer` wraps outside-in: added first = closest to the handlers,
/// so the request path is panic catcher → compression → cors-vary → fence →
/// cors, matching the TS stack exactly.
pub fn apply_stack(router: Router, ctx: &Arc<ServerContext>) -> Router {
    router
        // cors — innermost, short-circuits every OPTIONS request.
        .layer(cors::CorsLayer::new(ctx.cors.clone()))
        // fenceLayer
        .layer(fence::FenceLayer::from_env(ctx.storage.clone()))
        // corsVaryFix
        .layer(cors::CorsVaryLayer)
        // compressionLayer
        .layer(compression::CompressionLayer)
        // errorLayer
        .layer(crate::error::PanicLayer)
}

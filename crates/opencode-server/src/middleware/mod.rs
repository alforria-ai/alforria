//! Middleware stack — TS order (`httpapi/server.ts:276-297`):
//! `errorLayer → compressionLayer → corsVaryFix → fenceLayer → cors → route
//! handlers`. Auth, workspace-routing and location resolution are baked into
//! the route families in TS, so those layers run innermost (auth first, then
//! location — TS group middleware lists end with `Authorization`).

pub mod auth;
pub mod compression;
pub mod cors;
pub mod fence;
pub mod location;

use std::sync::Arc;

use axum::Router;

use crate::state::ServerContext;

/// Wrap the router in the global middleware stack.
///
/// `Router::layer` wraps outside-in: added first = closest to the handlers,
/// so the request path is panic catcher → compression → cors-vary → fence →
/// cors → auth → location → route handlers, matching the TS stack exactly.
pub fn apply_stack(router: Router, ctx: &Arc<ServerContext>) -> Router {
    router
        // workspace-routing + instance-context (v1) / location (v2) run
        // inside authorization.
        .layer(location::LocationLayer::new(ctx.clone()))
        // authorizationLayer — TS bakes it into route handlers.
        .layer(auth::AuthLayer::new(ctx.auth.clone()))
        // cors — short-circuits every OPTIONS request.
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

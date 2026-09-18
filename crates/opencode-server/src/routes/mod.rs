//! Router assembly: one axum router carrying both API generations, `GET /doc`,
//! `GET /openapi.json` and the UI catch-all, wrapped in the TS middleware
//! stack (`httpapi/server.ts:276-297`).

pub mod doc;
pub mod ui;
pub mod v1;
pub mod v2;

use std::sync::Arc;

use axum::routing::{delete, get, patch, post, put};
use axum::Router;

use crate::middleware;
use crate::state::ServerContext;

/// Assemble the full router for a server context.
pub fn build_router(ctx: Arc<ServerContext>) -> Router {
    let mut router = Router::<Arc<ServerContext>>::new();

    for (method, path) in v1::ROUTES.iter() {
        router = register_stub(router, method, path);
    }
    for (method, path) in v2::ROUTES.iter() {
        router = register_stub(router, method, path);
    }
    router = router
        .route("/doc", get(doc::doc))
        .route("/openapi.json", get(doc::openapi_json));

    let router = router
        .fallback(ui::serve_ui_handler)
        // The 405 → UI rewrite runs inside the global stack, like TS's
        // `router.add("*", "/*", serveUI)` handler.
        .layer(ui::UiFallbackLayer::new(ctx.clone()));
    let router = router.with_state(ctx.clone());
    middleware::apply_stack(router, &ctx)
}

/// Every registered route responds with the defect-500 envelope until its
/// chunk lands (M6.5-M6.9).
async fn stub() -> axum::response::Response {
    crate::error::defect_response()
}

fn register_stub(
    router: Router<Arc<ServerContext>>,
    method: &str,
    path: &'static str,
) -> Router<Arc<ServerContext>> {
    // M6.4 owns the four SSE streams.
    match (method, path) {
        ("GET", "/event") => {
            return router.route(path, get(crate::sse::v1_event));
        }
        ("GET", "/global/event") => {
            return router.route(path, get(crate::sse::global_event));
        }
        ("GET", "/api/event") => {
            return router.route(path, get(crate::sse::api_event));
        }
        ("GET", "/api/session/{sessionID}/event") => {
            return router.route(path, get(crate::sse::api_session_event));
        }
        _ => {}
    }
    // M6.5 owns the v1 session family.
    let (router, handled) = v1::session::register(router, method, path);
    if handled {
        return router;
    }
    // M6.6 owns the config/permission/question, provider and
    // global/control/instance/file/experimental/tui families.
    let (router, handled) = v1::config_permission_question::register(router, method, path);
    if handled {
        return router;
    }
    let (router, handled) = v1::provider::register(router, method, path);
    if handled {
        return router;
    }
    let (router, handled) = v1::global_control::register(router, method, path);
    if handled {
        return router;
    }
    // M6.7 owns the v2 /api families.
    let (router, handled) = v2::session::register(router, method, path);
    if handled {
        return router;
    }
    // M6.8 owns the PTY family (v1 + v2 + connect websockets).
    let (router, handled) = crate::pty::routes::register(router, method, path);
    if handled {
        return router;
    }
    let (router, handled) = v2::permission::register(router, method, path);
    if handled {
        return router;
    }
    let (router, handled) = v2::misc::register(router, method, path);
    if handled {
        return router;
    }
    match method {
        "GET" => router.route(path, get(stub)),
        "POST" => router.route(path, post(stub)),
        "PUT" => router.route(path, put(stub)),
        "DELETE" => router.route(path, delete(stub)),
        "PATCH" => router.route(path, patch(stub)),
        other => unreachable!("unsupported method {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_table_is_exhaustive() {
        assert_eq!(v1::ROUTES.len(), 127, "v1 route count (spec §2.1)");
        assert!(v1::ROUTES
            .iter()
            .all(|(method, _)| matches!(*method, "GET" | "POST" | "PUT" | "DELETE" | "PATCH")));
    }

    #[test]
    fn v2_table_is_exhaustive() {
        assert_eq!(v2::ROUTES.len(), 61, "v2 route count (spec §2.1)");
    }
}

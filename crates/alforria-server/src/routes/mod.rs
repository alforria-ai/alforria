//! Router assembly: one axum router carrying both API generations, `GET /doc`,
//! `GET /openapi.json` and the UI catch-all, wrapped in the TS middleware
//! stack (`httpapi/server.ts:276-297`).

pub mod attention;
pub mod doc;
pub mod file_ext;
pub mod goal;
pub mod memory;
pub mod posture;
pub mod restriction;
pub mod review;
pub mod trigger;
pub mod ui;
pub mod v1;
pub mod v2;
pub mod workflow;

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
    // alforria extension — dynamic workflows. Kept outside the frozen
    // v1/v2 tables so the TS OpenAPI surface stays byte-identical.
    router = workflow::register(router);
    router = goal::register(router);
    router = restriction::register(router);
    router = file_ext::register(router);
    router = attention::register(router);
    router = trigger::register(router);
    router = memory::register(router);
    router = posture::register(router);
    router = review::register(router);
    // alforria extension: one guard over every route registered above
    // that names a session, on the route as matched and the id as decoded
    // (`confine::guard_session_writes`). A route added above is under it.
    let router = router.route_layer(axum::middleware::from_fn_with_state(
        ctx.clone(),
        crate::confine::guard_session_writes,
    ));

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
    // M7.1 owns the v1 project family.
    let (router, handled) = v1::project::register(router, method, path);
    if handled {
        return router;
    }
    // M7.6 owns the v1 /mcp family.
    let (router, handled) = v1::mcp::register(router, method, path);
    if handled {
        return router;
    }
    // M7.8 owns the v1 sync, workspace and project-copy families.
    let (router, handled) = v1::sync::register(router, method, path);
    if handled {
        return router;
    }
    let (router, handled) = v1::workspace::register(router, method, path);
    if handled {
        return router;
    }
    let (router, handled) = v1::project_copy::register(router, method, path);
    if handled {
        return router;
    }
    // M6.7 owns the v2 /api families.
    let (router, handled) = v2::session::register(router, method, path);
    if handled {
        return router;
    }
    // M7.8 owns the v2 integration/credential/reference/project-copy
    // families.
    let (router, handled) = v2::integration::register(router, method, path);
    if handled {
        return router;
    }
    let (router, handled) = v2::credential::register(router, method, path);
    if handled {
        return router;
    }
    let (router, handled) = v2::reference::register(router, method, path);
    if handled {
        return router;
    }
    let (router, handled) = v2::project_copy::register(router, method, path);
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

/// Run a handler's storage work off the runtime's worker threads.
///
/// SQLite calls block, and so does waiting for a `std::sync::Mutex` a
/// service holds across them: made on a worker thread they stall every
/// other request that thread is driving. The memory and trigger routes do
/// their work through here (the same `spawn_blocking` the transcript
/// search and the provider logout use). The runtime's context is still
/// entered, so the work may spawn tasks.
pub(crate) async fn blocking<T, F>(work: F) -> Result<T, crate::error::ServerError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, crate::error::ServerError> + Send + 'static,
{
    tokio::task::spawn_blocking(work).await.map_err(|join| {
        crate::error::ServerError::Core(alforria_core::CoreError::Storage(format!(
            "a storage task did not finish: {join}"
        )))
    })?
}

#[cfg(test)]
mod blocking_tests {
    /// The work runs on a blocking thread, not on the runtime's worker —
    /// here the only one, which stays free to drive other tasks meanwhile
    /// — and inside the runtime, so it can spawn.
    #[tokio::test]
    async fn storage_work_runs_off_the_runtime_worker() {
        let worker = std::thread::current().id();
        let (started, wait) = std::sync::mpsc::channel::<()>();
        let (release, held) = std::sync::mpsc::channel::<()>();
        let work = tokio::spawn(super::blocking(move || {
            started.send(()).unwrap();
            // Held until the test, on the worker, lets go: a handler doing
            // this on the worker would never get there.
            held.recv().unwrap();
            let spawned = tokio::runtime::Handle::current().spawn(async { 7 });
            Ok((std::thread::current().id(), spawned))
        }));
        tokio::task::yield_now().await;
        tokio::task::spawn_blocking(move || wait.recv().unwrap())
            .await
            .unwrap();
        release.send(()).unwrap();
        let (thread, spawned) = work.await.unwrap().unwrap();
        assert_ne!(thread, worker);
        assert_eq!(spawned.await.unwrap(), 7);
    }
}

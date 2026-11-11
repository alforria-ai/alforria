//! The startup sweep: find the turns the last process died in the middle
//! of, repair their sessions, and do what `server.resume` asks
//! (`alforria_core::session::recovery`).
//!
//! The repair runs before the server takes its first request, so no client
//! ever sees a session stuck mid-turn. Notes and resumed turns are prompts
//! like any other and run once the server is up.

use std::path::Path;
use std::sync::Arc;

use alforria_core::session::recovery::{self, ResumePolicy, RunMarker, RunMarkers};

use crate::middleware::location::LocationContext;
use crate::state::ServerContext;

/// Repair every session a dead process left mid-turn. Returns how many
/// were found.
pub fn sweep(ctx: &Arc<ServerContext>, data_dir: &Path, policy: ResumePolicy) -> usize {
    let markers = RunMarkers::new(data_dir);
    let orphans = markers.orphans();
    for marker in &orphans {
        if let Err(error) = recover(ctx, &markers, marker, policy) {
            // The marker stays: the next start tries again.
            tracing::warn!(
                session = %marker.session_id,
                "could not repair a session interrupted by a restart: {error}"
            );
        }
    }
    orphans.len()
}

fn recover(
    ctx: &Arc<ServerContext>,
    markers: &RunMarkers,
    marker: &RunMarker,
    policy: ResumePolicy,
) -> Result<(), String> {
    let services = ctx
        .instances
        .load(&marker.directory)
        .map_err(|error| format!("{error:?}"))?;
    let Some(interrupted) =
        recovery::repair(&services, &marker.session_id).map_err(|error| error.to_string())?
    else {
        // The session was deleted since.
        markers.remove(&marker.session_id);
        return Ok(());
    };
    let action = interrupted.action(policy);
    let follow_up = recovery::follow_up(&services, &interrupted, action);
    let action = if follow_up.is_some() {
        action
    } else {
        ResumePolicy::None
    };
    if interrupted.found_anything() {
        tracing::info!(
            session = %interrupted.session_id,
            tools = interrupted.tools,
            messages = interrupted.messages,
            action = action.as_str(),
            "repaired a session interrupted by a restart"
        );
        recovery::announce(&services, &interrupted, policy, action)
            .map_err(|error| error.to_string())?;
    }
    let Some(input) = follow_up else {
        markers.remove(&marker.session_id);
        return Ok(());
    };
    let engine = (ctx.engine_factory)(&LocationContext {
        directory: marker.directory.clone(),
        workspace_id: None,
        services,
    })
    .map_err(|error| format!("{error:?}"))?;
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        // Nothing to run a prompt on: the session is repaired, and the
        // marker stays for a start that can do the rest.
        return Ok(());
    };
    let markers = markers.clone();
    let session_id = marker.session_id.clone();
    runtime.spawn(async move {
        // A note is written and done. A resumed turn replaces the dead
        // process's marker with its own as it starts, and clears that when
        // it ends.
        let resumed = action == ResumePolicy::Continue;
        match engine.prompt(input).await {
            Ok(_) if !resumed => markers.remove(&session_id),
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(
                    session = %session_id,
                    "the follow-up to an interrupted turn failed: {error:?}"
                );
                // A turn that could not even start is not tried again on
                // every boot.
                if markers
                    .orphans()
                    .iter()
                    .any(|marker| marker.session_id == session_id)
                {
                    markers.remove(&session_id);
                }
            }
        }
    });
    Ok(())
}

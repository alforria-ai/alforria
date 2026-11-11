//! The startup sweep: find the turns the last process died in the middle
//! of, repair their sessions, and do what `server.resume` asks
//! (`alforria_core::session::recovery`).
//!
//! The repair runs before the server takes its first request, so no client
//! ever sees a session stuck mid-turn. Notes and resumed turns are prompts
//! like any other and run once the server is up.

use std::path::Path;
use std::sync::Arc;

use alforria_core::session::recovery::{self, Claimed, ResumePolicy, RunMarkers};

use crate::middleware::location::LocationContext;
use crate::state::ServerContext;

/// Repair every session a dead process left mid-turn. Returns how many
/// this process took.
///
/// Several processes can start over one data directory at the same moment
/// and all see the same orphaned markers. Each marker is claimed first
/// (`RunMarkers::claim`, an atomic rename), so one process repairs and
/// resumes a given turn and the others never see it.
pub fn sweep(ctx: &Arc<ServerContext>, data_dir: &Path, policy: ResumePolicy) -> usize {
    let markers = RunMarkers::new(data_dir);
    let claimed = markers.claim();
    for claim in &claimed {
        if let Err(error) = recover(ctx, &markers, claim, policy) {
            // The marker goes back: the next start tries again.
            markers.give_back(claim);
            tracing::warn!(
                session = %claim.marker.session_id,
                "could not repair a session interrupted by a restart: {error}"
            );
        }
    }
    claimed.len()
}

fn recover(
    ctx: &Arc<ServerContext>,
    markers: &RunMarkers,
    claim: &Claimed,
    policy: ResumePolicy,
) -> Result<(), String> {
    let marker = &claim.marker;
    let services = ctx
        .instances
        .load(&marker.directory)
        .map_err(|error| format!("{error:?}"))?;
    let Some(interrupted) =
        recovery::repair(&services, &marker.session_id).map_err(|error| error.to_string())?
    else {
        // The session was deleted since.
        markers.release(claim);
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
        markers.release(claim);
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
        // marker goes back for a start that can do the rest.
        markers.give_back(claim);
        return Ok(());
    };
    let markers = markers.clone();
    let claim = claim.clone();
    runtime.spawn(async move {
        // The claim is held for as long as the follow-up runs: a resumed
        // turn writes a marker of its own as it starts and clears it when
        // it ends, and a process that starts meanwhile finds nothing to
        // take. A follow-up that fails is not tried again on every boot.
        if let Err(error) = engine.prompt(input).await {
            tracing::warn!(
                session = %claim.marker.session_id,
                "the follow-up to an interrupted turn failed: {error:?}"
            );
        }
        markers.release(&claim);
    });
    Ok(())
}

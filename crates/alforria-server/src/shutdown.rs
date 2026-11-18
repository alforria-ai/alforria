//! Graceful shutdown (alforria extension): what a supervised `alforria
//! serve` does between SIGTERM and exit.
//!
//! In order: stop accepting connections; settle the running turns (cancel
//! them, or let them finish until the deadline and cancel the rest); kill
//! the terminals and whatever process groups the tools still have running;
//! close the remaining connections (event streams never end on their own);
//! move the write-ahead log into the database file.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alforria_core::config::host::ShutdownSessions;

use crate::policy::ShutdownOptions;
use crate::state::ServerContext;

/// What a shutdown did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ShutdownReport {
    /// Turns that finished on their own while draining.
    pub drained: usize,
    /// Turns that were cancelled.
    pub cancelled: usize,
    /// Turns whose cancel had not settled when the deadline passed.
    pub abandoned: usize,
    /// Terminals killed.
    pub terminals: usize,
    /// Tool process groups force-killed after the sessions settled.
    pub processes: usize,
    /// Whether the whole write-ahead log reached the database file.
    pub checkpointed: bool,
}

/// How often a drain looks at the running turns.
const DRAIN_POLL: Duration = Duration::from_millis(50);
/// The part of the budget kept back from draining so the cancels that
/// follow can still settle.
const CANCEL_RESERVE: Duration = Duration::from_secs(2);

type Busy = Vec<(Arc<alforria_core::SessionServices>, String)>;

fn busy(ctx: &ServerContext) -> Busy {
    let mut busy = Vec::new();
    for services in ctx.instances.cached() {
        for session_id in services.run_state.busy_sessions() {
            busy.push((services.clone(), session_id));
        }
    }
    busy
}

/// Settle every running turn within `deadline`.
pub(crate) async fn settle_sessions(
    ctx: &ServerContext,
    options: ShutdownOptions,
    deadline: Instant,
    report: &mut ShutdownReport,
) {
    if options.sessions == ShutdownSessions::Drain {
        let before = busy(ctx).len();
        let drain_until = deadline
            .checked_sub(CANCEL_RESERVE.min(options.timeout / 2))
            .unwrap_or(deadline);
        while !busy(ctx).is_empty() && Instant::now() < drain_until {
            tokio::time::sleep(DRAIN_POLL).await;
        }
        report.drained = before.saturating_sub(busy(ctx).len());
    }
    // A turn can start while the others are being settled: a request that
    // was already in flight, or a trigger run that had passed its last
    // check when the schedulers stopped and has not reached its session
    // yet. So this is not one pass over what is running at one moment:
    // every pass takes the turns it has not told to stop, until nothing is
    // running and nothing is on its way.
    let mut told: HashSet<String> = HashSet::new();
    loop {
        let fresh: Busy = busy(ctx)
            .into_iter()
            .filter(|(_, session_id)| told.insert(session_id.clone()))
            .collect();
        let settled = fresh.is_empty() && in_flight(ctx) == 0 && (ctx.background_in_flight)() == 0;
        if settled || Instant::now() >= deadline {
            break;
        }
        let cancels = fresh.into_iter().map(|(services, session_id)| async move {
            if let Err(err) = services.run_state.cancel(&session_id).await {
                tracing::warn!(session_id, "cancel on shutdown failed: {err}");
            }
        });
        let remaining = deadline.saturating_duration_since(Instant::now());
        let _ = tokio::time::timeout(remaining, futures::future::join_all(cancels)).await;
        // A cancel returns as soon as the turn is told to stop. The turn
        // itself still has to kill its tool processes and record the
        // interruption.
        tokio::time::sleep(DRAIN_POLL).await;
    }
    let total = told.len();
    report.abandoned = (in_flight(ctx) as usize).min(total);
    report.cancelled = total - report.abandoned;
}

fn in_flight(ctx: &ServerContext) -> u64 {
    ctx.instances
        .cached()
        .iter()
        .map(|services| services.run_state.in_flight())
        .sum()
}

/// Kill what the agent still has running: terminals first, then every tool
/// process group that outlived its turn.
pub(crate) fn kill_processes(ctx: &ServerContext, report: &mut ShutdownReport) {
    report.terminals = ctx.ptys.kill_all();
    report.processes = alforria_core::process_group::kill_all();
}

/// Empty the write-ahead log into the database file.
pub(crate) fn checkpoint(ctx: &ServerContext, report: &mut ShutdownReport) {
    match ctx.storage.checkpoint_truncate() {
        Ok(complete) => report.checkpointed = complete,
        Err(err) => tracing::warn!("WAL checkpoint on shutdown failed: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use alforria_core::session::error::SessionError;
    use alforria_core::session::message::WithParts;
    use alforria_core::session::run_state::Work;
    use tokio_util::sync::CancellationToken;

    /// A turn that runs until it is cancelled.
    fn start(services: &Arc<alforria_core::SessionServices>, session: &'static str) {
        let cancel = CancellationToken::new();
        let turn: Work<WithParts, SessionError> = Arc::new({
            let cancel = cancel.clone();
            move || {
                let cancel = cancel.clone();
                Box::pin(async move {
                    cancel.cancelled().await;
                    Err(SessionError::not_found("interrupted"))
                })
            }
        });
        let run_state = services.run_state.clone();
        tokio::spawn(async move {
            let _ = run_state
                .ensure_running(session, cancel, turn.clone(), turn)
                .await;
        });
    }

    /// M10. A turn that starts while the shutdown is settling the others —
    /// a trigger run that had passed its last check when the schedulers
    /// stopped — was never told to stop: the pass over the running turns
    /// was made once. The shutdown now waits for what is on its way and
    /// settles it too.
    #[tokio::test]
    async fn a_turn_that_starts_during_shutdown_is_settled_too() {
        let root = tempfile::tempdir().unwrap();
        let paths = alforria_core::GlobalPaths::resolve_with(root.path().join("home"), |_| None);
        let ctx = crate::production_context(
            &crate::ListenOptions {
                paths: Some(paths.clone()),
                sweep: false,
                ..Default::default()
            },
            paths,
            crate::engine::EngineSeams::default(),
        )
        .unwrap();
        let mut ctx = Arc::try_unwrap(ctx).ok().expect("the only reference");
        // One turn on its way, as a stopped scheduler would report it.
        let coming = Arc::new(AtomicUsize::new(1));
        ctx.background_in_flight = Arc::new({
            let coming = coming.clone();
            move || coming.load(Ordering::SeqCst)
        });
        let project = root.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let services = ctx.instances.load(&project).unwrap();
        start(&services, "ses_first");
        while services.run_state.busy_sessions().is_empty() {
            tokio::task::yield_now().await;
        }
        // It reaches its session once the first turn has been told to stop.
        tokio::spawn({
            let (services, coming) = (services.clone(), coming.clone());
            async move {
                while services.run_state.cancel_count("ses_first") == 0 {
                    tokio::task::yield_now().await;
                }
                start(&services, "ses_late");
                while !services
                    .run_state
                    .busy_sessions()
                    .contains(&"ses_late".to_string())
                {
                    tokio::task::yield_now().await;
                }
                coming.store(0, Ordering::SeqCst);
            }
        });

        let options = ShutdownOptions {
            timeout: Duration::from_secs(20),
            sessions: ShutdownSessions::Abort,
        };
        let mut report = ShutdownReport::default();
        settle_sessions(&ctx, options, Instant::now() + options.timeout, &mut report).await;
        assert_eq!(report.cancelled, 2, "{report:?}");
        assert_eq!(report.abandoned, 0);
        assert!(busy(&ctx).is_empty(), "nothing is left running");
        assert_eq!(services.run_state.cancel_count("ses_late"), 1);
    }
}

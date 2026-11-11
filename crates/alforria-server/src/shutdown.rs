//! Graceful shutdown (alforria extension): what a supervised `alforria
//! serve` does between SIGTERM and exit.
//!
//! In order: stop accepting connections; settle the running turns (cancel
//! them, or let them finish until the deadline and cancel the rest); kill
//! the terminals and whatever process groups the tools still have running;
//! close the remaining connections (event streams never end on their own);
//! move the write-ahead log into the database file.

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
    let running = busy(ctx);
    if running.is_empty() {
        return;
    }
    let total = running.len();
    let cancels = running
        .into_iter()
        .map(|(services, session_id)| async move {
            if let Err(err) = services.run_state.cancel(&session_id).await {
                tracing::warn!(session_id, "cancel on shutdown failed: {err}");
            }
        });
    let remaining = deadline.saturating_duration_since(Instant::now());
    let _ = tokio::time::timeout(remaining, futures::future::join_all(cancels)).await;
    // A cancel returns as soon as the turn is told to stop. The turn itself
    // still has to kill its tool processes and record the interruption.
    while in_flight(ctx) > 0 && Instant::now() < deadline {
        tokio::time::sleep(DRAIN_POLL).await;
    }
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

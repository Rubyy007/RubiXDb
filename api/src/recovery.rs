//! Startup recovery of interrupted `CREATE INDEX` / `DROP INDEX` operations, and
//! the observable state of that recovery.
//!
//! Recovery re-runs a whole backfill, which measured 11-35 s on 400,000-600,000
//! rows, so it runs on its own thread **after** the server is serving (the
//! online build protocol is concurrency-safe: a `Building` index receives live
//! writes and is invisible to the planner). Until now nothing could say whether
//! that was still going on: `/readyz` answered `ready: true` either way. The
//! state kept here is reported by `GET /readyz` and by the admin shutdown
//! response, so an operator and a script can tell "serving, index recovery
//! still running" from "serving, nothing pending" without guessing.
//!
//! `ready` itself keeps its meaning (the product can safely accept normal
//! supported work, which is true while recovery runs -- verified by
//! `cli/tests/index_backfill_crash_integration.rs`); recovery is reported next to
//! it, never folded into it.
//!
//! Shutdown cancels a running recovery (`IndexRecovery::request_cancel`, ADR-
//! LIFECYCLE-001): the index builder stops at its next chunk boundary and leaves
//! the unfinished index `Building`/`Dropping`, exactly the state a kill leaves,
//! so the next start restarts it. The stop therefore never waits for a whole
//! backfill.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use crate::AppState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryState {
    /// No recovery was started for this state object (embedded test fixtures).
    NotStarted,
    /// The recovery thread is running (or about to look for work).
    Running,
    /// Every interrupted operation found at startup finished (or there was none).
    Complete,
    /// Recovery hit an error, or its thread could not be started. The server is
    /// still serving; the affected index stays `Building`/`Failed` in the catalog.
    Failed,
    /// Shutdown was requested while recovery was running (ADR-LIFECYCLE-001): it
    /// stopped at the next chunk boundary and left the unfinished index
    /// `Building`/`Dropping`; the next start restarts it from scratch.
    Cancelled,
}

impl RecoveryState {
    pub fn as_str(self) -> &'static str {
        match self {
            RecoveryState::NotStarted => "not_started",
            RecoveryState::Running => "running",
            RecoveryState::Complete => "complete",
            RecoveryState::Failed => "failed",
            RecoveryState::Cancelled => "cancelled",
        }
    }
}

#[derive(Default)]
pub struct IndexRecovery {
    state: AtomicU8,
    cancel: AtomicBool,
}

impl IndexRecovery {
    pub fn state(&self) -> RecoveryState {
        match self.state.load(Ordering::SeqCst) {
            1 => RecoveryState::Running,
            2 => RecoveryState::Complete,
            3 => RecoveryState::Failed,
            4 => RecoveryState::Cancelled,
            _ => RecoveryState::NotStarted,
        }
    }

    /// Asks a running recovery to stop at its next chunk boundary. Idempotent;
    /// harmless when nothing is running. Called as soon as a graceful shutdown
    /// is requested, so the stop never waits for a whole backfill.
    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    pub fn cancel_requested(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    fn set(&self, s: RecoveryState) {
        let v = match s {
            RecoveryState::NotStarted => 0,
            RecoveryState::Running => 1,
            RecoveryState::Complete => 2,
            RecoveryState::Failed => 3,
            RecoveryState::Cancelled => 4,
        };
        self.state.store(v, Ordering::SeqCst);
    }
}

/// What one recovery pass did; the caller decides how to log it (the embedded
/// host prints to stderr, the standalone binary uses `tracing`).
#[derive(Debug, Default)]
pub struct RecoveryReport {
    pub recovered_builds: Vec<u32>,
    pub recovered_drops: Vec<u32>,
    pub errors: Vec<String>,
    /// Stopped early because shutdown was requested; unfinished indexes were
    /// left `Building`/`Dropping` for the next start.
    pub cancelled: bool,
}

/// Runs the two recovery passes on the calling thread, stopping at the next
/// chunk boundary if cancellation is requested (ADR-LIFECYCLE-001).
pub fn run_index_recovery(state: &AppState) -> RecoveryReport {
    let mut report = RecoveryReport::default();
    let cancel = &state.index_recovery.cancel;
    match state
        .sql
        .index_builder
        .recover_incomplete_builds_cancellable(cancel)
    {
        Ok(s) => {
            report.recovered_builds = s.recovered;
            report.cancelled |= s.cancelled;
        }
        Err(e) => report
            .errors
            .push(format!("index build recovery failed at startup: {e}")),
    }
    // A cancelled build pass means shutdown is under way: do not start the drop
    // pass (it would only be cancelled at its first chunk anyway).
    if !report.cancelled {
        match state
            .sql
            .index_builder
            .recover_incomplete_drops_cancellable(cancel)
        {
            Ok(s) => {
                report.recovered_drops = s.recovered;
                report.cancelled |= s.cancelled;
            }
            Err(e) => report
                .errors
                .push(format!("index drop recovery failed at startup: {e}")),
        }
    }
    report
}

/// Marks recovery `Running` **before** returning (so a client that sees the
/// server ready never observes a stale `not_started`), then runs it on a named
/// thread. The returned handle must be joined before the engine is shut down;
/// `None` means the thread could not be spawned and the state is `Failed`.
pub fn spawn_index_recovery(
    state: &Arc<AppState>,
    on_report: impl FnOnce(&RecoveryReport) + Send + 'static,
) -> Option<JoinHandle<()>> {
    state.index_recovery.set(RecoveryState::Running);
    let st = Arc::clone(state);
    match std::thread::Builder::new()
        .name("rubixdb-index-recovery".to_string())
        .spawn(move || {
            let report = run_index_recovery(&st);
            let outcome = if !report.errors.is_empty() {
                RecoveryState::Failed
            } else if report.cancelled {
                RecoveryState::Cancelled
            } else {
                RecoveryState::Complete
            };
            on_report(&report);
            st.index_recovery.set(outcome);
        }) {
        Ok(h) => Some(h),
        Err(_) => {
            state.index_recovery.set(RecoveryState::Failed);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trips_and_defaults_to_not_started() {
        let r = IndexRecovery::default();
        assert_eq!(r.state(), RecoveryState::NotStarted);
        for s in [
            RecoveryState::Running,
            RecoveryState::Complete,
            RecoveryState::Failed,
            RecoveryState::NotStarted,
        ] {
            r.set(s);
            assert_eq!(r.state(), s);
        }
        r.set(RecoveryState::Cancelled);
        assert_eq!(r.state(), RecoveryState::Cancelled);
        assert_eq!(RecoveryState::Cancelled.as_str(), "cancelled");
        assert!(!r.cancel_requested());
        r.request_cancel();
        assert!(r.cancel_requested());
        assert_eq!(RecoveryState::Running.as_str(), "running");
        assert_eq!(RecoveryState::Complete.as_str(), "complete");
    }
}

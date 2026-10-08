//! The background SSTable data-block verification and how it reports (ADR-SST-01, F-08).
//!
//! The scan itself is `rubixdb::ops::sstable_integrity::run_verification` (engine public functions only, read-only,
//! throttled, cancellable). This module is the host side: it runs it on its own thread **after** the server is serving
//! (the same pattern, and the same reason, as the post-start index recovery in `crate::recovery`: a synchronous full
//! read of the dataset at start would hit the embedded host's 30 s readiness bound at about 1.5 s per GiB), and routes
//! each finding to stderr (the embedded host's callback) and the security log (`sstable.damaged`).
//!
//! It reports only; `ready` is a constant `true` (decision D5) and nothing here changes it, blocks a write, controls
//! compaction or repairs anything. Findings carry the table id, its relative path and two counts - never keys, row data
//! or credentials.

use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;

use rubixdb::ops::sstable_integrity::{
    run_verification, DamagedTable, PreflightTable, SstableIntegrity,
};

use crate::AppState;

/// Security-log event code of one damaged table.
pub const EVENT_SSTABLE_DAMAGED: &str = "sstable.damaged";

/// Starts the pass for `tables` (the Manifest-live tables the startup preflight validated) at `mib_per_sec`
/// (`0` leaves it `disabled` and starts nothing). `report` is called once per damaged table, on the pass thread.
/// The state is `running` before this returns, so `GET /readyz` never shows a stale `disabled`. Graceful shutdown asks
/// the pass to stop (`SstableIntegrity::request_cancel`) and joins the returned handle before the engine stops.
pub fn spawn_sstable_verification(
    state: &Arc<AppState>,
    data_dir: PathBuf,
    tables: Vec<PreflightTable>,
    mib_per_sec: u64,
    report: fn(&DamagedTable),
) -> Option<JoinHandle<()>> {
    if mib_per_sec == 0 {
        return None;
    }
    state.sstable_integrity.begin(tables.len() as u64);
    let st = Arc::clone(state);
    let spawned = std::thread::Builder::new()
        .name("rubixdb-sstable-verify".to_string())
        .spawn(move || {
            let integrity: &SstableIntegrity = &st.sstable_integrity;
            run_verification(integrity, &data_dir, &tables, mib_per_sec, &mut |d| {
                emit_damaged(d);
                report(d);
            });
        });
    match spawned {
        Ok(h) => Some(h),
        Err(_) => {
            // No thread, no pass: say so (`disabled`) rather than leave `running` forever.
            state.sstable_integrity.disable();
            None
        }
    }
}

fn emit_damaged(d: &DamagedTable) {
    crate::security_log::emit(&crate::security_log::SecurityEvent {
        code: EVENT_SSTABLE_DAMAGED,
        outcome: "failed",
        object_kind: Some("sstable"),
        object: Some(&d.security_object()),
        ..Default::default()
    });
}

//! `GET /v1/compaction/status`, `GET /v1/compaction/metrics` —
//! `PHASE_API_ARCHITECTURE.md` §2. Read-only: no manual-trigger
//! endpoint exists because no such capability exists on the certified
//! engine (`ADR-COMPACTION-001` Decision 13, unchanged).

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::Json;
use rubixdb::compaction::CompactionStats;
use rubixdb::lsm::CompactionFailure;
use serde::Serialize;

use crate::state::AppState;

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// ADR-COMPACTION-LEAK-01: the most recent failed compaction attempt. Never carries record bytes: `message`
/// is the engine error text (for example `corruption: block: checksum mismatch`).
#[derive(Serialize, Clone)]
pub struct CompactionFailureBody {
    at_unix_ms: u64,
    kind: &'static str,
    message: String,
}

impl From<&CompactionFailure> for CompactionFailureBody {
    fn from(f: &CompactionFailure) -> Self {
        CompactionFailureBody {
            at_unix_ms: f.at_unix_ms,
            kind: f.kind.as_str(),
            message: f.message.clone(),
        }
    }
}

/// The same value as JSON, for the endpoints assembled with `json!` (`/v1/admin/status`).
pub fn last_failure_value(f: &Option<CompactionFailure>) -> serde_json::Value {
    match f {
        Some(f) => {
            serde_json::to_value(CompactionFailureBody::from(f)).unwrap_or(serde_json::Value::Null)
        }
        None => serde_json::Value::Null,
    }
}

#[derive(Serialize)]
pub struct CompactionStatusBody {
    auto_trigger_enabled: bool,
    trigger_count: usize,
    live_sstable_count: usize,
    cycles_completed: u64,
    /// `idle` | `running` | `failing` | `blocked` (ADR-COMPACTION-LEAK-01). `blocked` = the worker gave up on
    /// a permanently failing source and stays idle until the process restarts.
    state: &'static str,
    failures_total: u64,
    consecutive_failures: u64,
    blocked: bool,
    last_failure: Option<CompactionFailureBody>,
}

pub async fn status(State(state): State<Arc<AppState>>) -> Json<CompactionStatusBody> {
    let m = state.engine.compaction_metrics();
    Json(CompactionStatusBody {
        auto_trigger_enabled: state.lsm_config.compaction_auto_trigger,
        trigger_count: state.lsm_config.compaction_trigger_count,
        live_sstable_count: state.engine.sstable_count(),
        cycles_completed: m.cycles_completed,
        state: state.engine.compaction_state().as_str(),
        failures_total: m.failures_total,
        consecutive_failures: m.consecutive_failures,
        blocked: m.blocked,
        last_failure: m.last_failure.as_ref().map(CompactionFailureBody::from),
    })
}

#[derive(Serialize)]
pub struct CompactionCycleBody {
    input_sstable_count: usize,
    output_sstable_count: usize,
    input_bytes: u64,
    output_bytes: u64,
    records_read: u64,
    records_retained: u64,
    records_dropped: u64,
    tombstones_dropped: u64,
    versions_dropped: u64,
    duration_ms: f64,
    peak_temp_disk_bytes: u64,
}

impl From<&CompactionStats> for CompactionCycleBody {
    fn from(s: &CompactionStats) -> Self {
        CompactionCycleBody {
            input_sstable_count: s.input_sstable_count,
            output_sstable_count: s.output_sstable_count,
            input_bytes: s.input_bytes,
            output_bytes: s.output_bytes,
            records_read: s.records_read,
            records_retained: s.records_retained,
            records_dropped: s.records_dropped,
            tombstones_dropped: s.tombstones_dropped,
            versions_dropped: s.versions_dropped,
            duration_ms: ms(s.duration),
            peak_temp_disk_bytes: s.peak_temp_disk_bytes,
        }
    }
}

#[derive(Serialize)]
pub struct CompactionMetricsBody {
    cycles_completed: u64,
    input_sstables_total: u64,
    input_bytes_total: u64,
    output_bytes_total: u64,
    records_read_total: u64,
    records_retained_total: u64,
    records_dropped_total: u64,
    tombstones_dropped_total: u64,
    versions_dropped_total: u64,
    duration_total_ms: f64,
    duration_max_ms: f64,
    peak_temp_disk_bytes_max: u64,
    last_cycle: Option<CompactionCycleBody>,
    /// ADR-COMPACTION-LEAK-01 (additive): `cycles_completed` counts successful cycles only; failures are here.
    state: &'static str,
    failures_total: u64,
    consecutive_failures: u64,
    blocked: bool,
    last_failure: Option<CompactionFailureBody>,
}

pub async fn metrics(State(state): State<Arc<AppState>>) -> Json<CompactionMetricsBody> {
    let m = state.engine.compaction_metrics();
    Json(CompactionMetricsBody {
        cycles_completed: m.cycles_completed,
        input_sstables_total: m.input_sstables_total,
        input_bytes_total: m.input_bytes_total,
        output_bytes_total: m.output_bytes_total,
        records_read_total: m.records_read_total,
        records_retained_total: m.records_retained_total,
        records_dropped_total: m.records_dropped_total,
        tombstones_dropped_total: m.tombstones_dropped_total,
        versions_dropped_total: m.versions_dropped_total,
        duration_total_ms: ms(m.duration_total),
        duration_max_ms: ms(m.duration_max),
        peak_temp_disk_bytes_max: m.peak_temp_disk_bytes_max,
        last_cycle: m.last_cycle.as_ref().map(CompactionCycleBody::from),
        state: state.engine.compaction_state().as_str(),
        failures_total: m.failures_total,
        consecutive_failures: m.consecutive_failures,
        blocked: m.blocked,
        last_failure: m.last_failure.as_ref().map(CompactionFailureBody::from),
    })
}

//! `GET /v1/compaction/status`, `GET /v1/compaction/metrics` —
//! `PHASE_API_ARCHITECTURE.md` §2. Read-only: no manual-trigger
//! endpoint exists because no such capability exists on the certified
//! engine (`ADR-COMPACTION-001` Decision 13, unchanged).

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::Json;
use rubixdb::compaction::CompactionStats;
use serde::Serialize;

use crate::state::AppState;

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

#[derive(Serialize)]
pub struct CompactionStatusBody {
    auto_trigger_enabled: bool,
    trigger_count: usize,
    live_sstable_count: usize,
    cycles_completed: u64,
}

pub async fn status(State(state): State<Arc<AppState>>) -> Json<CompactionStatusBody> {
    Json(CompactionStatusBody {
        auto_trigger_enabled: state.lsm_config.compaction_auto_trigger,
        trigger_count: state.lsm_config.compaction_trigger_count,
        live_sstable_count: state.engine.sstable_count(),
        cycles_completed: state.engine.compaction_metrics().cycles_completed,
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
    })
}

//! `GET /v1/status` and `GET /v1/metadata` — `PHASE_API_ARCHITECTURE.md`
//! §2.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde::Serialize;

use crate::state::{uptime, AppState};

#[derive(Serialize)]
pub struct StatusBody {
    storage_state: String,
    storage_pressure_events: u64,
    sstable_count: usize,
    checkpoint_seq: u64,
    manifest_record_count: u64,
    manifest_size_bytes: Option<u64>,
    live_sstable_count: usize,
    capacity_pressure_events: u64,
    uptime_secs: f64,
}

pub async fn status(State(state): State<Arc<AppState>>) -> Json<StatusBody> {
    let engine = &state.engine;
    Json(StatusBody {
        storage_state: format!("{:?}", engine.storage_state()),
        storage_pressure_events: engine.storage_pressure_events(),
        sstable_count: engine.sstable_count(),
        checkpoint_seq: engine.checkpoint_seq(),
        manifest_record_count: engine.manifest_record_count(),
        manifest_size_bytes: engine.manifest_size_bytes().ok(),
        live_sstable_count: engine.live_sstable_ids().len(),
        capacity_pressure_events: engine.capacity_pressure_events(),
        uptime_secs: uptime(&state).as_secs_f64(),
    })
}

#[derive(Serialize)]
pub struct MetadataBody {
    /// Stated explicitly, not left implicit: this engine is a single
    /// flat binary-key/binary-value keyspace. There is no multi-table/
    /// schema concept to report -- `PHASE_API_ARCHITECTURE.md` §0/§2.
    keyspace_model: &'static str,
    data_dir: String,
    memtable_max_size_bytes: usize,
    max_immutable_memtables: usize,
    sstable_target_block_size: usize,
    bloom_bits_per_key: u32,
    compaction_trigger_count: usize,
    compaction_auto_trigger: bool,
}

pub async fn metadata(State(state): State<Arc<AppState>>) -> Json<MetadataBody> {
    let cfg = &state.lsm_config;
    Json(MetadataBody {
        keyspace_model:
            "single flat binary-key/binary-value keyspace (no tables, no schema, no SQL)",
        data_dir: state.config.data_dir.display().to_string(),
        memtable_max_size_bytes: cfg.memtable_max_size_bytes,
        max_immutable_memtables: cfg.max_immutable_memtables,
        sstable_target_block_size: cfg.sstable_target_block_size,
        bloom_bits_per_key: cfg.bloom_bits_per_key,
        compaction_trigger_count: cfg.compaction_trigger_count,
        compaction_auto_trigger: cfg.compaction_auto_trigger,
    })
}

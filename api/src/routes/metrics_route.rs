//! `GET /v1/metrics` — `PHASE_API_ARCHITECTURE.md` §5. Combines the
//! certified engine's own read/write observability (read verbatim,
//! never recomputed) with this service's own request-level metrics.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde::Serialize;

use crate::metrics::RouteMetricsSnapshot;
use crate::state::AppState;

#[derive(Serialize)]
pub struct ReadMetricsBody {
    read_requests: u64,
    read_hits: u64,
    read_misses: u64,
    bloom_negatives: u64,
    blocks_read: u64,
    sstables_consulted: u64,
}

#[derive(Serialize)]
pub struct WriteMetricsBody {
    state: String,
    submitted: u64,
    completed_ok: u64,
    completed_err: u64,
    rejected_backpressure: u64,
    queue_depth: usize,
    queue_capacity: usize,
}

#[derive(Serialize)]
pub struct ServiceMetricsBody {
    active_requests: i64,
    uptime_secs: f64,
    routes: Vec<RouteMetricsSnapshot>,
}

#[derive(Serialize)]
pub struct MetricsBody {
    storage_state: String,
    sstable_count: usize,
    compaction_cycles_completed: u64,
    read: ReadMetricsBody,
    write: WriteMetricsBody,
    service: ServiceMetricsBody,
}

pub async fn metrics(State(state): State<Arc<AppState>>) -> Json<MetricsBody> {
    let rs = state.engine.read_stats();
    let ps = state.engine.pool_stats();
    Json(MetricsBody {
        storage_state: format!("{:?}", state.engine.storage_state()),
        sstable_count: state.engine.sstable_count(),
        compaction_cycles_completed: state.engine.compaction_metrics().cycles_completed,
        read: ReadMetricsBody {
            read_requests: rs.read_requests,
            read_hits: rs.read_hits,
            read_misses: rs.read_misses,
            bloom_negatives: rs.bloom_negatives,
            blocks_read: rs.blocks_read,
            sstables_consulted: rs.sstables_consulted,
        },
        write: WriteMetricsBody {
            state: format!("{:?}", ps.state),
            submitted: ps.submitted,
            completed_ok: ps.completed_ok,
            completed_err: ps.completed_err,
            rejected_backpressure: ps.rejected_backpressure,
            queue_depth: ps.queue_depth,
            queue_capacity: ps.queue_capacity,
        },
        service: ServiceMetricsBody {
            active_requests: state.metrics.active_requests(),
            uptime_secs: crate::state::uptime(&state).as_secs_f64(),
            routes: state.metrics.snapshot(),
        },
    })
}

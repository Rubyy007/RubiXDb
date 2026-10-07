//! `GET /v1/metrics/system` and `GET /v1/metrics/system/timeseries`.
//!
//! Both read the sampler's latest immutable snapshot (or its bounded rings); neither touches the
//! engine, runs SQL, or takes a lock the write path holds. A value that cannot be measured is
//! JSON `null`, never `0`. Every string value comes from a closed set (state names, class
//! labels); nothing in a response is derived from request input.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};

use crate::error::ApiError;
use crate::observability::events::now_unix_ms;
use crate::observability::ring::{SERIES_NAMES, WINDOWS};
use crate::observability::sampler::STALE_AFTER_MS;
use crate::state::AppState;

/// Largest timeseries response body.
pub const MAX_TIMESERIES_BYTES: usize = 2 * 1024 * 1024;

pub async fn system(State(state): State<Arc<AppState>>) -> Json<Value> {
    let t0 = Instant::now();
    let shared = &state.obs.sampler;
    let snap = shared.latest();
    let age_ms = snap
        .as_ref()
        .map(|s| s.taken_at.elapsed().as_millis() as u64);
    let sampler_state = shared.effective_state(age_ms);
    let stale = age_ms.is_some_and(|a| a > STALE_AFTER_MS.max(shared.tick_ms() * 3));

    let freshness = json!({
        "last_sample_ms": snap.as_ref().map(|s| s.taken_unix_ms),
        "age_ms": age_ms,
        "state": sampler_state.as_str(),
        "stale": stale,
        "tick_ms": shared.tick_ms(),
    });
    let uptime = crate::state::uptime(&state).as_secs_f64();

    let mut body = match &snap {
        None => json!({
            "timestamp_unix_ms": now_unix_ms(),
            "sample_freshness": freshness,
            "sample_generation": Value::Null,
            "instance": {
                "id": state.config.instance_id.map(|i| i.to_string()),
                "name": state.config.instance_name,
                "uptime_seconds": uptime,
                "healthy": Value::Null,
                "readiness": Value::Null,
                "lock_state": Value::Null,
                "coordinator_state": Value::Null,
            },
        }),
        Some(s) => {
            let disk_used_pct = match (s.disk_total_bytes, s.disk_free_bytes) {
                (Some(t), Some(f)) if t > 0 => {
                    Some((t.saturating_sub(f)) as f64 / t as f64 * 100.0)
                }
                _ => None,
            };
            let mem_used_pct = match (s.system_total_bytes, s.system_used_bytes) {
                (Some(t), Some(u)) if t > 0 => Some(u as f64 / t as f64 * 100.0),
                _ => None,
            };
            json!({
                "timestamp_unix_ms": now_unix_ms(),
                "sample_freshness": freshness,
                "sample_generation": s.generation,
                "instance": {
                    "id": state.config.instance_id.map(|i| i.to_string()),
                    "name": state.config.instance_name,
                    "uptime_seconds": uptime,
                    "healthy": s.health,
                    // The same value `GET /readyz` reports as `ready` (`ready` <=> true).
                    "readiness": s.readiness,
                    // held | not_held | unavailable (a probe that could not find out says so).
                    "lock_state": s.lock_state,
                    // alive | poisoned | not_started: the WAL coordinator's terminal state, from
                    // public engine state. Additive; it does not change `readiness`.
                    "coordinator_state": s.coordinator_state,
                },
                "cpu": {
                    "process_percent": s.cpu_percent,
                    "peak_percent": s.cpu_peak_percent,
                    "vcpu_count": s.vcpu_count,
                },
                "memory": {
                    "rss_bytes": s.rss_bytes,
                    "peak_rss_bytes": s.peak_rss_bytes,
                    "system_total_bytes": s.system_total_bytes,
                    "system_used_bytes": s.system_used_bytes,
                    "system_used_percent": mem_used_pct,
                },
                "disk": {
                    "volume_total_bytes": s.disk_total_bytes,
                    "volume_free_bytes": s.disk_free_bytes,
                    "volume_used_percent": disk_used_pct,
                    // Advisory only (never an input of `instance.healthy`): `low` when free space
                    // is below the provisional threshold below, `ok`, or `unknown`.
                    "free_advisory": s.disk_free_advisory,
                    "free_advisory_threshold_percent": state.obs.disk_low_percent(),
                    "db_bytes": s.db_bytes,
                    "wal_bytes": s.wal_bytes,
                    "sstable_bytes": s.sstable_bytes,
                    "sizes_age_ms": s.sizes_taken_unix_ms.map(|t| s.taken_unix_ms.saturating_sub(t)),
                },
                // This process's own I/O as the OS reports it (`GetProcessIoCounters`): the
                // operations and bytes the process requested, NOT device activity (a device does
                // more: sectors, file-system metadata, flushes). The `device_*` namespace is
                // reserved and intentionally empty in v1 (Decision D4).
                "process": {
                    "read_ops_per_sec": s.process_read_ops_per_sec,
                    "write_ops_per_sec": s.process_write_ops_per_sec,
                    "read_mb_per_sec": s.process_read_mb_per_sec,
                    "write_mb_per_sec": s.process_write_mb_per_sec,
                },
                "throughput": {
                    "http_requests_per_sec": s.http_requests_per_sec,
                    "sql_queries_per_sec": s.sql_queries_per_sec,
                    "write_commits_per_sec": s.write_commits_per_sec,
                    "active_connections": s.active_connections,
                    "active_sessions": s.active_sessions,
                    "active_transactions": s.active_transactions,
                    "active_queries": s.active_queries,
                },
                "latency": {
                    "query_p50_ms": s.query_p50_ms,
                    "query_p95_ms": s.query_p95_ms,
                    "query_p99_ms": s.query_p99_ms,
                },
                "wal": {
                    "segment_count": s.wal_segments,
                    "bytes": s.wal_bytes,
                    "state": s.wal_state,
                },
                "compaction": {
                    "running": s.compaction_running,
                    "cycles_since_start": s.compaction_cycles,
                    "live_sstable_count": s.live_sstable_count,
                    "last_duration_ms": s.last_compaction_ms,
                },
                "background": {
                    "flush_queue_depth": s.flush_queue_depth,
                    "pending_groups": s.wal_pending_waiters,
                    "index_build_state": s.index_build_state,
                    // The engine does not currently expose a flush-completion timestamp; this
                    // field is always null in v1 (Decision D6: no engine change, no
                    // sampler-observed substitute, which would be a different field).
                    "last_flush_ms": Value::Null,
                },
                "security": {
                    "auth_failures_since_start": s.auth_failures,
                    "admin_actions_since_start": s.admin_actions,
                    "last_admin_action": s.last_admin_action.as_ref().map(|(t, route, outcome)| json!({
                        "timestamp_unix_ms": t, "route": route, "outcome": outcome,
                    })),
                    "forbidden_since_start": s.auth_forbidden,
                },
                "limits": {
                    "rate_limited_since_start": s.rate_limited,
                    "sessions_rejected_since_start": s.sessions_rejected,
                    "wal_backpressure_rejections": s.wal_backpressure_rejections,
                    "sql_resource_limit_hits": s.sql_resource_limit_hits,
                },
                "errors": {
                    "http_server_errors_since_start": s.http_server_errors,
                    "sql_errors_since_start": s.sql_errors,
                    "wal_write_errors": s.wal_write_errors,
                    // Always null in v1 (ADR-OBS-03, NOT REQUIRED FOR V1): the only counter the engine
                    // exposes is `sync_attempts - sync_successes`, two independent atomics, which reads a
                    // phantom 1 while an fsync is in flight and no write has failed. A correct value needs
                    // an engine change. The terminal state is `/v1/admin/status` `wal.poisoned`.
                    "wal_sync_failures": Value::Null,
                },
                "storage_state": s.storage_state,
            })
        }
    };
    if let Value::Object(m) = &mut body {
        m.insert(
            "latency_of_response_ms".to_string(),
            json!(t0.elapsed().as_secs_f64() * 1000.0),
        );
    }
    Json(body)
}

fn window_index(w: &str) -> Option<usize> {
    WINDOWS.iter().position(|(name, _, _)| *name == w)
}

fn build_timeseries(state: &AppState, window: usize, trim_oldest_percent: usize) -> Value {
    let (name, step, _) = WINDOWS[window];
    let mut series = Map::new();
    state.obs.sampler.with_series(|store| {
        for (k, sname) in SERIES_NAMES.iter().enumerate() {
            let v = match store.query(k, window) {
                None => Value::Null,
                Some(mut samples) => {
                    let drop = samples.len() * trim_oldest_percent / 100;
                    samples.drain(..drop);
                    Value::Array(
                        samples
                            .into_iter()
                            .map(|(t, v)| json!({ "t": t, "v": v }))
                            .collect(),
                    )
                }
            };
            series.insert((*sname).to_string(), v);
        }
    });
    json!({
        "window": name,
        "resolution_seconds": step,
        "series": series,
        "generated_unix_ms": now_unix_ms(),
    })
}

pub async fn timeseries(
    State(state): State<Arc<AppState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let window = q
        .get("window")
        .and_then(|w| window_index(w))
        .ok_or_else(|| {
            ApiError::Validation("window must be exactly one of: 15m, 1h, 24h, 7d".to_string())
        })?;
    // Capped at 2 MiB by dropping the oldest samples (never reached with the fixed ring sizes;
    // enforced anyway so the bound does not depend on them).
    let mut trim = 0;
    let bytes = loop {
        let v = build_timeseries(&state, window, trim);
        let b = serde_json::to_vec(&v).unwrap_or_default();
        if b.len() <= MAX_TIMESERIES_BYTES || trim >= 90 {
            break b;
        }
        trim += 25;
    };
    let mut resp = (StatusCode::OK, bytes).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Ok(resp)
}

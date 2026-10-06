//! `GET /v1/observability/{sessions,queries,events,version}` -- read-only diagnostic state.
//!
//! Nothing here returns an API key, a token, raw SQL text, parameters, request bodies, a
//! principal name or a file path. Every list is bounded (at most 200 records).

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::Json;
use serde_json::{json, Value};

use crate::error::ApiError;
use crate::observability::events::{Event, MAX_EVENTS_RETURNED};
use crate::observability::queries::MAX_QUERIES_RETURNED;
use crate::observability::version;
use crate::state::AppState;

pub const MAX_SESSIONS_RETURNED: usize = 200;

/// `limit` query parameter: absent = `default`, otherwise an integer clamped to `1..=max`; a
/// value that is not an integer is a 400.
fn limit_param(q: &HashMap<String, String>, default: usize, max: usize) -> Result<usize, ApiError> {
    match q.get("limit") {
        None => Ok(default),
        Some(v) => v
            .parse::<u64>()
            .map(|n| (n as usize).clamp(1, max))
            .map_err(|_| ApiError::Validation("limit must be a non-negative integer".to_string())),
    }
}

pub async fn sessions(
    State(state): State<Arc<AppState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    let limit = limit_param(&q, MAX_SESSIONS_RETURNED, MAX_SESSIONS_RETURNED)?;
    let (views, total) = state.sql.sessions.list(limit);
    let items: Vec<Value> = views
        .iter()
        .map(|s| {
            json!({
                "session_id": s.id,
                "state": if s.executing { "executing" } else { "idle" },
                "age_seconds": s.age_secs,
                "transaction_state": "open",
                "operation_class": if s.executing { "statement" } else { "none" },
                "idle_seconds": s.idle_secs,
                "idle_timeout_remaining_seconds": s.idle_timeout_remaining_secs,
                "lifetime_remaining_seconds": s.lifetime_remaining_secs,
                "timeout_state": s.timeout_state,
                // Sessions cannot be cancelled from outside in v1; only statements can.
                "cancellation_state": "not_requested",
            })
        })
        .collect();
    Ok(Json(json!({
        "sessions": items,
        "returned": items.len(),
        "total": total,
        "truncated": total > items.len(),
        "max_records": MAX_SESSIONS_RETURNED,
    })))
}

pub async fn queries(
    State(state): State<Arc<AppState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    let limit = limit_param(&q, MAX_QUERIES_RETURNED, MAX_QUERIES_RETURNED)?;
    let views = state.obs.queries.list(limit);
    let items: Vec<Value> = views
        .iter()
        .map(|v| {
            json!({
                "query_id": v.id,
                "statement_class": v.class,
                "state": v.state,
                "start_unix_ms": v.start_unix_ms,
                "duration_ms": v.duration_ms,
                "rows_affected": v.rows_affected,
                "rows_returned": v.rows_returned,
                "timeout_state": v.timeout_state,
                "cancellation_state": v.cancellation_state,
                "error_class": v.error_class,
            })
        })
        .collect();
    Ok(Json(json!({
        "queries": items,
        "returned": items.len(),
        "active": state.obs.queries.active(),
        "untracked_active": state.obs.queries.untracked(),
        "max_records": MAX_QUERIES_RETURNED,
    })))
}

fn event_json(e: &Event) -> Value {
    json!({
        "timestamp": e.ts_unix_ms,
        "event_type": e.event_type,
        "severity": e.severity,
        "operation_class": e.operation_class,
        "result": e.result,
        "duration_ms": e.duration_ms,
        "request_id": e.request_id,
        "session_id": e.session_id,
        "error_class": e.error_class,
    })
}

pub async fn events(
    State(state): State<Arc<AppState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    let limit = limit_param(&q, 50, MAX_EVENTS_RETURNED)?;
    let security: Vec<Value> = state
        .obs
        .events
        .security(limit)
        .iter()
        .map(event_json)
        .collect();
    let operational: Vec<Value> = state
        .obs
        .events
        .operational(limit)
        .iter()
        .map(event_json)
        .collect();
    Ok(Json(json!({
        "limit": limit,
        "max_limit": MAX_EVENTS_RETURNED,
        // Kept apart on purpose: a flood of one kind cannot evict the other.
        "security": security,
        "operational": operational,
    })))
}

pub async fn version(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(version::info(state.started_at))
}

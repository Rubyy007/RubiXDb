//! `POST /v1/snapshots`, `GET /v1/snapshots`, `GET /v1/snapshots/{id}`,
//! `DELETE /v1/snapshots/{id}` — `PHASE_API_ARCHITECTURE.md` §2.1.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Serialize;
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::{AppState, HeldSnapshot};

#[derive(Serialize)]
pub struct SnapshotBody {
    id: Uuid,
    seq: u64,
    created_at_unix_secs: u64,
}

fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

pub async fn create(State(state): State<Arc<AppState>>) -> Json<SnapshotBody> {
    let snapshot = state.engine.snapshot();
    let seq = snapshot.seq();
    let id = Uuid::new_v4();
    let created_at = SystemTime::now();
    state
        .snapshots
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(
            id,
            HeldSnapshot {
                snapshot,
                created_at,
            },
        );
    Json(SnapshotBody {
        id,
        seq,
        created_at_unix_secs: unix_secs(created_at),
    })
}

pub async fn list(State(state): State<Arc<AppState>>) -> Json<Vec<SnapshotBody>> {
    let snapshots = state.snapshots.lock().unwrap_or_else(|p| p.into_inner());
    let body = snapshots
        .iter()
        .map(|(id, held)| SnapshotBody {
            id: *id,
            seq: held.snapshot.seq(),
            created_at_unix_secs: unix_secs(held.created_at),
        })
        .collect();
    Json(body)
}

pub async fn get(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<SnapshotBody>, ApiError> {
    let snapshots = state.snapshots.lock().unwrap_or_else(|p| p.into_inner());
    let held = snapshots
        .get(&id)
        .ok_or_else(|| ApiError::NotFound("snapshot".to_string()))?;
    Ok(Json(SnapshotBody {
        id,
        seq: held.snapshot.seq(),
        created_at_unix_secs: unix_secs(held.created_at),
    }))
}

pub async fn release(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let removed = state
        .snapshots
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&id);
    match removed {
        // `removed`'s `Drop` (outside the lock, once this scope ends)
        // releases the snapshot from the engine's `SnapshotRegistry`.
        Some(_) => Ok(StatusCode::NO_CONTENT),
        None => Err(ApiError::NotFound("snapshot".to_string())),
    }
}

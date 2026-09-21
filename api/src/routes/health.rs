//! `GET /healthz` (unauthenticated liveness) and `GET /readyz`
//! (authenticated readiness) — `PHASE_API_ARCHITECTURE.md` §2/§6.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde::Serialize;

use crate::state::AppState;

#[derive(Serialize)]
pub struct HealthBody {
    status: &'static str,
}

/// No engine call, no auth — a load balancer must be able to probe
/// process liveness without a credential.
pub async fn healthz() -> Json<HealthBody> {
    Json(HealthBody { status: "ok" })
}

#[derive(Serialize)]
pub struct ReadyBody {
    ready: bool,
    storage_state: String,
}

/// Requires auth (every route except `/healthz` does). Returns 200 as
/// long as the engine handle is alive and responsive, regardless of
/// `storage_state` value -- `StorageFull` still means "ready to serve
/// reads and rejects writes correctly," not "the service is down."
/// `/v1/status` is where `storage_state` itself is inspected.
pub async fn readyz(State(state): State<Arc<AppState>>) -> Json<ReadyBody> {
    Json(ReadyBody {
        ready: true,
        storage_state: format!("{:?}", state.engine.storage_state()),
    })
}

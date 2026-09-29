//! `GET /v1/instance` -- unauthenticated, additive, read-only identity
//! probe. `PHASE_RUBIXDB_INSTANCE_ARCHITECTURE.md` §6.
//!
//! This exists for exactly one reason: when `rubixdb gui`/`rubixdb
//! cli`'s instance manager finds its OS-level lock already held by
//! another live process, it must verify -- over real HTTP, not by
//! assumption -- that whatever is listening on the recorded port is
//! genuinely *this* instance before treating it as attachable (item
//! "verify instance identity through a real health/handshake
//! mechanism"). `/healthz` alone cannot answer that: it proves *some*
//! rubixdb-api process is alive on the port, not *which* one. No
//! secret is exposed here (mirrors `/healthz`'s own no-auth
//! reasoning: a load balancer / instance manager must be able to probe
//! without a credential it may not have yet).

use axum::extract::State;
use axum::Json;
use serde::Serialize;
use std::sync::Arc;

use crate::state::AppState;

#[derive(Serialize)]
pub struct InstanceIdentityBody {
    instance_id: Option<String>,
    name: Option<String>,
}

pub async fn instance(State(state): State<Arc<AppState>>) -> Json<InstanceIdentityBody> {
    Json(InstanceIdentityBody {
        instance_id: state.config.instance_id.map(|id| id.to_string()),
        name: state.config.instance_name.clone(),
    })
}

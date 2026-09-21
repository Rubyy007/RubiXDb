//! `GET /healthz` (unauthenticated liveness) and `GET /readyz`
//! (authenticated readiness) — `PHASE_API_ARCHITECTURE.md` §2/§6.

use std::sync::Arc;

use axum::extract::State;
use axum::Extension;
use axum::Json;
use serde::Serialize;

use crate::auth::Principal;
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

#[derive(Serialize)]
pub struct WhoAmIBody {
    principal_name: String,
    role: &'static str,
}

/// `GET /v1/whoami` -- not itself part of `PHASE_API_ARCHITECTURE.md`'s
/// original §2 contract table, added while building the frontend
/// (`PHASE_FRONTEND_ARCHITECTURE.md` §5): a role-aware UI needs to
/// know its own authenticated role to decide which actions to enable,
/// and no existing endpoint exposed it. Purely additive (one new GET
/// route, reads the `Principal` `auth_middleware` already attaches to
/// every authenticated request) -- no change to the auth model itself.
pub async fn whoami(Extension(principal): Extension<Principal>) -> Json<WhoAmIBody> {
    Json(WhoAmIBody {
        principal_name: principal.name,
        role: match principal.role {
            crate::config::Role::Admin => "admin",
            crate::config::Role::Reader => "reader",
        },
    })
}

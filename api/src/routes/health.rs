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
    /// `running` | `complete` | `failed` | `not_started`: the startup recovery
    /// of interrupted `CREATE INDEX` / `DROP INDEX` operations. Additive field;
    /// `ready` keeps its meaning (normal work is safe while this runs).
    index_recovery: &'static str,
}

/// Requires auth (every route except `/healthz` does). Returns 200 as
/// long as the engine handle is alive and responsive, regardless of
/// `storage_state` value -- `StorageFull` still means "ready to serve
/// reads and rejects writes correctly," not "the service is down."
/// `/v1/status` is where `storage_state` itself is inspected.
/// `index_recovery` says whether the post-start recovery of interrupted index
/// builds is still running, so "ready" is no longer the only thing a script can
/// learn from this endpoint.
pub async fn readyz(State(state): State<Arc<AppState>>) -> Json<ReadyBody> {
    Json(ReadyBody {
        // The one readiness definition (`observability::sampler::ready`), unchanged in meaning: the
        // same value `GET /v1/metrics/system` reports as `instance.readiness` (`true` <=> `"ready"`).
        ready: crate::observability::sampler::ready(),
        storage_state: format!("{:?}", state.engine.storage_state()),
        index_recovery: state.index_recovery.state().as_str(),
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

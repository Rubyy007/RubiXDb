//! Authentication/authorization — `PHASE_API_ARCHITECTURE.md` §4.
//! Bearer API-key, two roles (`reader`/`admin`), a request `Principal`
//! carried through the service layer for audit logging.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::Response;

use crate::config::{ApiKeyConfig, Role};
use crate::error::ApiError;
use crate::state::AppState;

#[derive(Debug, Clone)]
pub struct Principal {
    pub name: String,
    pub role: Role,
}

/// One implementation today (static, configuration-loaded keys) behind
/// a boundary a future external identity provider could replace
/// without touching any request-handling code that consumes it — the
/// architecture doc's own stated reason for this shape.
pub struct AuthProvider {
    by_key: HashMap<String, Principal>,
}

impl AuthProvider {
    pub fn from_config(keys: &[ApiKeyConfig]) -> Self {
        let by_key = keys
            .iter()
            .map(|k| {
                (
                    k.key.clone(),
                    Principal {
                        name: k.name.clone(),
                        role: k.role,
                    },
                )
            })
            .collect();
        AuthProvider { by_key }
    }

    pub fn authenticate(&self, bearer_token: &str) -> Option<Principal> {
        self.by_key.get(bearer_token).cloned()
    }
}

fn extract_bearer(req: &Request) -> Option<&str> {
    req.headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

/// Minimum role a request's HTTP method requires — `GET`/`HEAD` read
/// the database, everything else mutates it or its snapshot registry.
///
/// `POST /v1/sql` is a deliberate exception (item 14 of `PHASE_
/// RELATIONAL_SQL_API_ARCHITECTURE.md`'s own governing directive: "the
/// API must not implement its own competing table/column/index
/// authorization rules"): unlike every other route, one JSON body sent
/// as `POST` can carry a read-only `SELECT`/`EXPLAIN` (a `Reader`-
/// appropriate operation) or a mutating `INSERT`/`UPDATE`/`DELETE`/DDL —
/// the HTTP method alone cannot distinguish them the way it can for
/// every other route's fixed, single-purpose semantics. Gating the
/// whole endpoint at `Admin` would make an ordinary reader-role `SELECT`
/// through SQL strictly *more* restricted than the identical read via
/// `GET /v1/kv`, for no security reason; gating it at `Reader` and
/// deferring the real per-statement decision to `rubixdb_sql::bind`'s
/// own already-certified authorization model (`crate::routes::sql`'s
/// `AuthContext` mapping, immediately below) is the one-authorization-
/// boundary design the whole increment requires. A `Reader` who submits
/// `INSERT`/`UPDATE`/`DELETE`/DDL genuinely does then reach that check
/// and is correctly denied there — verified in `api/tests/api_
/// integration.rs`'s own SQL authorization matrix, never merely
/// asserted here.
fn required_role(req: &Request) -> Role {
    if req.uri().path() == "/v1/sql" {
        return Role::Reader;
    }
    // Operator endpoints disclose or change operational state (backup
    // names, integrity findings, resource and WAL internals): Admin for
    // every method, including GET.
    if req.uri().path().starts_with("/v1/admin/") {
        return Role::Admin;
    }
    match *req.method() {
        axum::http::Method::GET | axum::http::Method::HEAD => Role::Reader,
        _ => Role::Admin,
    }
}

/// The **only** place an API-layer `Role` is translated into a SQL-
/// layer `rubixdb_sql::auth::AuthContext` — item 14's own "D25's v1
/// default-privilege mapping" doc comment in `sql/src/auth.rs` names
/// this exact mapping as the wiring a future consumer would supply;
/// this is that consumer. `Role::Admin` -> `DefaultAccess::Admin`
/// (every privilege on every object), `Role::Reader` -> `DefaultAccess::
/// Reader` (`SELECT` on every object, plus whatever `system.grants` rows
/// exist for this principal by name) — never a third, API-invented
/// access tier.
pub fn to_sql_auth_context(principal: &Principal) -> rubixdb_sql::auth::AuthContext {
    match principal.role {
        Role::Admin => rubixdb_sql::auth::AuthContext::admin(principal.name.clone()),
        Role::Reader => rubixdb_sql::auth::AuthContext::reader(principal.name.clone()),
    }
}

/// Applied to every route except `/healthz` (see `routes::build_router`
/// — the liveness probe must not require a credential).
pub async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    use crate::observability::events::{kind, severity, Event};
    use std::sync::atomic::Ordering;
    let request_id = state.obs.counters.next_request_id();
    let principal = match extract_bearer(&req).and_then(|t| state.auth.authenticate(t)) {
        Some(p) => p,
        None => {
            // Phase 7 SG-3b: authentication failure (rate-bounded by the
            // sink). Only the method and the matched route *pattern* are
            // recorded -- never the presented credential or the raw URI.
            let route = req
                .extensions()
                .get::<axum::extract::MatchedPath>()
                .map(|m| m.as_str().to_string());
            crate::security_log::emit(&crate::security_log::SecurityEvent {
                code: crate::security_log::code::AUTH_FAILURE,
                method: Some(req.method().as_str()),
                route: route.as_deref(),
                status: Some(401),
                outcome: "denied",
                ..Default::default()
            });
            state
                .obs
                .counters
                .auth_failures
                .fetch_add(1, Ordering::Relaxed);
            state.obs.events.push_security(
                Event::new(kind::AUTH_FAILURE, severity::WARNING, "auth", "denied")
                    .request(Some(request_id))
                    .error_class("UNAUTHORIZED"),
            );
            return Err(ApiError::Unauthorized);
        }
    };
    let required = required_role(&req);
    if !principal.role.satisfies(required) {
        state
            .obs
            .counters
            .auth_forbidden
            .fetch_add(1, Ordering::Relaxed);
        state.obs.events.push_security(
            Event::new(kind::AUTH_FORBIDDEN, severity::WARNING, "auth", "denied")
                .request(Some(request_id))
                .error_class("FORBIDDEN"),
        );
        return Err(ApiError::Forbidden);
    }
    if !state.rate_limiter.check(&principal.name) {
        state
            .obs
            .counters
            .rate_limited
            .fetch_add(1, Ordering::Relaxed);
        state.obs.events.push_security(
            Event::new(
                kind::AUTH_RATE_LIMITED,
                severity::WARNING,
                "auth",
                "refused",
            )
            .request(Some(request_id))
            .error_class("RATE_LIMITED"),
        );
        return Err(ApiError::RateLimited);
    }
    req.extensions_mut().insert(principal);
    req.extensions_mut()
        .insert(crate::observability::RequestId(request_id));
    Ok(next.run(req).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ApiKeyConfig;

    fn keys() -> Vec<ApiKeyConfig> {
        vec![
            ApiKeyConfig {
                name: "admin-svc".to_string(),
                role: Role::Admin,
                key: "admin-key-0123456789".to_string(),
            },
            ApiKeyConfig {
                name: "reader-svc".to_string(),
                role: Role::Reader,
                key: "reader-key-0123456789".to_string(),
            },
        ]
    }

    #[test]
    fn authenticates_known_key() {
        let provider = AuthProvider::from_config(&keys());
        let p = provider.authenticate("admin-key-0123456789").unwrap();
        assert_eq!(p.name, "admin-svc");
        assert_eq!(p.role, Role::Admin);
    }

    #[test]
    fn rejects_unknown_key() {
        let provider = AuthProvider::from_config(&keys());
        assert!(provider.authenticate("not-a-real-key").is_none());
    }
}

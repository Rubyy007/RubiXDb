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
fn required_role(req: &Request) -> Role {
    match *req.method() {
        axum::http::Method::GET | axum::http::Method::HEAD => Role::Reader,
        _ => Role::Admin,
    }
}

/// Applied to every route except `/healthz` (see `routes::build_router`
/// — the liveness probe must not require a credential).
pub async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let token = extract_bearer(&req).ok_or(ApiError::Unauthorized)?;
    let principal = state
        .auth
        .authenticate(token)
        .ok_or(ApiError::Unauthorized)?;
    let required = required_role(&req);
    if !principal.role.satisfies(required) {
        return Err(ApiError::Forbidden);
    }
    if !state.rate_limiter.check(&principal.name) {
        return Err(ApiError::RateLimited);
    }
    req.extensions_mut().insert(principal);
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

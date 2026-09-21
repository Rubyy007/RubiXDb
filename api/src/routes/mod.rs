pub mod compaction;
pub mod health;
pub mod kv;
pub mod metrics_route;
pub mod range;
pub mod snapshots;
pub mod status;

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, MatchedPath, Request, State};
use axum::http::{header, Method};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post, put};
use axum::Router;
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::auth::auth_middleware;
use crate::state::AppState;

/// Records one `ServiceMetrics` sample per request -- route label from
/// `MatchedPath` when available (the normal case for every route this
/// service defines), falling back to the raw URI path only for a
/// request that matched no route at all (a 404 from the router itself).
async fn metrics_middleware(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|m| format!("{} {}", req.method(), m.as_str()))
        .unwrap_or_else(|| format!("{} {}", req.method(), req.uri().path()));
    let guard = state.metrics.start_request(route);
    let response = next.run(req).await;
    guard.finish(response.status().is_client_error() || response.status().is_server_error());
    response
}

/// axum's `Json` extractor enforces its own independent default body-
/// size limit (2 MiB) *before* any handler runs -- left alone, a
/// deployment that configures `max_value_bytes` above that default
/// would see requests rejected by an undocumented framework limit
/// instead of this service's own, intentional `VALIDATION_ERROR`
/// response (a real gap this crate's own integration tests caught:
/// an early version of `oversized_value_is_rejected` picked a value
/// large enough to trip axum's limit first, silently never exercising
/// `kv::put`'s own check at all). Base64 costs ~4/3 the raw byte
/// count, plus a small fixed allowance for JSON framing/the key
/// field/field names.
fn body_size_limit(config: &crate::config::Config) -> usize {
    (config.max_value_bytes + config.max_key_bytes) * 4 / 3 + 4096
}

/// `None` (no layer applied -- same-origin only, the safe default)
/// unless `RUBIXDB_CORS_ALLOWED_ORIGINS` names at least one origin.
/// Never wildcards the origin: this API is authenticated and mutable,
/// so an explicit allow-list is used even though the `Authorization`
/// header alone (no cookies, `credentials: 'include'` never set by
/// this project's own frontend) would not technically require one --
/// restricting the allow-list still prevents an arbitrary third-party
/// page's script from reading a response even if it somehow obtained
/// a valid bearer token some other way.
fn cors_layer(config: &crate::config::Config) -> Option<CorsLayer> {
    if config.cors_allowed_origins.is_empty() {
        return None;
    }
    let origins: Vec<_> = config
        .cors_allowed_origins
        .iter()
        .filter_map(|o| o.parse().ok())
        .collect();
    Some(
        CorsLayer::new()
            .allow_origin(AllowOrigin::list(origins))
            .allow_methods([Method::GET, Method::PUT, Method::POST, Method::DELETE])
            .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
    )
}

pub fn build_router(state: Arc<AppState>) -> Router {
    let protected = Router::new()
        .route("/readyz", get(health::readyz))
        .route("/v1/whoami", get(health::whoami))
        .route("/v1/status", get(status::status))
        .route("/v1/metadata", get(status::metadata))
        .route("/v1/kv", put(kv::put))
        .route("/v1/kv/:key_b64", get(kv::get).delete(kv::delete))
        .route("/v1/kv/:key_b64/exists", get(kv::exists))
        .route("/v1/range", get(range::range))
        .route(
            "/v1/snapshots",
            post(snapshots::create).get(snapshots::list),
        )
        .route(
            "/v1/snapshots/:id",
            get(snapshots::get).delete(snapshots::release),
        )
        .route("/v1/compaction/status", get(compaction::status))
        .route("/v1/compaction/metrics", get(compaction::metrics))
        .route("/v1/metrics", get(metrics_route::metrics))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            metrics_middleware,
        ))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    let limit = body_size_limit(&state.config);
    let cors = cors_layer(&state.config);
    let mut router = Router::new()
        .route("/healthz", get(health::healthz))
        .merge(protected)
        .layer(DefaultBodyLimit::max(limit));
    if let Some(cors) = cors {
        router = router.layer(cors);
    }
    router.with_state(state)
}

pub mod admin;
pub mod catalog;
pub mod compaction;
pub mod health;
pub mod instance;
pub mod kv;
pub mod metrics_route;
pub mod range;
pub mod snapshots;
pub mod sql;
pub mod status;

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, MatchedPath, Request, State};
use axum::http::{header, Method};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post, put};
use axum::Router;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::services::{ServeDir, ServeFile};

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
        .route("/v1/sql", post(sql::sql))
        .route("/v1/catalog/databases", get(catalog::databases))
        .route("/v1/catalog/schemas", get(catalog::schemas))
        .route(
            "/v1/catalog/schemas/:schema_id",
            axum::routing::delete(catalog::delete_schema),
        )
        .route("/v1/catalog/tables", get(catalog::tables))
        .route("/v1/catalog/tables/:name", get(catalog::describe_table))
        .route(
            "/v1/catalog/tables/by-id/:table_id",
            axum::routing::delete(catalog::delete_table),
        )
        .route("/v1/catalog/indexes", get(catalog::indexes))
        .route(
            "/v1/catalog/indexes/:index_id",
            axum::routing::delete(catalog::delete_index),
        )
        .route("/v1/catalog/authz", get(catalog::authz))
        .route("/v1/admin/status", get(admin::status))
        .route(
            "/v1/admin/backups",
            get(admin::list_backups).post(admin::create_backup),
        )
        .route("/v1/admin/backups/:name/verify", post(admin::verify_backup))
        .route(
            "/v1/admin/backups/:name",
            axum::routing::delete(admin::delete_backup),
        )
        .route("/v1/admin/check", post(admin::check))
        .route("/v1/admin/shutdown", post(admin::shutdown))
        .route("/v1/admin/storage", get(admin::storage))
        .route(
            "/v1/admin/maintenance/purge-orphans",
            post(admin::purge_orphans),
        )
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
        .route("/v1/instance", get(instance::instance))
        .merge(protected)
        .layer(DefaultBodyLimit::max(limit));
    if let Some(cors) = cors {
        router = router.layer(cors);
    }
    // Only when `rubixdb gui` (or an explicit `RUBIXDB_FRONTEND_DIST`
    // override) supplies a built frontend -- `PHASE_RUBIXDB_GUI_
    // ARCHITECTURE.md` §3. A `.fallback_service` only ever runs for a
    // request that matched none of the routes above, so this can never
    // shadow `/v1/*`, `/healthz`, or `/readyz` -- exact-match API
    // routes always win. Unmatched static-asset paths (`/assets/*.js`)
    // are served from disk; any other unmatched GET (a client-side
    // route like `/sql`) falls through `ServeDir`'s own `not_found_
    // service` to `index.html`, the standard SPA-fallback shape, so a
    // browser refresh on a deep link still works. The pre-existing
    // standalone-API deployment (`frontend_dist: None`) gets exactly
    // today's router, byte-for-byte -- this whole block is additive.
    if let Some(dist) = &state.config.frontend_dist {
        let index_html = dist.join("index.html");
        // Plain `.fallback(...)`, not `.not_found_service(...)` --
        // the latter forces every fallback response to HTTP 404
        // regardless of whether the file was actually served (tower-
        // http's own documented behavior), which would mean every
        // client-side route (`/sql`, a refreshed deep link) loads
        // with a 404 status. `.fallback` preserves `ServeFile`'s own
        // real 200 for a successful read -- the standard SPA-
        // fallback contract (a client-side route is a real,
        // successful page load, not an error).
        let serve_dir = ServeDir::new(dist).fallback(ServeFile::new(index_html));
        router = router.fallback_service(serve_dir);
    }
    router.with_state(state)
}

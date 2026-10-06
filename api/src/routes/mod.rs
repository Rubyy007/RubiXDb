pub mod admin;
pub mod catalog;
pub mod compaction;
pub mod health;
pub mod instance;
pub mod kv;
pub mod metrics_route;
pub mod metrics_system;
pub mod observability;
pub mod range;
pub mod snapshots;
pub mod sql;
pub mod status;

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, MatchedPath, Request, State};
use axum::http::{header, HeaderName, HeaderValue, Method};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post, put};
use axum::Router;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::services::{ServeDir, ServeFile};

use crate::auth::auth_middleware;
use crate::state::AppState;

/// The route label used as a metric key. **Closed set**: the route *template* from the router
/// (`MatchedPath`, a member of the static route table) combined with the HTTP method only when
/// the method is one of the standard ones; any other method token is folded into `OTHER`, and a
/// request that matched no route gets the constant `UNMATCHED`. Request input (path parameters,
/// query strings, arbitrary method tokens) therefore can never create a new key.
pub fn route_label(method: &Method, matched: Option<&str>) -> String {
    let m = match *method {
        Method::GET => "GET",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::DELETE => "DELETE",
        Method::HEAD => "HEAD",
        Method::OPTIONS => "OPTIONS",
        Method::PATCH => "PATCH",
        _ => "OTHER",
    };
    format!("{m} {}", matched.unwrap_or("UNMATCHED"))
}

/// Records one `ServiceMetrics` sample per request, labelled by [`route_label`], and counts
/// 5xx responses.
async fn metrics_middleware(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    let route = route_label(
        req.method(),
        req.extensions().get::<MatchedPath>().map(|m| m.as_str()),
    );
    let guard = state.metrics.start_request(route);
    let response = next.run(req).await;
    let status = response.status();
    if status.is_server_error() {
        state
            .obs
            .counters
            .http_server_errors
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    guard.finish(status.is_client_error() || status.is_server_error());
    response
}

/// Phase 7 SG-3b: records the D-4 events that are decided by route alone --
/// `/v1/admin/*` actions (every non-`GET`/`HEAD` call: backup create/verify/
/// delete, check, purge-orphans, shutdown; read-only inspection such as
/// `GET /v1/admin/status`, which the console polls, is not an action) and the
/// REST catalog drops. Runs *inside* `auth_middleware` (so the principal is
/// known) and after the handler (so the real status is recorded). The
/// record carries the route *pattern*, never the raw URI, so a backup name
/// or other path parameter is not logged; a catalog drop additionally carries
/// its numeric object id (digits only, bounded).
async fn audit_middleware(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    let classified = {
        let route = req.extensions().get::<MatchedPath>().map(|m| m.as_str());
        route.and_then(|r| classify_for_audit(req.method(), r))
    };
    let Some((code, kind)) = classified else {
        return next.run(req).await;
    };
    let method = req.method().as_str().to_string();
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_string())
        .unwrap_or_default();
    let principal = req
        .extensions()
        .get::<crate::auth::Principal>()
        .map(|p| p.name.clone());
    let object_id = kind.and_then(|_| {
        req.uri()
            .path()
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty() && s.len() <= 20 && s.bytes().all(|b| b.is_ascii_digit()))
            .map(str::to_string)
    });
    let request_id = req
        .extensions()
        .get::<crate::observability::RequestId>()
        .map(|r| r.0);
    let started = std::time::Instant::now();
    let response = next.run(req).await;
    let status = response.status().as_u16();
    {
        use crate::observability::events::{kind, severity, Event};
        let outcome = crate::security_log::outcome_for_status(status);
        let is_admin = code == crate::security_log::code::ADMIN_ACTION;
        if is_admin {
            state.obs.counters.record_admin_action(&route, outcome);
        }
        state.obs.events.push_security(
            Event::new(
                if is_admin {
                    kind::ADMIN_ACTION
                } else {
                    kind::CATALOG_DDL
                },
                if outcome == "ok" {
                    severity::INFO
                } else {
                    severity::WARNING
                },
                if is_admin { "admin" } else { "catalog" },
                outcome,
            )
            .request(request_id)
            .duration(started.elapsed().as_secs_f64() * 1000.0),
        );
    }
    crate::security_log::emit(&crate::security_log::SecurityEvent {
        code,
        principal: principal.as_deref(),
        method: Some(&method),
        route: Some(&route),
        status: Some(status),
        outcome: crate::security_log::outcome_for_status(status),
        object_kind: kind,
        object: object_id.as_deref(),
    });
    response
}

/// `(event code, object kind)` for the routes D-4 names, else `None`.
fn classify_for_audit(
    method: &Method,
    route: &str,
) -> Option<(&'static str, Option<&'static str>)> {
    use crate::security_log::code;
    if route.starts_with("/v1/admin/") && method != Method::GET && method != Method::HEAD {
        return Some((code::ADMIN_ACTION, None));
    }
    if method == Method::DELETE {
        return match route {
            "/v1/catalog/schemas/:schema_id" => Some((code::CATALOG_DROP, Some("schema"))),
            "/v1/catalog/tables/by-id/:table_id" => Some((code::CATALOG_DROP, Some("table"))),
            "/v1/catalog/indexes/:index_id" => Some((code::CATALOG_DROP, Some("index"))),
            _ => None,
        };
    }
    None
}

/// Phase 7 SG-5: defence-in-depth response headers on every response (SPA,
/// static assets, API, errors), applied by `tower-http`'s allocation-free
/// `SetResponseHeaderLayer` (an `axum::middleware::from_fn` layer was measured
/// at about +1.5 us per request here -- see PROGRESS.md). Rationale for the CSP is in
/// `PROGRESS.md` (2026-10-04, Increment C): the built console has no inline
/// `<script>`/`<style>` and no `data:` URIs, and React applies its `style`
/// props through the CSSOM (which CSP does not restrict), so neither
/// `'unsafe-inline'` nor `'unsafe-eval'` is needed anywhere. `Cache-Control:
/// no-store` is added to API responses (`/v1/*`) so credentials-bearing
/// responses are not cached; static assets stay cacheable.
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; frame-ancestors 'none'; base-uri 'none'; object-src 'none'";

/// One layer, one wrapper: adds the SG-5 headers to every response. It is a
/// hand-written `tower` layer with a non-boxing future on purpose -- stacking
/// four `Router::layer`/`SetResponseHeaderLayer` wrappers measured about
/// +3-4 us per request on the in-process router path (A/B in PROGRESS.md)
/// because each wrapper boxes and clones the inner service per request.
#[derive(Clone, Copy)]
struct SecurityHeadersLayer;

impl<S> tower::Layer<S> for SecurityHeadersLayer {
    type Service = SecurityHeaders<S>;
    fn layer(&self, inner: S) -> Self::Service {
        SecurityHeaders { inner }
    }
}

#[derive(Clone)]
struct SecurityHeaders<S> {
    inner: S,
}

impl<S> tower::Service<Request> for SecurityHeaders<S>
where
    S: tower::Service<Request, Response = Response>,
{
    type Response = Response;
    type Error = S::Error;
    type Future = SecurityHeadersFuture<S::Future>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        // `Cache-Control: no-store` for API responses only (static assets
        // stay cacheable); decided from the request path before it is moved.
        let is_api = req.uri().path().starts_with("/v1/");
        SecurityHeadersFuture {
            inner: self.inner.call(req),
            is_api,
        }
    }
}

pin_project_lite::pin_project! {
    struct SecurityHeadersFuture<F> {
        #[pin]
        inner: F,
        is_api: bool,
    }
}

impl<F, E> std::future::Future for SecurityHeadersFuture<F>
where
    F: std::future::Future<Output = Result<Response, E>>,
{
    type Output = Result<Response, E>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let this = self.project();
        let mut response = match this.inner.poll(cx) {
            std::task::Poll::Ready(Ok(r)) => r,
            std::task::Poll::Ready(Err(e)) => return std::task::Poll::Ready(Err(e)),
            std::task::Poll::Pending => return std::task::Poll::Pending,
        };
        let h = response.headers_mut();
        h.insert(
            HeaderName::from_static("content-security-policy"),
            HeaderValue::from_static(CSP),
        );
        h.insert(
            HeaderName::from_static("x-content-type-options"),
            HeaderValue::from_static("nosniff"),
        );
        h.insert(
            HeaderName::from_static("referrer-policy"),
            HeaderValue::from_static("no-referrer"),
        );
        if *this.is_api {
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        }
        std::task::Poll::Ready(Ok(response))
    }
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
    // The audit layer is attached only to the routes `classify_for_audit` can
    // ever match (admin actions and REST catalog drops), so the SQL/KV/read
    // hot paths pay nothing for it.
    let audit = middleware::from_fn_with_state(state.clone(), audit_middleware);
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
        .route("/v1/metrics/system", get(metrics_system::system))
        .route(
            "/v1/metrics/system/timeseries",
            get(metrics_system::timeseries),
        )
        .route("/v1/observability/sessions", get(observability::sessions))
        .route("/v1/observability/queries", get(observability::queries))
        .route("/v1/observability/events", get(observability::events))
        .route("/v1/observability/version", get(observability::version))
        .route("/v1/sql", post(sql::sql))
        .route("/v1/catalog/databases", get(catalog::databases))
        .route("/v1/catalog/schemas", get(catalog::schemas))
        .route(
            "/v1/catalog/schemas/:schema_id",
            axum::routing::delete(catalog::delete_schema).layer(audit.clone()),
        )
        .route("/v1/catalog/tables", get(catalog::tables))
        .route("/v1/catalog/tables/:name", get(catalog::describe_table))
        .route(
            "/v1/catalog/tables/by-id/:table_id",
            axum::routing::delete(catalog::delete_table).layer(audit.clone()),
        )
        .route("/v1/catalog/indexes", get(catalog::indexes))
        .route(
            "/v1/catalog/indexes/:index_id",
            axum::routing::delete(catalog::delete_index).layer(audit.clone()),
        )
        .route("/v1/catalog/authz", get(catalog::authz))
        .route("/v1/admin/status", get(admin::status))
        .route(
            "/v1/admin/backups",
            get(admin::list_backups)
                .post(admin::create_backup)
                .layer(audit.clone()),
        )
        .route(
            "/v1/admin/backups/:name/verify",
            post(admin::verify_backup).layer(audit.clone()),
        )
        .route(
            "/v1/admin/backups/:name",
            axum::routing::delete(admin::delete_backup).layer(audit.clone()),
        )
        .route("/v1/admin/check", post(admin::check).layer(audit.clone()))
        .route(
            "/v1/admin/shutdown",
            post(admin::shutdown).layer(audit.clone()),
        )
        .route("/v1/admin/storage", get(admin::storage))
        .route(
            "/v1/admin/maintenance/purge-orphans",
            post(admin::purge_orphans).layer(audit),
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
    router.layer(SecurityHeadersLayer).with_state(state)
}

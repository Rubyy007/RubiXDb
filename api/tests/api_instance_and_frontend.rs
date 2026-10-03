//! Real, end-to-end tests for two additive Increment-13 API changes:
//! `GET /v1/instance` (identity handshake, `PHASE_RUBIXDB_INSTANCE_
//! ARCHITECTURE.md` §6) and optional frontend static/SPA-fallback
//! serving (`PHASE_RUBIXDB_GUI_ARCHITECTURE.md` §3). Both are opt-in
//! (`Config::instance_id`/`instance_name`/`frontend_dist`, all
//! `Option`, default `None`) -- these tests prove both the "on" and
//! "off" (pre-existing, unchanged) behavior, against the real router
//! via `tower::ServiceExt::oneshot`, never mocked.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use rubixdb::execution::batch_coordinator::BatchCoordinatorConfig;
use rubixdb::lsm::{LsmConfig, LsmEngine};
use rubixdb::wal::{SyncMode, WalConfig};
use rubixdb_api::config::{ApiKeyConfig, Role};
use rubixdb_api::routes::build_router;
use rubixdb_api::{AppState, Config};
use serde_json::Value;
use std::time::Duration;
use tower::ServiceExt;
use uuid::Uuid;

const ADMIN_KEY: &str = "test-admin-key-0123456789ab";

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("rubixdb_api_if_{tag}_{nanos}"));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn base_config(data_dir: PathBuf) -> Config {
    Config {
        data_dir,
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        api_keys: vec![ApiKeyConfig {
            name: "admin".to_string(),
            role: Role::Admin,
            key: ADMIN_KEY.to_string(),
        }],
        max_value_bytes: 1024 * 1024,
        max_key_bytes: 4096,
        default_range_limit: 100,
        max_range_limit: 10_000,
        shutdown_drain_secs: 5,
        rate_limit_rps: 10_000.0,
        rate_limit_burst: 10_000,
        compaction_auto_trigger: false,
        compaction_trigger_count: 4,
        cors_allowed_origins: vec![],
        sql_max_sessions_per_principal: 50,
        sql_session_idle_timeout_secs: 300,
        sql_session_max_lifetime_secs: 1800,
        sql_statement_deadline_secs: 30,
        instance_id: None,
        instance_name: None,
        frontend_dist: None,
        backup_dir: None,
    }
}

fn wal_config() -> WalConfig {
    WalConfig {
        sync_mode: SyncMode::GroupCommit {
            max_wait: Duration::from_millis(5),
            max_batch_bytes: 256 * 1024,
        },
        ..WalConfig::default()
    }
}

fn pool_config() -> BatchCoordinatorConfig {
    BatchCoordinatorConfig {
        queue_capacity: 256,
        max_queued_bytes: 16 * 1024 * 1024,
        submission_timeout: Duration::from_secs(5),
        shutdown_drain_bound: Duration::from_secs(30),
        await_retry_budget: Duration::from_secs(5),
        max_drain_per_batch: 4096,
    }
}

fn build_app(dir: &Path, config: Config) -> (Arc<AppState>, Router) {
    let lsm_config = LsmConfig::default();
    let engine =
        Arc::new(LsmEngine::open(dir, wal_config(), pool_config(), lsm_config.clone()).unwrap());
    let state = Arc::new(AppState::new(engine, lsm_config, config));
    let router = build_router(state.clone());
    (state, router)
}

async fn json_body(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn get(uri: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn instance_route_reports_none_when_not_launched_by_the_instance_manager() {
    let dir = temp_dir("instance_none");
    let (_state, router) = build_app(&dir, base_config(dir.clone()));

    let resp = router.oneshot(get("/v1/instance")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp).await;
    assert_eq!(body["instance_id"], Value::Null);
    assert_eq!(body["name"], Value::Null);
}

#[tokio::test]
async fn instance_route_reports_real_identity_and_needs_no_auth() {
    let dir = temp_dir("instance_some");
    let id = Uuid::new_v4();
    let config = Config {
        instance_id: Some(id),
        instance_name: Some("default".to_string()),
        ..base_config(dir.clone())
    };
    let (_state, router) = build_app(&dir, config);

    // No Authorization header at all -- must still succeed, same
    // reasoning as /healthz.
    let resp = router.oneshot(get("/v1/instance")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp).await;
    assert_eq!(body["instance_id"], Value::String(id.to_string()));
    assert_eq!(body["name"], Value::String("default".to_string()));
}

#[tokio::test]
async fn without_frontend_dist_unmatched_routes_still_404_exactly_as_before() {
    let dir = temp_dir("no_frontend");
    let (_state, router) = build_app(&dir, base_config(dir.clone()));

    let resp = router.oneshot(get("/some/spa/route")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn with_frontend_dist_static_assets_are_served() {
    let dir = temp_dir("frontend_assets");
    let dist = temp_dir("frontend_assets_dist");
    std::fs::write(dist.join("index.html"), b"<html>shell</html>").unwrap();
    std::fs::create_dir_all(dist.join("assets")).unwrap();
    std::fs::write(dist.join("assets").join("app.js"), b"console.log(1);").unwrap();

    let config = Config {
        frontend_dist: Some(dist.clone()),
        backup_dir: None,
        ..base_config(dir.clone())
    };
    let (_state, router) = build_app(&dir, config);

    let resp = router.clone().oneshot(get("/assets/app.js")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&bytes[..], b"console.log(1);");
}

#[tokio::test]
async fn with_frontend_dist_unmatched_client_route_falls_back_to_index_html() {
    let dir = temp_dir("frontend_spa");
    let dist = temp_dir("frontend_spa_dist");
    std::fs::write(dist.join("index.html"), b"<html>shell</html>").unwrap();

    let config = Config {
        frontend_dist: Some(dist.clone()),
        backup_dir: None,
        ..base_config(dir.clone())
    };
    let (_state, router) = build_app(&dir, config);

    // "/sql" is a client-side React-Router path, not a real file and
    // not an API route -- must resolve to the SPA shell, not a 404,
    // so a browser refresh on a deep link works.
    let resp = router.clone().oneshot(get("/sql")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&bytes[..], b"<html>shell</html>");
}

#[tokio::test]
async fn frontend_fallback_never_shadows_a_real_protected_api_route() {
    let dir = temp_dir("frontend_shadow");
    let dist = temp_dir("frontend_shadow_dist");
    std::fs::write(dist.join("index.html"), b"<html>shell</html>").unwrap();

    let config = Config {
        frontend_dist: Some(dist.clone()),
        backup_dir: None,
        ..base_config(dir.clone())
    };
    let (_state, router) = build_app(&dir, config);

    // /v1/whoami is a real, defined, authenticated route -- it must
    // still require auth (401), never be captured by the static
    // fallback and served the SPA shell instead.
    let resp = router.clone().oneshot(get("/v1/whoami")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // And with a valid credential it must still return real JSON, not
    // the SPA shell.
    let authed = Request::builder()
        .method("GET")
        .uri("/v1/whoami")
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(authed).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp).await;
    assert_eq!(body["role"], "admin");
}

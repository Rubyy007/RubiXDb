//! Productization — Production Validation Phase, §4/§5/§16: authorization
//! matrix, malformed-input/security validation, rate-limiter integration,
//! and storage-pressure mapping, all against the **real** `LsmEngine`
//! through the real axum router via `tower::ServiceExt::oneshot` — the
//! same no-mocking discipline as `api_integration.rs`. Kept in its own
//! file rather than growing that one further, per this project's own
//! "keep commits/files focused" convention.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use http_body_util::BodyExt;
use rubixdb::execution::batch_coordinator::BatchCoordinatorConfig;
use rubixdb::lsm::{LsmConfig, LsmEngine};
use rubixdb::wal::{SyncMode, WalConfig};
use rubixdb_api::config::{ApiKeyConfig, Role};
use rubixdb_api::routes::build_router;
use rubixdb_api::{AppState, Config};
use serde_json::{json, Value};
use tower::ServiceExt;

const ADMIN_KEY: &str = "test-admin-key-0123456789ab";
const READER_KEY: &str = "test-reader-key-0123456789ab";
const OTHER_READER_KEY: &str = "test-other-reader-key-0123";

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("rubixdb_api_sec_{tag}_{nanos}"));
    std::fs::create_dir_all(&path).unwrap();
    path
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

/// Base config, mirroring `api_integration.rs`'s own `test_config` --
/// duplicated rather than shared, since each integration-test file is
/// its own compiled crate and this project prefers duplicated, readable
/// test setup over a shared `tests/common` module for this small amount
/// of code.
fn base_config(data_dir: PathBuf) -> Config {
    Config {
        data_dir,
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        api_keys: vec![
            ApiKeyConfig {
                name: "admin".to_string(),
                role: Role::Admin,
                key: ADMIN_KEY.to_string(),
            },
            ApiKeyConfig {
                name: "reader".to_string(),
                role: Role::Reader,
                key: READER_KEY.to_string(),
            },
            ApiKeyConfig {
                name: "other-reader".to_string(),
                role: Role::Reader,
                key: OTHER_READER_KEY.to_string(),
            },
        ],
        max_value_bytes: 1024 * 1024,
        max_key_bytes: 4096,
        default_range_limit: 100,
        max_range_limit: 10_000,
        shutdown_drain_secs: 5,
        rate_limit_rps: 10_000.0,
        rate_limit_burst: 10_000,
        compaction_auto_trigger: false,
        compaction_trigger_count: 4,
        cors_allowed_origins: vec!["http://localhost:5173".to_string()],
    }
}

fn build_app_with_config(dir: &std::path::Path, config: Config) -> (Arc<AppState>, Router) {
    let lsm_config = LsmConfig::default();
    let engine = LsmEngine::open(dir, wal_config(), pool_config(), lsm_config.clone()).unwrap();
    let state = Arc::new(AppState::new(engine, lsm_config, config));
    let router = build_router(state.clone());
    (state, router)
}

fn build_app(dir: &std::path::Path) -> (Arc<AppState>, Router) {
    build_app_with_config(dir, base_config(dir.to_path_buf()))
}

fn b64(s: &str) -> String {
    STANDARD.encode(s.as_bytes())
}

async fn json_body(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn raw_body(response: axum::response::Response) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&bytes).to_string()
}

fn req(method: &str, uri: &str, auth: Option<&str>, body: Option<Value>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(key) = auth {
        builder = builder.header("Authorization", format!("Bearer {key}"));
    }
    match body {
        Some(v) => builder
            .header("Content-Type", "application/json")
            .body(Body::from(serde_json::to_vec(&v).unwrap()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    }
}

fn raw_req(
    method: &str,
    uri: &str,
    auth: Option<&str>,
    content_type: &str,
    body: Vec<u8>,
) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("Content-Type", content_type);
    if let Some(key) = auth {
        builder = builder.header("Authorization", format!("Bearer {key}"));
    }
    builder.body(Body::from(body)).unwrap()
}

// --- §4: authorization matrix ---

#[tokio::test]
async fn reader_cannot_create_or_release_snapshots_admin_can() {
    let dir = temp_dir("snap_authz");
    let (state, router) = build_app(&dir);

    let denied = router
        .clone()
        .oneshot(req("POST", "/v1/snapshots", Some(READER_KEY), None))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    assert_eq!(json_body(denied).await["error"]["code"], "FORBIDDEN");

    let created = router
        .clone()
        .oneshot(req("POST", "/v1/snapshots", Some(ADMIN_KEY), None))
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::OK);
    let id = json_body(created).await["id"].as_str().unwrap().to_string();

    let denied_release = router
        .clone()
        .oneshot(req(
            "DELETE",
            &format!("/v1/snapshots/{id}"),
            Some(READER_KEY),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(denied_release.status(), StatusCode::FORBIDDEN);

    let released = router
        .clone()
        .oneshot(req(
            "DELETE",
            &format!("/v1/snapshots/{id}"),
            Some(ADMIN_KEY),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(released.status(), StatusCode::NO_CONTENT);

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn missing_and_invalid_credentials_are_rejected_without_leaking_anything() {
    let dir = temp_dir("bad_creds");
    let (state, router) = build_app(&dir);

    let no_header = router
        .clone()
        .oneshot(req("GET", "/v1/status", None, None))
        .await
        .unwrap();
    assert_eq!(no_header.status(), StatusCode::UNAUTHORIZED);
    let body = raw_body(no_header).await;
    assert!(!body.contains(ADMIN_KEY));
    assert!(!body
        .to_lowercase()
        .contains(&dir.display().to_string().to_lowercase()));

    let bad_key = router
        .clone()
        .oneshot(req(
            "GET",
            "/v1/status",
            Some("totally-invalid-not-a-real-key"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(bad_key.status(), StatusCode::UNAUTHORIZED);
    let body = raw_body(bad_key).await;
    assert!(!body.contains(ADMIN_KEY));
    assert!(!body.contains(READER_KEY));

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

// --- §5: malformed input / security ---

#[tokio::test]
async fn malformed_json_body_is_rejected_cleanly() {
    let dir = temp_dir("bad_json");
    let (state, router) = build_app(&dir);

    let resp = router
        .clone()
        .oneshot(raw_req(
            "PUT",
            "/v1/kv",
            Some(ADMIN_KEY),
            "application/json",
            b"{ this is not valid json ".to_vec(),
        ))
        .await
        .unwrap();
    assert!(resp.status().is_client_error(), "got {}", resp.status());
    let body = raw_body(resp).await;
    assert!(!body
        .to_lowercase()
        .contains(&dir.display().to_string().to_lowercase()));

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn malformed_range_parameters_are_rejected() {
    let dir = temp_dir("bad_range");
    let (state, router) = build_app(&dir);

    // start_b64 is not valid base64.
    let resp = router
        .clone()
        .oneshot(req(
            "GET",
            "/v1/range?start_b64=not!!valid!!base64",
            Some(READER_KEY),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json_body(resp).await["error"]["code"], "VALIDATION_ERROR");

    // limit above max_range_limit (10_000 in this test config).
    let resp = router
        .clone()
        .oneshot(req("GET", "/v1/range?limit=10001", Some(READER_KEY), None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json_body(resp).await["error"]["code"], "VALIDATION_ERROR");

    // limit is not a valid non-negative integer -- axum's Query
    // extractor itself rejects this before the handler runs.
    let resp = router
        .clone()
        .oneshot(req("GET", "/v1/range?limit=-5", Some(READER_KEY), None))
        .await
        .unwrap();
    assert!(resp.status().is_client_error(), "got {}", resp.status());

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn malformed_snapshot_id_is_rejected_not_500() {
    let dir = temp_dir("bad_snap_id");
    let (state, router) = build_app(&dir);

    let resp = router
        .clone()
        .oneshot(req(
            "GET",
            "/v1/snapshots/not-a-real-uuid",
            Some(READER_KEY),
            None,
        ))
        .await
        .unwrap();
    assert!(resp.status().is_client_error(), "got {}", resp.status());

    // A well-formed but unknown UUID must 404, not 500.
    let resp = router
        .clone()
        .oneshot(req(
            "GET",
            "/v1/snapshots/00000000-0000-0000-0000-000000000000",
            Some(READER_KEY),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn oversized_key_is_rejected_by_the_handler_not_the_framework() {
    let dir = temp_dir("bad_key_size");
    let (state, router) = build_app(&dir);
    let big_key = "k".repeat(4096 + 1);

    let resp = router
        .clone()
        .oneshot(req(
            "PUT",
            "/v1/kv",
            Some(ADMIN_KEY),
            Some(json!({"key_b64": STANDARD.encode(big_key.as_bytes()), "value_b64": b64("v")})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json_body(resp).await["error"]["code"], "VALIDATION_ERROR");

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn payload_beyond_the_configured_body_size_limit_is_rejected() {
    let dir = temp_dir("body_limit");
    let (state, router) = build_app(&dir);
    // base_config: max_value_bytes=1MiB, max_key_bytes=4096.
    // body_size_limit = (1MiB + 4096) * 4/3 + 4096 (~1.4MiB). This
    // payload is well beyond that, so it must trip axum's own
    // DefaultBodyLimit before the handler's validation ever runs.
    let huge_value = "x".repeat(4 * 1024 * 1024);
    let resp = router
        .clone()
        .oneshot(req(
            "PUT",
            "/v1/kv",
            Some(ADMIN_KEY),
            Some(json!({"key_b64": b64("k"), "value_b64": STANDARD.encode(huge_value.as_bytes())})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn unknown_routes_404_without_requiring_auth() {
    let dir = temp_dir("unknown_route");
    let (state, router) = build_app(&dir);

    let resp = router
        .clone()
        .oneshot(req("GET", "/v1/totally-not-a-real-route", None, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn method_mismatch_on_a_real_route_is_405() {
    let dir = temp_dir("method_mismatch");
    let (state, router) = build_app(&dir);

    // /v1/kv only registers PUT.
    let resp = router
        .clone()
        .oneshot(req("PATCH", "/v1/kv", Some(ADMIN_KEY), None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

// --- §16: rate limiter, driven through the real middleware end to end ---

#[tokio::test]
async fn rate_limiter_returns_429_after_burst_and_isolates_principals() {
    let dir = temp_dir("rate_limit");
    let mut config = base_config(dir.clone());
    // A small, deterministic burst so the test doesn't depend on
    // real wall-clock refill timing to observe the 429 itself.
    config.rate_limit_rps = 2.0;
    config.rate_limit_burst = 3;
    let (state, router) = build_app_with_config(&dir, config);

    let mut saw_429 = false;
    for _ in 0..10 {
        let resp = router
            .clone()
            .oneshot(req("GET", "/v1/status", Some(READER_KEY), None))
            .await
            .unwrap();
        if resp.status() == StatusCode::TOO_MANY_REQUESTS {
            saw_429 = true;
            assert_eq!(json_body(resp).await["error"]["code"], "RATE_LIMITED");
            break;
        }
        assert_eq!(resp.status(), StatusCode::OK);
    }
    assert!(
        saw_429,
        "expected the reader principal to be rate-limited within 10 rapid requests \
         (burst=3, rps=2.0)"
    );

    // A different principal (own token bucket) must be unaffected by
    // the first principal's exhausted bucket -- isolation, not a
    // shared/global limit.
    let other = router
        .clone()
        .oneshot(req("GET", "/v1/status", Some(OTHER_READER_KEY), None))
        .await
        .unwrap();
    assert_eq!(other.status(), StatusCode::OK);

    // After the refill window, the original principal recovers.
    tokio::time::sleep(Duration::from_millis(1600)).await;
    let recovered = router
        .clone()
        .oneshot(req("GET", "/v1/status", Some(READER_KEY), None))
        .await
        .unwrap();
    assert_eq!(recovered.status(), StatusCode::OK);

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

// --- storage-pressure mapping (StorageFull's sibling state) ---

#[tokio::test]
async fn storage_pressure_is_a_signal_not_a_write_rejection() {
    let dir = temp_dir("storage_pressure");
    let (state, router) = build_app(&dir);

    state
        .engine
        .set_storage_state_for_test(rubixdb::lsm::StorageState::StoragePressure);

    // Per the accepted CapacityExceeded/backpressure contract, pressure
    // is a freeze/backpressure *signal*, not a write rejection: a write
    // must still succeed while the state is StoragePressure.
    let resp = router
        .clone()
        .oneshot(req(
            "PUT",
            "/v1/kv",
            Some(ADMIN_KEY),
            Some(json!({"key_b64": b64("k"), "value_b64": b64("v")})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let status = router
        .clone()
        .oneshot(req("GET", "/v1/status", Some(READER_KEY), None))
        .await
        .unwrap();
    assert_eq!(json_body(status).await["storage_state"], "StoragePressure");

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

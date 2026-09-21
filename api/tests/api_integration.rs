//! Phase 8/9 integration tests — `PHASE_API_ARCHITECTURE.md`. Every
//! test in this file drives the **real** `LsmEngine` (real WAL, real
//! SSTables, real on-disk directory) through the real axum router via
//! `tower::ServiceExt::oneshot` — nothing here mocks the engine. The
//! one exception, `set_storage_state_for_test`, is the engine's own
//! established test-only hook (`test-util` feature, the same one its
//! own certified test suite uses), not a mock of engine behavior.

use std::path::{Path, PathBuf};
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

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("rubixdb_api_it_{tag}_{nanos}"));
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

fn test_config(data_dir: PathBuf) -> Config {
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
    }
}

/// Opens a real engine against `dir` and builds the real router --
/// used both for the initial app and, in the restart test, for the
/// post-restart app against the same directory.
fn build_app(dir: &Path) -> (Arc<AppState>, Router) {
    let lsm_config = LsmConfig::default();
    let engine = LsmEngine::open(dir, wal_config(), pool_config(), lsm_config.clone()).unwrap();
    let state = Arc::new(AppState::new(
        engine,
        lsm_config,
        test_config(dir.to_path_buf()),
    ));
    let router = build_router(state.clone());
    (state, router)
}

fn b64(s: &str) -> String {
    STANDARD.encode(s.as_bytes())
}

async fn json_body(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
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

// --- health / readiness ---

#[tokio::test]
async fn healthz_requires_no_auth() {
    let dir = temp_dir("health");
    let (state, router) = build_app(&dir);
    let resp = router
        .oneshot(req("GET", "/healthz", None, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn readyz_requires_auth_and_reports_storage_state() {
    let dir = temp_dir("ready");
    let (state, router) = build_app(&dir);

    let unauth = router
        .clone()
        .oneshot(req("GET", "/readyz", None, None))
        .await
        .unwrap();
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

    let ok = router
        .oneshot(req("GET", "/readyz", Some(READER_KEY), None))
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);
    let body = json_body(ok).await;
    assert_eq!(body["ready"], true);
    assert_eq!(body["storage_state"], "Healthy");

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

// --- core kv flow ---

#[tokio::test]
async fn put_get_delete_flow_against_the_real_engine() {
    let dir = temp_dir("kv_flow");
    let (state, router) = build_app(&dir);

    let put_resp = router
        .clone()
        .oneshot(req(
            "PUT",
            "/v1/kv",
            Some(ADMIN_KEY),
            Some(json!({"key_b64": b64("alpha"), "value_b64": b64("one")})),
        ))
        .await
        .unwrap();
    assert_eq!(put_resp.status(), StatusCode::OK);
    let put_body = json_body(put_resp).await;
    let put_seq = put_body["seq"].as_u64().unwrap();
    assert!(put_seq > 0);

    let get_resp = router
        .clone()
        .oneshot(req(
            "GET",
            &format!("/v1/kv/{}", b64("alpha")),
            Some(READER_KEY),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(get_resp.status(), StatusCode::OK);
    let get_body = json_body(get_resp).await;
    assert_eq!(get_body["value_b64"], b64("one"));

    // Reader may not write.
    let forbidden = router
        .clone()
        .oneshot(req(
            "DELETE",
            &format!("/v1/kv/{}", b64("alpha")),
            Some(READER_KEY),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);

    let delete_resp = router
        .clone()
        .oneshot(req(
            "DELETE",
            &format!("/v1/kv/{}", b64("alpha")),
            Some(ADMIN_KEY),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(delete_resp.status(), StatusCode::OK);
    let delete_seq = json_body(delete_resp).await["seq"].as_u64().unwrap();
    assert!(delete_seq > put_seq);

    let missing = router
        .clone()
        .oneshot(req(
            "GET",
            &format!("/v1/kv/{}", b64("alpha")),
            Some(READER_KEY),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    let err_body = json_body(missing).await;
    assert_eq!(err_body["error"]["code"], "NOT_FOUND");

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn get_as_of_returns_the_historical_value_after_a_later_delete() {
    let dir = temp_dir("get_as_of");
    let (state, router) = build_app(&dir);

    let put_resp = router
        .clone()
        .oneshot(req(
            "PUT",
            "/v1/kv",
            Some(ADMIN_KEY),
            Some(json!({"key_b64": b64("k"), "value_b64": b64("v1")})),
        ))
        .await
        .unwrap();
    let seq_after_put = json_body(put_resp).await["seq"].as_u64().unwrap();

    router
        .clone()
        .oneshot(req(
            "DELETE",
            &format!("/v1/kv/{}", b64("k")),
            Some(ADMIN_KEY),
            None,
        ))
        .await
        .unwrap();

    // "Now": deleted.
    let now = router
        .clone()
        .oneshot(req(
            "GET",
            &format!("/v1/kv/{}", b64("k")),
            Some(READER_KEY),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(now.status(), StatusCode::NOT_FOUND);

    // Historical: still visible at the seq right after the put.
    let historical = router
        .clone()
        .oneshot(req(
            "GET",
            &format!("/v1/kv/{}?as_of_seq={seq_after_put}", b64("k")),
            Some(READER_KEY),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(historical.status(), StatusCode::OK);
    assert_eq!(json_body(historical).await["value_b64"], b64("v1"));

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn exists_reflects_contains_semantics() {
    let dir = temp_dir("exists");
    let (state, router) = build_app(&dir);

    router
        .clone()
        .oneshot(req(
            "PUT",
            "/v1/kv",
            Some(ADMIN_KEY),
            Some(json!({"key_b64": b64("e"), "value_b64": b64("v")})),
        ))
        .await
        .unwrap();

    let exists = router
        .clone()
        .oneshot(req(
            "GET",
            &format!("/v1/kv/{}/exists", b64("e")),
            Some(READER_KEY),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(json_body(exists).await["exists"], true);

    let not_exists = router
        .clone()
        .oneshot(req(
            "GET",
            &format!("/v1/kv/{}/exists", b64("nope")),
            Some(READER_KEY),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(json_body(not_exists).await["exists"], false);

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn range_preserves_ordering_and_reports_truncation() {
    let dir = temp_dir("range");
    let (state, router) = build_app(&dir);

    for i in 0..10u32 {
        let k = format!("k{i:03}");
        router
            .clone()
            .oneshot(req(
                "PUT",
                "/v1/kv",
                Some(ADMIN_KEY),
                Some(json!({"key_b64": b64(&k), "value_b64": b64(&format!("v{i}"))})),
            ))
            .await
            .unwrap();
    }

    let full = router
        .clone()
        .oneshot(req("GET", "/v1/range", Some(READER_KEY), None))
        .await
        .unwrap();
    let full_body = json_body(full).await;
    let rows = full_body["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 10);
    assert_eq!(full_body["truncated"], false);
    let keys: Vec<String> = rows
        .iter()
        .map(|r| r["key_b64"].as_str().unwrap().to_string())
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted, "range results must preserve key ordering");

    let truncated = router
        .clone()
        .oneshot(req("GET", "/v1/range?limit=3", Some(READER_KEY), None))
        .await
        .unwrap();
    let truncated_body = json_body(truncated).await;
    assert_eq!(truncated_body["rows"].as_array().unwrap().len(), 3);
    assert_eq!(truncated_body["truncated"], true);

    let bad_limit = router
        .clone()
        .oneshot(req("GET", "/v1/range?limit=0", Some(READER_KEY), None))
        .await
        .unwrap();
    assert_eq!(bad_limit.status(), StatusCode::BAD_REQUEST);

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

// --- snapshots ---

#[tokio::test]
async fn snapshot_lifecycle_protects_historical_reads() {
    let dir = temp_dir("snapshot");
    let (state, router) = build_app(&dir);

    router
        .clone()
        .oneshot(req(
            "PUT",
            "/v1/kv",
            Some(ADMIN_KEY),
            Some(json!({"key_b64": b64("s"), "value_b64": b64("before")})),
        ))
        .await
        .unwrap();

    let create = router
        .clone()
        .oneshot(req("POST", "/v1/snapshots", Some(ADMIN_KEY), None))
        .await
        .unwrap();
    assert_eq!(create.status(), StatusCode::OK);
    let snap_body = json_body(create).await;
    let snap_id = snap_body["id"].as_str().unwrap().to_string();
    let snap_seq = snap_body["seq"].as_u64().unwrap();

    router
        .clone()
        .oneshot(req(
            "PUT",
            "/v1/kv",
            Some(ADMIN_KEY),
            Some(json!({"key_b64": b64("s"), "value_b64": b64("after")})),
        ))
        .await
        .unwrap();

    let get_now = router
        .clone()
        .oneshot(req(
            "GET",
            &format!("/v1/kv/{}", b64("s")),
            Some(READER_KEY),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(json_body(get_now).await["value_b64"], b64("after"));

    let get_snap = router
        .clone()
        .oneshot(req(
            "GET",
            &format!("/v1/kv/{}?as_of_seq={snap_seq}", b64("s")),
            Some(READER_KEY),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(json_body(get_snap).await["value_b64"], b64("before"));

    let list = router
        .clone()
        .oneshot(req("GET", "/v1/snapshots", Some(READER_KEY), None))
        .await
        .unwrap();
    assert_eq!(json_body(list).await.as_array().unwrap().len(), 1);

    let release = router
        .clone()
        .oneshot(req(
            "DELETE",
            &format!("/v1/snapshots/{snap_id}"),
            Some(ADMIN_KEY),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(release.status(), StatusCode::NO_CONTENT);

    let list_after = router
        .clone()
        .oneshot(req("GET", "/v1/snapshots", Some(READER_KEY), None))
        .await
        .unwrap();
    assert_eq!(json_body(list_after).await.as_array().unwrap().len(), 0);

    let double_release = router
        .clone()
        .oneshot(req(
            "DELETE",
            &format!("/v1/snapshots/{snap_id}"),
            Some(ADMIN_KEY),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(double_release.status(), StatusCode::NOT_FOUND);

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

// --- compaction (read-only) ---

#[tokio::test]
async fn compaction_status_and_metrics_reflect_the_real_engine() {
    let dir = temp_dir("compaction");
    let (state, router) = build_app(&dir);

    let status = router
        .clone()
        .oneshot(req("GET", "/v1/compaction/status", Some(READER_KEY), None))
        .await
        .unwrap();
    let status_body = json_body(status).await;
    assert_eq!(status_body["auto_trigger_enabled"], false);
    assert_eq!(status_body["trigger_count"], 4);
    assert_eq!(status_body["cycles_completed"], 0);

    let metrics = router
        .clone()
        .oneshot(req("GET", "/v1/compaction/metrics", Some(READER_KEY), None))
        .await
        .unwrap();
    let metrics_body = json_body(metrics).await;
    assert_eq!(metrics_body["cycles_completed"], 0);
    assert!(metrics_body["last_cycle"].is_null());

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

// --- validation / error mapping ---

#[tokio::test]
async fn malformed_base64_is_rejected_before_touching_the_engine() {
    let dir = temp_dir("bad_base64");
    let (state, router) = build_app(&dir);

    let resp = router
        .clone()
        .oneshot(req(
            "GET",
            "/v1/kv/not-valid-base64!!!",
            Some(READER_KEY),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json_body(resp).await["error"]["code"], "VALIDATION_ERROR");

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn empty_key_on_put_is_rejected() {
    let dir = temp_dir("empty_key");
    let (state, router) = build_app(&dir);

    let resp = router
        .clone()
        .oneshot(req(
            "PUT",
            "/v1/kv",
            Some(ADMIN_KEY),
            Some(json!({"key_b64": "", "value_b64": b64("v")})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn oversized_value_is_rejected() {
    let dir = temp_dir("oversized");
    let (state, router) = build_app(&dir);
    // Above `max_value_bytes` (1 MiB, `test_config`'s own default) but
    // comfortably under axum's own outer `Json` extractor body-size
    // limit, so this exercises *this handler's own* explicit check,
    // not the framework's independent, coarser one (a real, useful
    // defense-in-depth finding from this test's own first version,
    // which picked a value large enough to trip both at once and so
    // never actually reached this handler's own validation logic).
    let big_value = "x".repeat(1024 * 1024 + 1);

    let resp = router
        .clone()
        .oneshot(req(
            "PUT",
            "/v1/kv",
            Some(ADMIN_KEY),
            Some(json!({"key_b64": b64("k"), "value_b64": STANDARD.encode(big_value.as_bytes())})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json_body(resp).await["error"]["code"], "VALIDATION_ERROR");

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

// --- storage-pressure / storage-full mapping (real engine, test-only hook) ---

#[tokio::test]
async fn storage_full_maps_to_507_and_never_corrupts_state() {
    let dir = temp_dir("storage_full");
    let (state, router) = build_app(&dir);

    state
        .engine
        .set_storage_state_for_test(rubixdb::lsm::StorageState::StorageFull);

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
    assert_eq!(resp.status(), StatusCode::INSUFFICIENT_STORAGE);
    assert_eq!(json_body(resp).await["error"]["code"], "STORAGE_EXHAUSTED");

    // Reads must still work under StorageFull -- only writes are
    // rejected (`ADR-WE-SP-001`).
    let status = router
        .clone()
        .oneshot(req("GET", "/v1/status", Some(READER_KEY), None))
        .await
        .unwrap();
    assert_eq!(json_body(status).await["storage_state"], "StorageFull");

    state
        .engine
        .set_storage_state_for_test(rubixdb::lsm::StorageState::Healthy);
    let recovered = router
        .clone()
        .oneshot(req(
            "PUT",
            "/v1/kv",
            Some(ADMIN_KEY),
            Some(json!({"key_b64": b64("k"), "value_b64": b64("v")})),
        ))
        .await
        .unwrap();
    assert_eq!(recovered.status(), StatusCode::OK);

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

// --- Phase 9: persistence across a real restart ---

#[tokio::test]
async fn persists_across_a_real_restart_get_get_as_of_contains_and_range() {
    let dir = temp_dir("restart");

    // --- run 1: write data, take a snapshot-worthy seq, shut down ---
    let (state1, router1) = build_app(&dir);

    let mut expected: Vec<(String, String)> = Vec::new();
    for i in 0..20u32 {
        let k = format!("r{i:03}");
        let v = format!("v{i}");
        let resp = router1
            .clone()
            .oneshot(req(
                "PUT",
                "/v1/kv",
                Some(ADMIN_KEY),
                Some(json!({"key_b64": b64(&k), "value_b64": b64(&v)})),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        expected.push((k, v));
    }
    // A key that's deleted before restart -- must stay absent after.
    router1
        .clone()
        .oneshot(req(
            "PUT",
            "/v1/kv",
            Some(ADMIN_KEY),
            Some(json!({"key_b64": b64("deleted"), "value_b64": b64("gone-soon")})),
        ))
        .await
        .unwrap();
    let del_seq = {
        let resp = router1
            .clone()
            .oneshot(req(
                "DELETE",
                &format!("/v1/kv/{}", b64("deleted")),
                Some(ADMIN_KEY),
                None,
            ))
            .await
            .unwrap();
        json_body(resp).await["seq"].as_u64().unwrap()
    };
    let _ = del_seq;

    // A key with an earlier version still reachable via `as_of_seq`,
    // matching `PHASE_API_ARCHITECTURE.md` §2.1's own explicit claim
    // that historical reads survive a restart even though the
    // service's in-process `Snapshot` registry (and this test's own
    // held-snapshot map) does not.
    let seq_v1 = {
        let resp = router1
            .clone()
            .oneshot(req(
                "PUT",
                "/v1/kv",
                Some(ADMIN_KEY),
                Some(json!({"key_b64": b64("versioned"), "value_b64": b64("v1")})),
            ))
            .await
            .unwrap();
        json_body(resp).await["seq"].as_u64().unwrap()
    };
    router1
        .clone()
        .oneshot(req(
            "PUT",
            "/v1/kv",
            Some(ADMIN_KEY),
            Some(json!({"key_b64": b64("versioned"), "value_b64": b64("v2")})),
        ))
        .await
        .unwrap();

    // Real shutdown, then drop the engine to release the directory's
    // exclusive lock before reopening.
    state1.engine.shutdown();
    drop(router1);
    let state1 = Arc::try_unwrap(state1).unwrap_or_else(|_| panic!("outstanding Arc<AppState>"));
    drop(state1);

    // --- run 2: reopen fresh against the same directory ---
    let (state2, router2) = build_app(&dir);

    for (k, v) in &expected {
        let resp = router2
            .clone()
            .oneshot(req(
                "GET",
                &format!("/v1/kv/{}", b64(k)),
                Some(READER_KEY),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "key {k} must survive restart"
        );
        assert_eq!(json_body(resp).await["value_b64"], b64(v));
    }

    let deleted_resp = router2
        .clone()
        .oneshot(req(
            "GET",
            &format!("/v1/kv/{}", b64("deleted")),
            Some(READER_KEY),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(
        deleted_resp.status(),
        StatusCode::NOT_FOUND,
        "delete must survive restart"
    );

    let contains_deleted = router2
        .clone()
        .oneshot(req(
            "GET",
            &format!("/v1/kv/{}/exists", b64("deleted")),
            Some(READER_KEY),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(json_body(contains_deleted).await["exists"], false);

    // Historical read at the pre-restart seq still resolves correctly.
    let historical = router2
        .clone()
        .oneshot(req(
            "GET",
            &format!("/v1/kv/{}?as_of_seq={seq_v1}", b64("versioned")),
            Some(READER_KEY),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(json_body(historical).await["value_b64"], b64("v1"));
    let now = router2
        .clone()
        .oneshot(req(
            "GET",
            &format!("/v1/kv/{}", b64("versioned")),
            Some(READER_KEY),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(json_body(now).await["value_b64"], b64("v2"));

    // Range over the whole persisted keyspace: 20 surviving `r*` keys
    // + 1 `versioned` key (`deleted` must be absent).
    let range_resp = router2
        .clone()
        .oneshot(req("GET", "/v1/range?limit=1000", Some(READER_KEY), None))
        .await
        .unwrap();
    let range_body = json_body(range_resp).await;
    let rows = range_body["rows"].as_array().unwrap();
    assert_eq!(
        rows.len(),
        21,
        "range after restart must reflect exactly the persisted, non-deleted keys"
    );
    let keys: std::collections::HashSet<String> = rows
        .iter()
        .map(|r| r["key_b64"].as_str().unwrap().to_string())
        .collect();
    assert!(!keys.contains(&b64("deleted")));
    assert!(keys.contains(&b64("versioned")));

    // Snapshot creation/inspection also work correctly post-restart
    // (a fresh `SnapshotRegistry`, as documented -- not a leftover
    // from run 1).
    let snap_list = router2
        .clone()
        .oneshot(req("GET", "/v1/snapshots", Some(READER_KEY), None))
        .await
        .unwrap();
    assert_eq!(json_body(snap_list).await.as_array().unwrap().len(), 0);
    let new_snap = router2
        .clone()
        .oneshot(req("POST", "/v1/snapshots", Some(ADMIN_KEY), None))
        .await
        .unwrap();
    assert_eq!(new_snap.status(), StatusCode::OK);

    state2.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

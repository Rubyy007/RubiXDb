//! `/v1/admin/*` operator endpoints — real engine, real router, real SQL
//! pipeline (`tower::ServiceExt::oneshot`), the same no-mocking discipline as
//! the other API suites.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
use serde_json::{json, Value};
use tower::ServiceExt;

const ADMIN_KEY: &str = "test-admin-key-0123456789ab";
const READER_KEY: &str = "test-reader-key-0123456789ab";

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("rubixdb_admin_it_{tag}_{nanos}"));
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

fn test_config(data_dir: PathBuf, backup_dir: Option<PathBuf>) -> Config {
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
        rate_limit_rps: 1_000_000.0,
        rate_limit_burst: 1_000_000,
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
        backup_dir,
    }
}

struct App {
    dir: PathBuf,
    backups: PathBuf,
    state: Arc<AppState>,
    router: Router,
}

fn build_app(dir: &Path, with_backups: bool) -> App {
    let backups = dir.join("backups");
    let data = dir.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let lsm_config = LsmConfig::default();
    let engine =
        Arc::new(LsmEngine::open(&data, wal_config(), pool_config(), lsm_config.clone()).unwrap());
    let state = Arc::new(AppState::new(
        engine,
        lsm_config,
        test_config(data, with_backups.then(|| backups.clone())),
    ));
    let router = build_router(state.clone());
    App {
        dir: dir.to_path_buf(),
        backups,
        state,
        router,
    }
}

async fn call(
    router: &Router,
    method: &str,
    uri: &str,
    key: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value, String) {
    let mut b = Request::builder().method(method).uri(uri);
    if let Some(k) = key {
        b = b.header("Authorization", format!("Bearer {k}"));
    }
    let req = if let Some(body) = body {
        b.header("Content-Type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    } else {
        b.body(Body::empty()).unwrap()
    };
    let response = router.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes).to_string();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json, text)
}

async fn sql(router: &Router, stmt: &str) -> Value {
    let (s, v, t) = call(
        router,
        "POST",
        "/v1/sql",
        Some(ADMIN_KEY),
        Some(json!({"sql": stmt})),
    )
    .await;
    assert!(s.is_success(), "{stmt:?} -> {s}: {t}");
    v
}

async fn seed(router: &Router) {
    sql(
        router,
        "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT, city TEXT)",
    )
    .await;
    for i in 0..60 {
        sql(
            router,
            &format!(
                "INSERT INTO people (id, name, city) VALUES ({i}, 'name-{i}', 'city-{}')",
                i % 7
            ),
        )
        .await;
    }
    sql(router, "CREATE INDEX people_city ON people (city)").await;
}

// ---------------------------------------------------------------------

#[tokio::test]
async fn every_admin_route_requires_the_admin_role_for_every_method() {
    let app = build_app(&temp_dir("roles"), true);
    let routes = [
        ("GET", "/v1/admin/status"),
        ("GET", "/v1/admin/backups"),
        ("POST", "/v1/admin/backups"),
        ("POST", "/v1/admin/backups/x/verify"),
        ("DELETE", "/v1/admin/backups/x?confirm=x"),
        ("POST", "/v1/admin/check"),
        ("GET", "/v1/admin/storage"),
        ("POST", "/v1/admin/maintenance/purge-orphans"),
    ];
    for (m, u) in routes {
        let body = (m == "POST").then(|| json!({"name": "x", "apply": false}));
        let (s, _, _) = call(&app.router, m, u, Some(READER_KEY), body.clone()).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "reader must be refused: {m} {u}");
        let (s, _, _) = call(&app.router, m, u, None, body).await;
        assert_eq!(
            s,
            StatusCode::UNAUTHORIZED,
            "no key must be refused: {m} {u}"
        );
    }
}

#[tokio::test]
async fn backup_names_are_validated_and_errors_never_leak_paths() {
    let app = build_app(&temp_dir("names"), true);
    let dir_str = app.dir.to_string_lossy().to_string();
    for bad in [
        "../x",
        "a/b",
        "a\\b",
        "C:\\Windows\\x",
        "con",
        "",
        ".hidden",
        "x.",
        &"y".repeat(65),
        "a b",
        "%2e%2e",
    ] {
        let (s, v, text) = call(
            &app.router,
            "POST",
            "/v1/admin/backups",
            Some(ADMIN_KEY),
            Some(json!({"name": bad})),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad:?}: {text}");
        assert_eq!(v["error"]["code"], "VALIDATION_ERROR");
        assert!(!text.contains(&dir_str), "no path in {text}");
    }
    assert!(
        !app.backups.exists() || std::fs::read_dir(&app.backups).unwrap().next().is_none(),
        "nothing was created for a rejected name"
    );

    let (s, _, _) = call(
        &app.router,
        "POST",
        "/v1/admin/backups",
        Some(ADMIN_KEY),
        Some(json!({"name": "ok-1"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, v, text) = call(
        &app.router,
        "POST",
        "/v1/admin/backups",
        Some(ADMIN_KEY),
        Some(json!({"name": "ok-1"})),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(v["error"]["code"], "DESTINATION_EXISTS");
    assert!(!text.contains(&dir_str), "no path in {text}");
}

#[tokio::test]
async fn without_a_backup_directory_the_endpoints_are_501() {
    let app = build_app(&temp_dir("noconf"), false);
    let (s, v, _) = call(
        &app.router,
        "POST",
        "/v1/admin/backups",
        Some(ADMIN_KEY),
        Some(json!({"name": "a"})),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_IMPLEMENTED);
    assert_eq!(v["error"]["code"], "NOT_CONFIGURED");
    let (s, _, _) = call(
        &app.router,
        "GET",
        "/v1/admin/backups",
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_IMPLEMENTED);
}

#[tokio::test]
async fn backup_list_verify_restore_round_trip_through_the_api() {
    let app = build_app(&temp_dir("roundtrip"), true);
    seed(&app.router).await;
    let (s, created, t) = call(
        &app.router,
        "POST",
        "/v1/admin/backups",
        Some(ADMIN_KEY),
        Some(json!({"name": "nightly"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{t}");
    assert!(created["entries"].as_u64().unwrap() > 100);

    let (_, list, _) = call(
        &app.router,
        "GET",
        "/v1/admin/backups",
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(list["backups"][0]["name"], "nightly");

    let (s, v, t) = call(
        &app.router,
        "POST",
        "/v1/admin/backups/nightly/verify",
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{t}");
    assert_eq!(v["ok"], true);
    assert_eq!(v["catalog"]["tables"], 1);
    assert_eq!(v["tables"][0]["rows"], 60);

    // Restore into a fresh directory and serve it through a second app.
    let dest_root = temp_dir("roundtrip_restored");
    let dest = dest_root.join("data");
    rubixdb::ops::restore::restore_backup(
        &app.backups.join("nightly.rbxbackup"),
        &dest,
        &Default::default(),
    )
    .unwrap();
    let lsm = LsmConfig::default();
    let engine =
        Arc::new(LsmEngine::open(&dest, wal_config(), pool_config(), lsm.clone()).unwrap());
    let state2 = Arc::new(AppState::new(engine, lsm, test_config(dest, None)));
    let router2 = build_router(state2);
    let q = sql(&router2, "SELECT COUNT(*) FROM people").await;
    assert_eq!(q["result"]["rows"][0][0]["value"], "60");
    let by_city = sql(
        &router2,
        "SELECT id FROM people WHERE city = 'city-3' ORDER BY id",
    )
    .await;
    let expected: Vec<i64> = (0..60).filter(|i| i % 7 == 3).collect();
    let got: Vec<i64> = by_city["result"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            let v = &r[0]["value"];
            v.as_i64()
                .unwrap_or_else(|| v.as_str().unwrap().parse().unwrap())
        })
        .collect();
    assert_eq!(
        got, expected,
        "index-served read on the restored database must equal the model"
    );

    // Verify of an unknown name is 404, a corrupted file is 422 with a classification.
    let (s, _, _) = call(
        &app.router,
        "POST",
        "/v1/admin/backups/ghost/verify",
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let path = app.backups.join("nightly.rbxbackup");
    let mut bytes = std::fs::read(&path).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xFF;
    std::fs::write(&path, bytes).unwrap();
    let (s, v, text) = call(
        &app.router,
        "POST",
        "/v1/admin/backups/nightly/verify",
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{text}");
    assert!(
        v["error"]["code"].as_str().unwrap().starts_with("BACKUP_"),
        "{text}"
    );
    assert!(!text.contains(&app.dir.to_string_lossy().to_string()));
}

#[tokio::test]
async fn deleting_a_backup_needs_the_exact_name_and_is_otherwise_refused() {
    let app = build_app(&temp_dir("delete"), true);
    let (s, _, _) = call(
        &app.router,
        "POST",
        "/v1/admin/backups",
        Some(ADMIN_KEY),
        Some(json!({"name": "keepme"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let file = app.backups.join("keepme.rbxbackup");
    for uri in [
        "/v1/admin/backups/keepme",
        "/v1/admin/backups/keepme?confirm=",
        "/v1/admin/backups/keepme?confirm=keepm",
        "/v1/admin/backups/keepme?confirm=KEEPME",
        "/v1/admin/backups/keepme?confirm=other",
    ] {
        let (s, v, _) = call(&app.router, "DELETE", uri, Some(ADMIN_KEY), None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{uri}");
        assert_eq!(v["error"]["code"], "CONFIRMATION_REQUIRED");
        assert!(
            file.exists(),
            "refused delete must not remove the file ({uri})"
        );
    }
    let (s, _, _) = call(
        &app.router,
        "DELETE",
        "/v1/admin/backups/keepme?confirm=keepme",
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(!file.exists());
    let (s, _, _) = call(
        &app.router,
        "DELETE",
        "/v1/admin/backups/keepme?confirm=keepme",
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // A traversal attempt through the path segment never reaches the filesystem.
    let (s, _, _) = call(
        &app.router,
        "DELETE",
        "/v1/admin/backups/..%2f..%2fdata?confirm=..%2f..%2fdata",
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert!(
        s == StatusCode::BAD_REQUEST || s == StatusCode::NOT_FOUND,
        "{s}"
    );
    assert!(app.dir.join("data").exists());
}

#[tokio::test]
async fn check_endpoint_is_clean_then_reports_an_injected_fault() {
    let app = build_app(&temp_dir("check"), true);
    seed(&app.router).await;
    let (s, v, t) = call(
        &app.router,
        "POST",
        "/v1/admin/check",
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{t}");
    assert_eq!(v["clean"], true, "{t}");
    assert_eq!(v["errors"], 0);
    assert!(v["stats"]["rows_checked"].as_u64().unwrap() >= 60);

    // Delete one index entry behind the SQL layer's back.
    let (start, end) = {
        // table id 1.. : find the index range by scanning the catalog through the check's own object ids.
        let (_, tables, _) = call(
            &app.router,
            "GET",
            "/v1/catalog/indexes",
            Some(ADMIN_KEY),
            None,
        )
        .await;
        let idx = tables
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["name"] == "people_city")
            .unwrap()
            .clone();
        let tid = idx["table_id"].as_u64().unwrap() as u32;
        let iid = idx["index_id"].as_u64().unwrap() as u32;
        rubixdb::relational::index_key::index_entry_range(tid, iid)
    };
    let (k, _) = app
        .state
        .engine
        .range(as_bound(&start), as_bound(&end))
        .next()
        .unwrap()
        .unwrap();
    app.state.engine.delete(&k).unwrap();
    let (s, v, t) = call(
        &app.router,
        "POST",
        "/v1/admin/check",
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{t}");
    assert_eq!(v["clean"], false);
    assert!(v["errors"].as_u64().unwrap() >= 1);
    assert!(
        v["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["code"] == "INDEX_ENTRY_MISSING"),
        "{t}"
    );
}

#[tokio::test]
async fn purge_orphans_is_a_dry_run_until_the_exact_count_is_confirmed() {
    let app = build_app(&temp_dir("purge"), true);
    sql(
        &app.router,
        "CREATE TABLE doomed (id INTEGER PRIMARY KEY, v TEXT)",
    )
    .await;
    for i in 0..40 {
        sql(
            &app.router,
            &format!("INSERT INTO doomed (id, v) VALUES ({i}, 'x')"),
        )
        .await;
    }
    sql(&app.router, "DROP TABLE doomed").await;
    let (s, v, t) = call(
        &app.router,
        "POST",
        "/v1/admin/maintenance/purge-orphans",
        Some(ADMIN_KEY),
        Some(json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{t}");
    assert_eq!(v["applied"], false);
    assert_eq!(v["class"], "INSPECTION");
    let n = v["plan"]["entries"].as_u64().unwrap();
    assert!(n >= 40);

    for body in [
        json!({"apply": true}),
        json!({"apply": true, "expected_entries": n + 1}),
        json!({"apply": true, "expected_entries": 0}),
    ] {
        let (s, v, _) = call(
            &app.router,
            "POST",
            "/v1/admin/maintenance/purge-orphans",
            Some(ADMIN_KEY),
            Some(body),
        )
        .await;
        assert!(
            s == StatusCode::BAD_REQUEST || s == StatusCode::CONFLICT,
            "{s} {v}"
        );
    }
    let (_, again, _) = call(
        &app.router,
        "POST",
        "/v1/admin/maintenance/purge-orphans",
        Some(ADMIN_KEY),
        Some(json!({})),
    )
    .await;
    assert_eq!(
        again["plan"]["entries"], n,
        "refused applies changed nothing"
    );

    let (s, v, t) = call(
        &app.router,
        "POST",
        "/v1/admin/maintenance/purge-orphans",
        Some(ADMIN_KEY),
        Some(json!({"apply": true, "expected_entries": n})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{t}");
    assert_eq!(v["applied"], true);
    assert_eq!(v["class"], "DESTRUCTIVE");
    assert_eq!(v["deleted"], n);
    assert_eq!(v["remaining"], 0);
    let (_, chk, _) = call(
        &app.router,
        "POST",
        "/v1/admin/check",
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(chk["clean"], true);
    assert!(chk["counts"].get("ORPHAN_TABLE_DATA").is_none());
}

#[tokio::test]
async fn a_second_concurrent_operation_of_the_same_kind_is_refused() {
    let app = build_app(&temp_dir("busy"), true);
    app.state.admin.backup_running.store(true, Ordering::SeqCst);
    let (s, v, _) = call(
        &app.router,
        "POST",
        "/v1/admin/backups",
        Some(ADMIN_KEY),
        Some(json!({"name": "second"})),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(v["error"]["code"], "OPERATION_IN_PROGRESS");
    app.state
        .admin
        .backup_running
        .store(false, Ordering::SeqCst);
    app.state.admin.check_running.store(true, Ordering::SeqCst);
    let (s, _, _) = call(
        &app.router,
        "POST",
        "/v1/admin/check",
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    app.state.admin.check_running.store(false, Ordering::SeqCst);
    // The guard is released after a normal run.
    let (s, _, _) = call(
        &app.router,
        "POST",
        "/v1/admin/check",
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(!app.state.admin.check_running.load(Ordering::SeqCst));
}

#[tokio::test]
async fn status_reports_wal_resources_and_queries() {
    let app = build_app(&temp_dir("status"), true);
    seed(&app.router).await;
    let _ = call(
        &app.router,
        "POST",
        "/v1/sql",
        Some(ADMIN_KEY),
        Some(json!({"sql": "SELEKT nonsense"})),
    )
    .await;
    let (s, v, t) = call(
        &app.router,
        "GET",
        "/v1/admin/status",
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{t}");
    assert!(v["wal"]["durable_through"].as_u64().unwrap() > 60);
    assert_eq!(v["wal"]["poisoned"], false);
    assert!(v["wal"]["avg_batch_records"].as_f64().unwrap() >= 1.0);
    assert!(v["resources"]["rss_bytes"].as_u64().unwrap() > 1_000_000);
    assert!(v["resources"]["threads"].as_u64().unwrap() >= 2);
    assert!(v["resources"]["handles"].as_u64().unwrap() >= 10);
    assert!(v["disk"]["data_dir_bytes"].as_u64().unwrap() > 0);
    assert!(v["queries"]["requests"].as_u64().unwrap() >= 60);
    assert!(v["queries"]["errors"].as_u64().unwrap() >= 1);
    assert!(
        v["queries"]["latency_ms"]["max"].as_f64().unwrap()
            >= v["queries"]["latency_ms"]["p50"].as_f64().unwrap()
    );
    assert_eq!(v["sessions"]["active_transactions"], 0);
    assert_eq!(v["backups"]["configured"], true);
    assert!(
        !t.contains(&app.dir.to_string_lossy().to_string()),
        "the status document must not disclose filesystem paths"
    );
}

#[tokio::test]
async fn metrics_stay_bounded_under_hostile_input() {
    let app = build_app(&temp_dir("bounded"), true);
    seed(&app.router).await;
    let (_, before, before_text) = call(
        &app.router,
        "GET",
        "/v1/admin/status",
        Some(ADMIN_KEY),
        None,
    )
    .await;
    let (_, m_before, _) = call(&app.router, "GET", "/v1/metrics", Some(ADMIN_KEY), None).await;
    let routes_before = m_before["service"]["routes"].as_array().unwrap().len();
    for i in 0..1500 {
        let _ = call(
            &app.router,
            "GET",
            &format!("/random-{i}-{}", i * 7919),
            Some(ADMIN_KEY),
            None,
        )
        .await;
        let _ = call(
            &app.router,
            "GET",
            &format!("/v1/kv/{}", base64_like(i)),
            Some(ADMIN_KEY),
            None,
        )
        .await;
        let _ = call(
            &app.router,
            "POST",
            "/v1/sql",
            Some(ADMIN_KEY),
            Some(json!({"sql": format!("SELECT {i} FROM nothing_{i}")})),
        )
        .await;
    }
    let (_, after, after_text) = call(
        &app.router,
        "GET",
        "/v1/admin/status",
        Some(ADMIN_KEY),
        None,
    )
    .await;
    let (_, m_after, _) = call(&app.router, "GET", "/v1/metrics", Some(ADMIN_KEY), None).await;
    let routes_after = m_after["service"]["routes"].as_array().unwrap().len();
    assert!(
        routes_after <= routes_before + 3,
        "route label cardinality grew: {routes_before} -> {routes_after}"
    );
    let keys = |v: &Value| -> Vec<String> { v.as_object().unwrap().keys().cloned().collect() };
    assert_eq!(keys(&before), keys(&after));
    for k in keys(&before) {
        assert_eq!(
            keys(&before[&k]),
            keys(&after[&k]),
            "section {k} changed shape"
        );
    }
    let growth = after_text.len() as i64 - before_text.len() as i64;
    assert!(
        growth.abs() < 400,
        "status document size changed by {growth} bytes under hostile input"
    );
}

fn base64_like(i: usize) -> String {
    format!("QUJD{i:08}")
}

fn as_bound(b: &std::ops::Bound<Vec<u8>>) -> std::ops::Bound<&[u8]> {
    match b {
        std::ops::Bound::Included(v) => std::ops::Bound::Included(v.as_slice()),
        std::ops::Bound::Excluded(v) => std::ops::Bound::Excluded(v.as_slice()),
        std::ops::Bound::Unbounded => std::ops::Bound::Unbounded,
    }
}

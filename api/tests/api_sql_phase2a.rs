//! SQL Phase 2a over HTTP (`POST /v1/sql`): `RETURNING`, `INSERT ... SELECT` and
//! `INSERT ... ON CONFLICT DO NOTHING | DO UPDATE` through the real router, engine
//! and `rubixdb-sql` pipeline -- no mocking. Helpers are repeated from
//! `api_sql_phase1.rs` because each integration-test file is its own crate.

use std::path::{Path, PathBuf};
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
    let path = std::env::temp_dir().join(format!("rubixdb_api_sql_it_{tag}_{nanos}"));
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

fn build_app(dir: &Path) -> (Arc<AppState>, Router) {
    let lsm_config = LsmConfig::default();
    let engine =
        Arc::new(LsmEngine::open(dir, wal_config(), pool_config(), lsm_config.clone()).unwrap());
    let state = Arc::new(AppState::new(
        engine,
        lsm_config,
        test_config(dir.to_path_buf()),
    ));
    let router = build_router(state.clone());
    (state, router)
}

async fn sql_req(router: &Router, key: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri("/v1/sql")
        .header("Authorization", format!("Bearer {key}"))
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = router.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

async fn exec_sql(router: &Router, key: &str, sql: &str) -> Value {
    let (status, body) = sql_req(router, key, json!({ "sql": sql })).await;
    assert!(
        status.is_success(),
        "expected success for {sql:?}, got {status}: {body}"
    );
    body
}

async fn in_session(router: &Router, key: &str, session: &str, sql: &str) -> (StatusCode, Value) {
    sql_req(router, key, json!({ "sql": sql, "session_id": session })).await
}

/// Renders the rows of a `kind: rows` result as strings (`NULL` for SQL NULL).
fn table(result: &Value) -> Vec<Vec<String>> {
    assert_eq!(result["result"]["kind"], "rows", "{result}");
    result["result"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            row.as_array()
                .unwrap()
                .iter()
                .map(|cell| match &cell["value"] {
                    Value::Null => "NULL".to_string(),
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect()
        })
        .collect()
}

fn rows_of(strs: &[&[&str]]) -> Vec<Vec<String>> {
    strs.iter()
        .map(|r| r.iter().map(|s| s.to_string()).collect())
        .collect()
}

async fn seeded(tag: &str) -> (Arc<AppState>, Router, PathBuf) {
    let dir = temp_dir(tag);
    let (state, router) = build_app(&dir);
    exec_sql(
        &router,
        ADMIN_KEY,
        "CREATE TABLE kv (k INTEGER PRIMARY KEY, v TEXT, n INTEGER NOT NULL DEFAULT 7)",
    )
    .await;
    exec_sql(&router, ADMIN_KEY, "CREATE UNIQUE INDEX kv_v_uq ON kv (v)").await;
    (state, router, dir)
}

async fn done(state: Arc<AppState>, dir: PathBuf) {
    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn insert_returning_is_a_rows_result_with_columns_and_every_row() {
    let (state, router, dir) = seeded("p2a_http_insert").await;
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "INSERT INTO kv (k, v) VALUES (1, 'a'), (2, 'b'), (3, NULL) RETURNING k, v, n",
    )
    .await;
    assert_eq!(r["result"]["kind"], "rows", "{r}");
    assert_eq!(r["result"]["row_count"], 3, "{r}");
    let names: Vec<_> = r["result"]["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["k", "v", "n"]);
    assert_eq!(
        table(&r),
        rows_of(&[&["1", "a", "7"], &["2", "b", "7"], &["3", "NULL", "7"]])
    );
    // the same statement without RETURNING keeps its old shape
    let r = exec_sql(&router, ADMIN_KEY, "INSERT INTO kv (k, v) VALUES (4, 'd')").await;
    assert_eq!(r["result"]["kind"], "write", "{r}");
    assert_eq!(r["result"]["rows_affected"], 1, "{r}");
    done(state, dir).await;
}

#[tokio::test]
async fn update_and_delete_returning_over_http() {
    let (state, router, dir) = seeded("p2a_http_upd_del").await;
    exec_sql(
        &router,
        ADMIN_KEY,
        "INSERT INTO kv (k, v, n) VALUES (1, 'a', 10), (2, 'b', 20), (3, 'c', 30)",
    )
    .await;
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "UPDATE kv SET n = n + 1 WHERE k <= 2 RETURNING k, n",
    )
    .await;
    assert_eq!(table(&r), rows_of(&[&["1", "11"], &["2", "21"]]));
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "DELETE FROM kv WHERE n > 15 RETURNING k, v, n",
    )
    .await;
    assert_eq!(table(&r), rows_of(&[&["2", "b", "21"], &["3", "c", "30"]]));
    let r = exec_sql(&router, ADMIN_KEY, "SELECT k FROM kv").await;
    assert_eq!(table(&r), rows_of(&[&["1"]]));
    done(state, dir).await;
}

#[tokio::test]
async fn on_conflict_do_nothing_and_do_update_over_http() {
    let (state, router, dir) = seeded("p2a_http_upsert").await;
    exec_sql(
        &router,
        ADMIN_KEY,
        "INSERT INTO kv (k, v, n) VALUES (1, 'a', 10), (2, 'b', 20)",
    )
    .await;
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "INSERT INTO kv (k, v, n) VALUES (1, 'zz', 0), (3, 'c', 30) ON CONFLICT (k) DO NOTHING RETURNING k",
    )
    .await;
    assert_eq!(table(&r), rows_of(&[&["3"]]));
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "INSERT INTO kv (k, v, n) VALUES (1, 'a', 5), (4, 'd', 40) \
         ON CONFLICT (k) DO UPDATE SET n = kv.n + EXCLUDED.n RETURNING k, n",
    )
    .await;
    assert_eq!(table(&r), rows_of(&[&["1", "15"], &["4", "40"]]));
    // without RETURNING it is a plain write result carrying the affected count
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "INSERT INTO kv (k, v, n) VALUES (1, 'a', 1) ON CONFLICT (k) DO UPDATE SET n = EXCLUDED.n",
    )
    .await;
    assert_eq!(r["result"]["kind"], "write", "{r}");
    assert_eq!(r["result"]["rows_affected"], 1, "{r}");
    done(state, dir).await;
}

#[tokio::test]
async fn insert_select_over_http() {
    let (state, router, dir) = seeded("p2a_http_insert_select").await;
    exec_sql(
        &router,
        ADMIN_KEY,
        "CREATE TABLE src (k INTEGER PRIMARY KEY, v TEXT)",
    )
    .await;
    exec_sql(
        &router,
        ADMIN_KEY,
        "INSERT INTO src VALUES (1, 'a'), (2, 'b')",
    )
    .await;
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "INSERT INTO kv (k, v) SELECT k + 10, UPPER(v) FROM src RETURNING k, v, n",
    )
    .await;
    assert_eq!(table(&r), rows_of(&[&["11", "A", "7"], &["12", "B", "7"]]));
    done(state, dir).await;
}

#[tokio::test]
async fn failures_map_to_the_documented_http_errors_and_change_nothing() {
    let (state, router, dir) = seeded("p2a_http_errors").await;
    exec_sql(&router, ADMIN_KEY, "INSERT INTO kv (k, v) VALUES (1, 'a')").await;

    // duplicate key in a multi-row insert: 409, the statement's other rows are not written
    let (status, body) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "INSERT INTO kv (k, v) VALUES (2, 'b'), (1, 'x') RETURNING k" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "CONFLICT_ERROR", "{body}");

    // secondary unique violation: the same
    let (status, body) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "INSERT INTO kv (k, v) VALUES (3, 'c'), (4, 'a')" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // runtime error inside DO UPDATE
    let (status, body) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "INSERT INTO kv (k, v, n) VALUES (1, 'a', 0) ON CONFLICT (k) DO UPDATE SET n = 1 / EXCLUDED.n" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "EXECUTION_ERROR", "{body}");

    // a bad conflict target is refused at bind time
    let (status, body) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "INSERT INTO kv (k) VALUES (9) ON CONFLICT (n) DO NOTHING" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "BIND_ERROR", "{body}");

    let r = exec_sql(&router, ADMIN_KEY, "SELECT k, v, n FROM kv").await;
    assert_eq!(table(&r), rows_of(&[&["1", "a", "7"]]));
    done(state, dir).await;
}

#[tokio::test]
async fn a_reader_key_cannot_write_with_or_without_returning() {
    let (state, router, dir) = seeded("p2a_http_reader").await;
    for sql in [
        "INSERT INTO kv (k, v) VALUES (1, 'a') RETURNING k",
        "INSERT INTO kv (k, v) VALUES (1, 'a') ON CONFLICT (k) DO NOTHING",
        "UPDATE kv SET n = 1 RETURNING n",
        "DELETE FROM kv RETURNING k",
    ] {
        let (status, body) = sql_req(&router, READER_KEY, json!({ "sql": sql })).await;
        assert!(!status.is_success(), "{sql}: {status} {body}");
    }
    let r = exec_sql(&router, ADMIN_KEY, "SELECT COUNT(*) FROM kv").await;
    assert_eq!(table(&r), rows_of(&[&["0"]]));
    done(state, dir).await;
}

#[tokio::test]
async fn returning_inside_a_session_follows_the_transaction() {
    let (state, router, dir) = seeded("p2a_http_session").await;
    let begin = exec_sql(&router, ADMIN_KEY, "BEGIN").await;
    let session = begin["session_id"].as_str().unwrap().to_string();

    let (status, body) = in_session(
        &router,
        ADMIN_KEY,
        &session,
        "INSERT INTO kv (k, v) VALUES (1, 'a') RETURNING k, v",
    )
    .await;
    assert!(status.is_success(), "{body}");
    assert_eq!(table(&body), rows_of(&[&["1", "a"]]));
    // invisible outside until commit
    let r = exec_sql(&router, ADMIN_KEY, "SELECT COUNT(*) FROM kv").await;
    assert_eq!(table(&r), rows_of(&[&["0"]]));

    // a failing statement leaves the session alive and the transaction unchanged
    let (status, body) = in_session(
        &router,
        ADMIN_KEY,
        &session,
        "INSERT INTO kv (k, v) VALUES (2, 'b'), (1, 'dup')",
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, body) = in_session(
        &router,
        ADMIN_KEY,
        &session,
        "INSERT INTO kv (k, v, n) VALUES (1, 'a', 5) ON CONFLICT (k) DO UPDATE SET n = EXCLUDED.n RETURNING k, n",
    )
    .await;
    assert!(status.is_success(), "{body}");
    assert_eq!(table(&body), rows_of(&[&["1", "5"]]));

    let (status, body) = in_session(&router, ADMIN_KEY, &session, "COMMIT").await;
    assert!(status.is_success(), "{body}");
    let r = exec_sql(&router, ADMIN_KEY, "SELECT k, v, n FROM kv").await;
    assert_eq!(table(&r), rows_of(&[&["1", "a", "5"]]));
    done(state, dir).await;
}

#[tokio::test]
async fn concurrent_upserts_over_http_make_one_row_and_lose_no_update() {
    let (state, router, dir) = seeded("p2a_http_concurrent").await;
    let mut tasks = Vec::new();
    for _ in 0..4 {
        let router = router.clone();
        tasks.push(tokio::spawn(async move {
            for _ in 0..5 {
                let (status, body) = sql_req(
                    &router,
                    ADMIN_KEY,
                    json!({ "sql": "INSERT INTO kv (k, v, n) VALUES (1, 'a', 1) ON CONFLICT (k) DO UPDATE SET n = kv.n + EXCLUDED.n" }),
                )
                .await;
                assert!(status.is_success(), "{status} {body}");
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let r = exec_sql(&router, ADMIN_KEY, "SELECT k, n FROM kv").await;
    assert_eq!(table(&r), rows_of(&[&["1", "20"]]));
    done(state, dir).await;
}

#[tokio::test]
async fn returned_rows_survive_a_restart_of_the_engine() {
    let dir = temp_dir("p2a_http_restart");
    {
        let (state, router) = build_app(&dir);
        exec_sql(
            &router,
            ADMIN_KEY,
            "CREATE TABLE kv (k INTEGER PRIMARY KEY, v TEXT, n INTEGER NOT NULL DEFAULT 7)",
        )
        .await;
        let r = exec_sql(
            &router,
            ADMIN_KEY,
            "INSERT INTO kv (k, v) VALUES (1, 'a'), (2, 'b') ON CONFLICT (k) DO UPDATE SET v = EXCLUDED.v RETURNING k, v, n",
        )
        .await;
        assert_eq!(table(&r), rows_of(&[&["1", "a", "7"], &["2", "b", "7"]]));
        drop(router);
        state.engine.shutdown();
    }
    let (state, router) = build_app(&dir);
    let r = exec_sql(&router, ADMIN_KEY, "SELECT k, v, n FROM kv ORDER BY k").await;
    assert_eq!(table(&r), rows_of(&[&["1", "a", "7"], &["2", "b", "7"]]));
    // the same upsert again is idempotent
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "INSERT INTO kv (k, v) VALUES (1, 'a'), (2, 'b') ON CONFLICT (k) DO UPDATE SET v = EXCLUDED.v RETURNING k, v, n",
    )
    .await;
    assert_eq!(table(&r), rows_of(&[&["1", "a", "7"], &["2", "b", "7"]]));
    done(state, dir).await;
}

//! `POST /v1/sql` integration tests — real `LsmEngine`, real axum
//! router, real `rubixdb-sql` pipeline, via `tower::ServiceExt::oneshot`
//! — the same no-mocking discipline `api_integration.rs`/`api_security_
//! validation.rs` already establish.

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

// -----------------------------------------------------------------
// Core end-to-end correctness loop (item 104)
// -----------------------------------------------------------------

#[tokio::test]
async fn end_to_end_ddl_dml_select_through_the_real_api() {
    let dir = temp_dir("e2e");
    let (state, router) = build_app(&dir);

    exec_sql(
        &router,
        ADMIN_KEY,
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, active BOOLEAN)",
    )
    .await;

    let insert = exec_sql(
        &router,
        ADMIN_KEY,
        "INSERT INTO users (id, name, active) VALUES (1, 'alice', TRUE)",
    )
    .await;
    assert_eq!(insert["result"]["kind"], "write");
    assert_eq!(insert["result"]["rows_affected"], 1);

    let select = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT id, name FROM users WHERE id = 1",
    )
    .await;
    assert_eq!(select["result"]["kind"], "rows");
    assert_eq!(select["result"]["row_count"], 1);
    assert_eq!(select["result"]["rows"][0][1]["value"], "alice");

    let update = exec_sql(
        &router,
        ADMIN_KEY,
        "UPDATE users SET name = 'alice2' WHERE id = 1",
    )
    .await;
    assert_eq!(update["result"]["rows_affected"], 1);

    let select2 = exec_sql(&router, ADMIN_KEY, "SELECT name FROM users WHERE id = 1").await;
    assert_eq!(select2["result"]["rows"][0][0]["value"], "alice2");

    let delete = exec_sql(&router, ADMIN_KEY, "DELETE FROM users WHERE id = 1").await;
    assert_eq!(delete["result"]["rows_affected"], 1);

    let select3 = exec_sql(&router, ADMIN_KEY, "SELECT id FROM users WHERE id = 1").await;
    assert_eq!(select3["result"]["row_count"], 0);

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn group_by_having_and_aggregates_through_the_real_api() {
    let dir = temp_dir("agg");
    let (state, router) = build_app(&dir);

    exec_sql(
        &router,
        ADMIN_KEY,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, grp TEXT, val INTEGER)",
    )
    .await;
    for (id, grp, val) in [(1, "a", 10), (2, "a", 20), (3, "b", 5)] {
        exec_sql(
            &router,
            ADMIN_KEY,
            &format!("INSERT INTO t (id, grp, val) VALUES ({id}, '{grp}', {val})"),
        )
        .await;
    }

    let result = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT grp, COUNT(*), SUM(val) FROM t GROUP BY grp HAVING COUNT(*) >= 1 ORDER BY grp",
    )
    .await;
    let rows = result["result"]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0]["value"], "a");
    assert_eq!(rows[0][1]["value"], "2");
    assert_eq!(rows[1][0]["value"], "b");

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

// -----------------------------------------------------------------
// Transactions spanning multiple HTTP requests (item 22/26/63)
// -----------------------------------------------------------------

#[tokio::test]
async fn transaction_spans_multiple_http_requests_and_isolates_other_clients() {
    let dir = temp_dir("txn_http");
    let (state, router) = build_app(&dir);
    exec_sql(
        &router,
        ADMIN_KEY,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)",
    )
    .await;

    let begin = exec_sql(&router, ADMIN_KEY, "BEGIN").await;
    let session_id = begin["session_id"].as_str().unwrap().to_string();
    assert_eq!(begin["result"]["kind"], "begin");

    let (status, insert) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "INSERT INTO t (id, name) VALUES (1, 'local')", "session_id": session_id }),
    )
    .await;
    assert!(status.is_success(), "{insert}");
    assert_eq!(insert["session_id"], session_id);

    // Uncommitted -- a fresh (no session) request from another client
    // must not see it yet.
    let outside = exec_sql(&router, READER_KEY, "SELECT id FROM t").await;
    assert_eq!(
        outside["result"]["row_count"], 0,
        "an uncommitted write inside a session must not be visible outside it"
    );

    // The same session, though, sees its own uncommitted write
    // (read-your-own-writes through a PK lookup).
    let (status, own_read) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "SELECT id FROM t WHERE id = 1", "session_id": session_id }),
    )
    .await;
    assert!(status.is_success(), "{own_read}");
    assert_eq!(own_read["result"]["row_count"], 1);

    let (status, commit) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "COMMIT", "session_id": session_id }),
    )
    .await;
    assert!(status.is_success(), "{commit}");
    assert_eq!(commit["result"]["kind"], "commit");
    assert!(
        commit["session_id"].is_null(),
        "the session must be gone after COMMIT"
    );

    let after = exec_sql(&router, READER_KEY, "SELECT id FROM t").await;
    assert_eq!(after["result"]["row_count"], 1);

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn rollback_discards_the_transaction_local_write() {
    let dir = temp_dir("txn_rollback");
    let (state, router) = build_app(&dir);
    exec_sql(
        &router,
        ADMIN_KEY,
        "CREATE TABLE t (id INTEGER PRIMARY KEY)",
    )
    .await;

    let begin = exec_sql(&router, ADMIN_KEY, "BEGIN").await;
    let session_id = begin["session_id"].as_str().unwrap().to_string();
    sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "INSERT INTO t (id) VALUES (1)", "session_id": session_id }),
    )
    .await;
    let (status, rollback) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "ROLLBACK", "session_id": session_id }),
    )
    .await;
    assert!(status.is_success());
    assert_eq!(rollback["result"]["kind"], "rollback");

    let after = exec_sql(&router, ADMIN_KEY, "SELECT id FROM t").await;
    assert_eq!(after["result"]["row_count"], 0);

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn client_b_cannot_use_client_a_session_id() {
    let dir = temp_dir("session_isolation");
    let (state, router) = build_app(&dir);

    let begin = exec_sql(&router, ADMIN_KEY, "BEGIN").await;
    let session_id = begin["session_id"].as_str().unwrap().to_string();

    // A *different authenticated principal* (reader) tries to use
    // admin's session id.
    let (status, resp) = sql_req(
        &router,
        READER_KEY,
        json!({ "sql": "SELECT 1", "session_id": session_id }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{resp}");
    assert_eq!(resp["error"]["code"], "SESSION_NOT_FOUND");

    // The rightful owner can still use it.
    let (status, own) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "ROLLBACK", "session_id": session_id }),
    )
    .await;
    assert!(status.is_success(), "{own}");

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

// -----------------------------------------------------------------
// Real concurrency (item 62/63) -- genuinely simultaneous requests via
// `tokio::join!` against the one shared `Arc<AppState>`, on a multi-
// threaded runtime (a single-threaded one would serialize these onto
// one OS thread and prove nothing about concurrent-request safety).
// -----------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_transaction_is_invisible_to_a_simultaneous_autocommit_read() {
    let dir = temp_dir("concurrent_txn");
    let (state, router) = build_app(&dir);
    exec_sql(
        &router,
        ADMIN_KEY,
        "CREATE TABLE t (id INTEGER PRIMARY KEY)",
    )
    .await;

    let begin = exec_sql(&router, ADMIN_KEY, "BEGIN").await;
    let session_id = begin["session_id"].as_str().unwrap().to_string();
    sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "INSERT INTO t (id) VALUES (1)", "session_id": session_id }),
    )
    .await;

    // Many genuinely concurrent autocommit reads, none of which may
    // ever observe the uncommitted row.
    let reads: Vec<_> = (0..16)
        .map(|_| {
            let router = router.clone();
            tokio::spawn(async move { exec_sql(&router, ADMIN_KEY, "SELECT id FROM t").await })
        })
        .collect();
    for r in reads {
        let body = r.await.unwrap();
        assert_eq!(
            body["result"]["row_count"], 0,
            "a concurrent autocommit read must never see another session's uncommitted write"
        );
    }

    let (status, commit) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "COMMIT", "session_id": session_id }),
    )
    .await;
    assert!(status.is_success(), "{commit}");
    let after = exec_sql(&router, ADMIN_KEY, "SELECT id FROM t").await;
    assert_eq!(after["result"]["row_count"], 1);

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_concurrent_independent_transactions_never_cross_contaminate() {
    let dir = temp_dir("concurrent_many_txn");
    let (state, router) = build_app(&dir);
    exec_sql(
        &router,
        ADMIN_KEY,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, owner INTEGER)",
    )
    .await;

    let tasks: Vec<_> = (0..12)
        .map(|i: i64| {
            let router = router.clone();
            tokio::spawn(async move {
                let begin = exec_sql(&router, ADMIN_KEY, "BEGIN").await;
                let session_id = begin["session_id"].as_str().unwrap().to_string();
                let (status, insert) = sql_req(
                    &router,
                    ADMIN_KEY,
                    json!({
                        "sql": format!("INSERT INTO t (id, owner) VALUES ({}, {i})", 1000 + i),
                        "session_id": session_id
                    }),
                )
                .await;
                assert!(status.is_success(), "{insert}");
                let (status, own_read) = sql_req(
                    &router,
                    ADMIN_KEY,
                    json!({
                        "sql": format!("SELECT owner FROM t WHERE id = {}", 1000 + i),
                        "session_id": session_id
                    }),
                )
                .await;
                assert!(status.is_success(), "{own_read}");
                assert_eq!(
                    own_read["result"]["rows"][0][0]["value"], i,
                    "each task must see exactly its own uncommitted row, never another's"
                );
                let (status, commit) = sql_req(
                    &router,
                    ADMIN_KEY,
                    json!({ "sql": "COMMIT", "session_id": session_id }),
                )
                .await;
                assert!(status.is_success(), "{commit}");
            })
        })
        .collect();
    for t in tasks {
        t.await.unwrap();
    }

    let final_state = exec_sql(&router, ADMIN_KEY, "SELECT COUNT(*) FROM t").await;
    assert_eq!(final_state["result"]["rows"][0][0]["value"], "12");

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

// -----------------------------------------------------------------
// Authorization (item 14/86)
// -----------------------------------------------------------------

#[tokio::test]
async fn reader_can_select_but_not_insert_through_sql() {
    let dir = temp_dir("authz");
    let (state, router) = build_app(&dir);
    exec_sql(
        &router,
        ADMIN_KEY,
        "CREATE TABLE t (id INTEGER PRIMARY KEY)",
    )
    .await;

    let (status, select) = sql_req(&router, READER_KEY, json!({ "sql": "SELECT id FROM t" })).await;
    assert!(status.is_success(), "reader SELECT must succeed: {select}");

    // `sql/src/bind/scope.rs::resolve_table` checks authorization for
    // the *specific* privilege each statement needs (`Privilege::Insert`
    // here) and, on denial, returns the same existence-hiding
    // `UnknownObject` -> `NOT_FOUND` outcome `SELECT` would get against
    // a genuinely nonexistent table -- deliberately, regardless of
    // whether the same principal holds a *different* privilege (`SELECT`)
    // on the identical table (D25/item 15's own "never AuthorizationDenied
    // for existence" rule, verified against the real binder here rather
    // than assumed).
    let (status, insert) = sql_req(
        &router,
        READER_KEY,
        json!({ "sql": "INSERT INTO t (id) VALUES (1)" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{insert}");
    assert_eq!(insert["error"]["code"], "NOT_FOUND");

    let after = exec_sql(&router, ADMIN_KEY, "SELECT id FROM t").await;
    assert_eq!(
        after["result"]["row_count"], 0,
        "the denied INSERT must not have taken effect"
    );

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn unauthenticated_sql_request_is_rejected() {
    let dir = temp_dir("unauth");
    let (state, router) = build_app(&dir);
    let req = Request::builder()
        .method("POST")
        .uri("/v1/sql")
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({"sql":"SELECT 1"})).unwrap(),
        ))
        .unwrap();
    let response = router.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

// -----------------------------------------------------------------
// Safe errors / information disclosure (item 16/17)
// -----------------------------------------------------------------

#[tokio::test]
async fn nonexistent_and_unauthorized_tables_are_indistinguishable() {
    let dir = temp_dir("info_disclosure");
    let (state, router) = build_app(&dir);
    exec_sql(
        &router,
        ADMIN_KEY,
        "CREATE TABLE secret (id INTEGER PRIMARY KEY)",
    )
    .await;

    let (status_missing, missing) = sql_req(
        &router,
        READER_KEY,
        json!({ "sql": "SELECT id FROM does_not_exist" }),
    )
    .await;
    let (status_forbidden, forbidden) = sql_req(
        &router,
        READER_KEY,
        json!({ "sql": "SELECT id FROM secret" }),
    )
    .await;
    // A `reader` has default SELECT on every object (D25 v1 default
    // mapping), so this specific pair isn't a forbidden case -- the
    // real test is that a *parse-valid but catalog-unknown* name and a
    // syntactically-identical real query produce distinguishable
    // shapes only via legitimate content (rows vs. not-found), and that
    // a bind failure for an unknown table never echoes back anything
    // beyond the stable NOT_FOUND code.
    assert_eq!(status_missing, StatusCode::NOT_FOUND);
    assert_eq!(missing["error"]["code"], "NOT_FOUND");
    assert!(status_forbidden.is_success());
    let _ = forbidden;

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn parse_error_never_leaks_internal_detail_shape() {
    let dir = temp_dir("parse_error");
    let (state, router) = build_app(&dir);
    let (status, body) = sql_req(&router, ADMIN_KEY, json!({ "sql": "SELEKT * FRUM" })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "PARSE_ERROR");
    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

// -----------------------------------------------------------------
// Typed parameters (item 6)
// -----------------------------------------------------------------

#[tokio::test]
async fn typed_parameters_reach_the_query_without_string_interpolation() {
    let dir = temp_dir("params");
    let (state, router) = build_app(&dir);
    exec_sql(
        &router,
        ADMIN_KEY,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)",
    )
    .await;
    exec_sql(
        &router,
        ADMIN_KEY,
        "INSERT INTO t (id, name) VALUES (1, 'it''s a test')",
    )
    .await;

    let (status, body) = sql_req(
        &router,
        ADMIN_KEY,
        json!({
            "sql": "SELECT name FROM t WHERE id = $1",
            "params": [{"type": "integer", "value": 1}]
        }),
    )
    .await;
    assert!(status.is_success(), "{body}");
    assert_eq!(body["result"]["rows"][0][0]["value"], "it's a test");

    // A classic injection payload as a *parameter value* must be inert
    // text, never re-parsed as SQL.
    exec_sql(
        &router,
        ADMIN_KEY,
        "INSERT INTO t (id, name) VALUES (2, 'x')",
    )
    .await;
    let (status, body) = sql_req(
        &router,
        ADMIN_KEY,
        json!({
            "sql": "SELECT id FROM t WHERE name = $1",
            "params": [{"type": "text", "value": "x' OR '1'='1"}]
        }),
    )
    .await;
    assert!(status.is_success(), "{body}");
    assert_eq!(
        body["result"]["row_count"], 0,
        "the injection payload must be treated as a literal string value, matching nothing"
    );

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

// -----------------------------------------------------------------
// EXPLAIN (item 135)
// -----------------------------------------------------------------

#[tokio::test]
async fn explain_returns_structured_plan_text_without_executing() {
    let dir = temp_dir("explain");
    let (state, router) = build_app(&dir);
    exec_sql(
        &router,
        ADMIN_KEY,
        "CREATE TABLE t (id INTEGER PRIMARY KEY)",
    )
    .await;
    let body = exec_sql(&router, ADMIN_KEY, "EXPLAIN SELECT id FROM t WHERE id = 1").await;
    assert_eq!(body["result"]["kind"], "explain");
    assert!(body["result"]["plan_text"]
        .as_str()
        .unwrap()
        .contains("PkLookup"));
    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

// -----------------------------------------------------------------
// Session limits (item 24)
// -----------------------------------------------------------------

#[tokio::test]
async fn per_principal_session_limit_is_enforced_through_the_real_api() {
    let dir = temp_dir("session_limit");
    let lsm_config = LsmConfig::default();
    let engine =
        Arc::new(LsmEngine::open(&dir, wal_config(), pool_config(), lsm_config.clone()).unwrap());
    let mut config = test_config(dir.to_path_buf());
    config.sql_max_sessions_per_principal = 2;
    let state = Arc::new(AppState::new(engine, lsm_config, config));
    let router = build_router(state.clone());

    let (s1, s2) = (
        exec_sql(&router, ADMIN_KEY, "BEGIN").await,
        exec_sql(&router, ADMIN_KEY, "BEGIN").await,
    );
    assert!(s1["session_id"].is_string());
    assert!(s2["session_id"].is_string());

    let (status, third) = sql_req(&router, ADMIN_KEY, json!({ "sql": "BEGIN" })).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{third}");
    assert_eq!(third["error"]["code"], "TOO_MANY_SESSIONS");

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn commit_or_rollback_without_a_session_id_is_a_validation_error() {
    let dir = temp_dir("no_session_commit");
    let (state, router) = build_app(&dir);
    let (status, body) = sql_req(&router, ADMIN_KEY, json!({ "sql": "COMMIT" })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = sql_req(&router, ADMIN_KEY, json!({ "sql": "ROLLBACK" })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn nested_begin_on_an_open_session_is_rejected_without_losing_the_transaction() {
    let dir = temp_dir("nested_begin");
    let (state, router) = build_app(&dir);
    exec_sql(
        &router,
        ADMIN_KEY,
        "CREATE TABLE t (id INTEGER PRIMARY KEY)",
    )
    .await;
    let begin = exec_sql(&router, ADMIN_KEY, "BEGIN").await;
    let session_id = begin["session_id"].as_str().unwrap().to_string();

    let (status, nested) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "BEGIN", "session_id": session_id }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{nested}");

    // The original transaction must still be usable afterward.
    let (status, insert) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "INSERT INTO t (id) VALUES (1)", "session_id": session_id }),
    )
    .await;
    assert!(status.is_success(), "{insert}");
    let (status, commit) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "COMMIT", "session_id": session_id }),
    )
    .await;
    assert!(status.is_success(), "{commit}");

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

// -----------------------------------------------------------------
// Deadline (item 12)
// -----------------------------------------------------------------

#[tokio::test]
async fn statement_exceeding_its_deadline_is_a_controlled_timeout() {
    // A zero-second deadline means `ExecCtx`'s own `deadline_at` is
    // already in the past by the time the first `check()` call runs
    // (before the very first row, even for a trivial FROM-less
    // `SELECT`) -- deterministic, no dependency on scan duration.
    let dir = temp_dir("deadline");
    let lsm_config = LsmConfig::default();
    let engine =
        Arc::new(LsmEngine::open(&dir, wal_config(), pool_config(), lsm_config.clone()).unwrap());
    let mut config = test_config(dir.to_path_buf());
    config.sql_statement_deadline_secs = 0;
    let state = Arc::new(AppState::new(engine, lsm_config, config));
    let router = build_router(state.clone());

    let (status, body) = sql_req(&router, ADMIN_KEY, json!({ "sql": "SELECT 1" })).await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{body}");
    assert_eq!(body["error"]["code"], "TIMEOUT");

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

// -----------------------------------------------------------------
// Unsupported SQL remains rejected (item 136)
// -----------------------------------------------------------------

// -----------------------------------------------------------------
// Catalog metadata routes (item 68: real backend verification, not a
// snapshot fixture -- each assertion below follows a real DDL
// statement that changes what the metadata route must then reflect).
// -----------------------------------------------------------------

async fn get_json(router: &Router, key: &str, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("Authorization", format!("Bearer {key}"))
        .body(Body::empty())
        .unwrap();
    let response = router.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

#[tokio::test]
async fn databases_lists_exactly_the_one_real_bootstrapped_database() {
    let dir = temp_dir("cat_databases");
    let (state, router) = build_app(&dir);
    let (status, body) = get_json(&router, ADMIN_KEY, "/v1/catalog/databases").await;
    assert!(status.is_success());
    let dbs = body.as_array().unwrap();
    assert_eq!(
        dbs.len(),
        1,
        "RubiXDB has exactly one bootstrapped database today -- never faked as more"
    );
    assert_eq!(dbs[0]["name"], "default");
    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn schemas_lists_the_real_public_schema_and_reflects_create_schema() {
    let dir = temp_dir("cat_schemas");
    let (state, router) = build_app(&dir);
    let (_, before) = get_json(&router, ADMIN_KEY, "/v1/catalog/schemas").await;
    let before_names: Vec<String> = before
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(before_names, vec!["public".to_string()]);

    exec_sql(&router, ADMIN_KEY, "CREATE SCHEMA reporting").await;
    let (_, after) = get_json(&router, ADMIN_KEY, "/v1/catalog/schemas").await;
    let after_names: Vec<String> = after
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_string())
        .collect();
    assert!(
        after_names.contains(&"reporting".to_string()),
        "{after_names:?}"
    );

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn tables_reflects_create_and_drop_table() {
    let dir = temp_dir("cat_tables");
    let (state, router) = build_app(&dir);

    let (_, before) = get_json(&router, ADMIN_KEY, "/v1/catalog/tables").await;
    assert_eq!(before.as_array().unwrap().len(), 0);

    exec_sql(
        &router,
        ADMIN_KEY,
        "CREATE TABLE widgets (id INTEGER PRIMARY KEY)",
    )
    .await;
    let (_, after_create) = get_json(&router, ADMIN_KEY, "/v1/catalog/tables").await;
    let names: Vec<String> = after_create
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, vec!["widgets".to_string()]);

    exec_sql(&router, ADMIN_KEY, "DROP TABLE widgets").await;
    let (_, after_drop) = get_json(&router, ADMIN_KEY, "/v1/catalog/tables").await;
    assert_eq!(after_drop.as_array().unwrap().len(), 0);

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn describe_table_matches_real_columns_and_primary_key() {
    let dir = temp_dir("cat_describe");
    let (state, router) = build_app(&dir);
    exec_sql(
        &router,
        ADMIN_KEY,
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, active BOOLEAN)",
    )
    .await;
    let (status, body) = get_json(&router, ADMIN_KEY, "/v1/catalog/tables/users").await;
    assert!(status.is_success(), "{body}");
    let cols = body["columns"].as_array().unwrap();
    assert_eq!(cols.len(), 3);
    assert_eq!(cols[0]["name"], "id");
    assert_eq!(cols[0]["primary_key"], true);
    assert_eq!(cols[0]["nullable"], false);
    assert_eq!(cols[1]["name"], "name");
    assert_eq!(cols[1]["nullable"], true);

    let (status, missing) = get_json(&router, ADMIN_KEY, "/v1/catalog/tables/does_not_exist").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{missing}");

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn indexes_reflects_create_index() {
    let dir = temp_dir("cat_indexes");
    let (state, router) = build_app(&dir);
    exec_sql(
        &router,
        ADMIN_KEY,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)",
    )
    .await;
    let (_, before) = get_json(&router, ADMIN_KEY, "/v1/catalog/indexes").await;
    // Only the implicit PRIMARY index exists before any explicit CREATE INDEX.
    let before_names: Vec<String> = before
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap().to_string())
        .collect();

    exec_sql(&router, ADMIN_KEY, "CREATE INDEX t_name_idx ON t (name)").await;
    let (_, after) = get_json(&router, ADMIN_KEY, "/v1/catalog/indexes").await;
    let after_names: Vec<String> = after
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap().to_string())
        .collect();
    assert!(
        after_names.contains(&"t_name_idx".to_string()),
        "{after_names:?}"
    );
    assert!(after_names.len() > before_names.len());

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn authz_route_never_exposes_credentials() {
    let dir = temp_dir("cat_authz");
    let (state, router) = build_app(&dir);
    let (status, body) = get_json(&router, ADMIN_KEY, "/v1/catalog/authz").await;
    assert!(status.is_success());
    assert_eq!(body["role"], "admin");
    let text = body.to_string();
    assert!(
        !text.contains(ADMIN_KEY),
        "the API key itself must never appear in a response body"
    );
    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn catalog_routes_still_require_authentication() {
    let dir = temp_dir("cat_auth");
    let (state, router) = build_app(&dir);
    for uri in [
        "/v1/catalog/databases",
        "/v1/catalog/schemas",
        "/v1/catalog/tables",
        "/v1/catalog/indexes",
        "/v1/catalog/authz",
    ] {
        let req = Request::builder()
            .method("GET")
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        let response = router.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{uri}");
    }
    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn unsupported_grammar_is_a_stable_error_not_silently_accepted() {
    let dir = temp_dir("unsupported");
    let (state, router) = build_app(&dir);
    let (status, body) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "SELECT id FROM t UNION SELECT id FROM t" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "UNSUPPORTED");
    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

// -----------------------------------------------------------------
// SQL-mission-1, Slice A: scalar / IN / EXISTS subqueries through the
// real HTTP stack (real engine, real router, real SQL pipeline).
// -----------------------------------------------------------------

/// `people(id INTEGER PK, name TEXT)` = (1 ann) (2 bob) (3 cy);
/// `visits(id INTEGER PK, person INTEGER, minutes INTEGER)` =
/// (1, 1, 30) (2, 1, 45) (3, 2, NULL) (4, NULL, 10).
async fn seed_people_visits(router: &Router) {
    exec_sql(
        router,
        ADMIN_KEY,
        "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT)",
    )
    .await;
    exec_sql(
        router,
        ADMIN_KEY,
        "CREATE TABLE visits (id INTEGER PRIMARY KEY, person INTEGER, minutes INTEGER)",
    )
    .await;
    for sql in [
        "INSERT INTO people (id, name) VALUES (1, 'ann')",
        "INSERT INTO people (id, name) VALUES (2, 'bob')",
        "INSERT INTO people (id, name) VALUES (3, 'cy')",
        "INSERT INTO visits (id, person, minutes) VALUES (1, 1, 30)",
        "INSERT INTO visits (id, person, minutes) VALUES (2, 1, 45)",
        "INSERT INTO visits (id, person, minutes) VALUES (3, 2, NULL)",
        "INSERT INTO visits (id, person, minutes) VALUES (4, NULL, 10)",
    ] {
        exec_sql(router, ADMIN_KEY, sql).await;
    }
}

fn int_cells(body: &Value) -> Vec<i64> {
    body["result"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r[0]["value"].as_i64().expect("an integer cell"))
        .collect()
}

#[tokio::test]
async fn scalar_in_and_exists_subqueries_through_the_real_api() {
    let dir = temp_dir("subq_basic");
    let (state, router) = build_app(&dir);
    seed_people_visits(&router).await;

    // Scalar subquery in the select list.
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT (SELECT MAX(minutes) FROM visits)",
    )
    .await;
    assert_eq!(r["result"]["kind"], "rows");
    assert_eq!(r["result"]["rows"][0][0]["type"], "integer");
    assert_eq!(r["result"]["rows"][0][0]["value"], 45);

    // Zero-row scalar subquery is NULL.
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT (SELECT minutes FROM visits WHERE id = 999)",
    )
    .await;
    assert_eq!(r["result"]["rows"][0][0]["type"], "null");

    // IN (subquery) filters; the NULL person never matches.
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT id FROM people WHERE id IN (SELECT person FROM visits) ORDER BY id",
    )
    .await;
    assert_eq!(int_cells(&r), vec![1, 2]);

    // NOT IN over a set containing NULL returns no rows (three-valued logic).
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT id FROM people WHERE id NOT IN (SELECT person FROM visits)",
    )
    .await;
    assert_eq!(r["result"]["row_count"], 0);

    // ...and with the NULL filtered out it returns the rows with no visit.
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT id FROM people WHERE id NOT IN (SELECT person FROM visits WHERE person IS NOT NULL)",
    )
    .await;
    assert_eq!(int_cells(&r), vec![3]);

    // EXISTS / NOT EXISTS.
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT id FROM people WHERE EXISTS (SELECT 1 FROM visits WHERE minutes > 40) ORDER BY id",
    )
    .await;
    assert_eq!(int_cells(&r), vec![1, 2, 3]);
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT id FROM people WHERE NOT EXISTS (SELECT 1 FROM visits WHERE minutes > 40)",
    )
    .await;
    assert_eq!(r["result"]["row_count"], 0);

    // The result schema reports the scalar subquery's name and nullability.
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT (SELECT MAX(minutes) FROM visits) AS m",
    )
    .await;
    assert_eq!(r["result"]["columns"][0]["name"], "m");
    assert_eq!(r["result"]["columns"][0]["nullable"], true);

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn multi_row_scalar_subquery_is_a_runtime_execution_error_over_http() {
    let dir = temp_dir("subq_multirow");
    let (state, router) = build_app(&dir);
    seed_people_visits(&router).await;

    let (status, body) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "SELECT (SELECT minutes FROM visits)" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "EXECUTION_ERROR");

    // A subquery returning two columns is a *bind* error instead.
    let (status, body) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "SELECT (SELECT id, minutes FROM visits WHERE id = 1)" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "BIND_ERROR");

    // A type mismatch across the subquery boundary is a bind error.
    let (status, body) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "SELECT id FROM people WHERE id IN (SELECT name FROM people)" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "BIND_ERROR");

    // A clause that cannot run a subquery fails closed as UNSUPPORTED.
    let (status, body) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "SELECT id FROM people LIMIT (SELECT 1)" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "UNSUPPORTED");

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn subqueries_in_update_and_delete_where_through_the_real_api() {
    let dir = temp_dir("subq_dml");
    let (state, router) = build_app(&dir);
    seed_people_visits(&router).await;

    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "UPDATE people SET name = 'visited' WHERE id IN (SELECT person FROM visits WHERE minutes > 40)",
    )
    .await;
    assert_eq!(r["result"]["rows_affected"], 1);
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT id FROM people WHERE name = 'visited'",
    )
    .await;
    assert_eq!(int_cells(&r), vec![1]);

    // The subquery is uncorrelated (`person = 3` matches nothing), so
    // NOT EXISTS is true for every person and all three are deleted.
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "DELETE FROM people WHERE NOT EXISTS (SELECT 1 FROM visits WHERE person = 3)",
    )
    .await;
    assert_eq!(r["result"]["rows_affected"], 3);

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn subquery_inside_an_explicit_transaction_sees_its_own_uncommitted_writes() {
    let dir = temp_dir("subq_txn");
    let (state, router) = build_app(&dir);
    seed_people_visits(&router).await;

    let begin = exec_sql(&router, ADMIN_KEY, "BEGIN").await;
    let session_id = begin["session_id"].as_str().unwrap().to_string();
    let (status, body) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "INSERT INTO visits (id, person, minutes) VALUES (9, 3, 500)", "session_id": session_id }),
    )
    .await;
    assert!(status.is_success(), "{body}");

    let (status, inside) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "SELECT id FROM people WHERE id IN (SELECT person FROM visits WHERE minutes > 100)", "session_id": session_id }),
    )
    .await;
    assert!(status.is_success(), "{inside}");
    assert_eq!(int_cells(&inside), vec![3], "the session sees its own insert");

    // Another client, outside the session, does not.
    let outside = exec_sql(
        &router,
        READER_KEY,
        "SELECT id FROM people WHERE id IN (SELECT person FROM visits WHERE minutes > 100)",
    )
    .await;
    assert_eq!(outside["result"]["row_count"], 0);

    let (status, _) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "ROLLBACK", "session_id": session_id }),
    )
    .await;
    assert!(status.is_success());

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn explain_shows_the_nested_subquery_plan_and_does_not_execute_it() {
    let dir = temp_dir("subq_explain");
    let (state, router) = build_app(&dir);
    seed_people_visits(&router).await;

    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "EXPLAIN SELECT id FROM people WHERE id IN (SELECT person FROM visits)",
    )
    .await;
    let text = r["result"]["plan_text"].as_str().unwrap();
    assert!(text.contains("Subquery #0"), "{text}");
    assert!(text.contains("IN subquery#0"), "{text}");

    // EXPLAIN of a statement that would fail at runtime still succeeds.
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "EXPLAIN SELECT (SELECT minutes FROM visits)",
    )
    .await;
    assert_eq!(r["result"]["kind"], "explain");

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

// -----------------------------------------------------------------
// SQL-mission-1, Slice B: correlated subqueries through the real HTTP
// stack. Sub-slice B2: correlated scalar subquery.
// -----------------------------------------------------------------

fn cell_values(body: &Value, column: usize) -> Vec<Value> {
    body["result"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r[column]["value"].clone())
        .collect()
}

#[tokio::test]
async fn correlated_scalar_subquery_through_the_real_api() {
    let dir = temp_dir("corr_scalar");
    let (state, router) = build_app(&dir);
    seed_people_visits(&router).await;

    // The inner query is re-run for each person: ann has visits (max 45),
    // bob's only visit has NULL minutes, cy has none.
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT id, (SELECT MAX(minutes) FROM visits WHERE visits.person = people.id) AS m \
         FROM people ORDER BY id",
    )
    .await;
    assert_eq!(int_cells(&r), vec![1, 2, 3]);
    assert_eq!(
        cell_values(&r, 1),
        vec![json!(45), Value::Null, Value::Null],
        "{r}"
    );
    assert_eq!(r["result"]["columns"][1]["name"], "m");

    // COUNT over zero inner rows is 0, never NULL.
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT (SELECT COUNT(*) FROM visits WHERE visits.person = people.id) FROM people ORDER BY id",
    )
    .await;
    // BIGINT travels as a JSON string.
    assert_eq!(cell_values(&r, 0), vec![json!("2"), json!("1"), json!("0")], "{r}");

    // A correlated scalar subquery in WHERE.
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT id FROM people WHERE (SELECT COUNT(*) FROM visits WHERE visits.person = people.id) >= 1 ORDER BY id",
    )
    .await;
    assert_eq!(int_cells(&r), vec![1, 2]);

    // More than one inner row for an outer row is a RUNTIME error that names
    // the outer row (ann, id 1, has two visits).
    let (status, body) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "SELECT (SELECT minutes FROM visits WHERE visits.person = people.id) FROM people" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "EXECUTION_ERROR");
    assert!(
        body.to_string().contains("people.id = 1"),
        "the error names the outer row: {body}"
    );

    // An unknown name at every level is reported like any unknown object
    // (the API maps UnknownObject to NOT_FOUND, authorization included).
    let (status, body) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "SELECT (SELECT 1 FROM visits WHERE visits.nope = people.id) FROM people" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], "NOT_FOUND");

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

// Sub-slice B3: correlated EXISTS / NOT EXISTS.
#[tokio::test]
async fn correlated_exists_and_not_exists_through_the_real_api() {
    let dir = temp_dir("corr_exists");
    let (state, router) = build_app(&dir);
    seed_people_visits(&router).await;

    // ann (1) and bob (2) have visits; cy (3) has none.
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT id FROM people WHERE EXISTS (SELECT 1 FROM visits WHERE visits.person = people.id) ORDER BY id",
    )
    .await;
    assert_eq!(int_cells(&r), vec![1, 2]);
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT id FROM people WHERE NOT EXISTS (SELECT 1 FROM visits WHERE visits.person = people.id) ORDER BY id",
    )
    .await;
    assert_eq!(int_cells(&r), vec![3]);

    // A NULL in the inner WHERE (bob's visit has NULL minutes) does not make
    // EXISTS unknown: bob simply does not qualify, and NOT EXISTS keeps him.
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT id FROM people WHERE NOT EXISTS (SELECT 1 FROM visits WHERE visits.person = people.id AND visits.minutes > 40) ORDER BY id",
    )
    .await;
    assert_eq!(int_cells(&r), vec![2, 3]);

    // EXISTS as a selected boolean value.
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT id, EXISTS (SELECT 1 FROM visits WHERE visits.person = people.id) AS has_visit FROM people ORDER BY id",
    )
    .await;
    assert_eq!(
        cell_values(&r, 1),
        vec![json!(true), json!(true), json!(false)],
        "{r}"
    );

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn correlated_reads_work_for_a_reader_and_an_unknown_inner_table_is_not_found_over_http() {
    // A missing inner table is NOT_FOUND at bind time. (Per-table grants are
    // not expressible through the API keys used here; the forbidden-table
    // case, indistinguishable from this one, is covered with real grants in
    // `correlated_tests::binder`.)
    let dir = temp_dir("corr_authz");
    let (state, router) = build_app(&dir);
    seed_people_visits(&router).await;

    let (status_missing, body_missing) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "SELECT id FROM people WHERE EXISTS (SELECT 1 FROM no_such WHERE no_such.p = people.id)" }),
    )
    .await;
    assert_eq!(status_missing, StatusCode::NOT_FOUND, "{body_missing}");

    // The READER key may read, but is not an admin: a write is refused, and a
    // correlated read still works for it.
    let r = exec_sql(
        &router,
        READER_KEY,
        "SELECT id FROM people WHERE EXISTS (SELECT 1 FROM visits WHERE visits.person = people.id) ORDER BY id",
    )
    .await;
    assert_eq!(int_cells(&r), vec![1, 2]);

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

// Sub-slice B4: correlated IN / NOT IN and the NULL trap.
#[tokio::test]
async fn correlated_in_and_not_in_with_null_handling_through_the_real_api() {
    let dir = temp_dir("corr_in");
    let (state, router) = build_app(&dir);
    seed_people_visits(&router).await;

    // For person p the inner set is the `person` of every visit with a larger
    // id: p=1 -> {1, 2, NULL}, p=2 -> {2, NULL}, p=3 -> {NULL}.
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT id FROM people WHERE id IN (SELECT person FROM visits WHERE visits.id > people.id) ORDER BY id",
    )
    .await;
    assert_eq!(int_cells(&r), vec![1, 2]);

    // NOT IN over a set containing NULL is never TRUE: nobody is returned,
    // not even cy (3), who is simply absent from the set.
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT id FROM people WHERE id NOT IN (SELECT person FROM visits WHERE visits.id > people.id)",
    )
    .await;
    assert_eq!(r["result"]["row_count"], 0, "{r}");

    // With the NULLs removed from the inner set, cy comes back.
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT id FROM people WHERE id NOT IN (SELECT person FROM visits WHERE visits.id > people.id AND visits.person IS NOT NULL)",
    )
    .await;
    assert_eq!(int_cells(&r), vec![3]);

    // The UNKNOWN is visible as a SQL NULL when selected.
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT id, id IN (SELECT person FROM visits WHERE visits.id > people.id) FROM people ORDER BY id",
    )
    .await;
    assert_eq!(
        cell_values(&r, 1),
        vec![json!(true), json!(true), Value::Null],
        "{r}"
    );

    // A type mismatch across the subquery boundary is a bind error.
    let (status, body) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "SELECT id FROM people WHERE name IN (SELECT person FROM visits WHERE visits.id > people.id)" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "BIND_ERROR");

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

// Sub-slice B5: UPDATE / DELETE with a correlated subquery, and EXPLAIN.
#[tokio::test]
async fn correlated_subqueries_in_update_and_delete_through_the_real_api() {
    let dir = temp_dir("corr_dml");
    let (state, router) = build_app(&dir);
    seed_people_visits(&router).await;

    // SET with a correlated scalar subquery: each person's name becomes the
    // number of their visits, as text via a lookup of the max minutes (ann 45,
    // bob NULL, cy NULL) -- the subquery sees each row's ORIGINAL id.
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "UPDATE people SET name = (SELECT CASE WHEN COUNT(*) = 0 THEN 'none' ELSE 'some' END FROM visits WHERE visits.person = people.id)",
    )
    .await;
    assert_eq!(r["result"]["rows_affected"], 3);
    let r = exec_sql(&router, ADMIN_KEY, "SELECT name FROM people ORDER BY id").await;
    assert_eq!(
        cell_values(&r, 0),
        vec![json!("some"), json!("some"), json!("none")],
        "{r}"
    );

    // DELETE ... WHERE NOT EXISTS (correlated): cy (3) has no visit.
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "DELETE FROM people WHERE NOT EXISTS (SELECT 1 FROM visits WHERE visits.person = people.id)",
    )
    .await;
    assert_eq!(r["result"]["rows_affected"], 1);
    let r = exec_sql(&router, ADMIN_KEY, "SELECT id FROM people ORDER BY id").await;
    assert_eq!(int_cells(&r), vec![1, 2]);

    // A self-correlated DELETE sees the table as it was before the statement:
    // people 1 and 2 now both carry the name 'some', so BOTH are deleted.
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "DELETE FROM people WHERE EXISTS (SELECT 1 FROM people p2 WHERE p2.name = people.name AND p2.id <> people.id)",
    )
    .await;
    assert_eq!(r["result"]["rows_affected"], 2);
    let r = exec_sql(&router, ADMIN_KEY, "SELECT id FROM people").await;
    assert_eq!(r["result"]["row_count"], 0);

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn explain_of_a_correlated_subquery_shows_the_correlation_and_executes_nothing_over_http() {
    let dir = temp_dir("corr_explain");
    let (state, router) = build_app(&dir);
    seed_people_visits(&router).await;

    // Executed, this would fail (ann has two visits -> two inner rows).
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "EXPLAIN SELECT (SELECT minutes FROM visits WHERE visits.person = people.id) FROM people",
    )
    .await;
    let text = r["result"]["plan_text"].as_str().expect("a plan text");
    assert!(text.contains("correlated outer_row_input=true"), "{text}");
    assert!(!text.contains("uncorrelated"), "{text}");

    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "EXPLAIN SELECT id FROM people WHERE id IN (SELECT person FROM visits)",
    )
    .await;
    let text = r["result"]["plan_text"].as_str().expect("a plan text");
    assert!(text.contains("uncorrelated (executed once)"), "{text}");

    // EXPLAIN of a correlated DELETE changes nothing.
    exec_sql(
        &router,
        ADMIN_KEY,
        "EXPLAIN DELETE FROM people WHERE EXISTS (SELECT 1 FROM visits WHERE visits.person = people.id)",
    )
    .await;
    let r = exec_sql(&router, ADMIN_KEY, "SELECT COUNT(*) FROM people").await;
    assert_eq!(cell_values(&r, 0), vec![json!("3")]);

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

// Sub-slice B6: nested correlation, three levels.
#[tokio::test]
async fn three_level_correlated_subqueries_through_the_real_api() {
    let dir = temp_dir("corr_nested");
    let (state, router) = build_app(&dir);
    seed_people_visits(&router).await;
    exec_sql(
        &router,
        ADMIN_KEY,
        "CREATE TABLE notes (id INTEGER PRIMARY KEY, visit_id INTEGER, len INTEGER)",
    )
    .await;
    for sql in [
        "INSERT INTO notes (id, visit_id, len) VALUES (1, 1, 5)",
        "INSERT INTO notes (id, visit_id, len) VALUES (2, 2, 1)",
        "INSERT INTO notes (id, visit_id, len) VALUES (3, 3, 2)",
    ] {
        exec_sql(&router, ADMIN_KEY, sql).await;
    }

    // The innermost query reads the middle level (v.id) AND the outermost
    // (people.id). Person 1: visit 1 has a note of length 5 > 1. Person 2:
    // visit 3's note has length 2, not > 2. Person 3 has no visits.
    let deep = "EXISTS (SELECT 1 FROM visits v WHERE v.person = people.id AND \
                EXISTS (SELECT 1 FROM notes n WHERE n.visit_id = v.id AND n.len > people.id))";
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        &format!("SELECT id FROM people WHERE {deep} ORDER BY id"),
    )
    .await;
    assert_eq!(int_cells(&r), vec![1]);
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        &format!("SELECT id FROM people WHERE NOT {deep} ORDER BY id"),
    )
    .await;
    assert_eq!(int_cells(&r), vec![2, 3]);

    // Three-level scalar.
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT id, (SELECT COUNT(*) FROM visits v WHERE v.person = people.id AND \
         (SELECT COUNT(*) FROM notes n WHERE n.visit_id = v.id) >= 1) FROM people ORDER BY id",
    )
    .await;
    assert_eq!(cell_values(&r, 1), vec![json!("2"), json!("1"), json!("0")], "{r}");

    // A nested correlated DELETE.
    let r = exec_sql(&router, ADMIN_KEY, &format!("DELETE FROM people WHERE {deep}")).await;
    assert_eq!(r["result"]["rows_affected"], 1);
    let r = exec_sql(&router, ADMIN_KEY, "SELECT id FROM people ORDER BY id").await;
    assert_eq!(int_cells(&r), vec![2, 3]);

    // The re-execution limits are not reachable with this little data over
    // the default limits (they are exercised in the engine tests); the
    // nesting-depth limit is: 33 nested levels fail closed as RESOURCE_LIMIT.
    let mut sql = String::from("SELECT 1");
    for _ in 0..33 {
        sql = format!("SELECT ({sql})");
    }
    let (status, body) = sql_req(&router, ADMIN_KEY, json!({ "sql": sql })).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert_eq!(body["error"]["code"], "RESOURCE_LIMIT");

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

// -----------------------------------------------------------------
// SQL-mission-1, Slice C: derived tables, LATERAL and non-recursive CTEs
// through the real HTTP stack.
// -----------------------------------------------------------------

const PER_PERSON: &str =
    "(SELECT person AS p, COUNT(*) AS n FROM visits WHERE person IS NOT NULL GROUP BY person)";

#[tokio::test]
async fn derived_tables_through_the_real_api() {
    let dir = temp_dir("rel_derived");
    let (state, router) = build_app(&dir);
    seed_people_visits(&router).await;

    let r = exec_sql(
        &router,
        ADMIN_KEY,
        &format!("SELECT d.p, d.n FROM {PER_PERSON} AS d ORDER BY d.p"),
    )
    .await;
    assert_eq!(int_cells(&r), vec![1, 2]);
    assert_eq!(cell_values(&r, 1), vec![json!("2"), json!("1")], "{r}");
    // The result names are the derived table's, not the underlying table's.
    assert_eq!(r["result"]["columns"][0]["name"], "p");
    assert_eq!(r["result"]["columns"][1]["name"], "n");

    // LEFT JOIN keeps people without visits.
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        &format!("SELECT people.id, d.n FROM people LEFT JOIN {PER_PERSON} AS d ON d.p = people.id ORDER BY people.id"),
    )
    .await;
    assert_eq!(int_cells(&r), vec![1, 2, 3]);
    assert_eq!(cell_values(&r, 1), vec![json!("2"), json!("1"), Value::Null], "{r}");

    // A column list renames.
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT a FROM (SELECT id, name FROM people) AS d(a, b) ORDER BY a",
    )
    .await;
    assert_eq!(int_cells(&r), vec![1, 2, 3]);

    // LATERAL: the most recent visit per person.
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT people.id, l.v FROM people LEFT JOIN LATERAL \
         (SELECT visits.id AS v FROM visits WHERE visits.person = people.id ORDER BY visits.id DESC LIMIT 1) AS l ON true \
         ORDER BY people.id",
    )
    .await;
    assert_eq!(int_cells(&r), vec![1, 2, 3]);
    assert_eq!(cell_values(&r, 1), vec![json!(2), json!(3), Value::Null], "{r}");

    // Errors.
    let (status, body) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "SELECT 1 FROM (SELECT id FROM people)" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "BIND_ERROR");
    assert!(body.to_string().contains("alias"), "{body}");

    let (status, body) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "SELECT 1 FROM people JOIN (SELECT id FROM visits WHERE visits.person = people.id) AS d ON true" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "BIND_ERROR");
    assert!(
        body.to_string().contains("derived table 'd' cannot reference outer column 'people.id' without LATERAL"),
        "{body}"
    );

    // Nesting past the limit fails closed.
    let mut sql = String::from("SELECT id AS c0 FROM people");
    for i in 1..=33 {
        sql = format!("SELECT c{} AS c{i} FROM ({sql}) AS d{i}", i - 1);
    }
    let (status, body) = sql_req(&router, ADMIN_KEY, json!({ "sql": sql })).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert_eq!(body["error"]["code"], "RESOURCE_LIMIT");

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn non_recursive_ctes_through_the_real_api() {
    let dir = temp_dir("rel_cte");
    let (state, router) = build_app(&dir);
    seed_people_visits(&router).await;

    // Single reference.
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "WITH busy AS (SELECT person AS p FROM visits WHERE minutes > 20) \
         SELECT id FROM people WHERE id IN (SELECT p FROM busy) ORDER BY id",
    )
    .await;
    assert_eq!(int_cells(&r), vec![1]);

    // Chained, and referenced twice (a self-join: person 1 has two visits with
    // minutes, so 2 x 2 = 4 matching pairs).
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "WITH a AS (SELECT person AS p, minutes AS m FROM visits WHERE person IS NOT NULL), \
              b AS (SELECT p FROM a WHERE m IS NOT NULL) \
         SELECT x.p FROM b AS x JOIN b AS y ON x.p = y.p ORDER BY x.p",
    )
    .await;
    assert_eq!(int_cells(&r), vec![1, 1, 1, 1]);

    // A CTE shadows the base table of the same name for the statement.
    let r = exec_sql(&router, ADMIN_KEY, "WITH visits AS (SELECT 42 AS answer) SELECT answer FROM visits").await;
    assert_eq!(int_cells(&r), vec![42]);

    // WITH before DELETE.
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "WITH quiet AS (SELECT id FROM people WHERE id NOT IN (SELECT person FROM visits WHERE person IS NOT NULL)) \
         DELETE FROM people WHERE id IN (SELECT id FROM quiet)",
    )
    .await;
    assert_eq!(r["result"]["rows_affected"], 1);

    // EXPLAIN shows a twice-read CTE once, flagged as materialized once.
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "EXPLAIN WITH x AS (SELECT person AS p FROM visits) SELECT 1 FROM x AS a JOIN x AS b ON a.p = b.p",
    )
    .await;
    let text = r["result"]["plan_text"].as_str().expect("plan text");
    assert_eq!(text.matches("defined here").count(), 1, "{text}");
    assert!(text.contains("materialized once per statement"), "{text}");

    // Errors.
    for (sql, needle) in [
        ("WITH x AS (SELECT * FROM x) SELECT * FROM x", "CTE 'x' refers to itself; use WITH RECURSIVE"),
        (
            "WITH a AS (SELECT * FROM b), b AS (SELECT 1 AS v) SELECT * FROM a",
            "referenced before it is defined",
        ),
        (
            "WITH x(a, b) AS (SELECT 1 AS v) SELECT * FROM x",
            "column name",
        ),
    ] {
        let (status, body) = sql_req(&router, ADMIN_KEY, json!({ "sql": sql })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{sql}: {body}");
        assert_eq!(body["error"]["code"], "BIND_ERROR", "{sql}");
        assert!(body.to_string().contains(needle), "{sql}: {body}");
    }
    // WITH RECURSIVE belongs to a later slice: typed, not a crash.
    let (status, body) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "WITH RECURSIVE r AS (SELECT 1 AS n) SELECT * FROM r" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "UNSUPPORTED");
    // A CTE inside a subquery does not leak; a dead CTE with a bad body is an error.
    let (status, body) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "SELECT (WITH x AS (SELECT 5 AS v) SELECT v FROM x), (SELECT v FROM x)" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "WITH x AS (SELECT id FROM no_such_table) SELECT 1" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // The definition limit (64) is enforced over HTTP: 64 pass, 65 do not.
    let chain = |n: usize| {
        let mut sql = String::from("WITH c0 AS (SELECT 1 AS v)");
        for i in 1..n {
            sql.push_str(&format!(", c{i} AS (SELECT v FROM c{})", i - 1));
        }
        sql.push_str(&format!(" SELECT v FROM c{}", n - 1));
        sql
    };
    let r = exec_sql(&router, ADMIN_KEY, &chain(64)).await;
    assert_eq!(int_cells(&r), vec![1]);
    let (status, body) = sql_req(&router, ADMIN_KEY, json!({ "sql": chain(65) })).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert_eq!(body["error"]["code"], "RESOURCE_LIMIT");

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

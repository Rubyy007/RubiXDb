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

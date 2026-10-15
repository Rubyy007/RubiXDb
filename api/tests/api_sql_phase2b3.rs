//! SQL Phase 2b + 3 over HTTP (`POST /v1/sql`): `ALTER TABLE` and foreign keys through the
//! real router, engine and `rubixdb-sql` pipeline -- no mocking. Helpers are repeated from
//! `api_sql_phase2a.rs` because each integration-test file is its own crate.

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

async fn done(state: Arc<AppState>, dir: PathBuf) {
    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

async fn parent_child(tag: &str, actions: &str) -> (Arc<AppState>, Router, PathBuf) {
    let dir = temp_dir(tag);
    let (state, router) = build_app(&dir);
    exec_sql(
        &router,
        ADMIN_KEY,
        "CREATE TABLE parent (id INTEGER PRIMARY KEY, name TEXT)",
    )
    .await;
    exec_sql(
        &router,
        ADMIN_KEY,
        &format!(
            "CREATE TABLE child (id INTEGER PRIMARY KEY, pid INTEGER, \
             CONSTRAINT child_pid_fk FOREIGN KEY (pid) REFERENCES parent (id) {actions})"
        ),
    )
    .await;
    (state, router, dir)
}

#[tokio::test]
async fn alter_table_over_http_reports_a_ddl_result_and_changes_the_schema() {
    let dir = temp_dir("p23_http_alter");
    let (state, router) = build_app(&dir);
    exec_sql(
        &router,
        ADMIN_KEY,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT)",
    )
    .await;
    exec_sql(
        &router,
        ADMIN_KEY,
        "INSERT INTO t VALUES (1, 'x'), (2, 'y')",
    )
    .await;
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "ALTER TABLE t ADD COLUMN score INTEGER DEFAULT 5",
    )
    .await;
    assert_eq!(r["result"]["kind"], "ddl", "{r}");
    assert_eq!(r["result"]["statement"], "ALTER TABLE", "{r}");
    let r = exec_sql(&router, ADMIN_KEY, "SELECT id, score FROM t ORDER BY id").await;
    assert_eq!(table(&r), rows_of(&[&["1", "5"], &["2", "5"]]));
    exec_sql(
        &router,
        ADMIN_KEY,
        "ALTER TABLE t RENAME COLUMN score TO points",
    )
    .await;
    let r = exec_sql(&router, ADMIN_KEY, "SELECT points FROM t WHERE id = 2").await;
    assert_eq!(table(&r), rows_of(&[&["5"]]));
    done(state, dir).await;
}

#[tokio::test]
async fn invalid_alters_map_to_client_errors() {
    let dir = temp_dir("p23_http_alter_err");
    let (state, router) = build_app(&dir);
    exec_sql(
        &router,
        ADMIN_KEY,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT)",
    )
    .await;
    exec_sql(&router, ADMIN_KEY, "INSERT INTO t VALUES (1, NULL)").await;
    // DROP COLUMN: refused at parse time as unsupported
    let (status, body) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "ALTER TABLE t DROP COLUMN a" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "UNSUPPORTED", "{body}");
    // SET NOT NULL over a NULL: a 400 execution error that says why
    let (status, body) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "ALTER TABLE t ALTER COLUMN a SET NOT NULL" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "EXECUTION_ERROR", "{body}");
    // an unknown table
    let (status, body) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "ALTER TABLE nope ADD COLUMN x INTEGER" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    // a reader key may not alter
    let (status, _) = sql_req(
        &router,
        READER_KEY,
        json!({ "sql": "ALTER TABLE t ADD COLUMN x INTEGER" }),
    )
    .await;
    assert!(
        status == StatusCode::NOT_FOUND || status == StatusCode::FORBIDDEN,
        "{status}"
    );
    done(state, dir).await;
}

#[tokio::test]
async fn a_foreign_key_violation_is_a_409_with_its_own_code_and_no_row_values() {
    let (state, router, dir) = parent_child("p23_http_fk", "").await;
    exec_sql(&router, ADMIN_KEY, "INSERT INTO parent VALUES (1, 'a')").await;
    exec_sql(&router, ADMIN_KEY, "INSERT INTO child VALUES (10, 1)").await;
    let (status, body) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "INSERT INTO child VALUES (11, 4242)" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "FOREIGN_KEY_VIOLATION", "{body}");
    let detail = body["error"]["detail"].as_str().unwrap();
    assert!(detail.contains("child_pid_fk"), "{detail}");
    assert!(!detail.contains("4242"), "{detail}");
    assert!(body["error"]["hint"].is_string(), "{body}");
    // deleting the referenced parent: the same code, the rows are untouched
    let (status, body) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "DELETE FROM parent WHERE id = 1" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "FOREIGN_KEY_VIOLATION", "{body}");
    let r = exec_sql(&router, ADMIN_KEY, "SELECT COUNT(*) FROM parent").await;
    assert_eq!(table(&r), rows_of(&[&["1"]]));
    done(state, dir).await;
}

#[tokio::test]
async fn cascades_and_returning_over_http() {
    let (state, router, dir) =
        parent_child("p23_http_cascade", "ON DELETE CASCADE ON UPDATE CASCADE").await;
    exec_sql(
        &router,
        ADMIN_KEY,
        "INSERT INTO parent VALUES (1, 'a'), (2, 'b')",
    )
    .await;
    exec_sql(
        &router,
        ADMIN_KEY,
        "INSERT INTO child VALUES (10, 1), (11, 1), (20, 2)",
    )
    .await;
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "UPDATE parent SET id = 5 WHERE id = 2 RETURNING id",
    )
    .await;
    assert_eq!(table(&r), rows_of(&[&["5"]]));
    let r = exec_sql(&router, ADMIN_KEY, "SELECT pid FROM child WHERE id = 20").await;
    assert_eq!(table(&r), rows_of(&[&["5"]]));
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "DELETE FROM parent WHERE id = 1 RETURNING id",
    )
    .await;
    assert_eq!(table(&r), rows_of(&[&["1"]]));
    let r = exec_sql(&router, ADMIN_KEY, "SELECT id FROM child ORDER BY id").await;
    assert_eq!(table(&r), rows_of(&[&["20"]]));
    done(state, dir).await;
}

#[tokio::test]
async fn foreign_keys_inside_a_session_transaction_are_checked_at_the_statement_and_at_commit() {
    let (state, router, dir) = parent_child("p23_http_session", "").await;
    exec_sql(&router, ADMIN_KEY, "INSERT INTO parent VALUES (1, 'a')").await;
    let (status, body) = sql_req(&router, ADMIN_KEY, json!({ "sql": "BEGIN" })).await;
    assert!(status.is_success(), "{body}");
    let session = body["session_id"].as_str().unwrap().to_string();
    let (status, body) = in_session(
        &router,
        ADMIN_KEY,
        &session,
        "INSERT INTO child VALUES (10, 1)",
    )
    .await;
    assert!(status.is_success(), "{body}");
    // an immediate violation inside the session: that statement fails, the session lives
    let (status, body) = in_session(
        &router,
        ADMIN_KEY,
        &session,
        "INSERT INTO child VALUES (11, 99)",
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "FOREIGN_KEY_VIOLATION", "{body}");
    // the parent is deleted by someone else while the transaction is open ...
    exec_sql(&router, ADMIN_KEY, "DELETE FROM parent WHERE id = 1").await;
    // ... so the commit is refused
    let (status, body) = in_session(&router, ADMIN_KEY, &session, "COMMIT").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "FOREIGN_KEY_VIOLATION", "{body}");
    let r = exec_sql(&router, ADMIN_KEY, "SELECT COUNT(*) FROM child").await;
    assert_eq!(table(&r), rows_of(&[&["0"]]));
    done(state, dir).await;
}

#[tokio::test]
async fn dropping_a_referenced_table_is_a_client_error_over_sql() {
    let (state, router, dir) = parent_child("p23_http_drop", "").await;
    let (status, body) = sql_req(&router, ADMIN_KEY, json!({ "sql": "DROP TABLE parent" })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "EXECUTION_ERROR", "{body}");
    assert!(
        body["error"]["detail"]
            .as_str()
            .unwrap()
            .contains("child_pid_fk"),
        "{body}"
    );
    done(state, dir).await;
}

/// The REST catalog routes drop through the same catalog primitives: a referenced table or a
/// unique index a key resolves to is a 400 that names the constraint, not a 500.
#[tokio::test]
async fn rest_drops_of_a_referenced_table_and_a_referenced_unique_index_are_client_errors() {
    let (state, router, dir) = parent_child("p23_http_rest_drop", "").await;
    exec_sql(
        &router,
        ADMIN_KEY,
        "CREATE TABLE p2 (id INTEGER PRIMARY KEY, code TEXT)",
    )
    .await;
    exec_sql(
        &router,
        ADMIN_KEY,
        "CREATE UNIQUE INDEX p2_code ON p2 (code)",
    )
    .await;
    exec_sql(&router, ADMIN_KEY, "ALTER TABLE child ADD COLUMN code TEXT").await;
    exec_sql(
        &router,
        ADMIN_KEY,
        "ALTER TABLE child ADD FOREIGN KEY (code) REFERENCES p2 (code)",
    )
    .await;
    let catalog = &state.sql.catalog;
    let schema = catalog
        .list_schemas(catalog.list_databases().unwrap()[0].database_id)
        .unwrap()[0]
        .clone();
    let table_of = |name: &str| {
        catalog
            .get_table_by_name(schema.schema_id, name)
            .unwrap()
            .unwrap()
    };
    let delete = |uri: String, body: Value| {
        let router = router.clone();
        async move {
            let req = Request::builder()
                .method("DELETE")
                .uri(uri)
                .header("Authorization", format!("Bearer {ADMIN_KEY}"))
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap();
            let response = router.oneshot(req).await.unwrap();
            let status = response.status();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            (
                status,
                serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null),
            )
        }
    };
    let parent = table_of("parent");
    let (status, body) = delete(
        format!("/v1/catalog/tables/by-id/{}", parent.table_id),
        json!({ "schema_name": schema.name, "table_name": "parent" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["detail"]
            .as_str()
            .unwrap()
            .contains("child_pid_fk"),
        "{body}"
    );
    let p2 = table_of("p2");
    let index = catalog
        .list_indexes(p2.table_id)
        .unwrap()
        .into_iter()
        .find(|i| i.name == "p2_code")
        .unwrap();
    let (status, body) = delete(
        format!("/v1/catalog/indexes/{}", index.index_id),
        json!({ "schema_name": schema.name, "table_name": "p2", "index_name": "p2_code" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["detail"]
            .as_str()
            .unwrap()
            .contains("foreign key"),
        "{body}"
    );
    done(state, dir).await;
}

#[tokio::test]
async fn concurrent_http_children_and_parent_deletes_leave_no_orphan() {
    let (state, router, dir) = parent_child("p23_http_race", "").await;
    for p in 0..10 {
        exec_sql(
            &router,
            ADMIN_KEY,
            &format!("INSERT INTO parent VALUES ({p}, 'p')"),
        )
        .await;
    }
    let mut tasks = Vec::new();
    for w in 0..3 {
        let router = router.clone();
        tasks.push(tokio::spawn(async move {
            for i in 0..15 {
                let p = (i * 7 + w) % 10;
                let _ = sql_req(
                    &router,
                    ADMIN_KEY,
                    json!({ "sql": format!("INSERT INTO child VALUES ({}, {p})", w * 1000 + i) }),
                )
                .await;
            }
        }));
    }
    {
        let router = router.clone();
        tasks.push(tokio::spawn(async move {
            for round in 0..20 {
                let p = (round * 3) % 10;
                let _ = sql_req(
                    &router,
                    ADMIN_KEY,
                    json!({ "sql": format!("DELETE FROM parent WHERE id = {p}") }),
                )
                .await;
                let _ = sql_req(
                    &router,
                    ADMIN_KEY,
                    json!({ "sql": format!("INSERT INTO parent VALUES ({p}, 'again')") }),
                )
                .await;
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT COUNT(*) FROM child WHERE pid NOT IN (SELECT id FROM parent)",
    )
    .await;
    assert_eq!(table(&r), rows_of(&[&["0"]]));
    done(state, dir).await;
}

#[tokio::test]
async fn alter_table_and_foreign_keys_survive_a_restart() {
    let dir = temp_dir("p23_http_restart");
    {
        let (state, router) = build_app(&dir);
        exec_sql(
            &router,
            ADMIN_KEY,
            "CREATE TABLE parent (id INTEGER PRIMARY KEY)",
        )
        .await;
        exec_sql(
            &router,
            ADMIN_KEY,
            "CREATE TABLE child (id INTEGER PRIMARY KEY, pid INTEGER)",
        )
        .await;
        exec_sql(&router, ADMIN_KEY, "INSERT INTO parent VALUES (1), (2)").await;
        exec_sql(
            &router,
            ADMIN_KEY,
            "INSERT INTO child VALUES (10, 1), (11, 2)",
        )
        .await;
        exec_sql(
            &router,
            ADMIN_KEY,
            "ALTER TABLE child ADD CONSTRAINT child_fk FOREIGN KEY (pid) REFERENCES parent (id) ON DELETE CASCADE",
        )
        .await;
        exec_sql(
            &router,
            ADMIN_KEY,
            "ALTER TABLE child ADD COLUMN note TEXT DEFAULT 'n'",
        )
        .await;
        exec_sql(&router, ADMIN_KEY, "DELETE FROM parent WHERE id = 2").await;
        state.engine.shutdown();
    }
    let (state, router) = build_app(&dir);
    let r = exec_sql(&router, ADMIN_KEY, "SELECT id, pid, note FROM child").await;
    assert_eq!(table(&r), rows_of(&[&["10", "1", "n"]]));
    let (status, body) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "INSERT INTO child (id, pid) VALUES (12, 99)" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "FOREIGN_KEY_VIOLATION", "{body}");
    done(state, dir).await;
}

#[tokio::test]
async fn check_constraints_over_http_are_enforced_with_their_own_error_code() {
    let dir = temp_dir("p23_http_check");
    let (state, router) = build_app(&dir);
    exec_sql(
        &router,
        ADMIN_KEY,
        "CREATE TABLE acct (id INTEGER PRIMARY KEY, balance INTEGER, CONSTRAINT balance_ok CHECK (balance >= 0))",
    )
    .await;
    exec_sql(&router, ADMIN_KEY, "INSERT INTO acct VALUES (1, 10)").await;
    let (status, body) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "INSERT INTO acct VALUES (2, -987)" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "CHECK_VIOLATION", "{body}");
    let detail = body["error"]["detail"].as_str().unwrap();
    assert!(
        detail.contains("balance_ok") && !detail.contains("987"),
        "{detail}"
    );
    // ADD CONSTRAINT validates the existing rows
    exec_sql(
        &router,
        ADMIN_KEY,
        "ALTER TABLE acct ADD COLUMN age INTEGER DEFAULT 5",
    )
    .await;
    let (status, body) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "ALTER TABLE acct ADD CONSTRAINT age_ok CHECK (age > 10)" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "EXECUTION_ERROR", "{body}");
    exec_sql(
        &router,
        ADMIN_KEY,
        "ALTER TABLE acct ADD CONSTRAINT age_ok CHECK (age > 1)",
    )
    .await;
    exec_sql(
        &router,
        ADMIN_KEY,
        "ALTER TABLE acct DROP CONSTRAINT balance_ok",
    )
    .await;
    exec_sql(&router, ADMIN_KEY, "INSERT INTO acct VALUES (2, -987, 7)").await;
    done(state, dir).await;
}

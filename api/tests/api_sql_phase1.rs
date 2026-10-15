//! SQL Phase 1 over HTTP (`POST /v1/sql`): the correctness gaps closed by
//! `docs/SNOWFLAKE_SQL_COMPATIBILITY.md` Phase 1 (ORDER BY aliases / positions,
//! `MINUS`, `UNION BY NAME`, `COUNT(a, b)`, `GROUP BY ALL`, `ANY` / `ALL`,
//! `GROUPS` frames, `IGNORE NULLS`, `DISTINCT` window aggregates) through the
//! real router, engine and `rubixdb-sql` pipeline -- the same no-mocking
//! discipline as `api_sql_mission2.rs` (whose helpers are repeated here because
//! each integration-test file is its own crate).

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

fn strs(rows: &[Vec<String>], col: usize) -> Vec<&str> {
    rows.iter().map(|r| r[col].as_str()).collect()
}

/// `w(id INTEGER PK, g INTEGER, o INTEGER, x INTEGER, d DOUBLE)` with five rows:
/// `(id, g, o, x)` = (1, 1, 1, 10) (2, 1, 2, 20) (3, 2, 1, 30) (4, 2, 2, 40) (5, NULL, 1, NULL).
async fn seeded(tag: &str) -> (Arc<AppState>, Router, PathBuf) {
    let dir = temp_dir(tag);
    let (state, router) = build_app(&dir);
    exec_sql(
        &router,
        ADMIN_KEY,
        "CREATE TABLE w (id INTEGER PRIMARY KEY, g INTEGER, o INTEGER, x INTEGER, d DOUBLE)",
    )
    .await;
    exec_sql(
        &router,
        ADMIN_KEY,
        "INSERT INTO w VALUES (1, 1, 1, 10, 10.0), (2, 1, 2, 20, 20.0), (3, 2, 1, 30, 30.0), (4, 2, 2, 40, 40.0), (5, NULL, 1, NULL, NULL)",
    )
    .await;
    (state, router, dir)
}

async fn done(state: Arc<AppState>, dir: PathBuf) {
    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn order_by_positions_and_aliases_over_http() {
    let (state, router, dir) = seeded("p1_order").await;
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT id, x AS val FROM w ORDER BY 2 DESC, 1",
    )
    .await;
    // x DESC with the default NULLS FIRST: id 5 (NULL), then 40, 30, 20, 10
    assert_eq!(strs(&table(&r), 0), ["5", "4", "3", "2", "1"]);
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT id, x AS val FROM w ORDER BY val",
    )
    .await;
    assert_eq!(strs(&table(&r), 0), ["1", "2", "3", "4", "5"]);
    let (status, body) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "SELECT id FROM w ORDER BY 2" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "BIND_ERROR", "{body}");
    done(state, dir).await;
}

#[tokio::test]
async fn minus_and_union_by_name_over_http() {
    let (state, router, dir) = seeded("p1_setops").await;
    let a = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT g FROM w MINUS SELECT g FROM w WHERE id > 2 ORDER BY 1",
    )
    .await;
    let b = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT g FROM w EXCEPT SELECT g FROM w WHERE id > 2 ORDER BY 1",
    )
    .await;
    assert_eq!(table(&a), table(&b));
    assert_eq!(strs(&table(&a), 0), ["1"]);
    exec_sql(
        &router,
        ADMIN_KEY,
        "CREATE TABLE b (k INTEGER PRIMARY KEY, id INTEGER, label TEXT)",
    )
    .await;
    exec_sql(&router, ADMIN_KEY, "INSERT INTO b VALUES (1, 9, 'nine')").await;
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT id, x FROM w WHERE id = 1 UNION ALL BY NAME SELECT label, id FROM b ORDER BY id",
    )
    .await;
    assert_eq!(r["result"]["columns"].as_array().unwrap().len(), 3, "{r}");
    assert_eq!(
        table(&r),
        vec![vec!["1", "10", "NULL"], vec!["9", "NULL", "nine"]]
    );
    let (status, body) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "SELECT id FROM w INTERSECT BY NAME SELECT id FROM w" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "UNSUPPORTED", "{body}");
    done(state, dir).await;
}

#[tokio::test]
async fn count_of_several_expressions_and_group_by_all_over_http() {
    let (state, router, dir) = seeded("p1_count_gba").await;
    // (g, o): (1,1) (1,2) (2,1) (2,2) (NULL,1) -> 4 rows without a NULL, all distinct
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT COUNT(g, o), COUNT(DISTINCT g, o), COUNT(DISTINCT g), COUNT(DISTINCT o) FROM w",
    )
    .await;
    assert_eq!(table(&r), vec![vec!["4", "4", "2", "2"]]);
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT g, SUM(x), COUNT(*) FROM w GROUP BY ALL ORDER BY 1",
    )
    .await;
    assert_eq!(
        table(&r),
        vec![
            vec!["1", "30", "2"],
            vec!["2", "70", "2"],
            vec!["NULL", "NULL", "1"]
        ]
    );
    let (status, body) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "SELECT SUM(g, o) FROM w" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    done(state, dir).await;
}

#[tokio::test]
async fn any_and_all_subqueries_over_http() {
    let (state, router, dir) = seeded("p1_anyall").await;
    // S = x of g = 2 = {30, 40}
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT id FROM w WHERE x > ANY (SELECT x FROM w WHERE g = 2) ORDER BY id",
    )
    .await;
    assert_eq!(strs(&table(&r), 0), ["4"]);
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT id FROM w WHERE x >= ALL (SELECT x FROM w WHERE g = 2) ORDER BY id",
    )
    .await;
    assert_eq!(strs(&table(&r), 0), ["4"]);
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT id, x < SOME (SELECT x FROM w WHERE g = 1) FROM w ORDER BY id",
    )
    .await;
    assert_eq!(
        strs(&table(&r), 1),
        ["true", "false", "false", "false", "NULL"]
    );
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "EXPLAIN SELECT id FROM w WHERE x > ALL (SELECT x FROM w)",
    )
    .await;
    assert!(
        r["result"]["plan_text"]
            .as_str()
            .unwrap()
            .contains("> ALL subquery"),
        "{r}"
    );
    done(state, dir).await;
}

#[tokio::test]
async fn groups_frames_ignore_nulls_and_distinct_windows_over_http() {
    let (state, router, dir) = seeded("p1_windows").await;
    // o = 1,2,1,2,1 ; peer groups by o: {1,3,5} {2,4}
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT id, COUNT(*) OVER (ORDER BY o GROUPS BETWEEN CURRENT ROW AND 1 FOLLOWING) FROM w ORDER BY id",
    )
    .await;
    assert_eq!(strs(&table(&r), 1), ["5", "2", "5", "2", "5"]);
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT id, LAG(x) IGNORE NULLS OVER (ORDER BY id) FROM w ORDER BY id",
    )
    .await;
    assert_eq!(strs(&table(&r), 1), ["NULL", "10", "20", "30", "40"]);
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT id, COUNT(DISTINCT g) OVER (ORDER BY id), SUM(DISTINCT x) OVER (ORDER BY id) FROM w ORDER BY id",
    )
    .await;
    assert_eq!(strs(&table(&r), 1), ["1", "1", "2", "2", "2"]);
    assert_eq!(strs(&table(&r), 2), ["10", "30", "60", "100", "100"]);
    for sql in [
        // frame exclusion has no syntax in the parser
        "SELECT SUM(x) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW EXCLUDE CURRENT ROW) FROM w",
    ] {
        let (status, body) = sql_req(&router, ADMIN_KEY, json!({ "sql": sql })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{sql}: {body}");
    }
    for sql in [
        "SELECT SUM(x) OVER (GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM w",
        "SELECT ROW_NUMBER() IGNORE NULLS OVER (ORDER BY id) FROM w",
    ] {
        let (status, body) = sql_req(&router, ADMIN_KEY, json!({ "sql": sql })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{sql}: {body}");
        assert_eq!(body["error"]["code"], "BIND_ERROR", "{sql}: {body}");
    }
    done(state, dir).await;
}

#[tokio::test]
async fn a_reader_can_use_the_new_forms_on_what_it_can_read() {
    let (state, router, dir) = seeded("p1_reader").await;
    let (status, body) = sql_req(
        &router,
        READER_KEY,
        json!({ "sql": "SELECT g, COUNT(DISTINCT g, o) FROM w GROUP BY ALL ORDER BY 1" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    done(state, dir).await;
}

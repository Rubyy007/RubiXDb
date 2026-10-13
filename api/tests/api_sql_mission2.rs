//! SQL-mission-2 over HTTP (`POST /v1/sql`): window functions and advanced
//! aggregation through the real router, engine and `rubixdb-sql` pipeline --
//! the same no-mocking discipline as `api_sql_integration.rs` (whose helpers
//! are repeated here because each integration-test file is its own crate).

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

// -----------------------------------------------------------------
// 2A -- OVER, PARTITION BY, ORDER BY, default frames
// -----------------------------------------------------------------

#[tokio::test]
async fn over_is_implemented_and_never_silently_dropped() {
    let (state, router, dir) = seeded("m2_over").await;

    // A running total: with the OVER clause dropped this would be 100 on every row.
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT id, SUM(x) OVER (ORDER BY id) FROM w ORDER BY id",
    )
    .await;
    assert_eq!(strs(&table(&r), 1), ["10", "30", "60", "100", "100"]);

    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT id, SUM(x) OVER (PARTITION BY g), COUNT(*) OVER () FROM w ORDER BY id",
    )
    .await;
    let t = table(&r);
    assert_eq!(strs(&t, 1), ["30", "30", "70", "70", "NULL"]);
    assert_eq!(strs(&t, 2), ["5", "5", "5", "5", "5"]);

    // Ranking functions on tied data (o: 1,2,1,2,1 within the whole relation).
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT id, ROW_NUMBER() OVER (ORDER BY o, id), RANK() OVER (ORDER BY o), DENSE_RANK() OVER (ORDER BY o) FROM w ORDER BY id",
    )
    .await;
    let t = table(&r);
    assert_eq!(strs(&t, 1), ["1", "4", "2", "5", "3"]);
    assert_eq!(strs(&t, 2), ["1", "4", "1", "4", "1"]);
    assert_eq!(strs(&t, 3), ["1", "2", "1", "2", "1"]);

    done(state, dir).await;
}

#[tokio::test]
async fn the_response_to_over_before_mission_2_is_recorded_when_the_feature_is_off() {
    // Evidence helper: prints the response so the feature-off run (the
    // Mission-1-complete tree simulated by refusing OVER again) records it.
    let (state, router, dir) = seeded("m2_over_record").await;
    let (status, body) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "SELECT SUM(id) OVER () FROM w" }),
    )
    .await;
    println!("OVER_RESPONSE status={status} body={body}");
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(table(&body).len(), 5);
    done(state, dir).await;
}

#[tokio::test]
async fn a_window_function_outside_the_select_list_qualify_and_order_by_is_a_bind_error() {
    let (state, router, dir) = seeded("m2_window_refused").await;
    for sql in [
        "SELECT id FROM w WHERE SUM(x) OVER () > 1",
        "SELECT g FROM w GROUP BY g HAVING SUM(x) OVER () > 1",
    ] {
        let (status, body) = sql_req(&router, ADMIN_KEY, json!({ "sql": sql })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{sql}: {body}");
        assert_eq!(body["error"]["code"], "BIND_ERROR", "{sql}: {body}");
        assert!(
            body.to_string()
                .contains("window functions are not allowed"),
            "{sql}: {body}"
        );
    }
    done(state, dir).await;
}

#[tokio::test]
async fn too_many_window_functions_is_413_resource_limit() {
    let (state, router, dir) = seeded("m2_window_limit").await;
    // 65 distinct window calls; the limit is 64.
    let calls: Vec<String> = (1..=65).map(|i| format!("SUM(x + {i}) OVER ()")).collect();
    let sql = format!("SELECT {} FROM w", calls.join(", "));
    let (status, body) = sql_req(&router, ADMIN_KEY, json!({ "sql": sql })).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert_eq!(body["error"]["code"], "RESOURCE_LIMIT", "{body}");
    assert!(
        body.to_string()
            .contains("max_window_functions_per_statement"),
        "{body}"
    );
    done(state, dir).await;
}

#[tokio::test]
async fn a_reader_can_use_window_functions_on_what_it_can_read() {
    let (state, router, dir) = seeded("m2_reader").await;
    let (status, body) = sql_req(
        &router,
        READER_KEY,
        json!({ "sql": "SELECT id, SUM(x) OVER (ORDER BY id) FROM w ORDER BY id" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(strs(&table(&body), 1), ["10", "30", "60", "100", "100"]);
    done(state, dir).await;
}

// -----------------------------------------------------------------
// 2B -- frames
// -----------------------------------------------------------------

#[tokio::test]
async fn rows_and_range_frames_over_http() {
    let (state, router, dir) = seeded("m2_frames").await;
    // ROWS counts rows: the sliding sum of x (NULL is skipped by SUM).
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT id, SUM(x) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM w ORDER BY id",
    )
    .await;
    assert_eq!(strs(&table(&r), 1), ["30", "60", "90", "70", "40"]);
    // RANGE counts value distance and takes the ties: o = 1 2 1 2 1 -> the rows with the same o.
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT id, COUNT(*) OVER (ORDER BY o RANGE BETWEEN 0 PRECEDING AND 0 FOLLOWING) FROM w ORDER BY id",
    )
    .await;
    assert_eq!(strs(&table(&r), 1), ["3", "2", "3", "2", "3"]);
    // The default frame with an ORDER BY runs to the last peer, not to the current row.
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT id, COUNT(*) OVER (ORDER BY o) FROM w ORDER BY id",
    )
    .await;
    assert_eq!(strs(&table(&r), 1), ["3", "5", "3", "5", "3"]);
    done(state, dir).await;
}

#[tokio::test]
async fn illegal_frames_are_bind_errors_and_groups_frames_are_unsupported() {
    let (state, router, dir) = seeded("m2_frame_errors").await;
    for sql in [
        "SELECT SUM(x) OVER (ORDER BY id ROWS BETWEEN 1 FOLLOWING AND 1 PRECEDING) FROM w",
        "SELECT SUM(x) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED FOLLOWING AND CURRENT ROW) FROM w",
        "SELECT SUM(x) OVER (ORDER BY id, o RANGE BETWEEN 1 PRECEDING AND CURRENT ROW) FROM w",
        "SELECT LAG(x) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM w",
        "SELECT RANK() OVER () FROM w",
    ] {
        let (status, body) = sql_req(&router, ADMIN_KEY, json!({ "sql": sql })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{sql}: {body}");
        assert_eq!(body["error"]["code"], "BIND_ERROR", "{sql}: {body}");
    }
    for sql in [
        "SELECT SUM(x) OVER (ORDER BY id GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM w",
        "SELECT SUM(DISTINCT x) OVER () FROM w",
        "SELECT LAG(x) IGNORE NULLS OVER (ORDER BY id) FROM w",
    ] {
        let (status, body) = sql_req(&router, ADMIN_KEY, json!({ "sql": sql })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{sql}: {body}");
        assert_eq!(body["error"]["code"], "UNSUPPORTED", "{sql}: {body}");
    }
    done(state, dir).await;
}

// -----------------------------------------------------------------
// 2C -- navigation and distribution
// -----------------------------------------------------------------

#[tokio::test]
async fn navigation_and_distribution_functions_over_http() {
    let (state, router, dir) = seeded("m2_nav").await;
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT id, LAG(x) OVER (ORDER BY id), LEAD(x, 2, -1) OVER (ORDER BY id), FIRST_VALUE(x) OVER (ORDER BY id), \
         LAST_VALUE(x) OVER (ORDER BY id), NTH_VALUE(x, 2) OVER (ORDER BY id), NTILE(2) OVER (ORDER BY id) FROM w ORDER BY id",
    )
    .await;
    let t = table(&r);
    assert_eq!(strs(&t, 1), ["NULL", "10", "20", "30", "40"]);
    assert_eq!(strs(&t, 2), ["30", "40", "NULL", "-1", "-1"]);
    assert_eq!(strs(&t, 3), ["10", "10", "10", "10", "10"]);
    // LAST_VALUE with the default frame is the current row's value.
    assert_eq!(strs(&t, 4), ["10", "20", "30", "40", "NULL"]);
    assert_eq!(strs(&t, 5), ["NULL", "20", "20", "20", "20"]);
    assert_eq!(strs(&t, 6), ["1", "1", "1", "2", "2"]);
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT id, PERCENT_RANK() OVER (ORDER BY o), CUME_DIST() OVER (ORDER BY o) FROM w ORDER BY id",
    )
    .await;
    let t = table(&r);
    // o = 1 2 1 2 1: rank 1 (x3), 4 (x2); percent_rank = (rank-1)/4; cume_dist = 3/5, 5/5.
    assert_eq!(strs(&t, 1), ["0.0", "0.75", "0.0", "0.75", "0.0"]);
    assert_eq!(strs(&t, 2), ["0.6", "1.0", "0.6", "1.0", "0.6"]);
    done(state, dir).await;
}

// -----------------------------------------------------------------
// 2D -- QUALIFY
// -----------------------------------------------------------------

#[tokio::test]
async fn qualify_over_http() {
    let (state, router, dir) = seeded("m2_qualify").await;
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT id FROM w QUALIFY ROW_NUMBER() OVER (PARTITION BY g ORDER BY x DESC, id) = 1 ORDER BY id",
    )
    .await;
    // g = 1: id 2 (x 20) ; g = 2: id 4 (x 40) ; g NULL: id 5.
    assert_eq!(strs(&table(&r), 0), ["2", "4", "5"]);
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT id, ROW_NUMBER() OVER (ORDER BY id) AS rn FROM w QUALIFY rn > 2 ORDER BY id LIMIT 2",
    )
    .await;
    assert_eq!(strs(&table(&r), 0), ["3", "4"]);
    // QUALIFY with no window function anywhere is refused.
    let (status, body) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "SELECT id FROM w QUALIFY id > 2" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "BIND_ERROR", "{body}");
    done(state, dir).await;
}

// -----------------------------------------------------------------
// 2E -- GROUPING SETS / ROLLUP / CUBE / GROUPING()
// -----------------------------------------------------------------

#[tokio::test]
async fn grouping_sets_rollup_cube_and_grouping_over_http() {
    let (state, router, dir) = seeded("m2_grouping").await;
    // g = 1, 1, 2, 2, NULL (a NULL in the data); x = 10 20 30 40 NULL.
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT g, GROUPING(g), SUM(x) FROM w GROUP BY ROLLUP (g)",
    )
    .await;
    let t = table(&r);
    assert_eq!(strs(&t, 0), ["1", "2", "NULL", "NULL"]);
    // 0 for the NULL that is in the data, 1 for the grand-total row.
    assert_eq!(strs(&t, 1), ["0", "0", "0", "1"]);
    assert_eq!(strs(&t, 2), ["30", "70", "NULL", "100"]);

    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT g, o, COUNT(*) FROM w GROUP BY GROUPING SETS ((g, o), (g), ()) ORDER BY GROUPING(g), GROUPING(o), g, o",
    )
    .await;
    assert_eq!(table(&r).len(), 5 + 3 + 1);
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT g, o, COUNT(*) FROM w GROUP BY CUBE (g, o)",
    )
    .await;
    assert_eq!(table(&r).len(), 5 + 3 + 2 + 1);

    // More than 64 grouping sets in one statement: 413 RESOURCE_LIMIT, before any work.
    let cols: Vec<String> = (0..7).map(|i| format!("id + {i}")).collect();
    let sql = format!("SELECT COUNT(*) FROM w GROUP BY CUBE ({})", cols.join(", "));
    let (status, body) = sql_req(&router, ADMIN_KEY, json!({ "sql": sql })).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert_eq!(body["error"]["code"], "RESOURCE_LIMIT", "{body}");
    assert!(
        body.to_string().contains("max_grouping_sets_per_statement"),
        "{body}"
    );

    // GROUPING of something that is not grouped: a bind error.
    let (status, body) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "SELECT GROUPING(x) FROM w GROUP BY ROLLUP (g)" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "BIND_ERROR", "{body}");
    done(state, dir).await;
}

// -----------------------------------------------------------------
// 2F -- FILTER and DISTINCT aggregates
// -----------------------------------------------------------------

#[tokio::test]
async fn aggregate_filter_and_distinct_over_http() {
    let (state, router, dir) = seeded("m2_filter_distinct").await;
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT SUM(x) FILTER (WHERE o = 1), COUNT(*) FILTER (WHERE o = 2), COUNT(*) FROM w",
    )
    .await;
    // o = 1: ids 1, 3, 5 -> x 10 + 30 + NULL ; o = 2: ids 2, 4.
    assert_eq!(
        table(&r),
        vec![vec!["40".to_string(), "2".to_string(), "5".to_string()]]
    );
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "SELECT g, COUNT(DISTINCT o), COUNT(o), SUM(DISTINCT o) FROM w GROUP BY g ORDER BY g",
    )
    .await;
    let t = table(&r);
    // g = 1: o {1, 2}; g = 2: o {1, 2}; g NULL: o {1}.
    assert_eq!(strs(&t, 1), ["2", "2", "1"]);
    assert_eq!(strs(&t, 2), ["2", "2", "1"]);
    assert_eq!(strs(&t, 3), ["3", "3", "1"]);
    // COUNT(DISTINCT *) is not valid; FILTER needs a boolean condition.
    for sql in [
        "SELECT COUNT(DISTINCT *) FROM w",
        "SELECT SUM(x) FILTER (WHERE x) FROM w",
    ] {
        let (status, body) = sql_req(&router, ADMIN_KEY, json!({ "sql": sql })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{sql}: {body}");
    }
    done(state, dir).await;
}

#[tokio::test]
async fn explain_shows_windows_grouping_filters_and_distinct() {
    let (state, router, dir) = seeded("m2_explain").await;
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "EXPLAIN SELECT id, SUM(x) OVER (PARTITION BY g ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM w",
    )
    .await;
    let text = r.to_string();
    assert!(text.contains("Window partition_by="), "{text}");
    assert!(
        text.contains("ROWS BETWEEN 1 PRECEDING AND CURRENT ROW") || text.contains("PRECEDING"),
        "{text}"
    );
    let r = exec_sql(
        &router,
        ADMIN_KEY,
        "EXPLAIN SELECT g, COUNT(DISTINCT x), SUM(x) FILTER (WHERE o = 1) FROM w GROUP BY ROLLUP (g)",
    )
    .await;
    let text = r.to_string();
    assert!(text.contains("grouping_sets=2"), "{text}");
    assert!(
        text.contains("DISTINCT") && text.contains("FILTER"),
        "{text}"
    );
    done(state, dir).await;
}

// -----------------------------------------------------------------
// Mission 1 assertions that sit behind a superseded one
// -----------------------------------------------------------------
//
// `api_sql_integration.rs::recursive_cte_errors_and_bounds_over_http` (a
// Mission 1 test, left unchanged as instructed) asserts, half-way through,
// that a window function is `UNSUPPORTED`. Mission 2 makes that false, so the
// test now stops at that assertion and the checks after it never run. They are
// repeated here, unchanged in substance, so the coverage is not lost; the
// superseded assertion is replaced by the behaviour that is now true.

#[tokio::test]
async fn recursion_errors_and_bounds_still_hold_after_window_functions_were_added() {
    let dir = temp_dir("m2_rec_after");
    let (state, router) = build_app(&dir);
    for sql in [
        "CREATE TABLE rt (id INTEGER PRIMARY KEY, parent INTEGER)",
        "INSERT INTO rt (id, parent) VALUES (1, NULL), (2, 1), (3, 1)",
        "CREATE TABLE cy (id INTEGER PRIMARY KEY, nxt INTEGER)",
        "INSERT INTO cy (id, nxt) VALUES (1, 2), (2, 3), (3, 1)",
    ] {
        exec_sql(&router, ADMIN_KEY, sql).await;
    }

    // What used to be `400 UNSUPPORTED` is now: a window function in the
    // *recursive term* is a bind error (it would change the recursion) ...
    let (status, body) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT SUM(n) OVER () FROM t) SELECT n FROM t" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "BIND_ERROR", "{body}");
    // ... and anywhere else it runs, instead of being refused.
    let (status, body) = sql_req(&router, ADMIN_KEY, json!({ "sql": "SELECT SUM(id) OVER () FROM rt" })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(strs(&table(&body), 0), ["6", "6", "6"]);

    // The Mission 1 checks that follow the superseded assertion.
    for sql in [
        "WITH RECURSIVE r(n) AS (SELECT id FROM no_such UNION ALL SELECT n + 1 FROM r WHERE n < 3) SELECT n FROM r",
        "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT no_such.id FROM r JOIN no_such ON no_such.id = r.n) SELECT n FROM r",
    ] {
        let (status, body) = sql_req(&router, ADMIN_KEY, json!({ "sql": sql })).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{sql}: {body}");
    }
    let (status, body) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t) SELECT n FROM t" }),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert_eq!(body["error"]["code"], "RESOURCE_LIMIT");
    assert!(body.to_string().contains("recursive CTE 't'"), "{body}");
    assert!(body.to_string().contains("max_recursive_cte_depth (1000)"), "{body}");
    let (status, body) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": "WITH RECURSIVE reach(id) AS (SELECT 1 UNION ALL SELECT cy.nxt FROM reach JOIN cy ON cy.id = reach.id) SELECT id FROM reach" }),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert!(body.to_string().contains("recursive CTE 'reach'"), "{body}");
    let defs: Vec<String> = (1..=9)
        .map(|i| format!("c{i}(n) AS (SELECT {i} UNION ALL SELECT n + 1 FROM c{i} WHERE n < {})", i + 1))
        .collect();
    let uses: Vec<String> = (1..=9).map(|i| format!("SELECT n FROM c{i}")).collect();
    let (status, body) = sql_req(
        &router,
        ADMIN_KEY,
        json!({ "sql": format!("WITH RECURSIVE {} {}", defs.join(", "), uses.join(" UNION ALL ")) }),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert!(body.to_string().contains("max_recursive_cte_definitions is 8"), "{body}");

    done(state, dir).await;
}

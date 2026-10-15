//! Multi-schema workflow, guided errors and status codes of `POST /v1/sql`: one database ("default"),
//! many schemas inside it, many tables inside each schema. Real engine, real router, no mocks (the same
//! discipline as `api_sql_integration.rs`).

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
    let path = std::env::temp_dir().join(format!("rubixdb_api_schemas_it_{tag}_{nanos}"));
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

fn build_app(dir: &Path) -> Router {
    let lsm_config = LsmConfig::default();
    let engine =
        Arc::new(LsmEngine::open(dir, wal_config(), pool_config(), lsm_config.clone()).unwrap());
    let state = Arc::new(AppState::new(
        engine,
        lsm_config,
        test_config(dir.to_path_buf()),
    ));
    build_router(state)
}

async fn call(router: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let response = router.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn sql(router: &Router, key: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri("/v1/sql")
        .header("Authorization", format!("Bearer {key}"))
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    call(router, req).await
}

async fn run(router: &Router, statement: &str) -> (StatusCode, Value) {
    sql(router, ADMIN_KEY, json!({ "sql": statement })).await
}

async fn run_in(router: &Router, schema: &str, statement: &str) -> (StatusCode, Value) {
    sql(
        router,
        ADMIN_KEY,
        json!({ "sql": statement, "schema": schema }),
    )
    .await
}

async fn get(router: &Router, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .uri(uri)
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .body(Body::empty())
        .unwrap();
    call(router, req).await
}

fn error_code(v: &Value) -> &str {
    v["error"]["code"].as_str().unwrap_or("")
}

#[tokio::test]
async fn duplicate_schema_is_409_and_if_not_exists_is_a_reported_no_op() {
    let dir = temp_dir("dup_schema");
    let app = build_app(&dir);

    let (s, v) = run(&app, "CREATE SCHEMA analytics").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["result"]["kind"], "ddl");
    assert_eq!(v["result"]["statement"], "CREATE SCHEMA");
    assert_eq!(v["result"]["changed"], true);
    assert_eq!(v["result"]["message"], "Schema default.analytics created.");

    let (s, v) = run(&app, "CREATE SCHEMA analytics").await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(error_code(&v), "ALREADY_EXISTS");
    assert!(v["error"]["hint"]
        .as_str()
        .unwrap()
        .contains("IF NOT EXISTS"));

    let (s, v) = run(&app, "CREATE SCHEMA IF NOT EXISTS analytics").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["result"]["changed"], false);
    assert!(v["result"]["message"]
        .as_str()
        .unwrap()
        .contains("already exists; nothing changed"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn many_schemas_each_with_many_tables_and_a_session_schema() {
    let dir = temp_dir("multi");
    let app = build_app(&dir);

    for schema in ["analytics", "app_data"] {
        let (s, v) = run(&app, &format!("CREATE SCHEMA {schema}")).await;
        assert_eq!(s, StatusCode::OK, "{v}");
    }

    // USE SCHEMA answers with the schema; the client sends it back as `schema`.
    let (s, v) = run(&app, "USE SCHEMA analytics").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["result"]["kind"], "use_schema");
    assert_eq!(v["result"]["schema"], "analytics");
    assert_eq!(v["result"]["database"], "default");

    // Several tables in one schema, reported fully qualified.
    let (s, v) = run_in(
        &app,
        "analytics",
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)",
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        v["result"]["message"],
        "Table default.analytics.users created."
    );
    let (s, v) = run_in(
        &app,
        "analytics",
        "CREATE TABLE events (id INTEGER PRIMARY KEY)",
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    // The same table name in another schema is a different table.
    let (s, v) = run_in(
        &app,
        "app_data",
        "CREATE TABLE users (id INTEGER PRIMARY KEY)",
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    // A duplicate inside the same schema is a 409.
    let (s, v) = run_in(
        &app,
        "analytics",
        "CREATE TABLE users (id INTEGER PRIMARY KEY)",
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(error_code(&v), "ALREADY_EXISTS");
    // Fully qualified creation works without any schema context.
    let (s, v) = run(
        &app,
        "CREATE TABLE default.app_data.orders (id INTEGER PRIMARY KEY)",
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        v["result"]["message"],
        "Table default.app_data.orders created."
    );

    let (s, v) = run_in(
        &app,
        "analytics",
        "INSERT INTO users (id, name) VALUES (1, 'ada')",
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["result"]["message"], "INSERT completed: 1 row affected.");

    // Active schema, qualified schema.table and db.schema.table all reach the same row.
    for (schema, q) in [
        (Some("analytics"), "SELECT name FROM users"),
        (None, "SELECT name FROM analytics.users"),
        (None, "SELECT name FROM default.analytics.users"),
    ] {
        let (s, v) = match schema {
            Some(sc) => run_in(&app, sc, q).await,
            None => run(&app, q).await,
        };
        assert_eq!(s, StatusCode::OK, "{q}: {v}");
        assert_eq!(v["result"]["row_count"], 1, "{q}");
    }

    // Without the schema context, `users` is looked up in `public`, where it does not exist: 404.
    let (s, v) = run(&app, "SELECT name FROM users").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    assert_eq!(error_code(&v), "NOT_FOUND");
    assert!(v["error"]["hint"].as_str().unwrap().contains("USE SCHEMA"));

    // The other schema's `users` is empty.
    let (s, v) = run_in(&app, "app_data", "SELECT id FROM users").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["result"]["row_count"], 0);

    // Catalog reads: one schema, or every schema with its name attached.
    let (s, v) = get(&app, "/v1/catalog/tables?schema=analytics").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let mut names: Vec<_> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    names.sort();
    assert_eq!(names, ["events", "users"]);
    let (_, v) = get(&app, "/v1/catalog/tables?scope=all").await;
    let all: Vec<String> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            format!(
                "{}.{}",
                t["schema_name"].as_str().unwrap(),
                t["name"].as_str().unwrap()
            )
        })
        .collect();
    for want in [
        "analytics.users",
        "analytics.events",
        "app_data.users",
        "app_data.orders",
    ] {
        assert!(
            all.contains(&want.to_string()),
            "{want} missing from {all:?}"
        );
    }
    let (s, v) = get(&app, "/v1/catalog/tables?schema=nope").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    let (s, _) = get(&app, "/v1/catalog/tables/users?schema=analytics").await;
    assert_eq!(s, StatusCode::OK);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn use_schema_on_a_missing_schema_is_404_and_a_stale_schema_can_be_replaced() {
    let dir = temp_dir("use_missing");
    let app = build_app(&dir);

    let (s, v) = run(&app, "USE SCHEMA nope").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    assert_eq!(error_code(&v), "NOT_FOUND");

    // A request that still carries a schema which no longer exists is refused with advice ...
    let (s, v) = run_in(&app, "gone", "SELECT 1").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    assert!(v["error"]["hint"].as_str().unwrap().contains("USE SCHEMA"));
    // ... but USE SCHEMA itself must still work from that state, or the client could never recover.
    let (s, v) = run_in(&app, "gone", "USE SCHEMA public").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["result"]["schema"], "public");
    // The database-qualified form is accepted too.
    let (s, v) = run(&app, "USE SCHEMA default.public").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn create_and_use_database_are_refused_with_400_and_guidance() {
    let dir = temp_dir("db_refused");
    let app = build_app(&dir);

    for name in ["analytics_db", "IF NOT EXISTS other"] {
        let (s, v) = run(&app, &format!("CREATE DATABASE {name}")).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{name}: {v}");
        assert_eq!(error_code(&v), "UNSUPPORTED");
        assert_eq!(
            v["error"]["message"],
            "CREATE DATABASE is disabled. rubiXDb v1 runs on a single database ('default'). Use CREATE SCHEMA <schema_name> to organize your tables."
        );
    }
    for q in ["USE DATABASE other", "USE DATABASE default"] {
        let (s, v) = run(&app, q).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{q}: {v}");
        assert_eq!(error_code(&v), "UNSUPPORTED");
        assert_eq!(
            v["error"]["message"],
            "USE DATABASE is disabled. Active database is locked to 'default'. Use USE SCHEMA <schema_name> to switch working namespaces."
        );
    }
    // A bare `USE x` is not a silent no-op either.
    let (s, v) = run(&app, "USE analytics").await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("USE SCHEMA"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_reader_cannot_create_schemas() {
    let dir = temp_dir("reader");
    let app = build_app(&dir);
    // By design (D25, item 15) a denied action on a scope the key cannot change is indistinguishable from a
    // missing one: 404, never a 403 that would confirm what exists.
    let (s, v) = sql(&app, READER_KEY, json!({ "sql": "CREATE SCHEMA x" })).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    assert!(v["error"]["hint"].as_str().unwrap().contains("read-only"));
    let (_, listed) = get(&app, "/v1/catalog/schemas").await;
    assert!(!listed.as_array().unwrap().iter().any(|s| s["name"] == "x"));
    let (s, v) = sql(&app, READER_KEY, json!({ "sql": "USE SCHEMA public" })).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn mistyped_keywords_get_a_did_you_mean_and_a_400() {
    let dir = temp_dir("typos");
    let app = build_app(&dir);
    for (q, want) in [
        (
            "CREAT TABLE t (id INTEGER PRIMARY KEY)",
            "Did you mean CREATE?",
        ),
        ("CREATE SCEMA x", "Did you mean CREATE SCHEMA?"),
        ("USE SCHEM x", "Did you mean USE SCHEMA?"),
        ("SELCT 1", "Did you mean SELECT?"),
    ] {
        let (s, v) = run(&app, q).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{q}: {v}");
        assert_eq!(error_code(&v), "PARSE_ERROR", "{q}");
        assert_eq!(v["error"]["hint"], want, "{q}");
    }
    // A syntax error with no keyword typo still carries the generic advice.
    let (s, v) = run(&app, "SELECT ((1").await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(v["error"]["hint"].as_str().unwrap().contains("syntax"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn malformed_request_bodies_are_clean_4xx_in_the_api_error_shape() {
    let dir = temp_dir("bad_bodies");
    let app = build_app(&dir);
    let post = |content_type: Option<&str>, body: &str| {
        let mut b = Request::builder()
            .method("POST")
            .uri("/v1/sql")
            .header("Authorization", format!("Bearer {ADMIN_KEY}"));
        if let Some(ct) = content_type {
            b = b.header("Content-Type", ct);
        }
        b.body(Body::from(body.to_string())).unwrap()
    };

    let (s, v) = call(&app, post(Some("application/json"), "{ not json")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(error_code(&v), "VALIDATION_ERROR");

    let (s, v) = call(
        &app,
        post(Some("application/json"), r#"{"query": "SELECT 1"}"#),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(error_code(&v), "VALIDATION_ERROR");
    assert!(v["error"]["message"].as_str().unwrap().contains("\"sql\""));

    let (s, v) = call(&app, post(Some("text/plain"), r#"{"sql": "SELECT 1"}"#)).await;
    assert_eq!(s, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{v}");
    assert_eq!(error_code(&v), "UNSUPPORTED_MEDIA_TYPE");

    let (s, v) = call(&app, post(None, r#"{"sql": "SELECT 1"}"#)).await;
    assert_eq!(s, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{v}");

    // An empty statement is the caller's mistake, not a server fault.
    let (s, v) = run(&app, "   ").await;
    assert!(s.is_client_error(), "{s} {v}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn duplicate_index_and_missing_drops_use_client_status_codes() {
    let dir = temp_dir("index_codes");
    let app = build_app(&dir);
    assert_eq!(
        run(&app, "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        run(&app, "CREATE INDEX t_v ON t (v)").await.0,
        StatusCode::OK
    );
    let (s, v) = run(&app, "CREATE INDEX t_v ON t (v)").await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(error_code(&v), "ALREADY_EXISTS");

    let (s, v) = run(&app, "DROP TABLE nothing_here").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    let (s, v) = run(&app, "DROP TABLE IF EXISTS nothing_here").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["result"]["changed"], false);
    assert!(v["result"]["message"]
        .as_str()
        .unwrap()
        .contains("nothing dropped"));

    let (s, v) = run(&app, "DROP TABLE t").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["result"]["message"], "Table default.public.t dropped.");
    let _ = std::fs::remove_dir_all(&dir);
}

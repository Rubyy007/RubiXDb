//! Phase 7 Increment C -- security event log (SG-3b, D-4) and response
//! security headers (SG-5), against the real router. Each request future runs
//! under a `tracing` dispatcher carrying the real `SecurityLogLayer`, so what
//! is asserted is the file the product would write. No mocks of the log.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use rubixdb::execution::batch_coordinator::BatchCoordinatorConfig;
use rubixdb::lsm::{LsmConfig, LsmEngine};
use rubixdb::wal::{SyncMode, WalConfig};
use rubixdb_api::config::{ApiKeyConfig, Role};
use rubixdb_api::routes::build_router;
use rubixdb_api::security_log::{SecurityLog, SecurityLogLayer};
use rubixdb_api::{AppState, Config};
use serde_json::{json, Value};
use tower::ServiceExt;
use tracing::instrument::WithSubscriber;
use tracing::Dispatch;
use tracing_subscriber::layer::SubscriberExt;

const ADMIN_KEY: &str = "evt-admin-key-0123456789abcdef";
const READER_KEY: &str = "evt-reader-key-0123456789abcdef";

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rubixdb_evt_{tag}_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn config(data_dir: PathBuf, dist: Option<PathBuf>, backups: Option<PathBuf>) -> Config {
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
        rate_limit_rps: 1.0e6,
        rate_limit_burst: 2_000_000,
        compaction_auto_trigger: false,
        compaction_trigger_count: 4,
        cors_allowed_origins: vec![],
        sql_max_sessions_per_principal: 50,
        sql_session_idle_timeout_secs: 300,
        sql_session_max_lifetime_secs: 1800,
        sql_statement_deadline_secs: 30,
        instance_id: None,
        instance_name: None,
        frontend_dist: dist,
        backup_dir: backups,
    }
}

struct App {
    router: Router,
    dispatch: Dispatch,
    log: Arc<SecurityLog>,
    _dir: PathBuf,
    log_dir: PathBuf,
}

fn build(tag: &str, dist: Option<PathBuf>, with_backups: bool, log: Option<SecurityLog>) -> App {
    let dir = temp_dir(tag);
    let log_dir = temp_dir(&format!("{tag}_log"));
    let lsm = LsmConfig::default();
    let engine = Arc::new(
        LsmEngine::open(
            &dir.join("data"),
            WalConfig {
                sync_mode: SyncMode::GroupCommit {
                    max_wait: Duration::from_millis(5),
                    max_batch_bytes: 256 * 1024,
                },
                ..WalConfig::default()
            },
            BatchCoordinatorConfig {
                queue_capacity: 256,
                max_queued_bytes: 16 * 1024 * 1024,
                submission_timeout: Duration::from_secs(5),
                shutdown_drain_bound: Duration::from_secs(30),
                await_retry_budget: Duration::from_secs(5),
                max_drain_per_batch: 4096,
            },
            lsm.clone(),
        )
        .unwrap(),
    );
    let backups = with_backups.then(|| dir.join("backups"));
    let state = Arc::new(AppState::new(
        engine,
        lsm,
        config(dir.join("data"), dist, backups),
    ));
    let router = build_router(state);
    let log = Arc::new(log.unwrap_or_else(|| SecurityLog::open_in(&log_dir)));
    let dispatch =
        Dispatch::new(tracing_subscriber::registry().with(SecurityLogLayer::new(log.clone())));
    App {
        router,
        dispatch,
        log,
        _dir: dir,
        log_dir,
    }
}

impl App {
    async fn send(&self, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let resp = self
            .router
            .clone()
            .oneshot(req)
            .with_subscriber(self.dispatch.clone())
            .await
            .unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = resp
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        (status, headers, bytes)
    }

    async fn sql(&self, key: &str, sql: &str) -> (StatusCode, Value) {
        let (s, _, b) = self
            .send(
                Request::builder()
                    .method("POST")
                    .uri("/v1/sql")
                    .header("Authorization", format!("Bearer {key}"))
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({ "sql": sql })).unwrap(),
                    ))
                    .unwrap(),
            )
            .await;
        (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
    }

    async fn call(
        &self,
        method: &str,
        uri: &str,
        key: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut b = Request::builder()
            .method(method)
            .uri(uri)
            .header("Authorization", format!("Bearer {key}"));
        let body = match body {
            Some(v) => {
                b = b.header("Content-Type", "application/json");
                Body::from(serde_json::to_vec(&v).unwrap())
            }
            None => Body::empty(),
        };
        let (s, _, bytes) = self.send(b.body(body).unwrap()).await;
        (s, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    fn lines(&self) -> Vec<Value> {
        std::fs::read_to_string(self.log.path())
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).expect("one JSON object per line"))
            .collect()
    }

    fn raw_log(&self) -> String {
        std::fs::read_to_string(self.log.path()).unwrap_or_default()
    }
}

fn get(uri: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

// ---------------------------------------------------------------------
// SG-5 headers
// ---------------------------------------------------------------------

fn assert_baseline_headers(h: &axum::http::HeaderMap, what: &str) {
    let csp = h
        .get("content-security-policy")
        .unwrap_or_else(|| panic!("{what}: no CSP"))
        .to_str()
        .unwrap();
    for d in [
        "default-src 'self'",
        "script-src 'self'",
        "style-src 'self'",
        "frame-ancestors 'none'",
        "base-uri 'none'",
        "object-src 'none'",
    ] {
        assert!(csp.contains(d), "{what}: CSP lacks {d}: {csp}");
    }
    assert!(!csp.contains("unsafe-inline"), "{what}: {csp}");
    assert!(!csp.contains("unsafe-eval"), "{what}: {csp}");
    assert!(!csp.contains('*'), "{what}: {csp}");
    assert_eq!(
        h.get("x-content-type-options").unwrap(),
        "nosniff",
        "{what}"
    );
    assert_eq!(h.get("referrer-policy").unwrap(), "no-referrer", "{what}");
}

#[tokio::test]
async fn security_headers_on_spa_static_asset_and_api_401() {
    let dist = temp_dir("dist");
    std::fs::write(dist.join("index.html"), b"<html>shell</html>").unwrap();
    std::fs::create_dir_all(dist.join("assets")).unwrap();
    std::fs::write(dist.join("assets").join("app.js"), b"console.log(1);").unwrap();
    let app = build("hdr", Some(dist), false, None);

    // The SPA shell ("/") and a client-side route served by the fallback.
    for uri in ["/", "/sql"] {
        let (s, h, _) = app.send(get(uri)).await;
        assert_eq!(s, StatusCode::OK, "{uri}");
        assert_baseline_headers(&h, uri);
        assert!(
            h.get("cache-control").is_none(),
            "{uri}: not an API response"
        );
    }
    // A static asset: headers present, still cacheable (no no-store).
    let (s, h, _) = app.send(get("/assets/app.js")).await;
    assert_eq!(s, StatusCode::OK);
    assert_baseline_headers(&h, "static asset");
    assert!(h.get("cache-control").is_none());

    // An API 401 (auth middleware short-circuits): headers and no-store.
    let (s, h, _) = app.send(get("/v1/status")).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_baseline_headers(&h, "API 401");
    assert_eq!(h.get("cache-control").unwrap(), "no-store");

    // An authenticated API response, a public probe, and an unmatched API path.
    let mut authed = get("/v1/whoami");
    authed.headers_mut().insert(
        "authorization",
        format!("Bearer {ADMIN_KEY}").parse().unwrap(),
    );
    let (s, h, _) = app.send(authed).await;
    assert_eq!(s, StatusCode::OK);
    assert_baseline_headers(&h, "API 200");
    assert_eq!(h.get("cache-control").unwrap(), "no-store");
    let (_, h, _) = app.send(get("/healthz")).await;
    assert_baseline_headers(&h, "/healthz");
    let (_, h, _) = app.send(get("/v1/definitely-not-a-route")).await;
    assert_baseline_headers(&h, "unmatched /v1");
}

// ---------------------------------------------------------------------
// SG-3b events
// ---------------------------------------------------------------------

#[tokio::test]
async fn auth_failure_is_recorded_without_the_presented_credential() {
    let app = build("auth", None, false, None);
    let attempted = "attacker-guess-SECRET-TOKEN-9999";
    let (s, _, _) = app
        .send(
            Request::builder()
                .method("GET")
                .uri("/v1/status?probe=SECRET-QUERY-VALUE")
                .header("Authorization", format!("Bearer {attempted}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let l = app.lines();
    assert_eq!(l.len(), 1);
    assert_eq!(l[0]["code"], "auth.failure");
    assert_eq!(l[0]["method"], "GET");
    assert_eq!(l[0]["route"], "/v1/status");
    assert_eq!(l[0]["status"], 401);
    assert_eq!(l[0]["outcome"], "denied");
    let raw = app.raw_log();
    for secret in [attempted, "SECRET-QUERY-VALUE", "Bearer", ADMIN_KEY] {
        assert!(
            !raw.contains(secret),
            "log must not contain {secret:?}: {raw}"
        );
    }
}

#[tokio::test]
async fn auth_failure_flood_is_rate_bounded_and_never_contains_attempted_keys() {
    let app = build("flood", None, false, None);
    let n = 5_000;
    for i in 0..n {
        let (s, _, _) = app
            .send(
                Request::builder()
                    .method("GET")
                    .uri("/v1/status")
                    .header("Authorization", format!("Bearer guess-{i:06}-AAAAAAAAAAAA"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
    }
    let raw = app.raw_log();
    let l = app.lines();
    // Default bound: one line per 10 s; the whole flood finishes well inside it.
    assert_eq!(l.len(), 1, "{n} failures must collapse to one line: {raw}");
    assert!(raw.len() < 512);
    assert!(
        !raw.contains("guess-"),
        "attempted keys must never be logged"
    );
    assert!(!raw.contains("AAAAAAAAAAAA"));
    assert_eq!(app.log.write_failures(), 0);
}

#[tokio::test]
async fn flood_with_the_rate_bound_off_still_cannot_exceed_the_size_cap() {
    let dir = temp_dir("cap_log");
    // Worst case for the rate bound (disabled) and a tiny cap: total disk use
    // is still capped at max_bytes * (generations + 1).
    let log = SecurityLog::new(dir.join("security.log"), 4096, 2, Duration::ZERO);
    let app = build("cap", None, false, Some(log));
    for i in 0..3_000 {
        let _ = app
            .send(
                Request::builder()
                    .method("GET")
                    .uri("/v1/status")
                    .header("Authorization", format!("Bearer guess-{i}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
    }
    let mut total = 0;
    for e in std::fs::read_dir(&dir).unwrap() {
        let e = e.unwrap();
        let len = e.metadata().unwrap().len();
        assert!(len <= 4096, "{:?} is {len} bytes", e.file_name());
        total += len;
    }
    assert!(total <= 3 * 4096, "total {total}");
    assert!(!std::fs::read_to_string(dir.join("security.log"))
        .unwrap()
        .contains("guess-"));
}

#[tokio::test]
async fn admin_actions_are_logged_inspection_reads_are_not_and_names_stay_out() {
    let app = build("admin", None, true, None);
    // Read-only inspection: not an action.
    let (s, _) = app.call("GET", "/v1/admin/status", ADMIN_KEY, None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(app.lines().is_empty(), "GET inspection must not be logged");

    // A create, a verify, a refused delete (wrong confirmation) and a delete.
    let (s, _) = app
        .call(
            "POST",
            "/v1/admin/backups",
            ADMIN_KEY,
            Some(json!({ "name": "evt-backup-name" })),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = app
        .call(
            "POST",
            "/v1/admin/backups/evt-backup-name/verify",
            ADMIN_KEY,
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = app
        .call(
            "DELETE",
            "/v1/admin/backups/evt-backup-name?confirm=wrong",
            ADMIN_KEY,
            None,
        )
        .await;
    assert!(s.is_client_error(), "{s}");
    let (s, _) = app
        .call(
            "DELETE",
            "/v1/admin/backups/evt-backup-name?confirm=evt-backup-name",
            ADMIN_KEY,
            None,
        )
        .await;
    assert!(s.is_success(), "{s}");

    let l = app.lines();
    assert_eq!(l.len(), 4, "{}", app.raw_log());
    for e in &l {
        assert_eq!(e["code"], "admin.action");
        assert_eq!(e["principal"], "admin");
    }
    assert_eq!(l[0]["route"], "/v1/admin/backups");
    assert_eq!(l[0]["method"], "POST");
    assert_eq!(l[0]["outcome"], "ok");
    assert_eq!(l[1]["route"], "/v1/admin/backups/:name/verify");
    assert_eq!(l[2]["outcome"], "refused");
    assert_eq!(l[3]["outcome"], "ok");
    // The route PATTERN is recorded, never the path parameter or the query.
    let raw = app.raw_log();
    assert!(!raw.contains("evt-backup-name"), "{raw}");
    assert!(!raw.contains("confirm"), "{raw}");
}

#[tokio::test]
async fn reader_cannot_reach_admin_routes_and_nothing_is_recorded_as_an_action() {
    let app = build("reader", None, true, None);
    let (s, _) = app.call("POST", "/v1/admin/check", READER_KEY, None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert!(app.lines().is_empty());
}

#[tokio::test]
async fn catalog_rest_drops_are_logged_with_a_numeric_id_and_the_outcome() {
    let app = build("rest_drop", None, false, None);
    let (s, _) = app
        .sql(ADMIN_KEY, "CREATE TABLE victims (id INTEGER PRIMARY KEY)")
        .await;
    assert_eq!(s, StatusCode::OK);
    let (_, tables) = app.call("GET", "/v1/catalog/tables", ADMIN_KEY, None).await;
    let id = tables
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "victims")
        .unwrap()["table_id"]
        .as_u64()
        .unwrap();
    let uri = format!("/v1/catalog/tables/by-id/{id}");
    let (s, _) = app
        .call(
            "DELETE",
            &uri,
            ADMIN_KEY,
            Some(json!({"schema_name":"public","table_name":"nope"})),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = app
        .call(
            "DELETE",
            &uri,
            ADMIN_KEY,
            Some(json!({"schema_name":"public","table_name":"victims"})),
        )
        .await;
    assert!(s.is_success(), "{s}");

    let drops: Vec<_> = app
        .lines()
        .into_iter()
        .filter(|e| e["route"] == "/v1/catalog/tables/by-id/:table_id")
        .collect();
    assert_eq!(drops.len(), 2);
    for e in &drops {
        assert_eq!(e["code"], "catalog.drop");
        assert_eq!(e["object_kind"], "table");
        assert_eq!(e["object"], id.to_string());
        assert_eq!(e["method"], "DELETE");
    }
    assert_eq!(drops[0]["outcome"], "refused");
    assert_eq!(drops[1]["outcome"], "ok");
}

#[tokio::test]
async fn sql_ddl_is_logged_by_kind_and_name_only_and_dml_is_not() {
    let app = build("sqlddl", None, false, None);
    for sql in [
        "CREATE SCHEMA audit_s",
        "CREATE TABLE audit_t (id INTEGER PRIMARY KEY, topsecretcolumn TEXT)",
        "CREATE INDEX audit_i ON audit_t (topsecretcolumn)",
        "INSERT INTO audit_t (id, topsecretcolumn) VALUES (1, 'ROW-DATA-MUST-NOT-APPEAR')",
        "SELECT topsecretcolumn FROM audit_t WHERE id = 1",
        "EXPLAIN DROP TABLE audit_t",
        "DROP INDEX audit_i ON audit_t",
        "DROP TABLE audit_t",
    ] {
        let (s, body) = app.sql(ADMIN_KEY, sql).await;
        assert!(s.is_success(), "{sql}: {s} {body}");
    }
    let got: Vec<(String, String, String, String)> = app
        .lines()
        .iter()
        .map(|e| {
            (
                e["code"].as_str().unwrap().to_string(),
                e["object_kind"].as_str().unwrap().to_string(),
                e["object"].as_str().unwrap().to_string(),
                e["outcome"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let want = [
        ("catalog.create", "schema", "audit_s"),
        ("catalog.create", "table", "audit_t"),
        ("catalog.create", "index", "audit_i"),
        ("catalog.drop", "index", "audit_i"),
        ("catalog.drop", "table", "audit_t"),
    ];
    assert_eq!(
        got.len(),
        want.len(),
        "INSERT/SELECT/EXPLAIN must not log: {got:?}"
    );
    for (g, w) in got.iter().zip(want) {
        assert_eq!(
            (g.0.as_str(), g.1.as_str(), g.2.as_str(), g.3.as_str()),
            (w.0, w.1, w.2, "ok")
        );
    }
    let raw = app.raw_log();
    for leaked in [
        "ROW-DATA-MUST-NOT-APPEAR",
        "topsecretcolumn",
        "INSERT",
        "SELECT",
        "VALUES",
        "Bearer",
        ADMIN_KEY,
    ] {
        assert!(!raw.contains(leaked), "log leaked {leaked:?}: {raw}");
    }
}

#[tokio::test]
async fn ddl_failure_and_reader_denial_are_recorded_with_the_right_outcome() {
    let app = build("ddlfail", None, false, None);
    app.sql(ADMIN_KEY, "CREATE TABLE dup_t (id INTEGER PRIMARY KEY)")
        .await;
    let (s, _) = app
        .sql(ADMIN_KEY, "CREATE TABLE dup_t (id INTEGER PRIMARY KEY)")
        .await;
    assert!(!s.is_success());
    let (s, _) = app.sql(READER_KEY, "DROP TABLE dup_t").await;
    assert!(!s.is_success(), "reader must not drop");
    let l = app.lines();
    assert_eq!(l.len(), 3, "{}", app.raw_log());
    assert_eq!(l[0]["outcome"], "ok");
    assert_eq!(l[1]["outcome"], "failed");
    assert_eq!(l[2]["code"], "catalog.drop");
    assert_eq!(l[2]["principal"], "reader");
    assert!(
        l[2]["outcome"] == "denied" || l[2]["outcome"] == "failed",
        "reader DROP outcome was {}",
        l[2]["outcome"]
    );
    // The reader's attempt must not have dropped the table.
    let (s, _) = app.sql(ADMIN_KEY, "SELECT id FROM dup_t").await;
    assert!(s.is_success());
}

#[tokio::test]
async fn hostile_identifiers_cannot_forge_log_lines() {
    let app = build("hostile", None, false, None);
    // A quoted identifier is attacker-controlled text that reaches the log.
    let (s, _) = app
        .sql(ADMIN_KEY, "CREATE TABLE \"x\u{1b}[2J\u{7}{\\\"code\\\":\\\"forged\\\"}\" (id INTEGER PRIMARY KEY)")
        .await;
    let l = app.lines(); // panics if any line is not exactly one JSON object
    let raw = app.raw_log();
    assert!(
        !raw.contains('\u{1b}') && !raw.contains('\u{7}'),
        "control characters must be escaped"
    );
    for e in &l {
        assert_ne!(e["code"], "forged");
    }
    let _ = s; // accepted or rejected by the parser -- either way the log stays well-formed
    assert!(raw.lines().all(|x| x.starts_with('{') && x.ends_with('}')));
    let _ = Path::new(&app.log_dir);
}

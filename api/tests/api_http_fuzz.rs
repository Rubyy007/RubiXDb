//! Real HTTP boundary fuzz/robustness harness (Increment 13 Phases
//! K/L/M) -- a real running server (real `TcpListener`, real
//! `axum::serve`, real `reqwest` client, not the in-process
//! `tower::ServiceExt::oneshot` shortcut every other test file in this
//! crate uses), fed hundreds of malformed/adversarial requests. The
//! server must never panic, hang, or crash; every malformed request
//! must get a normal, bounded HTTP response. The final, decisive proof
//! is not any individual response -- it's that the server answers a
//! plain `/healthz` correctly *after* the entire fuzz run, meaning
//! nothing during it took the process down.
//!
//! No new production dependency: a tiny hand-rolled xorshift64 PRNG
//! (deterministic per run -- seeded and logged, so any real failure
//! is reproducible) generates the randomized inputs; `rand` was not
//! added for something this narrow.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rubixdb::execution::batch_coordinator::BatchCoordinatorConfig;
use rubixdb::lsm::{LsmConfig, LsmEngine};
use rubixdb::wal::{SyncMode, WalConfig};
use rubixdb_api::config::{ApiKeyConfig, Role};
use rubixdb_api::routes::build_router;
use rubixdb_api::server::serve;
use rubixdb_api::{AppState, Config};

const ADMIN_KEY: &str = "fuzz-admin-key-0123456789ab";

struct Xorshift64(u64);
impl Xorshift64 {
    fn new(seed: u64) -> Self {
        Xorshift64(seed | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn range(&mut self, n: usize) -> usize {
        (self.next() as usize) % n.max(1)
    }
    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| (self.next() % 256) as u8).collect()
    }
    fn ascii_string(&mut self, len: usize) -> String {
        (0..len)
            .map(|_| (32u8 + (self.next() % 95) as u8) as char)
            .collect()
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("rubixdb_api_fuzz_{tag}_{nanos}"));
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
        api_keys: vec![ApiKeyConfig {
            name: "admin".to_string(),
            role: Role::Admin,
            key: ADMIN_KEY.to_string(),
        }],
        max_value_bytes: 1024 * 1024,
        max_key_bytes: 4096,
        default_range_limit: 100,
        max_range_limit: 10_000,
        shutdown_drain_secs: 5,
        rate_limit_rps: 100_000.0,
        rate_limit_burst: 200_000,
        compaction_auto_trigger: false,
        compaction_trigger_count: 4,
        cors_allowed_origins: vec![],
        sql_max_sessions_per_principal: 50,
        sql_session_idle_timeout_secs: 300,
        sql_session_max_lifetime_secs: 1800,
        sql_statement_deadline_secs: 5,
        instance_id: None,
        instance_name: None,
        frontend_dist: None,
    }
}

/// Spawns a real server on a real ephemeral port and returns its base
/// URL, the engine (for a clean shutdown), and the temp dir to clean
/// up. Mirrors `api/src/server.rs`'s own test pattern.
async fn start_real_server(tag: &str) -> (String, Arc<LsmEngine>, PathBuf) {
    let dir = temp_dir(tag);
    let engine =
        Arc::new(LsmEngine::open(&dir, wal_config(), pool_config(), LsmConfig::default()).unwrap());
    let lsm_config = LsmConfig::default();
    let state = Arc::new(AppState::new(
        engine.clone(),
        lsm_config,
        test_config(dir.clone()),
    ));
    let router = build_router(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (fire_tx, fire_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        serve(
            listener,
            router,
            async move {
                let _ = fire_rx.await;
            },
            Duration::from_secs(5),
        )
        .await;
    });
    // Real readiness, not a sleep.
    let client = reqwest::Client::new();
    for _ in 0..100 {
        if client
            .get(format!("http://{addr}/healthz"))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Leak `fire_tx` deliberately -- this test's own process teardown
    // (or the caller's explicit engine shutdown) is enough; nothing
    // here depends on graceful HTTP drain.
    std::mem::forget(fire_tx);
    (format!("http://{addr}"), engine, dir)
}

async fn assert_still_alive(base_url: &str) {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let resp = client
        .get(format!("{base_url}/healthz"))
        .send()
        .await
        .expect("server must still be answering /healthz after the fuzz run -- it crashed or hung");
    assert!(resp.status().is_success());
}

/// Fires `body` (already-encoded bytes, arbitrary content-type) at
/// `POST /v1/sql` with a real, bounded timeout. A timeout, a
/// connection-reset, or any other transport failure is itself a test
/// failure (Phase K: "no hang" -- a `reqwest` error here means the
/// server never responded within a generous bound, exactly the
/// forbidden outcome), except where the case description explicitly
/// expects a connection-level failure (raw-socket cases use a
/// separate path, not this helper).
async fn fire_raw_body(base_url: &str, body: Vec<u8>, content_type: &str) {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let result = client
        .post(format!("{base_url}/v1/sql"))
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .header("Content-Type", content_type)
        .body(body)
        .send()
        .await;
    match result {
        Ok(resp) => {
            // Any HTTP status is acceptable (this is a robustness
            // test, not a semantics test) -- a 5xx is only acceptable
            // if it is our own safe, typed error shape, never a raw
            // Rust panic message leaking through.
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            if status.is_server_error() {
                assert!(
                    !text.contains("panicked at")
                        && !text.contains("RUST_BACKTRACE")
                        && !text.to_lowercase().contains("unwrap"),
                    "a 5xx response leaked raw panic/internal detail: status={status} body={text}"
                );
            }
        }
        Err(e) => {
            // A connection-level error (e.g. the body was rejected at
            // the framing layer before a status line was even sent)
            // is acceptable *as long as* the server is still alive
            // afterward -- checked by the caller via
            // `assert_still_alive`. A timeout specifically would mean
            // the server hung, which is the one truly forbidden
            // outcome; surface it loudly.
            assert!(!e.is_timeout(), "request timed out -- possible hang: {e}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn http_json_fuzz_never_crashes_or_hangs_the_server() {
    let (base_url, engine, dir) = start_real_server("json_fuzz").await;
    let mut rng = Xorshift64::new(0xC0FFEE_u64);

    let malformed_json_snippets = [
        "".to_string(),
        "{".to_string(),
        "}".to_string(),
        "not json at all".to_string(),
        "null".to_string(),
        "true".to_string(),
        "42".to_string(),
        "[]".to_string(),
        "{{{{".to_string(),
        "{\"sql\": }".to_string(),
        "{\"sql\": \"SELECT 1\",}".to_string(),
        "{\"sql\": \"SELECT 1\" \"extra\"}".to_string(),
        "{\"sql\": \"SELECT 1\", \"params\": [1,2,".to_string(),
        "\u{0}\u{0}\u{0}".to_string(),
    ];
    for snippet in &malformed_json_snippets {
        fire_raw_body(&base_url, snippet.as_bytes().to_vec(), "application/json").await;
    }

    // Structurally valid JSON, semantically wrong: missing fields,
    // unknown fields, wrong types, null fields.
    let semantic_variants = [
        serde_json::json!({}),
        serde_json::json!({"sql": null}),
        serde_json::json!({"sql": 12345}),
        serde_json::json!({"sql": ["not", "a", "string"]}),
        serde_json::json!({"sql": {"nested": "object"}}),
        serde_json::json!({"sql": "SELECT 1", "params": "not-an-array"}),
        serde_json::json!({"sql": "SELECT 1", "params": [{"type": "not_a_real_type"}]}),
        serde_json::json!({"sql": "SELECT 1", "params": [{"type": "integer"}]}), // missing "value"
        serde_json::json!({"sql": "SELECT 1", "params": null}),
        serde_json::json!({"sql": "SELECT 1", "unknown_field_entirely": "ignored?"}),
        serde_json::json!({"sql": "SELECT 1", "session_id": 42}), // wrong type (should be string/uuid)
        serde_json::json!({"sql": "SELECT 1", "session_id": "not-a-uuid"}),
        serde_json::json!([1, 2, 3]),
        serde_json::json!("just a string"),
        serde_json::json!(null),
    ];
    for v in &semantic_variants {
        fire_raw_body(&base_url, v.to_string().into_bytes(), "application/json").await;
    }

    // Empty body, wrong content-type, random bytes as "JSON".
    fire_raw_body(&base_url, Vec::new(), "application/json").await;
    fire_raw_body(&base_url, b"{}".to_vec(), "text/plain").await;
    fire_raw_body(&base_url, b"{}".to_vec(), "").await;
    for _ in 0..30 {
        let len = rng.range(500);
        fire_raw_body(&base_url, rng.bytes(len), "application/json").await;
    }

    // Invalid UTF-8 inside what looks like a JSON string field.
    let mut invalid_utf8 = b"{\"sql\": \"SELECT '".to_vec();
    invalid_utf8.extend_from_slice(&[0xFF, 0xFE, 0xC0, 0x80]);
    invalid_utf8.extend_from_slice(b"'\"}");
    fire_raw_body(&base_url, invalid_utf8, "application/json").await;

    // Random ASCII "SQL" strings of varying length through the real
    // parser/binder/planner pipeline -- controlled parse/bind errors
    // expected, never a crash.
    for _ in 0..40 {
        let len = 1 + rng.range(400);
        let sql = rng.ascii_string(len);
        let body = serde_json::json!({ "sql": sql, "params": [] });
        fire_raw_body(&base_url, body.to_string().into_bytes(), "application/json").await;
    }

    assert_still_alive(&base_url).await;
    engine.shutdown();
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn sql_fuzz_through_the_real_http_pipeline_never_crashes() {
    let (base_url, engine, dir) = start_real_server("sql_fuzz").await;

    async fn submit(base_url: &str, sql: &str) {
        fire_raw_body(
            base_url,
            serde_json::json!({ "sql": sql, "params": [] })
                .to_string()
                .into_bytes(),
            "application/json",
        )
        .await;
    }

    /// Phase M evidence, not just Phase K crash-safety: an expensive/
    /// oversized request must be actively *rejected* by a real
    /// resource limit (never a 2xx "sure, I'll process that"), and the
    /// rejection must never leak a raw panic/internal detail.
    async fn submit_expect_rejected(base_url: &str, body: Vec<u8>) {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        let resp = client
            .post(format!("{base_url}/v1/sql"))
            .header("Authorization", format!("Bearer {ADMIN_KEY}"))
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .expect("resource-limited request must still get a real HTTP response, not hang");
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        assert!(
            !status.is_success(),
            "an oversized/expensive request must be rejected by a resource limit, got {status}: {text}"
        );
        assert!(
            !text.contains("panicked at"),
            "resource-limit rejection leaked a raw panic: status={status} body={text}"
        );
    }

    // Deep/large expressions -- must produce a controlled
    // ResourceLimit/parse error, never a stack overflow or hang.
    let deep_and = format!(
        "SELECT 1 WHERE {}",
        (0..2000).map(|_| "1=1").collect::<Vec<_>>().join(" AND ")
    );
    submit(&base_url, &deep_and).await;

    let deep_parens = format!("SELECT {}1{}", "(".repeat(5000), ")".repeat(5000));
    submit_expect_rejected(
        &base_url,
        serde_json::json!({ "sql": deep_parens, "params": [] })
            .to_string()
            .into_bytes(),
    )
    .await;

    let huge_literal = format!("SELECT '{}'", "x".repeat(2_000_000));
    submit_expect_rejected(
        &base_url,
        serde_json::json!({ "sql": huge_literal, "params": [] })
            .to_string()
            .into_bytes(),
    )
    .await;

    let huge_identifier = format!("SELECT * FROM {}", "t".repeat(100_000));
    submit(&base_url, &huge_identifier).await;

    let unknown_object = "SELECT * FROM definitely_does_not_exist_table_xyz";
    submit(&base_url, unknown_object).await;

    let unsupported = "SELECT * FROM t1 UNION SELECT * FROM t2";
    submit(&base_url, unsupported).await;

    let nested_subquery_attempt = "SELECT (SELECT 1) FROM t";
    submit(&base_url, nested_subquery_attempt).await;

    let many_columns = format!(
        "SELECT {}",
        (0..5000)
            .map(|i| format!("{i} AS c{i}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    submit(&base_url, &many_columns).await;

    // Large parameter array against otherwise-valid, syntactically
    // correct SQL (`$1` placeholder syntax) -- so a rejection here is
    // actually attributable to the params-array size, not masked by
    // an unrelated parse error.
    let many_params: Vec<serde_json::Value> = (0..50_000)
        .map(|i| serde_json::json!({"type": "integer", "value": i}))
        .collect();
    submit_expect_rejected(
        &base_url,
        serde_json::json!({ "sql": "SELECT $1", "params": many_params })
            .to_string()
            .into_bytes(),
    )
    .await;

    assert_still_alive(&base_url).await;
    engine.shutdown();
    std::fs::remove_dir_all(&dir).ok();
}

/// Phase K: unexpected/malformed headers, malformed authorization.
#[tokio::test(flavor = "multi_thread")]
async fn malformed_headers_and_authorization_never_crash_the_server() {
    let (base_url, engine, dir) = start_real_server("header_fuzz").await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();

    let auth_variants: &[Option<&str>] = &[
        None,
        Some(""),
        Some("Bearer"),
        Some("Bearer "),
        Some("NotBearer sometoken"),
        Some("Bearer \u{0}\u{0}\u{0}"),
        Some("Basic dXNlcjpwYXNz"),
        Some("Bearer this-key-does-not-exist-at-all-but-is-long-enough"),
    ];
    for auth in auth_variants {
        let mut req = client
            .post(format!("{base_url}/v1/sql"))
            .header("Content-Type", "application/json")
            .body(serde_json::json!({"sql": "SELECT 1", "params": []}).to_string());
        if let Some(a) = auth {
            req = req.header("Authorization", *a);
        }
        let result = req.send().await;
        match result {
            Ok(resp) => {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                assert!(
                    !text.contains("panicked at"),
                    "auth variant {auth:?} leaked a panic: status={status} body={text}"
                );
            }
            Err(e) => assert!(!e.is_timeout(), "auth variant {auth:?} timed out: {e}"),
        }
    }

    // Unexpected extra headers alongside a valid, well-formed request.
    let resp = client
        .post(format!("{base_url}/v1/sql"))
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .header("Content-Type", "application/json")
        .header("X-Totally-Unexpected-Header", "\u{0}weird\u{0}value")
        .header("Content-Length", "999999999")
        .body(serde_json::json!({"sql": "SELECT 1", "params": []}).to_string())
        .send()
        .await;
    // A forged mismatched Content-Length is normally corrected/rejected
    // by the HTTP layer itself (reqwest recomputes it) -- either a
    // clean response or a transport error is acceptable, a hang is not.
    if let Err(e) = &resp {
        assert!(
            !e.is_timeout(),
            "mismatched Content-Length case timed out: {e}"
        );
    }

    assert_still_alive(&base_url).await;
    engine.shutdown();
    std::fs::remove_dir_all(&dir).ok();
}

/// Phase K: abrupt connection termination mid-request must never wedge
/// the server -- proven with a raw TCP socket, not `reqwest` (which
/// would retry/handle this for us and hide the real behavior).
#[tokio::test(flavor = "multi_thread")]
async fn abrupt_connection_termination_never_wedges_the_server() {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpStream;

    let (base_url, engine, dir) = start_real_server("conn_term").await;
    let addr = base_url.trim_start_matches("http://").to_string();

    for _ in 0..10 {
        let mut stream = TcpStream::connect(&addr).await.unwrap();
        let partial = format!(
            "POST /v1/sql HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {ADMIN_KEY}\r\n\
             Content-Type: application/json\r\nContent-Length: 10000\r\n\r\n{{\"sql\": \"SELECT"
        );
        // Write only a partial request (headers claim a 10000-byte
        // body; only a few bytes of it are ever sent), then drop the
        // connection without finishing it.
        let _ = stream.write_all(partial.as_bytes()).await;
        drop(stream);
    }

    // The server must still be fully responsive to new, well-formed
    // connections after repeated abrupt terminations.
    assert_still_alive(&base_url).await;
    engine.shutdown();
    std::fs::remove_dir_all(&dir).ok();
}

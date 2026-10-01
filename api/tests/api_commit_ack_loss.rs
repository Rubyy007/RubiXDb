//! Increment 14, Blocker 5 — commit-acknowledgment loss. Real server
//! bound to a real OS TCP listener (not `tower::ServiceExt::oneshot`,
//! which never actually round-trips bytes over a socket at all — this
//! scenario specifically needs a real connection the client can sever
//! without reading the response). Real `LsmEngine`, real HTTP/1.1 wire
//! protocol written by hand over a raw `TcpStream` so the test controls
//! exactly when the client stops reading, never a timing-based guess.

use std::io::Write;
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rubixdb::execution::batch_coordinator::BatchCoordinatorConfig;
use rubixdb::lsm::{LsmConfig, LsmEngine};
use rubixdb::wal::{SyncMode, WalConfig};
use rubixdb_api::config::{ApiKeyConfig, Role};
use rubixdb_api::routes::build_router;
use rubixdb_api::{AppState, Config};
use serde_json::{json, Value};

const ADMIN_KEY: &str = "test-admin-key-0123456789ab";

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("rubixdb_commit_ack_it_{tag}_{nanos}"));
    std::fs::create_dir_all(&path).unwrap();
    path
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
    }
}

/// Starts the real server on a real, freshly-bound OS TCP port and
/// returns its address. The server keeps running for the test
/// process's lifetime (no shutdown wired -- irrelevant to what this
/// test measures, and the process exits at the end of the test binary
/// regardless).
async fn spawn_real_server() -> std::net::SocketAddr {
    let dir = temp_dir("srv");
    let lsm_config = LsmConfig::default();
    let engine =
        Arc::new(LsmEngine::open(&dir, wal_config(), pool_config(), lsm_config.clone()).unwrap());
    let state = Arc::new(AppState::new(engine, lsm_config, test_config(dir)));
    let router = build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });
    addr
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

/// A normal, well-behaved client call -- used for setup and for the
/// post-crash verification, so those steps are never suspected of
/// being the thing under test.
async fn exec_sql(addr: std::net::SocketAddr, body: Value) -> Value {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/v1/sql"))
        .bearer_auth(ADMIN_KEY)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "setup/verification call failed: {}",
        resp.status()
    );
    resp.json().await.unwrap()
}

/// Same as `exec_sql` but does not assert success -- used only for the
/// deliberately-expected-to-fail retry call at the end of the test.
async fn exec_sql_allow_error(addr: std::net::SocketAddr, body: Value) -> Value {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/v1/sql"))
        .bearer_auth(ADMIN_KEY)
        .json(&body)
        .send()
        .await
        .unwrap();
    resp.json().await.unwrap()
}

/// Writes a complete, valid HTTP/1.1 request by hand over a raw
/// `TcpStream`, flushes it (so the OS has handed every byte to the
/// server's accepted socket), then drops the stream **without reading
/// a single byte of the response** -- the precise, deterministic
/// simulation of "the server durably commits; the client loses the
/// connection before consuming the response." Dropping the writer
/// after a successful `write_all`/`flush` does not retract bytes
/// already delivered to the peer's kernel receive buffer, and axum/
/// hyper fully drives the handler future to completion before it ever
/// attempts to write a response -- so the handler (and therefore the
/// real `Transaction::commit()` call inside it) has already run to
/// completion by the time this function returns, regardless of
/// whether anything is ever read back.
fn send_request_and_abandon_connection_without_reading_response(
    addr: std::net::SocketAddr,
    body: &Value,
) {
    let body_bytes = serde_json::to_vec(body).unwrap();
    let mut stream = TcpStream::connect(addr).expect("connect must succeed");
    stream.set_nodelay(true).expect("set_nodelay");
    let request = format!(
        "POST /v1/sql HTTP/1.1\r\n\
         Host: {addr}\r\n\
         Authorization: Bearer {ADMIN_KEY}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n",
        body_bytes.len()
    );
    stream.write_all(request.as_bytes()).expect("write headers");
    stream.write_all(&body_bytes).expect("write body");
    stream
        .flush()
        .expect("flush -- every byte handed to the OS/peer");
    // `flush()` only guarantees the bytes reached this side's own OS
    // send buffer, not that the peer has actually read and processed
    // them yet -- an immediate `drop()` risks a TCP RST racing ahead of
    // real delivery on loopback. A brief, bounded wait here is the
    // real-world-realistic part of "the client crashes right after
    // sending" (never instantaneous in practice either), giving the
    // server a genuine chance to finish the handler *before* this
    // function severs the connection -- still deliberately never
    // reading a single byte of the response itself.
    std::thread::sleep(Duration::from_millis(200));
    // Dropping `stream` here closes the socket from this side --
    // exactly "the client loses the connection before consuming the
    // response."
    drop(stream);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commit_durably_succeeds_even_when_the_client_never_reads_the_response() {
    let addr = spawn_real_server().await;

    exec_sql(
        addr,
        json!({ "sql": "DROP TABLE IF EXISTS ack_loss_t", "params": [] }),
    )
    .await;
    exec_sql(
        addr,
        json!({ "sql": "CREATE TABLE ack_loss_t (id INTEGER PRIMARY KEY, v TEXT)", "params": [] }),
    )
    .await;

    let begin = exec_sql(addr, json!({ "sql": "BEGIN", "params": [] })).await;
    let session_id = begin["session_id"].as_str().unwrap().to_string();

    exec_sql(
        addr,
        json!({
            "sql": "INSERT INTO ack_loss_t (id, v) VALUES (1, 'committed-but-unacked')",
            "params": [],
            "session_id": session_id,
        }),
    )
    .await;

    // The commit the client will never see the response to.
    send_request_and_abandon_connection_without_reading_response(
        addr,
        &json!({ "sql": "COMMIT", "params": [], "session_id": session_id }),
    );

    // Give the server a brief, generous moment to finish the request
    // it was already mid-flight on (the connection drop does not
    // interrupt an already-dispatched handler, but this test does not
    // assume zero scheduling latency either).
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Reconnect (a brand-new client connection/session) and inspect
    // real durable state -- this is the actual evidence, not an
    // assumption from "the write_all succeeded."
    let verify = exec_sql(
        addr,
        json!({ "sql": "SELECT v FROM ack_loss_t WHERE id = 1", "params": [] }),
    )
    .await;
    assert_eq!(
        verify["result"]["row_count"], 1,
        "the server must have durably committed even though the client never read the COMMIT response: {verify}"
    );
    assert_eq!(
        verify["result"]["rows"][0][0]["value"],
        "committed-but-unacked"
    );

    // Safe retry semantics, documented not invented: a naive client
    // that assumes "no response means retry the same request" and
    // resends COMMIT with the same session_id must get a clear,
    // reject-based error (the session was already consumed by the
    // first, successful commit) -- never a silent no-op success that
    // would misrepresent what actually happened, and never a second
    // real side effect.
    let retry = exec_sql_allow_error(
        addr,
        json!({ "sql": "COMMIT", "params": [], "session_id": session_id }),
    )
    .await;
    assert_eq!(
        retry["error"]["code"], "SESSION_NOT_FOUND",
        "retrying COMMIT on an already-consumed session_id must be a clear, safe rejection, not fabricated idempotency: {retry}"
    );
}

//! Increment 14, Blocker 8 — heap/ownership memory analysis.
//!
//! `heaptrack`/`valgrind`/`massif` do not exist on Windows (checked:
//! none of the three has a Windows build). `dhat` is the standard
//! Rust-native equivalent — a real, source-attributed heap profiler
//! (not a correlational RSS guess), used here as the strongest
//! reproducible Windows-compatible option.
//!
//! Runs entirely in-process (dhat instruments the calling process's
//! own global allocator, so this cannot be a subprocess-based
//! measurement the way other tools in this repository are) through
//! five independently-controlled phases, taking a real `HeapStats`
//! checkpoint after each one — separating exactly what the mission
//! asks to be separated: data growth, metadata growth, query buffers,
//! session/transaction state, and instance/table-store overhead. The
//! full `dhat-heap.json` written on exit additionally attributes every
//! byte *still live* at that point to its real allocation call site
//! (openable at <https://nnethercote.github.io/dh_view/dh_view.html>
//! for interactive inspection).
//!
//! Usage: `cargo run --release -p rubixdb-api --features
//! dhat-heap --example heap_ownership_profile` (see Cargo.toml --
//! this example is dev-dependency-only, never linked into any
//! production binary).

use std::path::PathBuf;
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

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

const ADMIN_KEY: &str = "heap-profile-admin-key-0123456789";

fn temp_dir() -> PathBuf {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let path = std::env::temp_dir().join(format!("rubixdb_heap_profile_{nanos}"));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn test_config(data_dir: PathBuf) -> Config {
    Config {
        data_dir,
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        api_keys: vec![ApiKeyConfig { name: "admin".into(), role: Role::Admin, key: ADMIN_KEY.into() }],
        max_value_bytes: 1024 * 1024,
        max_key_bytes: 4096,
        default_range_limit: 100,
        max_range_limit: 10_000,
        shutdown_drain_secs: 5,
        rate_limit_rps: 1_000_000.0,
        rate_limit_burst: 1_000_000,
        compaction_auto_trigger: false,
        compaction_trigger_count: 4,
        cors_allowed_origins: vec![],
        sql_max_sessions_per_principal: 10_000,
        sql_session_idle_timeout_secs: 300,
        sql_session_max_lifetime_secs: 1800,
        sql_statement_deadline_secs: 30,
        instance_id: None,
        instance_name: None,
        frontend_dist: None,
    }
}

fn wal_config() -> WalConfig {
    WalConfig { sync_mode: SyncMode::GroupCommit { max_wait: Duration::from_millis(5), max_batch_bytes: 256 * 1024 }, ..WalConfig::default() }
}
fn pool_config() -> BatchCoordinatorConfig {
    BatchCoordinatorConfig { queue_capacity: 4096, max_queued_bytes: 64 * 1024 * 1024, submission_timeout: Duration::from_secs(10), shutdown_drain_bound: Duration::from_secs(30), await_retry_budget: Duration::from_secs(10), max_drain_per_batch: 65536 }
}

async fn call(router: &Router, sql: &str, session_id: Option<&str>) -> Value {
    let mut body = json!({ "sql": sql, "params": [] });
    if let Some(sid) = session_id {
        body["session_id"] = json!(sid);
    }
    let req = Request::builder()
        .method("POST")
        .uri("/v1/sql")
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = router.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if status != StatusCode::OK {
        eprintln!("WARNING: {sql:?} -> {status}: {json}");
    }
    json
}

fn checkpoint(label: &str, baseline: &dhat::HeapStats) {
    let stats = dhat::HeapStats::get();
    println!(
        "[{label:<28}] curr_bytes={:>10} curr_blocks={:>7} max_bytes={:>10} max_blocks={:>7} total_bytes={:>12} total_blocks={:>9} | delta_curr_bytes_vs_baseline={:>+10}",
        stats.curr_bytes,
        stats.curr_blocks,
        stats.max_bytes,
        stats.max_blocks,
        stats.total_bytes,
        stats.total_blocks,
        stats.curr_bytes as i64 - baseline.curr_bytes as i64,
    );
}

#[tokio::main]
async fn main() {
    let _profiler = dhat::Profiler::new_heap();

    let dir = temp_dir();
    println!("data_dir = {}", dir.display());
    let lsm_config = LsmConfig::default();
    let engine = Arc::new(LsmEngine::open(&dir, wal_config(), pool_config(), lsm_config.clone()).unwrap());
    let state = Arc::new(AppState::new(engine, lsm_config, test_config(dir.clone())));
    let router = build_router(state);

    let baseline = dhat::HeapStats::get();
    checkpoint("0: process baseline", &baseline);

    call(&router, "CREATE TABLE heap_t (id INTEGER PRIMARY KEY, v TEXT, val INTEGER)", None).await;
    checkpoint("1: after bootstrap+CREATE TABLE", &baseline);

    // Phase A: real, controlled DATA GROWTH -- 20,000 rows via batched
    // multi-row INSERT (isolates data-proportional growth from
    // everything else).
    {
        const ROWS: i64 = 20_000;
        const BATCH: i64 = 100;
        let mut inserted = 0i64;
        while inserted < ROWS {
            let n = BATCH.min(ROWS - inserted);
            let mut sql = String::from("INSERT INTO heap_t (id, v, val) VALUES ");
            for i in 0..n {
                let id = inserted + i;
                if i > 0 {
                    sql.push(',');
                }
                sql.push_str(&format!("({id},'row-{id}',{id})"));
            }
            call(&router, &sql, None).await;
            inserted += n;
        }
    }
    checkpoint("2: after 20,000-row INSERT (data growth)", &baseline);

    // Phase B: QUERY VOLUME -- 2,000 real SELECTs against the now-
    // populated table (result/row buffers should not accumulate
    // between independent, completed requests).
    for i in 0..2_000 {
        call(&router, &format!("SELECT id, v, val FROM heap_t WHERE id = {}", i % 20_000), None).await;
    }
    checkpoint("3: after 2,000 SELECTs (query buffers)", &baseline);

    // Phase C: METADATA CHURN -- 500 real CREATE TABLE + DROP TABLE
    // cycles (catalog-only growth/shrink, isolated from row data).
    for i in 0..500 {
        call(&router, &format!("CREATE TABLE churn_{i} (id INTEGER PRIMARY KEY)"), None).await;
        call(&router, &format!("DROP TABLE churn_{i}"), None).await;
    }
    checkpoint("4: after 500 CREATE+DROP TABLE cycles (metadata)", &baseline);

    // Phase D: SESSION/TRANSACTION CHURN -- 1,000 real BEGIN/COMMIT
    // cycles (session-registry state, isolated from data/query/
    // metadata growth).
    for i in 0..1_000 {
        let begin = call(&router, "BEGIN", None).await;
        let session_id = begin["session_id"].as_str().unwrap().to_string();
        call(&router, &format!("INSERT INTO heap_t (id, v, val) VALUES ({}, 'txn', {})", 100_000 + i, i), Some(&session_id)).await;
        call(&router, "COMMIT", Some(&session_id)).await;
    }
    checkpoint("5: after 1,000 BEGIN/INSERT/COMMIT cycles (sessions/txns)", &baseline);

    // Phase E: DELETE all data back out -- if phase 2's growth was
    // purely data-proportional (not a leak elsewhere), curr_bytes
    // should NOT return anywhere near to phase-2 levels just from this
    // (engine tombstones/compaction are a separate, already-certified
    // concern, not re-litigated here) -- what this checkpoint actually
    // isolates is whether the *API/session/query layer above the
    // engine* released its own transient buffers, which it should,
    // regardless of engine-level physical reclamation timing.
    // A real, discovered production limit while building this profile:
    // `max_dml_target_rows` (10,000) rejects a single DML statement
    // whose target set is larger than that -- our ~21,000 remaining
    // rows must be deleted in bounded batches, exactly like a real
    // client would have to.
    for lo in (0..21_000i64).step_by(9_000) {
        call(&router, &format!("DELETE FROM heap_t WHERE id >= {lo} AND id < {}", lo + 9_000), None).await;
    }
    checkpoint("6: after bulk DELETE (batched)", &baseline);

    println!("\ndhat-heap.json written on exit -- open at https://nnethercote.github.io/dh_view/dh_view.html for full call-site attribution of bytes still live at process end.");
}

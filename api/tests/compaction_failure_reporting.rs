//! ADR-COMPACTION-LEAK-01 at the API level: a compaction worker that keeps failing on an unreadable input is
//! visible on every existing compaction surface (additive fields), stops after its budget, leaves no partial
//! file, writes its state changes (not its retries) to the security log, and changes nothing about readiness.
//!
//! Real engine, real background worker (100 ms retry tick), real SSTables - one flipped bit in the first data
//! block of the oldest of four live tables - driven through the real router.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use rubixdb::execution::batch_coordinator::BatchCoordinatorConfig;
use rubixdb::lsm::{LsmConfig, LsmEngine};
use rubixdb::wal::{SyncMode, WalConfig};
use rubixdb_api::config::{ApiKeyConfig, Role};
use rubixdb_api::observability::sampler;
use rubixdb_api::routes::build_router;
use rubixdb_api::security_log::{SecurityLog, SecurityLogLayer};
use rubixdb_api::{AppState, Config};
use serde_json::Value;
use tower::ServiceExt;
use tracing_subscriber::layer::SubscriberExt;

const ADMIN_KEY: &str = "test-admin-key-0123456789ab";

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("rubixdb_api_compfail_{tag}_{nanos}"));
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

fn lsm_config(auto: bool) -> LsmConfig {
    LsmConfig {
        memtable_max_size_bytes: 200,
        max_immutable_memtables: 32,
        compaction_trigger_count: 4,
        compaction_auto_trigger: auto,
        storage_pressure_retry_interval: Duration::from_millis(100),
        ..LsmConfig::default()
    }
}

fn test_config(data_dir: PathBuf, auto: bool) -> Config {
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
        compaction_auto_trigger: auto,
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

/// Exactly four live tables built offline (worker off), the oldest one's first data block damaged by one bit.
fn four_tables_oldest_damaged(dir: &Path, damage: bool) {
    let engine = LsmEngine::open(dir, wal_config(), pool_config(), lsm_config(false)).unwrap();
    let mut i = 0u64;
    while engine.sstable_count() < 4 {
        engine
            .put(
                format!("k{:06}", i % 500).as_bytes(),
                format!("v{i}").as_bytes(),
            )
            .unwrap();
        i += 1;
        let deadline = Instant::now() + Duration::from_secs(10);
        while engine.immutable_count() != 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    engine.shutdown();
    drop(engine);
    if damage {
        let mut ssts: Vec<PathBuf> = std::fs::read_dir(dir.join("sstables"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "sst"))
            .collect();
        ssts.sort();
        let mut bytes = std::fs::read(&ssts[0]).unwrap();
        bytes[6] ^= 0x01;
        std::fs::write(&ssts[0], bytes).unwrap();
    }
}

fn tmp_files(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir.join("sstables"))
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| n.ends_with(".sst.tmp"))
                .collect()
        })
        .unwrap_or_default()
}

async fn get(router: &Router, uri: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("GET")
        .uri(uri)
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .body(Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(request).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Installs the real `SecurityLogLayer` as this test binary's global tracing subscriber (the sampler runs on its
/// own thread, so a thread-local dispatcher would not see its events) and returns the log file path.
fn security_log_path() -> PathBuf {
    LOG_PATH
        .get_or_init(|| {
            let dir = temp_dir("seclog");
            let log = Arc::new(SecurityLog::open_in(&dir));
            let path = log.path().to_path_buf();
            let subscriber = tracing_subscriber::registry().with(SecurityLogLayer::new(log));
            tracing::subscriber::set_global_default(subscriber)
                .expect("this binary installs exactly one global subscriber");
            path
        })
        .clone()
}

fn log_lines(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

#[tokio::test]
async fn a_healthy_engine_reports_idle_and_zero_failures_on_every_compaction_surface() {
    let dir = temp_dir("healthy");
    four_tables_oldest_damaged(&dir, false);
    let lsm = lsm_config(false); // the worker is off: nothing runs
    let engine = Arc::new(LsmEngine::open(&dir, wal_config(), pool_config(), lsm.clone()).unwrap());
    let state = Arc::new(AppState::new(engine, lsm, test_config(dir.clone(), false)));
    let router = build_router(state.clone());

    let (st, status) = get(&router, "/v1/compaction/status").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(status["state"], "idle");
    assert_eq!(status["failures_total"], 0);
    assert_eq!(status["consecutive_failures"], 0);
    assert_eq!(status["blocked"], false);
    assert!(status["last_failure"].is_null());
    assert_eq!(status["cycles_completed"], 0);

    let (_, metrics) = get(&router, "/v1/compaction/metrics").await;
    assert_eq!(metrics["state"], "idle");
    assert_eq!(metrics["failures_total"], 0);
    assert!(metrics["last_failure"].is_null());

    let (_, admin) = get(&router, "/v1/admin/status").await;
    assert_eq!(admin["compaction"]["state"], "idle");
    assert_eq!(admin["compaction"]["blocked"], false);
    assert!(admin["compaction"]["last_failure"].is_null());

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_blocked_compaction_is_visible_on_every_surface_and_changes_nothing_else() {
    let log_path = security_log_path();
    let dir = temp_dir("blocked");
    four_tables_oldest_damaged(&dir, true);
    let lsm = lsm_config(true);
    let engine = Arc::new(LsmEngine::open(&dir, wal_config(), pool_config(), lsm.clone()).unwrap());
    let state = Arc::new(AppState::new(engine, lsm, test_config(dir.clone(), true)));
    let router = build_router(state.clone());
    let _sampler = sampler::start(&state).expect("sampler starts");

    // wait for the worker to spend its budget
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        let (st, body) = get(&router, "/v1/compaction/status").await;
        assert_eq!(st, StatusCode::OK);
        if body["blocked"] == true {
            break body;
        }
        assert!(
            Instant::now() < deadline,
            "the worker never blocked: {body}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    // /v1/compaction/status
    assert_eq!(status["state"], "blocked");
    assert_eq!(
        status["cycles_completed"], 0,
        "a failure is not a completed cycle"
    );
    assert_eq!(status["failures_total"], 3);
    assert_eq!(status["consecutive_failures"], 3);
    let lf = &status["last_failure"];
    assert_eq!(lf["kind"], "corruption");
    assert!(
        lf["message"]
            .as_str()
            .unwrap()
            .contains("checksum mismatch"),
        "{lf}"
    );
    assert!(lf["at_unix_ms"].as_u64().unwrap() > 0);

    // /v1/compaction/metrics
    let (_, metrics) = get(&router, "/v1/compaction/metrics").await;
    assert_eq!(metrics["state"], "blocked");
    assert_eq!(metrics["cycles_completed"], 0);
    assert!(metrics["last_cycle"].is_null());
    assert_eq!(
        (
            metrics["failures_total"].clone(),
            metrics["blocked"].clone()
        ),
        (3.into(), true.into())
    );
    assert_eq!(metrics["last_failure"]["kind"], "corruption");

    // /v1/admin/status
    let (_, admin) = get(&router, "/v1/admin/status").await;
    let c = &admin["compaction"];
    assert_eq!(c["state"], "blocked");
    assert_eq!(c["failures_total"], 3);
    assert_eq!(c["cycles_completed"], 0);
    assert_eq!(c["last_failure"]["kind"], "corruption");

    // /v1/metrics/system (from the sampler snapshot; wait for one that has seen the block)
    let deadline = Instant::now() + Duration::from_secs(15);
    let sys = loop {
        let (st, body) = get(&router, "/v1/metrics/system").await;
        if st == StatusCode::OK && body["compaction"]["state"] == "blocked" {
            break body;
        }
        assert!(
            Instant::now() < deadline,
            "metrics/system never showed the block: {st} {body}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(sys["compaction"]["failures_total"], 3);
    assert_eq!(sys["compaction"]["blocked"], true);
    assert_eq!(sys["compaction"]["last_failure"]["kind"], "corruption");

    // readiness and the storage state are NOT touched (decision D5; ADR-WE-SP-001's model is unchanged)
    let (_, ready) = get(&router, "/readyz").await;
    assert_eq!(ready["ready"], true);
    assert_eq!(ready["storage_state"], "Healthy");
    let (_, st) = get(&router, "/v1/status").await;
    assert_eq!(st["storage_state"], "Healthy");

    // the worker stopped: no more failures, no leftovers
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let (_, later) = get(&router, "/v1/compaction/status").await;
    assert_eq!(later["failures_total"], 3);
    assert!(tmp_files(&dir).is_empty(), "{:?}", tmp_files(&dir));

    // security log: ONE event per state change (not one per retry), no bytes
    let deadline = Instant::now() + Duration::from_secs(10);
    while !log_lines(&log_path)
        .iter()
        .any(|e| e["code"] == "compaction.blocked")
    {
        assert!(Instant::now() < deadline, "no compaction.blocked event");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    tokio::time::sleep(Duration::from_millis(2500)).await; // two more sampler ticks
    let lines = log_lines(&log_path);
    let failing: Vec<_> = lines
        .iter()
        .filter(|e| e["code"] == "compaction.failing")
        .collect();
    let blocked: Vec<_> = lines
        .iter()
        .filter(|e| e["code"] == "compaction.blocked")
        .collect();
    assert_eq!(failing.len(), 1, "{failing:?}");
    assert_eq!(blocked.len(), 1, "{blocked:?}");
    assert_eq!(blocked[0]["object_kind"], "compaction");
    let object = blocked[0]["object"].as_str().unwrap();
    assert!(
        object.starts_with("kind=corruption consecutive=3"),
        "{object}"
    );

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

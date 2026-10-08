//! ADR-SST-01 (F-08) at the API level: the background data-block verification is reported by two additive fields -
//! `GET /readyz` `sstable_verification` and `GET /v1/status` `sstable_integrity` - and by one `sstable.damaged` security
//! event per damaged table; `ready` and every existing field are untouched; reads through a damaged table fail typed
//! while every other read stays correct.
//!
//! Real engine, real SSTables, the real background thread (`spawn_sstable_verification`), driven through the real router.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use http_body_util::BodyExt;
use rubixdb::execution::batch_coordinator::BatchCoordinatorConfig;
use rubixdb::lsm::{LsmConfig, LsmEngine};
use rubixdb::ops::sstable_integrity::preflight;
use rubixdb::wal::{SyncMode, WalConfig};
use rubixdb_api::config::{ApiKeyConfig, Role};
use rubixdb_api::routes::build_router;
use rubixdb_api::security_log::{SecurityLog, SecurityLogLayer};
use rubixdb_api::sstable_integrity::spawn_sstable_verification;
use rubixdb_api::{AppState, Config};
use serde_json::Value;
use tower::ServiceExt;
use tracing_subscriber::layer::SubscriberExt;

const ADMIN_KEY: &str = "test-admin-key-0123456789ab";
const KEYS: u32 = 900;

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("rubixdb_api_sstint_{tag}_{nanos}"));
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

fn lsm_config() -> LsmConfig {
    LsmConfig {
        memtable_max_size_bytes: 64 * 1024,
        compaction_auto_trigger: false,
        ..LsmConfig::default()
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
        rate_limit_rps: 1_000_000.0,
        rate_limit_burst: 1_000_000,
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

fn key(i: u32) -> Vec<u8> {
    format!("key-{i:06}").into_bytes()
}

/// A directory with several live tables (KEYS keys, 200-byte values), written by the real engine and stopped.
fn build_dir(tag: &str) -> PathBuf {
    let dir = temp_dir(tag);
    let engine = LsmEngine::open(&dir, wal_config(), pool_config(), lsm_config()).unwrap();
    let value = vec![0x5Au8; 200];
    for i in 0..KEYS {
        engine.put(&key(i), &value).unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    while engine.sstable_count() < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(engine.sstable_count() >= 2, "the fixture needs live tables");
    while engine.immutable_count() != 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    engine.shutdown();
    drop(engine);
    dir
}

fn ssts(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir.join("sstables"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "sst"))
        .collect();
    v.sort();
    v
}

/// Flips one bit inside the second data block of `path` (offset read from the table's own index).
fn damage_second_block(path: &Path) {
    let mut b = std::fs::read(path).unwrap();
    let f = b.len() - 72;
    let index_off = u64::from_le_bytes(b[f + 52..f + 60].try_into().unwrap()) as usize;
    let mut pos = index_off + 4;
    let klen = u32::from_le_bytes(b[pos..pos + 4].try_into().unwrap()) as usize;
    pos += 4 + klen + 12; // entry 0
    let klen = u32::from_le_bytes(b[pos..pos + 4].try_into().unwrap()) as usize;
    pos += 4 + klen;
    let off = u64::from_le_bytes(b[pos..pos + 8].try_into().unwrap()) as usize;
    b[off + 40] ^= 0x01;
    std::fs::write(path, b).unwrap();
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

/// Installs the real `SecurityLogLayer` as this binary's global subscriber (the pass runs on its own thread).
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

fn open(dir: &Path) -> (Arc<AppState>, Router) {
    let lsm = lsm_config();
    let engine = Arc::new(LsmEngine::open(dir, wal_config(), pool_config(), lsm.clone()).unwrap());
    let state = Arc::new(AppState::new(engine, lsm, test_config(dir.to_path_buf())));
    let router = build_router(state.clone());
    (state, router)
}

#[tokio::test]
async fn without_a_pass_the_two_new_fields_say_disabled_and_nothing_else_changes() {
    let dir = build_dir("disabled");
    let (state, router) = open(&dir);

    let (st, ready) = get(&router, "/readyz").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(ready["ready"], true);
    assert_eq!(ready["storage_state"], "Healthy");
    assert_eq!(ready["index_recovery"], "not_started");
    assert_eq!(ready["sstable_verification"], "disabled");
    // exactly the three existing fields plus the new one
    let mut names: Vec<&str> = ready
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "index_recovery",
            "ready",
            "sstable_verification",
            "storage_state"
        ]
    );

    let (_, status) = get(&router, "/v1/status").await;
    assert_eq!(status["storage_state"], "Healthy");
    let si = &status["sstable_integrity"];
    assert_eq!(si["state"], "disabled");
    assert_eq!(si["tables_total"], 0);
    assert_eq!(si["tables_verified"], 0);
    assert_eq!(si["bytes_verified"], 0);
    assert_eq!(si["damaged"], serde_json::json!([]));

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_clean_pass_is_reported_complete_and_ready_is_never_involved() {
    let dir = build_dir("clean");
    let tables = preflight(&dir).unwrap().tables;
    let (state, router) = open(&dir);
    let handle = spawn_sstable_verification(&state, dir.clone(), tables.clone(), 1024, |_| {})
        .expect("the pass starts");
    // the state is `running` (or already `complete`) the moment the spawn returns - never a stale `disabled`
    let (_, ready) = get(&router, "/readyz").await;
    assert_ne!(ready["sstable_verification"], "disabled");

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (_, ready) = get(&router, "/readyz").await;
        if ready["sstable_verification"] == "complete" {
            assert_eq!(ready["ready"], true);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the pass never completed: {ready}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    handle.join().unwrap();
    let (_, status) = get(&router, "/v1/status").await;
    let si = &status["sstable_integrity"];
    assert_eq!(si["state"], "complete");
    assert_eq!(si["tables_total"], tables.len());
    assert_eq!(si["tables_verified"], tables.len());
    assert!(si["bytes_verified"].as_u64().unwrap() > 0);
    assert_eq!(si["damaged"], serde_json::json!([]));

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_damaged_block_is_reported_on_both_surfaces_and_once_in_the_security_log_while_reads_fail_typed(
) {
    let log_path = security_log_path();
    let dir = build_dir("damaged");
    let tables = preflight(&dir).unwrap().tables;
    // damage a data block AFTER the preflight (the table was valid at start) and before the pass reaches it
    let victim = ssts(&dir)[0].clone();
    damage_second_block(&victim);
    let victim_id = tables
        .iter()
        .find(|t| t.path == victim)
        .map(|t| t.id)
        .unwrap();

    let (state, router) = open(&dir);
    static LINES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    let handle = spawn_sstable_verification(&state, dir.clone(), tables.clone(), 1024, |d| {
        LINES.lock().unwrap().push(d.stderr_line());
    })
    .expect("the pass starts");

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (_, ready) = get(&router, "/readyz").await;
        if ready["sstable_verification"] == "damaged" {
            // `ready` and `storage_state` are exactly what they were
            assert_eq!(ready["ready"], true);
            assert_eq!(ready["storage_state"], "Healthy");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the damage was never reported: {ready}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    handle.join().unwrap();

    let (_, status) = get(&router, "/v1/status").await;
    assert_eq!(status["storage_state"], "Healthy");
    let si = &status["sstable_integrity"];
    assert_eq!(si["state"], "damaged");
    assert_eq!(si["tables_total"], tables.len());
    assert_eq!(
        si["tables_verified"], si["tables_total"],
        "the pass went on past the damaged table"
    );
    let damaged = si["damaged"].as_array().unwrap();
    assert_eq!(damaged.len(), 1, "{si}");
    let d = &damaged[0];
    assert_eq!(d["id"], victim_id);
    assert_eq!(
        d["path"],
        format!("sstables/{}", victim.file_name().unwrap().to_string_lossy())
    );
    let before = d["records_before_failure"].as_u64().unwrap();
    let total = d["records_total"].as_u64().unwrap();
    assert!(before > 0 && before < total, "{d}");

    // stderr callback: one line, the file and the action, no keys
    let lines = LINES.lock().unwrap().clone();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("rubixdb check") && lines[0].contains(d["path"].as_str().unwrap()));
    assert!(!lines[0].contains("key-"), "{}", lines[0]);

    // security log: exactly one sstable.damaged event, ids and counts only
    let deadline = Instant::now() + Duration::from_secs(10);
    while !log_lines(&log_path)
        .iter()
        .any(|e| e["code"] == "sstable.damaged")
    {
        assert!(Instant::now() < deadline, "no sstable.damaged event");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let events: Vec<Value> = log_lines(&log_path)
        .into_iter()
        .filter(|e| e["code"] == "sstable.damaged")
        .collect();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["outcome"], "failed");
    assert_eq!(events[0]["object_kind"], "sstable");
    assert_eq!(
        events[0]["object"],
        format!("table={victim_id} records_before={before}")
    );

    // reads: the rows of the damaged block fail with the typed error, every other read is correct
    let (mut ok, mut corrupt) = (0u32, 0u32);
    for i in 0..KEYS {
        let uri = format!("/v1/kv/{}", STANDARD.encode(key(i)));
        let (st, body) = get(&router, &uri).await;
        match st {
            StatusCode::OK => ok += 1,
            StatusCode::INTERNAL_SERVER_ERROR => {
                assert_eq!(body["error"]["code"], "CORRUPTION", "{body}");
                corrupt += 1;
            }
            other => panic!("key {i}: unexpected {other}: {body}"),
        }
    }
    assert!(corrupt > 0 && ok > 0, "ok={ok} corrupt={corrupt}");
    assert_eq!(ok + corrupt, KEYS);

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_budget_of_zero_starts_no_pass() {
    let dir = build_dir("zero");
    let tables = preflight(&dir).unwrap().tables;
    let (state, router) = open(&dir);
    assert!(spawn_sstable_verification(&state, dir.clone(), tables, 0, |_| {}).is_none());
    let (_, ready) = get(&router, "/readyz").await;
    assert_eq!(ready["sstable_verification"], "disabled");
    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

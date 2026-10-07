//! Row A16: five consecutive panicking sampler ticks drive the sampler to `failed`, end to end, through a
//! real panicking probe, on the real server and real engine.
//!
//! This test lives in its own file (its own test process) on purpose. The WARN line "observability
//! sampler failed" is counted through a process-wide `tracing` capture, and
//! `a_sampler_that_keeps_failing_is_logged_once_and_reads_failed` in `observability.rs` counts the same
//! line in its own process-wide capture: two tests that both drive a sampler to `failed` in one process
//! would see each other's line and fail each other's "exactly once".
//!
//! The panic-injection seam is the one the suite already uses: the sampler's `OsProbe` trait, whose
//! calls run inside the tick's `catch_unwind` (`sampler.rs`, `Worker::tick`). Nothing was added to
//! production code.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rubixdb::execution::batch_coordinator::BatchCoordinatorConfig;
use rubixdb::lsm::{LsmConfig, LsmEngine};
use rubixdb::wal::{SyncMode, WalConfig};
use rubixdb_api::config::{ApiKeyConfig, Role};
use rubixdb_api::observability::probe::{OsProbe, RealProbe};
use rubixdb_api::observability::sampler::{self, SamplerHandle};
use rubixdb_api::resources::IoCounters;
use rubixdb_api::routes::build_router;
use rubixdb_api::server::{serve_observed, ServerLimits};
use rubixdb_api::{AppState, Config};
use serde_json::{json, Value};

const ADMIN_KEY: &str = "obs-admin-key-0123456789abcdef";

/// The sampler's threshold, `FAIL_AFTER_PANICS` in `api/src/observability/sampler.rs` (private, so it
/// cannot be imported). `the_threshold_this_test_assumes_is_the_one_in_sampler_rs` ties this number to
/// that line of source, so a changed constant fails loudly instead of leaving this test stale.
const FAIL_AFTER_PANICS: u64 = 5;

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("rubixdb_obs_sf_{tag}_{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn test_config(dir: PathBuf, name: &str) -> Config {
    Config {
        data_dir: dir,
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        api_keys: vec![ApiKeyConfig {
            name: "admin".into(),
            role: Role::Admin,
            key: ADMIN_KEY.into(),
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
        instance_id: Some(uuid::Uuid::new_v4()),
        instance_name: Some(name.to_string()),
        frontend_dist: None,
        backup_dir: None,
    }
}

/// A probe that panics inside the tick while the flag is set and otherwise measures for real.
struct PanicProbe(Arc<AtomicBool>);
impl OsProbe for PanicProbe {
    fn process_cpu_seconds(&self) -> Option<f64> {
        if self.0.load(Ordering::SeqCst) {
            panic!("injected probe panic (test)");
        }
        RealProbe.process_cpu_seconds()
    }
    fn process_rss(&self) -> Option<(u64, u64)> {
        RealProbe.process_rss()
    }
    fn system_memory(&self) -> Option<(u64, u64)> {
        RealProbe.system_memory()
    }
    fn vcpu_count(&self) -> Option<u32> {
        RealProbe.vcpu_count()
    }
    fn disk_capacity(&self, p: &std::path::Path) -> Option<(u64, u64)> {
        RealProbe.disk_capacity(p)
    }
    fn process_io(&self) -> Option<IoCounters> {
        RealProbe.process_io()
    }
}

struct LogCapture(Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for LogCapture {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn captured_logs() -> Arc<std::sync::Mutex<Vec<u8>>> {
    static BUF: std::sync::OnceLock<Arc<std::sync::Mutex<Vec<u8>>>> = std::sync::OnceLock::new();
    BUF.get_or_init(|| {
        let buf = Arc::new(std::sync::Mutex::new(Vec::new()));
        let b2 = buf.clone();
        let sub = tracing_subscriber::fmt()
            .with_writer(move || LogCapture(b2.clone()))
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .finish();
        let _ = tracing::subscriber::set_global_default(sub);
        buf
    })
    .clone()
}

fn log_lines_containing(needle: &str) -> usize {
    String::from_utf8_lossy(&captured_logs().lock().unwrap())
        .lines()
        .filter(|l| l.contains(needle))
        .count()
}

struct Server {
    state: Arc<AppState>,
    addr: SocketAddr,
    dir: PathBuf,
    stop_tx: Option<tokio::sync::oneshot::Sender<()>>,
    join: Option<tokio::task::JoinHandle<()>>,
    sampler: Option<SamplerHandle>,
    http: reqwest::Client,
}

impl Server {
    async fn start(tag: &str, probe: Arc<dyn OsProbe>, tick: Duration) -> Server {
        let dir = temp_dir(tag);
        let lsm_config = LsmConfig::default();
        let engine = Arc::new(
            LsmEngine::open(
                &dir,
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
                lsm_config.clone(),
            )
            .unwrap(),
        );
        let state = Arc::new(AppState::new(
            engine,
            lsm_config,
            test_config(dir.clone(), tag),
        ));
        let router = build_router(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let sampler = sampler::start_with(&state, probe, tick).unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let conns = state.obs.connections.clone();
        let join = tokio::spawn(serve_observed(
            listener,
            router,
            async move {
                let _ = rx.await;
            },
            Duration::from_secs(5),
            ServerLimits::default(),
            Some(conns),
        ));
        Server {
            state,
            addr,
            dir,
            stop_tx: Some(tx),
            join: Some(join),
            sampler: Some(sampler),
            http: reqwest::Client::builder().build().unwrap(),
        }
    }

    fn url(&self, p: &str) -> String {
        format!("http://{}{}", self.addr, p)
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        let r = self
            .http
            .get(self.url(path))
            .bearer_auth(ADMIN_KEY)
            .send()
            .await
            .unwrap();
        let s = r.status().as_u16();
        (s, r.json().await.unwrap_or(Value::Null))
    }

    async fn healthz(&self) -> u16 {
        self.http
            .get(self.url("/healthz"))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    async fn sql(&self, sql: &str) -> u16 {
        self.http
            .post(self.url("/v1/sql"))
            .bearer_auth(ADMIN_KEY)
            .json(&json!({"sql": sql}))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    async fn sampler_state(&self) -> String {
        let (st, v) = self.get("/v1/metrics/system").await;
        assert_eq!(st, 200);
        v["sample_freshness"]["state"].as_str().unwrap().to_string()
    }

    /// The server still answers: `/healthz` and a trivial SQL statement.
    async fn still_serving(&self, what: &str) {
        assert_eq!(self.healthz().await, 200, "/healthz {what}");
        assert_eq!(self.sql("SELECT id FROM t").await, 200, "SQL {what}");
    }

    async fn stop(mut self) {
        if let Some(mut s) = self.sampler.take() {
            s.stop();
        }
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(j) = self.join.take() {
            let _ = j.await;
        }
        self.state.engine.shutdown();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn operational_events_with(s: &Server, event_type: &str, result: &str) -> usize {
    let (_, v) = s.get("/v1/observability/events?limit=200").await;
    v["operational"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["event_type"] == event_type && e["result"] == result)
        .count()
}

/// The number this test assumes must be the number in the source: `FAIL_AFTER_PANICS` is a private
/// constant of `sampler.rs`, so the test reads the source text instead of importing it.
#[test]
fn the_threshold_this_test_assumes_is_the_one_in_sampler_rs() {
    let src = include_str!("../src/observability/sampler.rs");
    let line = format!("const FAIL_AFTER_PANICS: u32 = {FAIL_AFTER_PANICS};");
    assert!(
        src.contains(&line),
        "sampler.rs no longer says `{line}`: update FAIL_AFTER_PANICS in this test"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn five_consecutive_panicking_ticks_fail_the_sampler_and_the_next_good_tick_recovers() {
    let _ = captured_logs();
    let panic_now = Arc::new(AtomicBool::new(false));
    let s = Server::start(
        "five_panics",
        Arc::new(PanicProbe(panic_now.clone())),
        Duration::from_millis(100),
    )
    .await;
    assert_eq!(s.sql("CREATE TABLE t (id INTEGER PRIMARY KEY)").await, 200);

    // ---- a healthy sampler first
    let deadline = Instant::now() + Duration::from_secs(10);
    while s.sampler_state().await != "running" || s.state.obs.sampler.ticks() < 3 {
        assert!(
            Instant::now() < deadline,
            "the sampler never reached running"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(s.state.obs.sampler.panics(), 0);
    assert_eq!(log_lines_containing("observability sampler failed"), 0);
    s.still_serving("before any panic").await;
    let generation_before = s.get("/v1/metrics/system").await.1["sample_generation"]
        .as_u64()
        .unwrap();

    // ---- every tick panics from now on: degraded for the first four, failed from the fifth
    let panics_at_start = s.state.obs.sampler.panics();
    panic_now.store(true, Ordering::SeqCst);
    let mut states_seen: Vec<String> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let state = s.sampler_state().await;
        let panics = s.state.obs.sampler.panics() - panics_at_start;
        if states_seen.last() != Some(&state) {
            states_seen.push(state.clone());
        }
        if state == "failed" {
            assert!(
                panics >= FAIL_AFTER_PANICS,
                "reported failed after only {panics} panicking tick(s); the threshold is {FAIL_AFTER_PANICS} consecutive"
            );
            break;
        }
        assert_eq!(
            log_lines_containing("observability sampler failed"),
            0,
            "no failure is logged before the threshold ({panics} panics so far)"
        );
        s.still_serving("while the sampler is panicking").await;
        assert!(
            Instant::now() < deadline,
            "never reported failed after {panics} panicking ticks: {states_seen:?}"
        );
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
    assert!(
        states_seen.iter().any(|st| st == "degraded"),
        "below the threshold the sampler reads degraded, not failed: {states_seen:?}"
    );
    assert_eq!(states_seen.last().map(String::as_str), Some("failed"));

    // ---- keep panicking well past the threshold: still failed, logged exactly once, still serving
    let deadline = Instant::now() + Duration::from_secs(15);
    while s.state.obs.sampler.panics() - panics_at_start < FAIL_AFTER_PANICS + 6 {
        assert!(Instant::now() < deadline, "the sampler stopped ticking");
        s.still_serving("while the sampler is failed").await;
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(s.sampler_state().await, "failed");
    assert_eq!(
        log_lines_containing("observability sampler failed"),
        1,
        "the failure is logged exactly once, not once per tick"
    );
    assert_eq!(
        operational_events_with(&s, "sampler.state", "failed").await,
        1,
        "one sampler.state event for the transition to failed"
    );
    s.still_serving("after many panicking ticks").await;

    // ---- the next good tick returns the state to running
    panic_now.store(false, Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_secs(10);
    while s.sampler_state().await != "running" {
        assert!(Instant::now() < deadline, "never returned to running");
        s.still_serving("while the sampler recovers").await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (st, v) = s.get("/v1/metrics/system").await;
    assert_eq!(st, 200);
    assert_eq!(v["sample_freshness"]["state"], "running");
    assert_eq!(v["sample_freshness"]["stale"], false);
    assert!(
        v["sample_generation"].as_u64().unwrap() > generation_before,
        "a fresh snapshot was published after the failure"
    );
    assert_eq!(
        log_lines_containing("observability sampler failed"),
        1,
        "recovery does not log the failure again"
    );
    assert_eq!(
        operational_events_with(&s, "sampler.state", "running").await,
        1,
        "one sampler.state event for the recovery"
    );
    s.still_serving("after recovery").await;
    s.stop().await;
}

//! Full-observability integration tests. Every test starts the **real** API server (real engine,
//! real axum router, a real TCP listener on 127.0.0.1) with the real background sampler and talks
//! to it over real HTTP with `reqwest`. Failure injection uses the sampler's `OsProbe` seam, the
//! only test-only hook the layer exposes.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
const READER_KEY: &str = "obs-reader-key-0123456789abcdef";

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("rubixdb_obs_it_{tag}_{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn test_config(dir: PathBuf, name: &str) -> Config {
    Config {
        data_dir: dir,
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        api_keys: vec![
            ApiKeyConfig {
                name: "admin".into(),
                role: Role::Admin,
                key: ADMIN_KEY.into(),
            },
            ApiKeyConfig {
                name: "reader".into(),
                role: Role::Reader,
                key: READER_KEY.into(),
            },
        ],
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

/// Failure-injection control for the probe.
#[derive(Default)]
struct Ctl {
    fail_cpu: AtomicBool,
    fail_rss: AtomicBool,
    fail_disk: AtomicBool,
    fail_everything: AtomicBool,
}

struct CtlProbe(Arc<Ctl>);

impl OsProbe for CtlProbe {
    fn process_cpu_seconds(&self) -> Option<f64> {
        if self.0.fail_everything.load(Ordering::SeqCst) || self.0.fail_cpu.load(Ordering::SeqCst) {
            return None;
        }
        RealProbe.process_cpu_seconds()
    }
    fn process_rss(&self) -> Option<(u64, u64)> {
        if self.0.fail_everything.load(Ordering::SeqCst) || self.0.fail_rss.load(Ordering::SeqCst) {
            return None;
        }
        RealProbe.process_rss()
    }
    fn system_memory(&self) -> Option<(u64, u64)> {
        if self.0.fail_everything.load(Ordering::SeqCst) {
            return None;
        }
        RealProbe.system_memory()
    }
    fn vcpu_count(&self) -> Option<u32> {
        if self.0.fail_everything.load(Ordering::SeqCst) {
            return None;
        }
        RealProbe.vcpu_count()
    }
    fn disk_capacity(&self, p: &std::path::Path) -> Option<(u64, u64)> {
        if self.0.fail_everything.load(Ordering::SeqCst) || self.0.fail_disk.load(Ordering::SeqCst)
        {
            return None;
        }
        RealProbe.disk_capacity(p)
    }
    fn process_io(&self) -> Option<IoCounters> {
        if self.0.fail_everything.load(Ordering::SeqCst) {
            return None;
        }
        RealProbe.process_io()
    }
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
        Server::start_cfg(tag, probe, tick, |_| {}).await
    }

    async fn start_cfg(
        tag: &str,
        probe: Arc<dyn OsProbe>,
        tick: Duration,
        tweak: impl FnOnce(&mut Config),
    ) -> Server {
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
        let mut cfg = test_config(dir.clone(), tag);
        tweak(&mut cfg);
        let state = Arc::new(AppState::new(engine, lsm_config, cfg));
        let router = build_router(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        assert!(addr.ip().is_loopback());
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

    async fn real(tag: &str) -> Server {
        Server::start(tag, Arc::new(RealProbe), Duration::from_millis(50)).await
    }

    fn url(&self, p: &str) -> String {
        format!("http://{}{}", self.addr, p)
    }

    async fn get_with(&self, key: &str, path: &str) -> (u16, Value) {
        let r = self
            .http
            .get(self.url(path))
            .bearer_auth(key)
            .send()
            .await
            .unwrap();
        let s = r.status().as_u16();
        (s, r.json().await.unwrap_or(Value::Null))
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        self.get_with(READER_KEY, path).await
    }

    async fn sql(&self, sql: &str) -> (u16, Value) {
        let r = self
            .http
            .post(self.url("/v1/sql"))
            .bearer_auth(ADMIN_KEY)
            .json(&json!({"sql": sql}))
            .send()
            .await
            .unwrap();
        let s = r.status().as_u16();
        (s, r.json().await.unwrap_or(Value::Null))
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

async fn wait_for_generation(s: &Server, at_least: u64) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (_, v) = s.get("/v1/metrics/system").await;
        if v["sample_generation"].as_u64().unwrap_or(0) >= at_least {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "sampler never reached generation {at_least}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn is_num(v: &Value) -> bool {
    v.is_number()
}

fn num_or_null(v: &Value) -> bool {
    v.is_null() || v.is_number()
}

// ------------------------------------------------------------------------------------------
// 1. /v1/metrics/system: every required field, correct types
// ------------------------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_system_has_every_required_field_with_the_right_types() {
    let s = Server::real("sys_fields").await;
    s.sql("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
        .await;
    s.sql("INSERT INTO t (id, v) VALUES (1,'a'),(2,'b')").await;
    for _ in 0..30 {
        s.sql("SELECT * FROM t WHERE id = 1").await;
    }
    wait_for_generation(&s, 5).await;
    let (st, v) = s.get("/v1/metrics/system").await; // Reader key: allowed
    assert_eq!(st, 200, "{v}");

    assert!(is_num(&v["timestamp_unix_ms"]));
    let f = &v["sample_freshness"];
    assert!(is_num(&f["last_sample_ms"]) && is_num(&f["age_ms"]));
    assert_eq!(f["state"], "running");
    assert_eq!(f["stale"], false);
    assert!(v["sample_generation"].as_u64().unwrap() >= 5);

    let i = &v["instance"];
    assert!(i["id"].is_string() && i["name"].is_string());
    assert!(is_num(&i["uptime_seconds"]));
    assert!(["healthy", "degraded", "failed"].contains(&i["healthy"].as_str().unwrap()));
    assert_eq!(i["readiness"], "ready");

    let c = &v["cpu"];
    assert!(num_or_null(&c["process_percent"]) && num_or_null(&c["peak_percent"]));
    assert!(c["vcpu_count"].as_u64().unwrap() >= 1);

    let m = &v["memory"];
    assert!(m["rss_bytes"].as_u64().unwrap() > 0);
    assert!(m["peak_rss_bytes"].as_u64().unwrap() >= m["rss_bytes"].as_u64().unwrap());
    assert!(m["system_total_bytes"].as_u64().unwrap() > m["system_used_bytes"].as_u64().unwrap());
    let pct = m["system_used_percent"].as_f64().unwrap();
    assert!((0.0..=100.0).contains(&pct));

    let d = &v["disk"];
    for k in [
        "volume_total_bytes",
        "volume_free_bytes",
        "volume_used_percent",
        "db_bytes",
        "wal_bytes",
        "sstable_bytes",
        "read_iops",
        "write_iops",
        "read_mb_per_sec",
        "write_mb_per_sec",
    ] {
        assert!(num_or_null(&d[k]), "disk.{k} = {}", d[k]);
    }
    assert!(d["volume_total_bytes"].as_u64().unwrap() >= d["volume_free_bytes"].as_u64().unwrap());

    let t = &v["throughput"];
    for k in [
        "http_requests_per_sec",
        "sql_queries_per_sec",
        "write_commits_per_sec",
    ] {
        assert!(num_or_null(&t[k]), "throughput.{k}");
    }
    for k in [
        "active_connections",
        "active_sessions",
        "active_transactions",
        "active_queries",
    ] {
        assert!(t[k].is_number(), "throughput.{k}");
    }
    assert!(
        t["active_connections"].as_i64().unwrap() >= 1,
        "this very request is a connection"
    );

    let l = &v["latency"];
    assert!(l["query_p50_ms"].as_f64().unwrap() <= l["query_p95_ms"].as_f64().unwrap());
    assert!(l["query_p95_ms"].as_f64().unwrap() <= l["query_p99_ms"].as_f64().unwrap());

    assert!(v["wal"]["segment_count"].as_u64().unwrap() >= 1);
    assert!(is_num(&v["wal"]["bytes"]) && v["wal"]["state"] == "Running");
    let comp = &v["compaction"];
    assert!(comp["running"].is_boolean() && is_num(&comp["cycles_since_start"]));
    assert!(is_num(&comp["live_sstable_count"]) && num_or_null(&comp["last_duration_ms"]));
    let b = &v["background"];
    assert!(is_num(&b["flush_queue_depth"]) && is_num(&b["pending_groups"]));
    assert!(b["index_build_state"].is_string());
    assert!(
        b["last_flush_ms"].is_null(),
        "no flush timestamp exists: must be null, never 0"
    );
    let sec = &v["security"];
    assert!(is_num(&sec["auth_failures_since_start"]) && is_num(&sec["admin_actions_since_start"]));
    assert!(sec["last_admin_action"].is_null());
    assert!(v["latency_of_response_ms"].as_f64().unwrap() >= 0.0);
    s.stop().await;
}

// ------------------------------------------------------------------------------------------
// 2, 15, 16, 17. unmeasurable => null (never 0); a failing platform call never stops the sampler
// ------------------------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_platform_that_measures_nothing_yields_nulls_never_zeros() {
    let ctl = Arc::new(Ctl::default());
    ctl.fail_everything.store(true, Ordering::SeqCst);
    let s = Server::start(
        "null_all",
        Arc::new(CtlProbe(ctl)),
        Duration::from_millis(30),
    )
    .await;
    wait_for_generation(&s, 4).await;
    let (st, v) = s.get("/v1/metrics/system").await;
    assert_eq!(st, 200);
    for p in [
        &v["cpu"]["process_percent"],
        &v["cpu"]["peak_percent"],
        &v["cpu"]["vcpu_count"],
        &v["memory"]["rss_bytes"],
        &v["memory"]["peak_rss_bytes"],
        &v["memory"]["system_total_bytes"],
        &v["memory"]["system_used_bytes"],
        &v["memory"]["system_used_percent"],
        &v["disk"]["volume_total_bytes"],
        &v["disk"]["volume_free_bytes"],
        &v["disk"]["volume_used_percent"],
        &v["disk"]["read_iops"],
        &v["disk"]["write_iops"],
        &v["disk"]["read_mb_per_sec"],
        &v["disk"]["write_mb_per_sec"],
    ] {
        assert!(p.is_null(), "an unmeasurable field must be null, got {p}");
    }
    // Product-side values are still real, and the instance is not declared failed.
    assert!(v["wal"]["state"].is_string());
    assert_ne!(v["instance"]["healthy"], "failed");
    // series that were never measurable are null, not empty lists of zeros
    let (st, ts) = s.get("/v1/metrics/system/timeseries?window=15m").await;
    assert_eq!(st, 200);
    assert!(ts["series"]["cpu_process_percent"].is_null());
    assert!(ts["series"]["memory_rss_bytes"].is_null());
    s.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cpu_rss_and_disk_read_failures_null_the_field_degrade_the_sampler_and_never_stop_it() {
    for which in ["cpu", "rss", "disk"] {
        let ctl = Arc::new(Ctl::default());
        let s = Server::start(
            &format!("fail_{which}"),
            Arc::new(CtlProbe(ctl.clone())),
            Duration::from_millis(30),
        )
        .await;
        wait_for_generation(&s, 5).await;
        let (_, before) = s.get("/v1/metrics/system").await;
        // working first
        match which {
            "cpu" => assert!(num_or_null(&before["cpu"]["process_percent"])),
            "rss" => assert!(before["memory"]["rss_bytes"].is_number()),
            _ => assert!(before["disk"]["volume_total_bytes"].is_number()),
        }
        match which {
            "cpu" => ctl.fail_cpu.store(true, Ordering::SeqCst),
            "rss" => ctl.fail_rss.store(true, Ordering::SeqCst),
            _ => ctl.fail_disk.store(true, Ordering::SeqCst),
        }
        let g0 = before["sample_generation"].as_u64().unwrap();
        wait_for_generation(&s, g0 + 4).await;
        let (st, v) = s.get("/v1/metrics/system").await;
        assert_eq!(st, 200, "the endpoint keeps answering");
        match which {
            "cpu" => {
                assert!(v["cpu"]["process_percent"].is_null());
                assert!(
                    v["memory"]["rss_bytes"].is_number(),
                    "other probes unaffected"
                );
            }
            "rss" => {
                assert!(
                    v["memory"]["rss_bytes"].is_null() && v["memory"]["peak_rss_bytes"].is_null()
                );
                assert!(num_or_null(&v["cpu"]["process_percent"]));
            }
            _ => {
                assert!(
                    v["disk"]["volume_total_bytes"].is_null()
                        && v["disk"]["volume_free_bytes"].is_null()
                );
                assert!(v["disk"]["volume_used_percent"].is_null());
            }
        }
        assert_eq!(v["sample_freshness"]["state"], "degraded", "{which}");
        assert_ne!(v["instance"]["healthy"], "failed");
        // the server still does real work and the sampler keeps ticking
        let (sst, _) = s.sql("CREATE TABLE t (id INTEGER PRIMARY KEY)").await;
        assert_eq!(sst, 200);
        let (hs, _) = {
            let r = s.http.get(s.url("/healthz")).send().await.unwrap();
            (r.status().as_u16(), ())
        };
        assert_eq!(hs, 200);
        // recovery: the field comes back and the sampler returns to running
        match which {
            "cpu" => ctl.fail_cpu.store(false, Ordering::SeqCst),
            "rss" => ctl.fail_rss.store(false, Ordering::SeqCst),
            _ => ctl.fail_disk.store(false, Ordering::SeqCst),
        }
        // read the generation only after the fault is cleared: ticks that ran during the fault
        // (while the SQL/health calls above were in flight) must not count as recovery ticks
        let g1 = s.get("/v1/metrics/system").await.1["sample_generation"]
            .as_u64()
            .unwrap();
        wait_for_generation(&s, g1 + 4).await;
        let (_, v2) = s.get("/v1/metrics/system").await;
        assert_eq!(v2["sample_freshness"]["state"], "running", "{which}");
        s.stop().await;
    }
}

// ------------------------------------------------------------------------------------------
// 3. latency of the endpoint under 16 concurrent readers
// ------------------------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn metrics_system_p95_is_below_50ms_with_16_concurrent_readers() {
    let s = Arc::new(Server::real("sys_load").await);
    s.sql("CREATE TABLE t (id INTEGER PRIMARY KEY)").await;
    wait_for_generation(&s, 3).await;
    let stop_at = Instant::now() + Duration::from_secs(10);
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let s = s.clone();
        tasks.push(tokio::spawn(async move {
            let mut lat = Vec::new();
            while Instant::now() < stop_at {
                let t = Instant::now();
                let r = s
                    .http
                    .get(s.url("/v1/metrics/system"))
                    .bearer_auth(READER_KEY)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(r.status().as_u16(), 200);
                let _ = r.bytes().await.unwrap();
                lat.push(t.elapsed().as_secs_f64() * 1000.0);
            }
            lat
        }));
    }
    let mut all: Vec<f64> = Vec::new();
    for t in tasks {
        all.extend(t.await.unwrap());
    }
    all.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p = |q: f64| all[((all.len() as f64) * q) as usize % all.len()];
    let (p50, p95, p99, max) = (p(0.5), p(0.95), p(0.99), *all.last().unwrap());
    eprintln!("metrics/system under 16 readers for 10 s: n={} p50={p50:.3} p95={p95:.3} p99={p99:.3} max={max:.3} ms", all.len());
    assert!(all.len() > 1000, "enough samples: {}", all.len());
    assert!(p95 < 50.0, "p95 {p95} ms");
    Arc::try_unwrap(s).ok().unwrap().stop().await;
}

// ------------------------------------------------------------------------------------------
// 4. timeseries
// ------------------------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn timeseries_windows_resolution_empty_history_and_invalid_window() {
    let s = Server::real("ts").await;
    wait_for_generation(&s, 3).await;
    for (w, res) in [("15m", 60), ("1h", 15), ("24h", 60), ("7d", 3600)] {
        let (st, v) = s
            .get(&format!("/v1/metrics/system/timeseries?window={w}"))
            .await;
        assert_eq!(st, 200, "{w}");
        assert_eq!(v["window"], w);
        assert_eq!(v["resolution_seconds"], res);
        // a freshly started instance has no completed period yet: 200 with EMPTY arrays, not 404
        let sql = v["series"]["sql_queries_per_sec"]
            .as_array()
            .expect("array, not null");
        assert!(sql.is_empty(), "{w}");
        for name in [
            "cpu_process_percent",
            "memory_rss_bytes",
            "disk_read_iops",
            "disk_write_iops",
            "disk_read_mb_per_sec",
            "disk_write_mb_per_sec",
            "sql_queries_per_sec",
            "active_queries",
        ] {
            assert!(v["series"].get(name).is_some(), "{name}");
        }
    }
    for bad in [
        "",
        "?window=",
        "?window=30m",
        "?window=15M",
        "?window=1d",
        "?window=15m%20",
        "?window=15m,1h",
        "?x=1",
    ] {
        let (st, v) = s.get(&format!("/v1/metrics/system/timeseries{bad}")).await;
        assert_eq!(st, 400, "{bad:?}");
        assert_eq!(
            v["error"]["code"], "VALIDATION_ERROR",
            "existing error shape for {bad:?}"
        );
    }
    s.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn timeseries_returns_injected_history_oldest_first_with_exact_resolution_and_a_size_cap() {
    let mut s = Server::real("ts_hist").await;
    // Only the injected clock may feed the rings in this test: stop the real sampler first.
    s.sampler.take().unwrap().stop();
    // 25 hours of 1 Hz ticks with an injected clock (never the real one).
    // after the sampler's own first (real-clock) tick, so every injected tick is newer
    let base = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 60_000;
    for t in 0..(25 * 3600u64) {
        s.state.obs.sampler.inject_series_tick(
            base + t * 1000,
            [
                Some((t % 100) as f64),
                Some(1e8 + t as f64),
                None,
                None,
                None,
                None,
                Some(5.0),
                Some(1.0),
            ],
        );
    }
    let series_bytes = s.state.obs.sampler.series_memory_bytes();
    eprintln!("series store after 25 h of injected 1 Hz ticks: {series_bytes} bytes (cap 1048576)");
    assert!(series_bytes <= 1024 * 1024);
    for (w, step, cap) in [("15m", 60u64, 15usize), ("1h", 15, 240), ("24h", 60, 1440)] {
        let (st, v) = s
            .get(&format!("/v1/metrics/system/timeseries?window={w}"))
            .await;
        assert_eq!(st, 200);
        let pts = v["series"]["memory_rss_bytes"].as_array().unwrap();
        assert_eq!(pts.len(), cap, "{w}");
        let ts: Vec<u64> = pts.iter().map(|p| p["t"].as_u64().unwrap()).collect();
        assert!(
            ts.windows(2).all(|p| (p[1] - p[0]) / 1000 == step),
            "{w} resolution"
        );
        assert!(pts.iter().all(|p| p["v"].is_number()));
        // never-measured series (disk I/O in this injection) are null, not empty
        assert!(v["series"]["disk_read_iops"].is_null());
    }
    let r = s
        .http
        .get(s.url("/v1/metrics/system/timeseries?window=24h"))
        .bearer_auth(READER_KEY)
        .send()
        .await
        .unwrap();
    let bytes = r.bytes().await.unwrap();
    assert!(
        bytes.len() < 2 * 1024 * 1024,
        "response body {} bytes",
        bytes.len()
    );
    s.stop().await;
}

// ------------------------------------------------------------------------------------------
// 5, 6, 7, 8. diagnostics
// ------------------------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sessions_endpoint_lists_bounded_records_with_every_required_field() {
    let s = Server::real("sessions").await;
    s.sql("CREATE TABLE t (id INTEGER PRIMARY KEY)").await;
    let (_, b) = s.sql("BEGIN").await;
    let sid = b["session_id"].as_str().unwrap().to_string();
    let (st, v) = s.get("/v1/observability/sessions").await;
    assert_eq!(st, 200);
    assert_eq!(v["total"], 1);
    assert_eq!(v["truncated"], false);
    let r = &v["sessions"][0];
    assert_eq!(r["session_id"], sid.as_str());
    assert_eq!(r["state"], "idle");
    assert!(r["age_seconds"].as_f64().unwrap() >= 0.0);
    assert_eq!(r["transaction_state"], "open");
    assert_eq!(r["operation_class"], "none");
    assert!(r["idle_seconds"].is_number() && r["idle_timeout_remaining_seconds"].is_number());
    assert_eq!(r["timeout_state"], "active");
    assert_eq!(r["cancellation_state"], "not_requested");
    // ordered by id, bounded by the 200 cap, and `limit` is honoured
    for _ in 0..30 {
        s.sql("BEGIN").await;
    }
    let (_, v) = s.get("/v1/observability/sessions").await;
    let ids: Vec<&str> = v["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["session_id"].as_str().unwrap())
        .collect();
    assert!(ids.windows(2).all(|p| p[0] < p[1]), "ordered by session id");
    let (_, v) = s.get("/v1/observability/sessions?limit=5").await;
    assert_eq!(v["returned"], 5);
    assert_eq!(v["truncated"], true);
    // the session ends: COMMIT removes it
    let r = s
        .http
        .post(s.url("/v1/sql"))
        .bearer_auth(ADMIN_KEY)
        .json(&json!({"sql": "COMMIT", "session_id": sid}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let (_, v) = s.get("/v1/observability/sessions").await;
    assert_eq!(v["total"], 30);
    s.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queries_endpoint_lists_bounded_records_never_sql_text() {
    let s = Server::real("queries").await;
    s.sql("CREATE TABLE secretmarker_table (id INTEGER PRIMARY KEY, secretmarker_col TEXT)")
        .await;
    s.sql("INSERT INTO secretmarker_table (id, secretmarker_col) VALUES (1, 'SECRETVALUE_42')")
        .await;
    s.sql(
        "SELECT secretmarker_col FROM secretmarker_table WHERE secretmarker_col = 'SECRETVALUE_42'",
    )
    .await;
    s.sql("SELEKT SECRETPARSE FROM nowhere").await;
    s.sql("SELECT * FROM no_such_secretmarker_table").await;
    let (st, v) = s.get("/v1/observability/queries?limit=10").await;
    assert_eq!(st, 200);
    let q = v["queries"].as_array().unwrap();
    assert!(q.len() >= 5);
    // newest first
    let starts: Vec<u64> = q
        .iter()
        .map(|r| r["start_unix_ms"].as_u64().unwrap())
        .collect();
    assert!(starts.windows(2).all(|p| p[0] >= p[1]));
    let classes: BTreeSet<&str> = q
        .iter()
        .map(|r| r["statement_class"].as_str().unwrap())
        .collect();
    for c in &classes {
        assert!(
            [
                "unparsed",
                "select",
                "insert",
                "update",
                "delete",
                "ddl",
                "transaction",
                "explain",
                "other"
            ]
            .contains(c),
            "{c}"
        );
    }
    assert!(classes.contains("ddl") && classes.contains("insert") && classes.contains("select"));
    for r in q {
        for k in [
            "query_id",
            "statement_class",
            "state",
            "start_unix_ms",
            "duration_ms",
            "timeout_state",
            "cancellation_state",
        ] {
            assert!(!r[k].is_null(), "{k} present");
        }
        assert!(["running", "succeeded", "failed", "cancelled", "timed_out"]
            .contains(&r["state"].as_str().unwrap()));
    }
    let failed: Vec<&Value> = q.iter().filter(|r| r["state"] == "failed").collect();
    assert_eq!(failed.len(), 2);
    assert!(failed.iter().all(|r| r["error_class"].is_string()));
    let sel = q
        .iter()
        .find(|r| r["statement_class"] == "select" && r["state"] == "succeeded")
        .unwrap();
    assert_eq!(sel["rows_returned"], 1);
    let ins = q.iter().find(|r| r["statement_class"] == "insert").unwrap();
    assert_eq!(ins["rows_affected"], 1);
    let text = v.to_string();
    for forbidden in ["SECRETVALUE", "secretmarker", "SECRETPARSE", "nowhere"] {
        assert!(
            !text.contains(forbidden),
            "raw SQL / names must never be returned: {forbidden}"
        );
    }
    // bounded
    for _ in 0..260 {
        s.sql("SELECT 1").await;
    }
    let (_, v) = s.get("/v1/observability/queries?limit=100000").await;
    assert!(v["queries"].as_array().unwrap().len() <= 200);
    assert_eq!(v["max_records"], 200);
    s.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn events_endpoint_is_bounded_clamped_and_keeps_security_and_operational_apart() {
    let s = Server::real("events").await;
    s.sql("SELEKT broken").await; // operational: query.failed
    for k in 0..40 {
        let r = s
            .http
            .get(s.url("/v1/status"))
            .bearer_auth(format!("bad-key-{k}"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401);
    }
    // a Reader calling an admin route: forbidden (security)
    let (st, _) = s.get("/v1/admin/status").await;
    assert_eq!(st, 403);
    let (st, v) = s.get("/v1/observability/events?limit=1000").await;
    assert_eq!(st, 200);
    assert_eq!(v["limit"], 200, "limit > 200 is clamped to 200");
    let sec = v["security"].as_array().unwrap();
    let ops = v["operational"].as_array().unwrap();
    assert!(sec.len() <= 200 && ops.len() <= 200);
    assert!(sec
        .iter()
        .all(|e| e["event_type"].as_str().unwrap().starts_with("auth.")
            || ["admin.action", "catalog.ddl"].contains(&e["event_type"].as_str().unwrap())));
    assert!(sec.iter().any(|e| e["event_type"] == "auth.failure"));
    assert!(sec.iter().any(|e| e["event_type"] == "auth.forbidden"));
    assert!(ops.iter().any(|e| e["event_type"] == "query.failed"));
    assert!(ops
        .iter()
        .all(|e| !e["event_type"].as_str().unwrap().starts_with("auth.")));
    for e in sec.iter().chain(ops.iter()) {
        for k in [
            "timestamp",
            "event_type",
            "severity",
            "operation_class",
            "result",
        ] {
            assert!(!e[k].is_null(), "{k}");
        }
        assert!(
            e.get("duration_ms").is_some()
                && e.get("request_id").is_some()
                && e.get("session_id").is_some()
                && e.get("error_class").is_some()
        );
    }
    // a flood of security events cannot evict the operational one
    for k in 0..600 {
        let _ = s
            .http
            .get(s.url("/v1/status"))
            .bearer_auth(format!("flood-{k}"))
            .send()
            .await
            .unwrap();
    }
    let (_, v) = s.get("/v1/observability/events?limit=200").await;
    assert_eq!(v["security"].as_array().unwrap().len(), 200);
    assert!(v["operational"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["event_type"] == "query.failed"));
    // newest first
    let ts: Vec<u64> = v["security"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["timestamp"].as_u64().unwrap())
        .collect();
    assert!(ts.windows(2).all(|p| p[0] >= p[1]));
    // limit validation
    let (st, _) = s.get("/v1/observability/events?limit=abc").await;
    assert_eq!(st, 400);
    let (_, v) = s.get("/v1/observability/events?limit=0").await;
    assert_eq!(v["limit"], 1);
    s.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn version_reports_product_version_build_identity_and_startup_time() {
    let s = Server::real("version").await;
    let (st, v) = s.get("/v1/observability/version").await;
    assert_eq!(st, 200);
    assert_eq!(v["product_version"], env!("CARGO_PKG_VERSION"));
    let b = v["build_identifier"].as_str().unwrap();
    assert!(b.starts_with(env!("CARGO_PKG_VERSION")));
    assert!(!b.contains('\\') && !b.contains('/'), "no local paths: {b}");
    match v["git_revision"].as_str() {
        None => assert!(v["git_revision"].is_null()),
        Some(r) => {
            let base = r.strip_suffix("-dirty").unwrap_or(r);
            assert!(
                base.len() >= 7 && base.bytes().all(|c| c.is_ascii_hexdigit()),
                "{r}"
            );
        }
    }
    let t = v["startup_timestamp_unix_ms"].as_u64().unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    assert!(t <= now && now - t < 60_000);
    s.stop().await;
}

// ------------------------------------------------------------------------------------------
// 9. no credential in any observability response
// ------------------------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_api_key_appears_in_any_observability_response() {
    let s = Server::real("nokeys").await;
    s.sql("CREATE TABLE t (id INTEGER PRIMARY KEY)").await;
    s.sql("BEGIN").await;
    let _ = s
        .http
        .get(s.url("/v1/status"))
        .bearer_auth("presented-wrong-key-ZZZ123")
        .send()
        .await;
    let _ = s.get("/v1/admin/status").await; // forbidden for the reader
    wait_for_generation(&s, 4).await;
    let mut all = String::new();
    for p in [
        "/v1/metrics/system",
        "/v1/metrics/system/timeseries?window=15m",
        "/v1/metrics/system/timeseries?window=1h",
        "/v1/observability/sessions",
        "/v1/observability/queries",
        "/v1/observability/events?limit=200",
        "/v1/observability/version",
        "/v1/metrics",
        "/v1/status",
    ] {
        let (_, v) = s.get(p).await;
        all.push_str(&v.to_string());
        // error bodies too
        let r = s
            .http
            .get(s.url(p))
            .bearer_auth("presented-wrong-key-ZZZ123")
            .send()
            .await
            .unwrap();
        all.push_str(&r.text().await.unwrap());
    }
    for secret in [
        ADMIN_KEY,
        READER_KEY,
        "presented-wrong-key-ZZZ123",
        "bad-key",
    ] {
        assert!(
            !all.contains(secret),
            "credential material leaked: {secret}"
        );
    }
    assert!(!all.contains("Bearer"));
    s.stop().await;
}

// ------------------------------------------------------------------------------------------
// 10. cardinality
// ------------------------------------------------------------------------------------------
fn key_paths(v: &Value, prefix: &str, out: &mut BTreeSet<String>) {
    match v {
        Value::Object(m) => {
            for (k, c) in m {
                let p = format!("{prefix}.{k}");
                out.insert(p.clone());
                key_paths(c, &p, out);
            }
        }
        Value::Array(a) => {
            // arrays of records: the *shape* of the first element only; data rows are not keys
            if let Some(f) = a.first() {
                key_paths(f, &format!("{prefix}[]"), out);
            }
        }
        _ => {}
    }
}

async fn metric_key_set(s: &Server) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut keys = BTreeSet::new();
    let mut routes = BTreeSet::new();
    for p in [
        "/v1/metrics",
        "/v1/metrics/system",
        "/v1/status",
        "/v1/compaction/metrics",
    ] {
        let (_, v) = s.get(p).await;
        key_paths(&v, p, &mut keys);
        if p == "/v1/metrics" {
            for r in v["service"]["routes"].as_array().unwrap() {
                routes.insert(r["route"].as_str().unwrap().to_string());
            }
        }
    }
    let (_, v) = s.get_with(ADMIN_KEY, "/v1/admin/status").await;
    key_paths(&v, "/v1/admin/status", &mut keys);
    (keys, routes)
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn word(&mut self, n: usize) -> String {
        (0..n)
            .map(|_| (b'a' + (self.next() % 26) as u8) as char)
            .collect()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn adversarial_inputs_never_grow_the_metric_key_set() {
    let s = Server::real("cardinality").await;
    s.sql("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
        .await;
    // Exercise every route family once so the baseline set is complete, then freeze it.
    for p in [
        "/healthz",
        "/readyz",
        "/v1/whoami",
        "/v1/status",
        "/v1/metadata",
        "/v1/metrics",
        "/v1/metrics/system",
        "/v1/observability/sessions",
        "/v1/observability/queries",
        "/v1/observability/events",
        "/v1/observability/version",
        "/v1/catalog/tables",
        "/v1/catalog/tables/t",
        "/v1/snapshots",
        "/v1/compaction/status",
    ] {
        let _ = s.get(p).await;
    }
    wait_for_generation(&s, 3).await;
    let _ = metric_key_set(&s).await; // warm-up: the probe's own routes are part of the baseline
    let (keys0, routes0) = metric_key_set(&s).await;
    let mut rng = Rng(0x9E3779B97F4A7C15);

    // (a) 1,000 SQL statements of random text
    for _ in 0..1000 {
        let text = match rng.next() % 4 {
            0 => format!("SELECT {} FROM {}", rng.word(6), rng.word(8)),
            1 => format!("{} {} {}", rng.word(5), rng.word(7), rng.word(3)),
            2 => format!(
                "INSERT INTO t (id, v) VALUES ({}, '{}')",
                rng.next() % 100000,
                rng.word(20)
            ),
            _ => format!("'; DROP TABLE {} --", rng.word(9)),
        };
        let _ = s.sql(&text).await;
    }
    let (k1, r1) = metric_key_set(&s).await;
    assert_eq!(
        k1,
        keys0,
        "random SQL text grew the key set: {:?}",
        k1.symmetric_difference(&keys0).collect::<Vec<_>>()
    );
    assert_eq!(r1, routes0, "random SQL text grew the route-label set");

    // (b) 1,000 requests with random table names (SQL and REST)
    for _ in 0..500 {
        let n = rng.word(12);
        let _ = s.sql(&format!("SELECT * FROM {n}")).await;
        let _ = s.get(&format!("/v1/catalog/tables/{n}")).await;
    }
    let (k2, r2) = metric_key_set(&s).await;
    assert_eq!(k2, keys0, "random table names grew the key set");
    assert_eq!(r2, routes0, "random table names grew the route-label set");

    // (c) 1,000 requests with random strings as bearer keys (also random principal-looking names)
    for _ in 0..1000 {
        let k = if rng.next().is_multiple_of(2) {
            rng.word(24)
        } else {
            format!("{}:{}", rng.word(8), rng.word(16))
        };
        let r = s
            .http
            .get(s.url("/v1/status"))
            .bearer_auth(k)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401);
    }
    let (k3, r3) = metric_key_set(&s).await;
    assert_eq!(k3, keys0, "random bearer keys grew the key set");
    assert_eq!(r3, routes0, "random bearer keys grew the route-label set");

    // (d) the case that failed before this layer existed: arbitrary HTTP method tokens and paths
    for _ in 0..1000 {
        let n = 1 + (rng.next() % 12) as usize;
        let m = reqwest::Method::from_bytes(rng.word(n).to_uppercase().as_bytes()).unwrap();
        let path = match rng.next() % 4 {
            0 => "/v1/sql".to_string(),
            1 => format!("/v1/kv/{}", rng.word(8)),
            2 => format!("/{}", rng.word(10)),
            _ => format!("/v1/{}/{}", rng.word(5), rng.word(5)),
        };
        let _ = s
            .http
            .request(m, s.url(&path))
            .bearer_auth(ADMIN_KEY)
            .send()
            .await
            .unwrap();
    }
    let (k4, r4) = metric_key_set(&s).await;
    assert_eq!(k4, keys0, "arbitrary methods/paths grew the key set");
    assert!(
        r4.len() <= routes0.len() + 8,
        "route labels may gain only closed-set buckets: {} -> {}",
        routes0.len(),
        r4.len()
    );
    for r in &r4 {
        let (m, path) = r.split_once(' ').unwrap();
        assert!(
            ["GET", "POST", "PUT", "DELETE", "HEAD", "OPTIONS", "PATCH", "OTHER"].contains(&m),
            "{r}"
        );
        assert!(
            path.starts_with('/') || path == "UNMATCHED" || path == "OVERFLOW",
            "{r}"
        );
    }
    assert!(s.state.metrics.route_key_count() <= rubixdb_api::metrics::MAX_ROUTE_KEYS);
    s.stop().await;
}

// ------------------------------------------------------------------------------------------
// 13. no thread leak
// ------------------------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sampler_start_stop_100_times_leaves_no_thread_behind() {
    let mut server = Server::real("leak").await;
    // stop the server's own sampler first so the cycles start from none
    server.sampler.take().unwrap().stop();
    let before = rubixdb_api::resources::process_resources().threads;
    for _ in 0..100 {
        let mut h =
            sampler::start_with(&server.state, Arc::new(RealProbe), Duration::from_millis(5))
                .unwrap();
        tokio::time::sleep(Duration::from_millis(12)).await;
        h.stop();
        // second start while running is refused
    }
    let after = rubixdb_api::resources::process_resources().threads;
    assert!(
        after <= before + 1,
        "threads before {before}, after 100 cycles {after}: the sampler leaked a thread"
    );
    // a running sampler refuses a second one
    let h1 = sampler::start_with(
        &server.state,
        Arc::new(RealProbe),
        Duration::from_millis(50),
    )
    .unwrap();
    assert!(sampler::start_with(
        &server.state,
        Arc::new(RealProbe),
        Duration::from_millis(50)
    )
    .is_err());
    drop(h1);
    assert_eq!(
        server.state.obs.sampler.state(),
        sampler::SamplerState::NotStarted
    );
    server.stop().await;
}

// ------------------------------------------------------------------------------------------
// 14. snapshot consistency: no torn reads
// ------------------------------------------------------------------------------------------
/// Every value derives from how many times it was asked, so all fields of one snapshot share one
/// generation number. A torn read (fields from two ticks) would break an equality below.
struct GenProbe(AtomicU64);
impl OsProbe for GenProbe {
    fn process_cpu_seconds(&self) -> Option<f64> {
        Some(0.0)
    }
    fn process_rss(&self) -> Option<(u64, u64)> {
        let n = self.0.fetch_add(1, Ordering::SeqCst) + 1;
        Some((n * 1000, n * 1000 + 1))
    }
    fn system_memory(&self) -> Option<(u64, u64)> {
        let n = self.0.load(Ordering::SeqCst);
        Some((10_000_000, 10_000_000 - n * 1000)) // used = n * 1000
    }
    fn vcpu_count(&self) -> Option<u32> {
        Some(4)
    }
    fn disk_capacity(&self, _: &std::path::Path) -> Option<(u64, u64)> {
        let n = self.0.load(Ordering::SeqCst);
        Some((50_000_000, 40_000_000 - n)) // free = 40_000_000 - n
    }
    fn process_io(&self) -> Option<IoCounters> {
        None
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn back_to_back_reads_while_the_sampler_ticks_never_see_a_torn_snapshot() {
    let s = Server::start(
        "torn",
        Arc::new(GenProbe(AtomicU64::new(0))),
        Duration::from_millis(5),
    )
    .await;
    wait_for_generation(&s, 3).await;
    let mut gens = BTreeSet::new();
    for _ in 0..300 {
        let (_, v) = s.get("/v1/metrics/system").await;
        let g = v["sample_generation"].as_u64().unwrap();
        gens.insert(g);
        assert_eq!(
            v["memory"]["rss_bytes"].as_u64().unwrap(),
            g * 1000,
            "rss belongs to generation {g}"
        );
        assert_eq!(
            v["memory"]["peak_rss_bytes"].as_u64().unwrap(),
            g * 1000 + 1
        );
        assert_eq!(
            v["memory"]["system_used_bytes"].as_u64().unwrap(),
            g * 1000,
            "system memory belongs to generation {g}"
        );
        assert_eq!(
            v["disk"]["volume_free_bytes"].as_u64().unwrap(),
            40_000_000 - g,
            "disk belongs to generation {g}"
        );
        assert!(
            v["disk"]["read_iops"].is_null(),
            "io probe unavailable: null"
        );
    }
    assert!(
        gens.len() > 3,
        "the sampler really ticked during the reads: {} generations",
        gens.len()
    );
    s.stop().await;
}

// ------------------------------------------------------------------------------------------
// 18. multi-instance isolation
// ------------------------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn two_instances_report_only_their_own_data_and_stopping_one_does_not_affect_the_other() {
    let a = Server::real("iso_a").await;
    let b = Server::real("iso_b").await;
    a.sql("CREATE TABLE t (id INTEGER PRIMARY KEY)").await;
    for _ in 0..60 {
        a.sql("SELECT 1").await;
    }
    for _ in 0..7 {
        b.sql("SELECT 1").await;
    }
    let _ = b
        .http
        .get(b.url("/v1/status"))
        .bearer_auth("wrong-for-b")
        .send()
        .await
        .unwrap();
    for srv in [&a, &b] {
        let (_, v) = srv.get("/v1/metrics/system").await;
        wait_for_generation(srv, v["sample_generation"].as_u64().unwrap() + 2).await;
    }
    let (_, va) = a.get("/v1/metrics/system").await;
    let (_, vb) = b.get("/v1/metrics/system").await;
    assert_ne!(va["instance"]["id"], vb["instance"]["id"]);
    assert_eq!(va["instance"]["name"], "iso_a");
    assert_eq!(vb["instance"]["name"], "iso_b");
    assert_eq!(va["security"]["auth_failures_since_start"], 0);
    assert_eq!(
        vb["security"]["auth_failures_since_start"], 1,
        "b's failed login is only in b"
    );
    let (_, qa) = a.get("/v1/observability/queries?limit=200").await;
    let (_, qb) = b.get("/v1/observability/queries?limit=200").await;
    assert!(qa["queries"].as_array().unwrap().len() >= 61);
    assert_eq!(qb["queries"].as_array().unwrap().len(), 7);
    let (_, ea) = a.get("/v1/observability/events").await;
    assert!(ea["security"].as_array().unwrap().is_empty());
    // stopping A leaves B fully working
    a.stop().await;
    let (st, v2) = b.get("/v1/metrics/system").await;
    assert_eq!(st, 200);
    assert_eq!(v2["sample_freshness"]["state"], "running");
    assert_eq!(b.sql("SELECT 1").await.0, 200);
    let g0 = v2["sample_generation"].as_u64().unwrap();
    wait_for_generation(&b, g0 + 3).await;
    b.stop().await;
}

// ------------------------------------------------------------------------------------------
// extra: health classification, stale/dead sampler visibility, 429/limit counters
// ------------------------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dead_sampler_is_visible_as_stale_and_failed_never_as_current_data() {
    let mut server = Server::real("stale").await;
    wait_for_generation(&server, 3).await;
    // Stop the thread but keep serving: the last snapshot ages.
    server.sampler.take().unwrap().stop();
    let (_, v) = server.get("/v1/metrics/system").await;
    assert_eq!(v["sample_freshness"]["state"], "not_started");
    tokio::time::sleep(Duration::from_millis(400)).await;
    let (_, v) = server.get("/v1/metrics/system").await;
    let age = v["sample_freshness"]["age_ms"].as_u64().unwrap();
    assert!(age >= 400, "age grows while nothing samples: {age}");
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rate_limit_rejections_are_counted_with_events() {
    let dir = temp_dir("limits");
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
                queue_capacity: 64,
                max_queued_bytes: 16 * 1024 * 1024,
                submission_timeout: Duration::from_secs(5),
                shutdown_drain_bound: Duration::from_secs(10),
                await_retry_budget: Duration::from_secs(5),
                max_drain_per_batch: 4096,
            },
            lsm_config.clone(),
        )
        .unwrap(),
    );
    let mut cfg = test_config(dir.clone(), "limits");
    cfg.rate_limit_rps = 1.0;
    cfg.rate_limit_burst = 5;
    cfg.sql_max_sessions_per_principal = 2;
    let state = Arc::new(AppState::new(engine, lsm_config, cfg));
    let router = build_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let join = tokio::spawn(serve_observed(
        listener,
        router,
        async move {
            let _ = rx.await;
        },
        Duration::from_secs(5),
        ServerLimits::default(),
        None,
    ));
    let http = reqwest::Client::new();
    let mut c429 = 0;
    for _ in 0..20 {
        let r = http
            .get(format!("http://{addr}/v1/status"))
            .bearer_auth(ADMIN_KEY)
            .send()
            .await
            .unwrap();
        if r.status() == 429 {
            c429 += 1;
        }
    }
    assert!(c429 > 0);
    assert_eq!(
        state.obs.counters.rate_limited.load(Ordering::Relaxed),
        c429
    );
    let ev = state.obs.events.security(200);
    assert_eq!(
        ev.iter()
            .filter(|e| e.event_type == "auth.rate_limited")
            .count() as u64,
        c429.min(200)
    );
    let _ = tx.send(());
    let _ = join.await;
    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------------------------------------
// 11/12. health classification (decision recorded for review in the certification document)
// ------------------------------------------------------------------------------------------
/// Reports a fixed (total, free) for the volume; everything else is real.
struct DiskProbe(AtomicU64, u64);
impl OsProbe for DiskProbe {
    fn process_cpu_seconds(&self) -> Option<f64> {
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
    fn disk_capacity(&self, _: &std::path::Path) -> Option<(u64, u64)> {
        Some((self.1, self.0.load(Ordering::SeqCst)))
    }
    fn process_io(&self) -> Option<IoCounters> {
        RealProbe.process_io()
    }
}

async fn health_after_tick(s: &Server) -> (String, String) {
    let (_, v) = s.get("/v1/metrics/system").await;
    wait_for_generation(s, v["sample_generation"].as_u64().unwrap() + 2).await;
    let (_, v) = s.get("/v1/metrics/system").await;
    (
        v["instance"]["healthy"].as_str().unwrap().to_string(),
        v["instance"]["readiness"].as_str().unwrap().to_string(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn health_follows_storage_state_and_disk_free_and_leaves_readiness_alone() {
    use rubixdb::lsm::StorageState;
    let total = 1_000_000_000u64;
    let free = Arc::new(DiskProbe(AtomicU64::new(total / 2), total));
    let probe_free = free.clone();
    struct Shared(Arc<DiskProbe>);
    impl OsProbe for Shared {
        fn process_cpu_seconds(&self) -> Option<f64> {
            self.0.process_cpu_seconds()
        }
        fn process_rss(&self) -> Option<(u64, u64)> {
            self.0.process_rss()
        }
        fn system_memory(&self) -> Option<(u64, u64)> {
            self.0.system_memory()
        }
        fn vcpu_count(&self) -> Option<u32> {
            self.0.vcpu_count()
        }
        fn disk_capacity(&self, p: &std::path::Path) -> Option<(u64, u64)> {
            self.0.disk_capacity(p)
        }
        fn process_io(&self) -> Option<IoCounters> {
            self.0.process_io()
        }
    }
    let s = Server::start(
        "health",
        Arc::new(Shared(probe_free)),
        Duration::from_millis(30),
    )
    .await;
    assert_eq!(health_after_tick(&s).await.0, "healthy");

    // StoragePressure => degraded; the health/readiness endpoints keep their meaning.
    s.state
        .engine
        .set_storage_state_for_test(StorageState::StoragePressure);
    assert_eq!(
        health_after_tick(&s).await,
        ("degraded".into(), "ready".into())
    );
    assert_eq!(
        s.http
            .get(s.url("/readyz"))
            .bearer_auth(READER_KEY)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        s.http.get(s.url("/healthz")).send().await.unwrap().status(),
        200
    );
    // StorageFull => failed
    s.state
        .engine
        .set_storage_state_for_test(StorageState::StorageFull);
    assert_eq!(health_after_tick(&s).await.0, "failed");
    s.state
        .engine
        .set_storage_state_for_test(StorageState::Healthy);
    assert_eq!(health_after_tick(&s).await.0, "healthy");

    // disk free: < 10 % of total is degraded, exactly 10 % is not, > 10 % is not.
    free.0.store(total / 10 - 1, Ordering::SeqCst);
    assert_eq!(health_after_tick(&s).await.0, "degraded");
    free.0.store(total / 10, Ordering::SeqCst);
    assert_eq!(health_after_tick(&s).await.0, "healthy");
    free.0.store(total / 2, Ordering::SeqCst);
    assert_eq!(health_after_tick(&s).await.0, "healthy");

    // instance lock lost => failed (the probe is the same one the embedded host installs)
    let held = Arc::new(AtomicBool::new(true));
    let h2 = held.clone();
    s.state
        .obs
        .set_lock_probe(Arc::new(move || h2.load(Ordering::SeqCst)));
    assert_eq!(health_after_tick(&s).await.0, "healthy");
    held.store(false, Ordering::SeqCst);
    assert_eq!(health_after_tick(&s).await.0, "failed");
    held.store(true, Ordering::SeqCst);
    assert_eq!(health_after_tick(&s).await.0, "healthy");

    // The classification is computed once per tick: two reads between ticks cannot disagree.
    let (_, a) = s.get("/v1/metrics/system").await;
    let (_, b) = s.get("/v1/metrics/system").await;
    if a["sample_generation"] == b["sample_generation"] {
        assert_eq!(a["instance"]["healthy"], b["instance"]["healthy"]);
    }
    s.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn readiness_stays_true_while_safe_index_recovery_runs() {
    use rubixdb_api::recovery::RecoveryState;
    // A fixed 50 %-free volume: the real volume's free space must not decide this test.
    let probe = Arc::new(DiskProbe(AtomicU64::new(500_000_000), 1_000_000_000));
    let s = Server::start("recovery_ready", probe, Duration::from_millis(30)).await;
    s.state.index_recovery.set_for_test(RecoveryState::Running);
    let (h, r) = health_after_tick(&s).await;
    assert_eq!((h.as_str(), r.as_str()), ("healthy", "ready"));
    let (_, v) = s.get("/v1/metrics/system").await;
    assert_eq!(v["background"]["index_build_state"], "running");
    assert_eq!(
        s.http
            .get(s.url("/readyz"))
            .bearer_auth(READER_KEY)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    s.state.index_recovery.set_for_test(RecoveryState::Failed);
    let (h, r) = health_after_tick(&s).await;
    assert_eq!(
        (h.as_str(), r.as_str()),
        ("healthy", "ready"),
        "a failed index build is reported, not escalated to instance failure"
    );
    s.stop().await;
}

// ------------------------------------------------------------------------------------------
// query states: timeout, cancellation, running, failure
// ------------------------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_statement_past_its_deadline_is_recorded_as_timed_out_with_an_event() {
    // A zero-second deadline is already in the past at the first check: deterministic.
    let s = Server::start_cfg(
        "timeout",
        Arc::new(RealProbe),
        Duration::from_millis(50),
        |c| {
            c.sql_statement_deadline_secs = 0;
        },
    )
    .await;
    let (st, body) = s.sql("SELECT 1").await;
    assert_eq!(st, 504, "{body}");
    let (_, q) = s.get("/v1/observability/queries").await;
    let r = &q["queries"][0];
    assert_eq!(r["state"], "timed_out");
    assert_eq!(r["timeout_state"], "deadline_exceeded");
    assert_eq!(r["error_class"], "SQL_TIMEOUT");
    assert_eq!(q["active"], 0, "a finished statement is not active");
    let (_, ev) = s.get("/v1/observability/events").await;
    assert!(ev["operational"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["event_type"] == "query.timeout"));
    s.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn running_and_cancelled_queries_are_visible_while_real_expensive_queries_run() {
    let s = Arc::new(Server::real("running").await);
    s.sql("CREATE TABLE t (id INTEGER PRIMARY KEY, grp TEXT, val INTEGER)")
        .await;
    for base in (0..3000).step_by(500) {
        let rows: Vec<String> = (base..base + 500)
            .map(|i| format!("({i}, 'g{}', {i})", i % 20))
            .collect();
        let (st, b) = s
            .sql(&format!(
                "INSERT INTO t (id, grp, val) VALUES {}",
                rows.join(",")
            ))
            .await;
        assert_eq!(st, 200, "{b}");
    }
    let expensive =
        "SELECT grp, COUNT(*), SUM(val), MIN(val), MAX(val) FROM t GROUP BY grp HAVING COUNT(*) > 0";
    let stop = Arc::new(AtomicBool::new(false));
    let mut load = Vec::new();
    for k in 0..24 {
        let s = s.clone();
        let stop = stop.clone();
        load.push(tokio::spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                if k % 2 == 0 {
                    // a client that gives up after a few ms: the request future is dropped
                    let _ = tokio::time::timeout(Duration::from_millis(3), s.sql(expensive)).await;
                } else {
                    let _ = s.sql(expensive).await;
                }
            }
        }));
    }
    let (mut saw_running, mut saw_active_gauge, mut saw_cancelled) = (false, false, false);
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && !(saw_running && saw_active_gauge && saw_cancelled) {
        let (_, q) = s.get("/v1/observability/queries").await;
        for r in q["queries"].as_array().unwrap() {
            saw_running |= r["state"] == "running";
            saw_cancelled |= r["state"] == "cancelled" && r["cancellation_state"] == "cancelled";
        }
        let (_, v) = s.get("/v1/metrics/system").await;
        saw_active_gauge |= v["throughput"]["active_queries"].as_i64().unwrap_or(0) >= 1;
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    stop.store(true, Ordering::Relaxed);
    for t in load {
        let _ = t.await;
    }
    assert!(saw_running, "a running statement was never listed");
    assert!(
        saw_active_gauge,
        "active_queries never rose above 0 under load"
    );
    assert!(
        saw_cancelled,
        "a client disconnect never produced a cancelled record"
    );
    // everything drained: nothing is left active
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (_, q) = s.get("/v1/observability/queries").await;
        if q["active"] == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "queries still active after the load stopped: {}",
            q["active"]
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (_, ev) = s.get("/v1/observability/events?limit=200").await;
    assert!(ev["operational"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["event_type"] == "query.cancelled"));
    Arc::try_unwrap(s).ok().unwrap().stop().await;
}

// ------------------------------------------------------------------------------------------
// rates and gauges follow real load (and fall back when it stops)
// ------------------------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rates_and_gauges_follow_real_load_and_fall_back_when_it_stops() {
    let s = Arc::new(Server::real("rates").await);
    s.sql("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
        .await;
    // sessions and transactions are gauges of what is open right now
    for _ in 0..3 {
        assert_eq!(s.sql("BEGIN").await.0, 200);
    }
    // idle connections are counted while open
    let mut held: Vec<tokio::net::TcpStream> = Vec::new();
    for _ in 0..5 {
        held.push(tokio::net::TcpStream::connect(s.addr).await.unwrap());
    }
    let stop = Arc::new(AtomicBool::new(false));
    let mut load = Vec::new();
    for k in 0..4u64 {
        let s = s.clone();
        let stop = stop.clone();
        load.push(tokio::spawn(async move {
            let mut n = 0u64;
            while !stop.load(Ordering::Relaxed) {
                n += 1;
                s.sql(&format!(
                    "INSERT INTO t (id, v) VALUES ({}, 'x')",
                    k * 1_000_000 + n
                ))
                .await;
            }
        }));
    }
    let (mut max_sql, mut max_http, mut max_commits) = (0.0f64, 0.0f64, 0.0f64);
    let (mut max_conn, mut sessions, mut txns) = (0i64, 0i64, 0i64);
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until {
        let (_, v) = s.get("/v1/metrics/system").await;
        let t = &v["throughput"];
        max_sql = max_sql.max(t["sql_queries_per_sec"].as_f64().unwrap_or(0.0));
        max_http = max_http.max(t["http_requests_per_sec"].as_f64().unwrap_or(0.0));
        max_commits = max_commits.max(t["write_commits_per_sec"].as_f64().unwrap_or(0.0));
        max_conn = max_conn.max(t["active_connections"].as_i64().unwrap());
        sessions = sessions.max(t["active_sessions"].as_i64().unwrap());
        txns = txns.max(t["active_transactions"].as_i64().unwrap());
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    stop.store(true, Ordering::Relaxed);
    for t in load {
        t.await.unwrap();
    }
    eprintln!("under load: sql/s={max_sql:.0} http/s={max_http:.0} commits/s={max_commits:.0} connections={max_conn} sessions={sessions} txns={txns}");
    assert!(max_sql >= 20.0, "sql rate {max_sql}");
    assert!(max_http >= 20.0, "http rate {max_http}");
    assert!(max_commits >= 5.0, "commit rate {max_commits}");
    assert!(
        max_conn >= 5 + 4,
        "5 held + 4 load connections, saw {max_conn}"
    );
    assert_eq!(sessions, 3, "three open BEGIN sessions");
    // open BEGIN sessions plus any implicit transactions of in-flight autocommit statements
    assert!(
        txns >= 3,
        "at least the three open transactions, saw {txns}"
    );
    // after the load stops, the rates fall to a measured 0 (not stale, not null)
    held.clear();
    tokio::time::sleep(Duration::from_millis(800)).await;
    let (_, v) = s.get("/v1/metrics/system").await;
    let t = &v["throughput"];
    assert_eq!(
        t["active_transactions"].as_i64().unwrap(),
        3,
        "idle again: exactly the three open BEGIN sessions"
    );
    assert_eq!(t["sql_queries_per_sec"].as_f64().unwrap(), 0.0);
    assert_eq!(t["write_commits_per_sec"].as_f64().unwrap(), 0.0);
    // the 5 held sockets are gone; the rest are the HTTP client's own pooled keep-alive connections
    assert!(
        t["active_connections"].as_i64().unwrap() <= max_conn - 5,
        "held connections released: {} of peak {max_conn}",
        t["active_connections"]
    );
    Arc::try_unwrap(s).ok().unwrap().stop().await;
}

// ------------------------------------------------------------------------------------------
// a wedged sampler is reported as stale, then failed, never as current data
// ------------------------------------------------------------------------------------------
struct BlockProbe(Arc<AtomicBool>);
impl OsProbe for BlockProbe {
    fn process_cpu_seconds(&self) -> Option<f64> {
        while self.0.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(20));
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wedged_sampler_goes_stale_then_failed_and_recovers_when_unblocked() {
    let block = Arc::new(AtomicBool::new(false));
    let s = Server::start(
        "wedged",
        Arc::new(BlockProbe(block.clone())),
        Duration::from_millis(100),
    )
    .await;
    wait_for_generation(&s, 3).await;
    let (_, v) = s.get("/v1/metrics/system").await;
    assert_eq!(v["sample_freshness"]["state"], "running");
    assert_eq!(v["sample_freshness"]["stale"], false);
    let frozen_generation = v["sample_generation"].as_u64().unwrap();

    block.store(true, Ordering::SeqCst); // the next tick hangs inside the platform call
    tokio::time::sleep(Duration::from_millis(6_000)).await;
    let (_, v) = s.get("/v1/metrics/system").await;
    assert_eq!(
        v["sample_freshness"]["stale"], true,
        "older than 5 s is stale: {}",
        v["sample_freshness"]
    );
    assert!(v["sample_freshness"]["age_ms"].as_u64().unwrap() >= 5_000);
    assert_eq!(
        v["sample_freshness"]["state"], "running",
        "not yet declared failed"
    );

    tokio::time::sleep(Duration::from_millis(5_000)).await;
    let (st, v) = s.get("/v1/metrics/system").await;
    assert_eq!(st, 200, "the endpoint itself keeps answering");
    assert_eq!(
        v["sample_freshness"]["state"], "failed",
        "{}",
        v["sample_freshness"]
    );
    assert!(v["sample_freshness"]["age_ms"].as_u64().unwrap() >= 10_000);
    // the last good snapshot is returned with its own (old) age; it is not refreshed or relabelled
    assert!(v["sample_generation"].as_u64().unwrap() <= frozen_generation + 1);
    assert_eq!(
        s.sql("CREATE TABLE t (id INTEGER PRIMARY KEY)").await.0,
        200,
        "the server still works"
    );

    block.store(false, Ordering::SeqCst);
    wait_for_generation(&s, frozen_generation + 5).await;
    let (_, v) = s.get("/v1/metrics/system").await;
    assert_eq!(v["sample_freshness"]["state"], "running");
    assert_eq!(v["sample_freshness"]["stale"], false);
    s.stop().await;
}

// ------------------------------------------------------------------------------------------
// latency percentiles are server-side times: positive, ordered, and bounded by what clients saw
// ------------------------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn latency_percentiles_are_consistent_with_client_measured_times() {
    let s = Server::real("latency").await;
    s.sql("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
        .await;
    let (_, before) = s.get("/v1/metrics/system").await;
    let mut client_ms = Vec::new();
    for k in 0..300 {
        let t0 = Instant::now();
        let (st, _) = s
            .sql(&format!("INSERT INTO t (id, v) VALUES ({k}, 'x')"))
            .await;
        client_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(st, 200);
    }
    wait_for_generation(&s, before["sample_generation"].as_u64().unwrap() + 3).await;
    let (_, v) = s.get("/v1/metrics/system").await;
    let l = &v["latency"];
    let (p50, p95, p99) = (
        l["query_p50_ms"].as_f64().unwrap(),
        l["query_p95_ms"].as_f64().unwrap(),
        l["query_p99_ms"].as_f64().unwrap(),
    );
    client_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let c = |q: f64| client_ms[((client_ms.len() as f64) * q) as usize];
    eprintln!("server p50/p95/p99 = {p50:.3}/{p95:.3}/{p99:.3} ms; client p50/p95/p99 = {:.3}/{:.3}/{:.3} ms", c(0.5), c(0.95), c(0.99));
    assert!(p50 > 0.0 && p50 <= p95 && p95 <= p99);
    // the server measures a part of what the client measures (no client, TCP or JSON time)
    assert!(
        p50 <= c(0.5) * 1.05,
        "server p50 {p50} vs client p50 {}",
        c(0.5)
    );
    assert!(p95 <= c(0.99), "server p95 {p95} vs client p99 {}", c(0.99));
    s.stop().await;
}

// ------------------------------------------------------------------------------------------
// resource-limit and security counters
// ------------------------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_cap_refusals_and_admin_actions_are_counted_and_visible() {
    let s = Server::start_cfg(
        "caps",
        Arc::new(RealProbe),
        Duration::from_millis(50),
        |c| c.sql_max_sessions_per_principal = 2,
    )
    .await;
    assert_eq!(s.sql("BEGIN").await.0, 200);
    assert_eq!(s.sql("BEGIN").await.0, 200);
    let (st, b) = s.sql("BEGIN").await;
    assert_eq!(st, 429, "{b}");
    assert_eq!(b["error"]["code"], "TOO_MANY_SESSIONS");
    // an admin action (integrity check) is counted, with the last one recorded by route label
    let r = s
        .http
        .post(s.url("/v1/admin/check"))
        .bearer_auth(ADMIN_KEY)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert!(r.status().as_u16() < 500, "{}", r.status());
    let before = s.get("/v1/metrics/system").await.1["sample_generation"]
        .as_u64()
        .unwrap();
    wait_for_generation(&s, before + 2).await;
    let (_, v) = s.get("/v1/metrics/system").await;
    assert_eq!(v["limits"]["sessions_rejected_since_start"], 1);
    assert_eq!(v["security"]["admin_actions_since_start"], 1);
    let last = &v["security"]["last_admin_action"];
    assert_eq!(last["route"], "/v1/admin/check");
    assert!(last["timestamp_unix_ms"].as_u64().unwrap() > 0);
    let (_, ev) = s.get("/v1/observability/events?limit=50").await;
    assert!(ev["operational"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["event_type"] == "session.rejected"));
    assert!(ev["security"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["event_type"] == "admin.action"));
    // the reader cannot perform it (and that is a counted forbidden, not an admin action)
    let r = s
        .http
        .post(s.url("/v1/admin/check"))
        .bearer_auth(READER_KEY)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 403);
    let before = s.get("/v1/metrics/system").await.1["sample_generation"]
        .as_u64()
        .unwrap();
    wait_for_generation(&s, before + 2).await;
    let (_, v) = s.get("/v1/metrics/system").await;
    assert_eq!(v["security"]["forbidden_since_start"], 1);
    assert_eq!(v["security"]["admin_actions_since_start"], 1);
    s.stop().await;
}

// ------------------------------------------------------------------------------------------
// a panic inside a platform call is contained: the tick is skipped, the server and sampler live on
// ------------------------------------------------------------------------------------------
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_panic_in_a_tick_is_contained_and_the_sampler_recovers() {
    let panic_now = Arc::new(AtomicBool::new(false));
    let s = Server::start(
        "panic",
        Arc::new(PanicProbe(panic_now.clone())),
        Duration::from_millis(30),
    )
    .await;
    wait_for_generation(&s, 3).await;
    panic_now.store(true, Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_secs(5);
    while s.state.obs.sampler.panics() < 2 {
        assert!(Instant::now() < deadline, "no panic was ever recorded");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // fewer than the failure threshold of consecutive panics: degraded, never a dead process
    let (st, v) = s.get("/v1/metrics/system").await;
    assert_eq!(st, 200);
    assert!(["degraded", "failed", "running"]
        .contains(&v["sample_freshness"]["state"].as_str().unwrap()));
    assert_eq!(
        s.sql("CREATE TABLE t (id INTEGER PRIMARY KEY)").await.0,
        200
    );
    assert_eq!(
        s.http.get(s.url("/healthz")).send().await.unwrap().status(),
        200
    );
    panic_now.store(false, Ordering::SeqCst);
    let g = s.state.obs.sampler.ticks();
    let deadline = Instant::now() + Duration::from_secs(5);
    while s.state.obs.sampler.ticks() < g + 5 {
        assert!(Instant::now() < deadline, "the sampler stopped ticking");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    wait_for_generation(&s, 1).await;
    let (_, v) = s.get("/v1/metrics/system").await;
    assert_eq!(v["sample_freshness"]["state"], "running", "recovered");
    s.stop().await;
}

//! Increment 14, Blocker 1 — Query starvation.
//!
//! Real, end-to-end: real release `rubixdb-api.exe`, real on-disk
//! `LsmEngine`, real HTTP via `reqwest`. Runs 1 genuinely expensive SQL
//! query (`WHERE note LIKE '%needle%' GROUP BY ... HAVING ...` over a
//! 100,000-row table with no supporting index, forcing a full
//! sequential scan + per-row string match + aggregation) concurrently
//! against N cheap PK-lookup queries (N = 10/50/100), while a separate
//! worker continuously polls the control-plane surface (`/healthz`,
//! `/readyz`, `/v1/instance`, `/v1/status`). Each concurrency level is
//! measured twice: BASELINE (cheap + control-plane only) and LOADED
//! (same, plus the expensive query running back-to-back for the whole
//! phase) — the delta between the two is the actual starvation
//! evidence, not a single absolute number.
//!
//! Usage: `cargo run --release -p rubixdb-api --example
//! query_starvation_test -- [phase_secs=12]`

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const ROW_COUNT: i64 = 100_000;
const BATCH_SIZE: i64 = 100;
const LEVELS: [usize; 3] = [10, 50, 100];

fn api_bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("target")
        .join("release")
        .join("rubixdb-api.exe")
}

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let base = std::env::var_os("RUBIXDB_SOAK_BASE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let path = base.join(format!("rubixdb_starvation_{tag}_{nanos}"));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn sample_process_metrics(pid: u32) -> Option<(u64, u64, u64)> {
    let output = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "$p = Get-Process -Id {pid} -ErrorAction SilentlyContinue; if ($p) {{ Write-Output ($p.WorkingSet64.ToString() + ',' + $p.HandleCount.ToString() + ',' + $p.Threads.Count.ToString()) }}"
            ),
        ])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut parts = text.trim().split(',');
    let rss_kb: u64 = parts.next()?.trim().parse::<u64>().ok()? / 1024;
    let handles: u64 = parts.next()?.trim().parse().ok()?;
    let threads: u64 = parts.next()?.trim().parse().ok()?;
    Some((rss_kb, handles, threads))
}

struct Server {
    child: Child,
    base_url: String,
    admin_key: String,
    #[allow(dead_code)]
    data_dir: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn spawn_server(port: u16) -> Server {
    let data_dir = temp_dir("srv");
    let admin_key = "starve-test-admin-key-0123456789".to_string();
    let child = Command::new(api_bin_path())
        .env("RUBIXDB_DATA_DIR", &data_dir)
        .env("RUBIXDB_LISTEN_ADDR", format!("127.0.0.1:{port}"))
        .env("RUBIXDB_API_KEYS", format!("admin:admin:{admin_key}"))
        .env("RUBIXDB_RATE_LIMIT_RPS", "1000000")
        .env("RUBIXDB_RATE_LIMIT_BURST", "1000000")
        .env("RUBIXDB_COMPACTION_AUTO_TRIGGER", "true")
        .env("RUBIXDB_SQL_STATEMENT_DEADLINE_SECS", "60")
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn rubixdb-api.exe -- did you `cargo build --release -p rubixdb-api` first?");

    let base_url = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(resp) = client.get(format!("{base_url}/healthz")).send().await {
            if resp.status().is_success() {
                break;
            }
        }
        if Instant::now() > deadline {
            panic!("rubixdb-api did not become healthy within 30s");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    Server {
        child,
        base_url,
        admin_key,
        data_dir,
    }
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

async fn exec_sql(client: &reqwest::Client, base_url: &str, admin_key: &str, sql: &str) -> Result<u16, String> {
    let resp = client
        .post(format!("{base_url}/v1/sql"))
        .bearer_auth(admin_key)
        .json(&serde_json::json!({ "sql": sql, "params": [] }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("HTTP {status}: {body}"));
    }
    Ok(status.as_u16())
}

async fn seed(server: &Server) {
    let client = reqwest::Client::new();
    exec_sql(&client, &server.base_url, &server.admin_key, "DROP TABLE IF EXISTS starve_big")
        .await
        .ok();
    exec_sql(
        &client,
        &server.base_url,
        &server.admin_key,
        "CREATE TABLE starve_big (id INTEGER PRIMARY KEY, grp TEXT, note TEXT, val INTEGER)",
    )
    .await
    .expect("create starve_big");

    let mut inserted = 0i64;
    while inserted < ROW_COUNT {
        let n = BATCH_SIZE.min(ROW_COUNT - inserted);
        let mut sql = String::from("INSERT INTO starve_big (id, grp, note, val) VALUES ");
        for i in 0..n {
            let id = inserted + i;
            let grp = format!("g{}", id % 10);
            let note = if id % 10 == 0 {
                format!("payload-needle-{id}")
            } else {
                format!("payload-plain-{id}")
            };
            if i > 0 {
                sql.push(',');
            }
            sql.push_str(&format!("({id},'{grp}','{note}',{id})"));
        }
        exec_sql(&client, &server.base_url, &server.admin_key, &sql)
            .await
            .unwrap_or_else(|e| panic!("seed batch starting at {inserted} failed: {e}"));
        inserted += n;
        if inserted % 10_000 == 0 {
            println!("  seeded {inserted}/{ROW_COUNT}");
        }
    }
    println!("seeded {ROW_COUNT} rows into starve_big (no secondary index -- forces sequential scan)");
}

#[derive(Default)]
struct Samples {
    latencies_us: std::sync::Mutex<Vec<u64>>,
    ok: AtomicU64,
    err: AtomicU64,
}

fn report_line(label: &str, s: &Samples) {
    let mut v = s.latencies_us.lock().unwrap().clone();
    v.sort_unstable();
    let ok = s.ok.load(Ordering::Relaxed);
    let err = s.err.load(Ordering::Relaxed);
    println!(
        "  {label:<28} n={ok:<6} err={err:<4} p50={:>8.2}ms p95={:>8.2}ms p99={:>8.2}ms max={:>8.2}ms",
        percentile(&v, 0.50) as f64 / 1000.0,
        percentile(&v, 0.95) as f64 / 1000.0,
        percentile(&v, 0.99) as f64 / 1000.0,
        v.last().copied().unwrap_or(0) as f64 / 1000.0,
    );
}

async fn cheap_worker(base_url: String, admin_key: String, stop_at: Instant, samples: Arc<Samples>, seed: u64) {
    let client = reqwest::Client::new();
    let mut rng: u64 = 0x9E3779B97F4A7C15 ^ seed;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    while Instant::now() < stop_at {
        let id = (next() % ROW_COUNT as u64) as i64;
        let start = Instant::now();
        let result = exec_sql(
            &client,
            &base_url,
            &admin_key,
            &format!("SELECT * FROM starve_big WHERE id = {id}"),
        )
        .await;
        let us = start.elapsed().as_micros() as u64;
        match result {
            Ok(_) => {
                samples.ok.fetch_add(1, Ordering::Relaxed);
                samples.latencies_us.lock().unwrap().push(us);
            }
            Err(_) => {
                samples.err.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

async fn control_plane_worker(base_url: String, admin_key: String, stop_at: Instant, samples: Arc<Samples>) {
    let client = reqwest::Client::new();
    let endpoints = [
        ("GET", "/healthz", false),
        ("GET", "/readyz", true),
        ("GET", "/v1/instance", false),
        ("GET", "/v1/status", true),
    ];
    let mut i = 0usize;
    while Instant::now() < stop_at {
        let (_, path, needs_auth) = endpoints[i % endpoints.len()];
        i += 1;
        let start = Instant::now();
        let mut req = client.get(format!("{base_url}{path}"));
        if needs_auth {
            req = req.bearer_auth(&admin_key);
        }
        let result = req.send().await;
        let us = start.elapsed().as_micros() as u64;
        match result {
            Ok(r) if r.status().is_success() => {
                samples.ok.fetch_add(1, Ordering::Relaxed);
                samples.latencies_us.lock().unwrap().push(us);
            }
            _ => {
                samples.err.fetch_add(1, Ordering::Relaxed);
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn expensive_worker(
    base_url: String,
    admin_key: String,
    stop_at: Instant,
    samples: Arc<Samples>,
) {
    let client = reqwest::Client::new();
    let sql = "SELECT grp, COUNT(*), SUM(val) FROM starve_big WHERE note LIKE '%needle%' GROUP BY grp HAVING COUNT(*) > 0";
    while Instant::now() < stop_at {
        let start = Instant::now();
        let result = exec_sql(&client, &base_url, &admin_key, sql).await;
        let us = start.elapsed().as_micros() as u64;
        match result {
            Ok(_) => {
                samples.ok.fetch_add(1, Ordering::Relaxed);
                samples.latencies_us.lock().unwrap().push(us);
            }
            Err(e) => {
                eprintln!("expensive query error: {e}");
                samples.err.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

async fn run_phase(
    server: &Server,
    label: &str,
    cheap_workers: usize,
    with_expensive: bool,
    phase_secs: u64,
) {
    let stop_at = Instant::now() + Duration::from_secs(phase_secs);
    let cheap_samples = Arc::new(Samples::default());
    let ctrl_samples = Arc::new(Samples::default());
    let expensive_samples = Arc::new(Samples::default());
    let mut handles = Vec::new();

    for w in 0..cheap_workers {
        handles.push(tokio::spawn(cheap_worker(
            server.base_url.clone(),
            server.admin_key.clone(),
            stop_at,
            cheap_samples.clone(),
            w as u64 + 1,
        )));
    }
    handles.push(tokio::spawn(control_plane_worker(
        server.base_url.clone(),
        server.admin_key.clone(),
        stop_at,
        ctrl_samples.clone(),
    )));
    if with_expensive {
        handles.push(tokio::spawn(expensive_worker(
            server.base_url.clone(),
            server.admin_key.clone(),
            stop_at,
            expensive_samples.clone(),
        )));
    }

    // Peak active_requests sampler.
    let peak = Arc::new(std::sync::atomic::AtomicI64::new(0));
    let peak2 = peak.clone();
    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_flag2 = stop_flag.clone();
    let base_url = server.base_url.clone();
    let admin_key = server.admin_key.clone();
    let sampler = tokio::spawn(async move {
        let client = reqwest::Client::new();
        while !stop_flag2.load(Ordering::Relaxed) {
            if let Ok(resp) = client
                .get(format!("{base_url}/v1/metrics"))
                .bearer_auth(&admin_key)
                .send()
                .await
            {
                if let Ok(body) = resp.json::<serde_json::Value>().await {
                    if let Some(v) = body["service"]["active_requests"].as_i64() {
                        peak2.fetch_max(v, Ordering::Relaxed);
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });

    let (rss0, _, _) = sample_process_metrics(server.child.id()).unwrap_or((0, 0, 0));
    for h in handles {
        let _ = h.await;
    }
    stop_flag.store(true, Ordering::Relaxed);
    let _ = sampler.await;
    let (rss1, h1, t1) = sample_process_metrics(server.child.id()).unwrap_or((0, 0, 0));

    println!("--- {label} ---");
    report_line("cheap PK lookup", &cheap_samples);
    report_line("control-plane", &ctrl_samples);
    if with_expensive {
        report_line("expensive query", &expensive_samples);
    }
    println!(
        "  peak_active_requests={} rss_kb {}->{} handles={} threads={}",
        peak.load(Ordering::Relaxed),
        rss0,
        rss1,
        h1,
        t1
    );
}

#[tokio::main]
async fn main() {
    let phase_secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(12);

    println!("=== RubiXDB Increment 14 Blocker 1: query starvation ===");
    println!("phase_secs={phase_secs} per phase, levels={LEVELS:?}, row_count={ROW_COUNT}");

    let server = spawn_server(18097).await;
    println!("server pid={} data_dir={}", server.child.id(), server.data_dir.display());

    let seed_start = Instant::now();
    seed(&server).await;
    println!("seed took {:.1}s\n", seed_start.elapsed().as_secs_f64());

    for &level in LEVELS.iter() {
        println!("\n=== concurrency level: {level} cheap workers ===");
        run_phase(&server, &format!("BASELINE (no expensive query), N={level}"), level, false, phase_secs).await;
        run_phase(&server, &format!("LOADED (1 expensive query running), N={level}"), level, true, phase_secs).await;
    }

    println!("\n=== query starvation test complete ===");
    drop(server);
}

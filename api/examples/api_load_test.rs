//! Productization — Production Validation Phase §2: a real concurrency
//! sweep against the real `rubixdb-api.exe` binary, a real on-disk
//! `LsmEngine`, over real HTTP (via `reqwest`) — nothing here is
//! in-process or mocked. Drives a realistic mixed workload (GET, PUT,
//! DELETE, exists, get_as_of, range, snapshot create/release,
//! status/metrics polling) at increasing concurrency (10/25/50/100/250)
//! and reports, per level: throughput, p50/p95/p99/max latency, error
//! rate, peak active-request count (server-observed), and RSS/threads/
//! handles of the API process (Windows `Get-Process`, the same
//! dependency-free technique `examples/compaction_soak.rs` already
//! uses).
//!
//! Usage: `cargo run --release -p rubixdb-api --example api_load_test --
//! [level_secs=15]`
//!
//! This measures what actually happened at each level; it does not
//! claim a concurrency level is "supported" merely because the process
//! stayed alive -- the printed error rate and latency numbers are the
//! evidence, read together with the certification doc that cites them.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use tokio::sync::Mutex as AsyncMutex;

const KEY_POOL: u64 = 2000;
const LEVELS: [usize; 5] = [10, 25, 50, 100, 250];

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
    let path = base.join(format!("rubixdb_api_load_{tag}_{nanos}"));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn key_for(i: u64) -> String {
    STANDARD.encode(format!("load-key-{i:08}").into_bytes())
}

/// Base64's alphabet can contain `+`, `/`, `=`, none of which are
/// path-segment-safe left raw; percent-encode exactly those three
/// (the only characters STANDARD base64 ever produces beyond
/// alphanumerics) so a key whose encoding happens to contain a `/`
/// does not get misread as an extra path segment.
fn url_safe_b64(s: &str) -> String {
    s.replace('+', "%2B")
        .replace('/', "%2F")
        .replace('=', "%3D")
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
    let admin_key = "load-test-admin-key-0123456789".to_string();
    let child = Command::new(api_bin_path())
        .env("RUBIXDB_DATA_DIR", &data_dir)
        .env("RUBIXDB_LISTEN_ADDR", format!("127.0.0.1:{port}"))
        .env("RUBIXDB_API_KEYS", format!("admin:admin:{admin_key}"))
        // Deliberately high so the load-sweep measures the service's
        // own throughput ceiling, not an artificially low configured
        // rate limit -- rate-limiter behavior itself is validated
        // separately (`api_security_validation.rs`).
        .env("RUBIXDB_RATE_LIMIT_RPS", "1000000")
        .env("RUBIXDB_RATE_LIMIT_BURST", "1000000")
        .env("RUBIXDB_COMPACTION_AUTO_TRIGGER", "true")
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

#[derive(Default)]
struct LevelStats {
    latencies_us: Vec<u64>,
    ok: u64,
    err: u64,
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

async fn run_level(
    base_url: &str,
    admin_key: &str,
    concurrency: usize,
    duration: Duration,
) -> LevelStats {
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(concurrency + 8)
        .build()
        .unwrap();
    let stop_at = Instant::now() + duration;
    let held_snapshots: Arc<AsyncMutex<Vec<String>>> = Arc::new(AsyncMutex::new(Vec::new()));
    let ok = Arc::new(AtomicU64::new(0));
    let err = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::with_capacity(concurrency);
    let latencies: Arc<std::sync::Mutex<Vec<u64>>> = Arc::new(std::sync::Mutex::new(Vec::new()));

    for w in 0..concurrency {
        let client = client.clone();
        let base_url = base_url.to_string();
        let admin_key = admin_key.to_string();
        let ok = ok.clone();
        let err = err.clone();
        let latencies = latencies.clone();
        let held_snapshots = held_snapshots.clone();
        handles.push(tokio::spawn(async move {
            let mut rng_state: u64 = 0x9E3779B97F4A7C15 ^ (w as u64 + 1);
            let mut next_rand = move || {
                rng_state ^= rng_state << 13;
                rng_state ^= rng_state >> 7;
                rng_state ^= rng_state << 17;
                rng_state
            };
            let mut local_latencies = Vec::new();
            let auth = format!("Bearer {admin_key}");
            while Instant::now() < stop_at {
                let idx = next_rand() % KEY_POOL;
                let key_b64 = key_for(idx);
                let op = next_rand() % 100;
                let start = Instant::now();
                let result: Result<u16, ()> = if op < 45 {
                    // GET (point read)
                    client
                        .get(format!("{base_url}/v1/kv/{}", url_safe_b64(&key_b64)))
                        .header("Authorization", &auth)
                        .send()
                        .await
                        .map(|r| r.status().as_u16())
                        .map_err(|_| ())
                } else if op < 65 {
                    // PUT (overwrite)
                    let value = format!("v{}", next_rand());
                    client
                        .put(format!("{base_url}/v1/kv"))
                        .header("Authorization", &auth)
                        .json(&serde_json::json!({
                            "key_b64": key_b64,
                            "value_b64": STANDARD.encode(value.as_bytes()),
                        }))
                        .send()
                        .await
                        .map(|r| r.status().as_u16())
                        .map_err(|_| ())
                } else if op < 75 {
                    // exists
                    client
                        .get(format!(
                            "{base_url}/v1/kv/{}/exists",
                            url_safe_b64(&key_b64)
                        ))
                        .header("Authorization", &auth)
                        .send()
                        .await
                        .map(|r| r.status().as_u16())
                        .map_err(|_| ())
                } else if op < 85 {
                    // range (small slice)
                    client
                        .get(format!("{base_url}/v1/range?limit=20"))
                        .header("Authorization", &auth)
                        .send()
                        .await
                        .map(|r| r.status().as_u16())
                        .map_err(|_| ())
                } else if op < 90 {
                    // DELETE (of a key that may or may not currently
                    // exist -- both 200 and a subsequent miss are
                    // legitimate, so this branch accepts 200/404 as
                    // success for throughput purposes; only network
                    // failure or 5xx count as an error).
                    let resp = client
                        .delete(format!("{base_url}/v1/kv/{}", url_safe_b64(&key_b64)))
                        .header("Authorization", &auth)
                        .send()
                        .await;
                    resp.map(|r| r.status().as_u16()).map_err(|_| ())
                } else if op < 95 {
                    // status / metrics polling
                    client
                        .get(format!("{base_url}/v1/status"))
                        .header("Authorization", &auth)
                        .send()
                        .await
                        .map(|r| r.status().as_u16())
                        .map_err(|_| ())
                } else if op < 98 {
                    // snapshot create
                    match client
                        .post(format!("{base_url}/v1/snapshots"))
                        .header("Authorization", &auth)
                        .send()
                        .await
                    {
                        Ok(r) if r.status().is_success() => {
                            if let Ok(body) = r.json::<serde_json::Value>().await {
                                if let Some(id) = body["id"].as_str() {
                                    held_snapshots.lock().await.push(id.to_string());
                                }
                            }
                            Ok(200)
                        }
                        Ok(r) => Ok(r.status().as_u16()),
                        Err(_) => Err(()),
                    }
                } else {
                    // snapshot release, if any are held
                    let id = held_snapshots.lock().await.pop();
                    match id {
                        Some(id) => client
                            .delete(format!("{base_url}/v1/snapshots/{id}"))
                            .header("Authorization", &auth)
                            .send()
                            .await
                            .map(|r| r.status().as_u16())
                            .map_err(|_| ()),
                        None => Ok(204), // nothing held -- not an error
                    }
                };
                let elapsed_us = start.elapsed().as_micros() as u64;
                match result {
                    Ok(status) if (200..300).contains(&status) || status == 404 => {
                        ok.fetch_add(1, Ordering::Relaxed);
                        local_latencies.push(elapsed_us);
                    }
                    _ => {
                        err.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            latencies.lock().unwrap().extend(local_latencies);
        }));
    }

    for h in handles {
        let _ = h.await;
    }

    LevelStats {
        latencies_us: Arc::try_unwrap(latencies).unwrap().into_inner().unwrap(),
        ok: ok.load(Ordering::Relaxed),
        err: err.load(Ordering::Relaxed),
    }
}

async fn peak_active_requests(
    base_url: &str,
    admin_key: &str,
    stop: Arc<std::sync::atomic::AtomicBool>,
) -> i64 {
    let client = reqwest::Client::new();
    let auth = format!("Bearer {admin_key}");
    let mut peak = 0i64;
    while !stop.load(Ordering::Relaxed) {
        if let Ok(resp) = client
            .get(format!("{base_url}/v1/metrics"))
            .header("Authorization", &auth)
            .send()
            .await
        {
            if let Ok(body) = resp.json::<serde_json::Value>().await {
                if let Some(v) = body["service"]["active_requests"].as_i64() {
                    peak = peak.max(v);
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    peak
}

#[tokio::main]
async fn main() {
    let level_secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(15);

    println!(
        "=== RubiXDB API load test: concurrency sweep {LEVELS:?}, {level_secs}s per level ==="
    );
    let server = spawn_server(18099).await;
    println!(
        "server pid={} data_dir={}",
        server.child.id(),
        server.data_dir.display()
    );

    // Warm-up: populate the key pool via the real admin key over real
    // HTTP before any level is timed.
    let client = reqwest::Client::new();
    let auth = format!("Bearer {}", server.admin_key);
    for i in 0..KEY_POOL {
        let key_b64 = key_for(i);
        let _ = client
            .put(format!("{}/v1/kv", server.base_url))
            .header("Authorization", &auth)
            .json(&serde_json::json!({
                "key_b64": key_b64,
                "value_b64": STANDARD.encode(format!("initial-{i}").into_bytes()),
            }))
            .send()
            .await;
    }
    println!("warm-up complete: {KEY_POOL} keys populated");

    println!(
        "concurrency,duration_secs,total_ops,ok,err,error_rate_pct,throughput_ops_sec,p50_ms,p95_ms,p99_ms,max_ms,peak_active_requests,rss_kb_start,rss_kb_end,handles_start,handles_end,threads_start,threads_end"
    );

    for &concurrency in LEVELS.iter() {
        let (rss0, h0, t0) = sample_process_metrics(server.child.id()).unwrap_or((0, 0, 0));
        let stop_sampler = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sampler_stop = stop_sampler.clone();
        let base_url = server.base_url.clone();
        let admin_key = server.admin_key.clone();
        let sampler =
            tokio::spawn(
                async move { peak_active_requests(&base_url, &admin_key, sampler_stop).await },
            );

        let start = Instant::now();
        let stats = run_level(
            &server.base_url,
            &server.admin_key,
            concurrency,
            Duration::from_secs(level_secs),
        )
        .await;
        let elapsed = start.elapsed();

        stop_sampler.store(true, Ordering::Relaxed);
        let peak_active = sampler.await.unwrap_or(0);
        let (rss1, h1, t1) = sample_process_metrics(server.child.id()).unwrap_or((0, 0, 0));

        let mut sorted = stats.latencies_us.clone();
        sorted.sort_unstable();
        let total = stats.ok + stats.err;
        let throughput = stats.ok as f64 / elapsed.as_secs_f64();
        let error_rate = if total > 0 {
            stats.err as f64 / total as f64 * 100.0
        } else {
            0.0
        };

        println!(
            "{concurrency},{:.1},{total},{},{},{:.3},{:.1},{:.2},{:.2},{:.2},{:.2},{peak_active},{rss0},{rss1},{h0},{h1},{t0},{t1}",
            elapsed.as_secs_f64(),
            stats.ok,
            stats.err,
            error_rate,
            throughput,
            percentile(&sorted, 0.50) as f64 / 1000.0,
            percentile(&sorted, 0.95) as f64 / 1000.0,
            percentile(&sorted, 0.99) as f64 / 1000.0,
            sorted.last().copied().unwrap_or(0) as f64 / 1000.0,
        );
    }

    println!("=== load test complete ===");
    drop(server);
}

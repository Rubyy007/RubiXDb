//! Increment 14, Blocker 7 — multi-instance simultaneous sustained
//! load. Two real `rubixdb gui --no-browser` processes, two real
//! independent instance directories/ports, real concurrent HTTP load
//! against both at once, real `Get-Process` resource sampling.
//!
//! Phase 1: Instance A read-heavy, Instance B write-heavy,
//! simultaneously. Phase 2: reversed. Verifies zero data/catalog/
//! transaction/lock/port crossover between the two, not just that both
//! stayed alive.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

const PHASE_SECS: u64 = 20;
const READERS: usize = 8;
const WRITERS: usize = 4;

fn fresh_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("rubixdb_multi_inst_{tag}_{nanos}"))
}

fn rubixdb_cmd(root: &Path, instance_name: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rubixdb"));
    cmd.env("RUBIXDB_INSTANCES_ROOT", root);
    cmd.env("RUBIXDB_INSTANCE_NAME", instance_name);
    cmd.env_remove("RUBIXDB_API_URL");
    cmd.env_remove("RUBIXDB_API_KEY");
    cmd
}

struct Instance {
    #[allow(dead_code)]
    name: &'static str,
    child: std::process::Child,
    base_url: String,
    admin_key: String,
}

fn read_manifest_and_credentials(root: &Path, name: &str) -> Option<(u16, String)> {
    let manifest_path = root.join(name).join("instance.json");
    let creds_path = root.join(name).join("credentials.json");
    if !manifest_path.is_file() || !creds_path.is_file() {
        return None;
    }
    let manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path).ok()?).ok()?;
    let creds: Value = serde_json::from_str(&std::fs::read_to_string(&creds_path).ok()?).ok()?;
    let port = manifest["api_port"].as_u64()? as u16;
    let admin_key = creds["admin_key"].as_str()?.to_string();
    Some((port, admin_key))
}

// `clippy::zombie_processes` cannot see that the spawned child is moved into
// the returned `Instance`, whose owner `kill()`s and `wait()`s it (see the
// end of the test); the only other path kills and waits explicitly below.
#[allow(clippy::zombie_processes)]
fn start_instance(root: &Path, name: &'static str) -> Instance {
    // `rubixdb gui` selects a named instance via the `--instance NAME`
    // flag (`cli/src/gui.rs`), not `RUBIXDB_INSTANCE_NAME` (that env
    // var is only read by the plain client role in `main.rs` --
    // verified by inspecting `gui.rs` after an initial version of this
    // test used the env var and silently got two processes racing for
    // the same "default" instance instead of two independent ones).
    let child = rubixdb_cmd(root, name)
        .arg("gui")
        .arg("--instance")
        .arg(name)
        .arg("--no-browser")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some((port, admin_key)) = read_manifest_and_credentials(root, name) {
            let client = reqwest::blocking::Client::builder()
                .timeout(Duration::from_millis(500))
                .build()
                .unwrap();
            if let Ok(resp) = client
                .get(format!("http://127.0.0.1:{port}/healthz"))
                .send()
            {
                if resp.status().is_success() {
                    return Instance {
                        name,
                        child,
                        base_url: format!("http://127.0.0.1:{port}"),
                        admin_key,
                    };
                }
            }
        }
        if Instant::now() >= deadline {
            let mut child = child;
            let _ = child.kill();
            let _ = child.wait();
            panic!("instance {name} never became ready");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap()
}

async fn exec_sql(
    client: &reqwest::Client,
    base_url: &str,
    admin_key: &str,
    sql: &str,
) -> Result<Value, String> {
    let resp = client
        .post(format!("{base_url}/v1/sql"))
        .bearer_auth(admin_key)
        .json(&json!({ "sql": sql, "params": [] }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(format!("HTTP {status}: {body}"));
    }
    Ok(body)
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

struct PhaseStats {
    read_ok: AtomicU64,
    read_err: AtomicU64,
    write_ok: AtomicU64,
    write_err: AtomicU64,
    read_latency_us: std::sync::Mutex<Vec<u64>>,
}
impl Default for PhaseStats {
    fn default() -> Self {
        PhaseStats {
            read_ok: AtomicU64::new(0),
            read_err: AtomicU64::new(0),
            write_ok: AtomicU64::new(0),
            write_err: AtomicU64::new(0),
            read_latency_us: std::sync::Mutex::new(Vec::new()),
        }
    }
}

async fn run_phase(
    read_instance: (&str, &str, &str),
    write_instance: (&str, &str, &str),
    write_table: &str,
    write_id_start: i64,
    duration: Duration,
) -> (Arc<PhaseStats>, Arc<PhaseStats>, i64) {
    let (r_label, r_url, r_key) = read_instance;
    let (w_label, w_url, w_key) = write_instance;
    let read_stats = Arc::new(PhaseStats::default());
    let write_stats = Arc::new(PhaseStats::default());
    let stop_at = Instant::now() + duration;
    let mut handles = Vec::new();

    for _ in 0..READERS {
        let stats = read_stats.clone();
        let url = r_url.to_string();
        let key = r_key.to_string();
        let label = r_label.to_string();
        handles.push(tokio::spawn(async move {
            let client = http_client();
            while Instant::now() < stop_at {
                let start = Instant::now();
                match exec_sql(
                    &client,
                    &url,
                    &key,
                    &format!("SELECT COUNT(*) FROM {label}_t"),
                )
                .await
                {
                    Ok(_) => {
                        stats.read_ok.fetch_add(1, Ordering::Relaxed);
                        stats
                            .read_latency_us
                            .lock()
                            .unwrap()
                            .push(start.elapsed().as_micros() as u64);
                    }
                    Err(_) => {
                        stats.read_err.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }));
    }

    let write_id_counter = Arc::new(AtomicU64::new(write_id_start as u64));
    for _ in 0..WRITERS {
        let stats = write_stats.clone();
        let url = w_url.to_string();
        let key = w_key.to_string();
        let table = write_table.to_string();
        let counter = write_id_counter.clone();
        handles.push(tokio::spawn(async move {
            let client = http_client();
            while Instant::now() < stop_at {
                let id = counter.fetch_add(1, Ordering::Relaxed);
                match exec_sql(
                    &client,
                    &url,
                    &key,
                    &format!("INSERT INTO {table} (id, v) VALUES ({id}, 'w')"),
                )
                .await
                {
                    Ok(_) => {
                        stats.write_ok.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) => {
                        stats.write_err.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }));
    }

    for h in handles {
        let _ = h.await;
    }
    let final_id = write_id_counter.load(Ordering::Relaxed) as i64;
    let _ = w_label;
    (read_stats, write_stats, final_id)
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn two_instances_simultaneous_sustained_read_and_write_load_never_cross_contaminate() {
    let root = fresh_root("main");
    let mut a = start_instance(&root, "instA");
    let mut b = start_instance(&root, "instB");
    assert_ne!(
        a.base_url, b.base_url,
        "the two instances must be on independent ports, never colliding"
    );
    println!("instance A: {} pid={}", a.base_url, a.child.id());
    println!("instance B: {} pid={}", b.base_url, b.child.id());

    let client = http_client();
    exec_sql(
        &client,
        &a.base_url,
        &a.admin_key,
        "CREATE TABLE instA_t (id INTEGER PRIMARY KEY, v TEXT)",
    )
    .await
    .unwrap();
    exec_sql(
        &client,
        &b.base_url,
        &b.admin_key,
        "CREATE TABLE instB_t (id INTEGER PRIMARY KEY, v TEXT)",
    )
    .await
    .unwrap();
    // Seed a few rows so read-heavy workers have something real to
    // read from the start.
    for i in 0..20 {
        exec_sql(
            &client,
            &a.base_url,
            &a.admin_key,
            &format!("INSERT INTO instA_t (id, v) VALUES ({i}, 'seed')"),
        )
        .await
        .unwrap();
        exec_sql(
            &client,
            &b.base_url,
            &b.admin_key,
            &format!("INSERT INTO instB_t (id, v) VALUES ({i}, 'seed')"),
        )
        .await
        .unwrap();
    }

    let (rss_a0, h_a0, t_a0) = sample_process_metrics(a.child.id()).unwrap_or((0, 0, 0));
    let (rss_b0, h_b0, t_b0) = sample_process_metrics(b.child.id()).unwrap_or((0, 0, 0));

    // Phase 1: A read-heavy, B write-heavy.
    println!("=== phase 1: A=read-heavy, B=write-heavy, {PHASE_SECS}s ===");
    let (a_read1, a_write1, _) = run_phase(
        ("instA", &a.base_url, &a.admin_key),
        ("instB", &b.base_url, &b.admin_key),
        "instB_t",
        1000,
        Duration::from_secs(PHASE_SECS),
    )
    .await;

    let (rss_a1, h_a1, t_a1) = sample_process_metrics(a.child.id()).unwrap_or((0, 0, 0));
    let (rss_b1, h_b1, t_b1) = sample_process_metrics(b.child.id()).unwrap_or((0, 0, 0));

    // Phase 2: reversed -- A write-heavy, B read-heavy.
    println!("=== phase 2: A=write-heavy, B=read-heavy, {PHASE_SECS}s ===");
    let (b_read2, b_write2, _) = run_phase(
        ("instB", &b.base_url, &b.admin_key),
        ("instA", &a.base_url, &a.admin_key),
        "instA_t",
        2000,
        Duration::from_secs(PHASE_SECS),
    )
    .await;

    let (rss_a2, h_a2, t_a2) = sample_process_metrics(a.child.id()).unwrap_or((0, 0, 0));
    let (rss_b2, h_b2, t_b2) = sample_process_metrics(b.child.id()).unwrap_or((0, 0, 0));

    let report = |label: &str, read: &PhaseStats, write: &PhaseStats| {
        let mut lat = read.read_latency_us.lock().unwrap().clone();
        lat.sort_unstable();
        println!(
            "{label}: reads ok={} err={} p50={:.2}ms p99={:.2}ms | writes ok={} err={}",
            read.read_ok.load(Ordering::Relaxed),
            read.read_err.load(Ordering::Relaxed),
            percentile(&lat, 0.5) as f64 / 1000.0,
            percentile(&lat, 0.99) as f64 / 1000.0,
            write.write_ok.load(Ordering::Relaxed),
            write.write_err.load(Ordering::Relaxed),
        );
    };
    report("phase1 (A read / B write)", &a_read1, &a_write1);
    report("phase2 (B read / A write)", &b_read2, &b_write2);
    println!("resource A: rss {rss_a0}->{rss_a1}->{rss_a2} kb, handles {h_a0}->{h_a1}->{h_a2}, threads {t_a0}->{t_a1}->{t_a2}");
    println!("resource B: rss {rss_b0}->{rss_b1}->{rss_b2} kb, handles {h_b0}->{h_b1}->{h_b2}, threads {t_b0}->{t_b1}->{t_b2}");

    assert_eq!(
        a_read1.read_err.load(Ordering::Relaxed),
        0,
        "instance A reads must have zero errors under simultaneous cross-instance load"
    );
    assert_eq!(
        a_write1.write_err.load(Ordering::Relaxed),
        0,
        "instance B writes must have zero errors in phase 1"
    );
    assert_eq!(
        b_read2.read_err.load(Ordering::Relaxed),
        0,
        "instance B reads must have zero errors in phase 2"
    );
    assert_eq!(
        b_write2.write_err.load(Ordering::Relaxed),
        0,
        "instance A writes must have zero errors in phase 2"
    );

    // --- Crossover verification ---

    // Port confusion: still independent.
    assert_ne!(a.base_url, b.base_url);

    // Catalog/data crossover: instance A must never see instance B's
    // table, and vice versa.
    let a_tables = exec_sql(
        &client,
        &a.base_url,
        &a.admin_key,
        "SELECT COUNT(*) FROM instA_t",
    )
    .await;
    assert!(a_tables.is_ok(), "instance A must see its own table");
    let a_sees_b = exec_sql(
        &client,
        &a.base_url,
        &a.admin_key,
        "SELECT COUNT(*) FROM instB_t",
    )
    .await;
    assert!(
        a_sees_b.is_err(),
        "instance A must NOT see instance B's table -- zero catalog crossover"
    );
    let b_sees_a = exec_sql(
        &client,
        &b.base_url,
        &b.admin_key,
        "SELECT COUNT(*) FROM instA_t",
    )
    .await;
    assert!(
        b_sees_a.is_err(),
        "instance B must NOT see instance A's table -- zero catalog crossover"
    );

    // Lock/credential crossover: instance A's admin key must not
    // authenticate against instance B's server.
    let cross_auth = http_client()
        .post(format!("{}/v1/sql", b.base_url))
        .bearer_auth(&a.admin_key)
        .json(&json!({ "sql": "SELECT 1", "params": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        cross_auth.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "instance A's credential must never authenticate against instance B's server"
    );

    // Transaction crossover: begin a transaction on A, confirm B has
    // no knowledge of that session_id at all.
    let begin_a = exec_sql(&client, &a.base_url, &a.admin_key, "BEGIN")
        .await
        .unwrap();
    let session_id = begin_a["session_id"].as_str().unwrap().to_string();
    let commit_attempt_on_b = http_client()
        .post(format!("{}/v1/sql", b.base_url))
        .bearer_auth(&b.admin_key)
        .json(&json!({ "sql": "COMMIT", "params": [], "session_id": session_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        commit_attempt_on_b.status(),
        reqwest::StatusCode::NOT_FOUND,
        "a session_id opened on instance A must be completely unknown to instance B -- zero transaction crossover"
    );
    // No further cleanup of the still-open session on A needed --
    // both processes are killed immediately below, and their entire
    // instance directories removed with them.

    kill(&mut a.child);
    kill(&mut b.child);
    std::fs::remove_dir_all(&root).ok();
}

fn kill(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

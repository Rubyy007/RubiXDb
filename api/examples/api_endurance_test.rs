//! Productization — Production Validation Phase §3: a real, long-duration
//! endurance soak against the real `rubixdb-api.exe` binary over real
//! HTTP, with every read verified against an **independently tracked
//! reference model** (never the engine's own result) — the same
//! discipline `examples/compaction_soak.rs` established for the engine
//! itself, extended one layer up through the real HTTP/auth/JSON
//! boundary this time.
//!
//! Correctness design (stated precisely, mirroring `compaction_soak.rs`'s
//! own §-comment so a reader can judge exactly what is and is not
//! proven): each of `KEY_CARDINALITY` keys owns a small, bounded ring
//! buffer (`HISTORY_DEPTH` entries) of its own most recent `(seq,
//! Option<value>)` versions, updated by writers under a per-key lock
//! immediately after a `PUT`/`DELETE` HTTP call returns its assigned
//! `seq`. A point-read check captures a key's *current* `(seq, value)`
//! entry from the model under that lock, then issues a real
//! `GET /v1/kv/{key}?as_of_seq={seq}` at that exact seq -- race-free by
//! construction, since the seq and the expected value were captured
//! together, not sampled from two different points in time. A
//! range/snapshot check pins a real held snapshot (`POST
//! /v1/snapshots`), then answers each probed key from its ring buffer's
//! highest entry with `seq <= snapshot.seq`; if the buffer's oldest
//! entry is already newer than the snapshot, that key is skipped for
//! this round (`skipped_buffer_gap`) rather than silently trusted or
//! falsely failed.
//!
//! Usage: `cargo run --release -p rubixdb-api --example
//! api_endurance_test -- <duration_secs> [writers=4] [readers=8]
//! [sample_interval_secs=60]`

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;

const KEY_CARDINALITY: u64 = 5_000;
const HISTORY_DEPTH: usize = 8;

fn api_bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("target")
        .join("release")
        .join("rubixdb-api.exe")
}

fn base_dir() -> PathBuf {
    std::env::var_os("RUBIXDB_SOAK_BASE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = base_dir().join(format!("rubixdb_api_endurance_{tag}_{nanos}"));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn key_bytes(i: u64) -> Vec<u8> {
    format!("end-k{i:08}").into_bytes()
}

fn key_b64(i: u64) -> String {
    STANDARD.encode(key_bytes(i))
}

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

fn dir_size_bytes(path: &std::path::Path) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                total += dir_size_bytes(&p);
            } else if let Ok(meta) = entry.metadata() {
                total += meta.len();
            }
        }
    }
    total
}

#[derive(Clone, Debug)]
struct Version {
    seq: u64,
    value: Option<Vec<u8>>,
}

struct KeyModel {
    history: Mutex<VecDeque<Version>>,
}

impl KeyModel {
    fn new() -> Self {
        KeyModel {
            history: Mutex::new(VecDeque::new()),
        }
    }
    /// Inserts in seq-sorted position, not blind `push_back` --
    /// multiple writer tasks can race on the *same* key (`PUT`
    /// assigning seq N then a different writer's `PUT` assigning
    /// N+1) and then call `record()` in the opposite order if the
    /// lower-seq writer's HTTP response is slower to arrive. A real
    /// race this harness must tolerate, not assume away, since
    /// `at_or_before`/`latest` both depend on the buffer staying
    /// seq-ordered (the exact discipline `examples/compaction_soak.rs`'s
    /// own `RefModel::record` already established for the in-process
    /// case; this is the same fix adapted for independent HTTP
    /// round trips).
    fn record(&self, v: Version, last_recorded_seq: &AtomicU64) {
        let mut h = self.history.lock().unwrap();
        let pos = h
            .iter()
            .rposition(|existing| existing.seq <= v.seq)
            .map(|p| p + 1)
            .unwrap_or(0);
        let seq = v.seq;
        h.insert(pos, v);
        while h.len() > HISTORY_DEPTH {
            h.pop_front();
        }
        drop(h);
        last_recorded_seq.fetch_max(seq, Ordering::Release);
    }
    fn latest(&self) -> Option<Version> {
        self.history.lock().unwrap().back().cloned()
    }
    /// Highest entry with `seq <= as_of`, or `None` with `gap=true` if
    /// the buffer's oldest entry is already newer than `as_of`.
    fn at_or_before(&self, as_of: u64) -> (Option<Version>, bool) {
        let h = self.history.lock().unwrap();
        if let Some(oldest) = h.front() {
            if oldest.seq > as_of {
                return (None, true);
            }
        } else {
            return (None, false); // never written -- correctly absent
        }
        let found = h.iter().rev().find(|v| v.seq <= as_of).cloned();
        (found, false)
    }
}

struct Server {
    child: Child,
    base_url: String,
    admin_key: String,
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
    let admin_key = "endurance-admin-key-01234567890".to_string();
    let child = Command::new(api_bin_path())
        .env("RUBIXDB_DATA_DIR", &data_dir)
        .env("RUBIXDB_LISTEN_ADDR", format!("127.0.0.1:{port}"))
        .env("RUBIXDB_API_KEYS", format!("admin:admin:{admin_key}"))
        // Realistic, not artificially raised -- unlike the load-sweep
        // harness, this run deliberately keeps the *default-scale*
        // rate limit so any 429s the mixed workload produces are real,
        // observed, and tracked (§3's own explicit instruction), not
        // engineered away.
        .env("RUBIXDB_RATE_LIMIT_RPS", "500")
        .env("RUBIXDB_RATE_LIMIT_BURST", "1000")
        .env("RUBIXDB_COMPACTION_AUTO_TRIGGER", "true")
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn rubixdb-api.exe -- build it first with --release");

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

struct Counters {
    writes: AtomicU64,
    deletes: AtomicU64,
    point_checks: AtomicU64,
    point_mismatches: AtomicU64,
    range_checks: AtomicU64,
    range_probe_mismatches: AtomicU64,
    range_probe_skipped_gap: AtomicU64,
    http_errors: AtomicU64,
    timeouts: AtomicU64,
    rate_limited_429: AtomicU64,
}

impl Counters {
    fn new() -> Self {
        Counters {
            writes: AtomicU64::new(0),
            deletes: AtomicU64::new(0),
            point_checks: AtomicU64::new(0),
            point_mismatches: AtomicU64::new(0),
            range_checks: AtomicU64::new(0),
            range_probe_mismatches: AtomicU64::new(0),
            range_probe_skipped_gap: AtomicU64::new(0),
            http_errors: AtomicU64::new(0),
            timeouts: AtomicU64::new(0),
            rate_limited_429: AtomicU64::new(0),
        }
    }
}

fn log_line(log: &Mutex<std::fs::File>, line: &str) {
    use std::io::Write;
    let mut f = log.lock().unwrap();
    let _ = writeln!(f, "{line}");
    let _ = f.flush();
    println!("{line}");
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let duration_secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(7200);
    let writer_count: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(4);
    let reader_count: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(8);
    let sample_interval_secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(60);

    let log_path = base_dir().join("rubixdb_api_endurance_log.csv");
    let log_file = Mutex::new(std::fs::File::create(&log_path).expect("create log file"));
    log_line(
        &log_file,
        &format!(
            "# RubiXDB API endurance soak: duration={duration_secs}s writers={writer_count} readers={reader_count} sample_interval={sample_interval_secs}s log={}",
            log_path.display()
        ),
    );

    let server = spawn_server(18199).await;
    log_line(
        &log_file,
        &format!(
            "# server pid={} data_dir={}",
            server.child.id(),
            server.data_dir.display()
        ),
    );

    let models: Arc<Vec<KeyModel>> =
        Arc::new((0..KEY_CARDINALITY).map(|_| KeyModel::new()).collect());
    let last_recorded_seq = Arc::new(AtomicU64::new(0));
    let counters = Arc::new(Counters::new());
    let stop = Arc::new(AtomicBool::new(false));
    let base_url = server.base_url.clone();
    let admin_key = server.admin_key.clone();

    // Warm-up: every key starts with an initial value, so readers
    // always have something real to check from t=0. Retries through
    // 429s (the warm-up's own burst easily exceeds the realistic
    // steady-state rate limit the soak intentionally runs under) so
    // every key is genuinely seeded rather than silently skipped.
    {
        let client = reqwest::Client::new();
        let auth = format!("Bearer {admin_key}");
        for i in 0..KEY_CARDINALITY {
            let value = format!("init-{i}").into_bytes();
            for attempt in 0..40 {
                let resp = client
                    .put(format!("{base_url}/v1/kv"))
                    .header("Authorization", &auth)
                    .json(&serde_json::json!({
                        "key_b64": key_b64(i),
                        "value_b64": STANDARD.encode(&value),
                    }))
                    .send()
                    .await;
                match resp {
                    Ok(r) if r.status() == 429 => {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                    Ok(r) if r.status().is_success() => {
                        if let Ok(body) = r.json::<serde_json::Value>().await {
                            if let Some(seq) = body["seq"].as_u64() {
                                models[i as usize].record(
                                    Version {
                                        seq,
                                        value: Some(value.clone()),
                                    },
                                    &last_recorded_seq,
                                );
                            }
                        }
                        break;
                    }
                    _ => {
                        if attempt == 39 {
                            eprintln!("warm-up: giving up on key {i} after 40 attempts");
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
        }
    }
    log_line(
        &log_file,
        &format!("# warm-up complete: {KEY_CARDINALITY} keys"),
    );

    let start = Instant::now();
    let deadline = start + Duration::from_secs(duration_secs);
    let mut handles = Vec::new();

    for w in 0..writer_count {
        let base_url = base_url.clone();
        let admin_key = admin_key.clone();
        let models = models.clone();
        let last_recorded_seq = last_recorded_seq.clone();
        let counters = counters.clone();
        let stop = stop.clone();
        handles.push(tokio::spawn(async move {
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap();
            let auth = format!("Bearer {admin_key}");
            let mut rng_state: u64 = 0x9E3779B97F4A7C15 ^ (w as u64 + 1);
            let mut next_rand = move || {
                rng_state ^= rng_state << 13;
                rng_state ^= rng_state >> 7;
                rng_state ^= rng_state << 17;
                rng_state
            };
            while !stop.load(Ordering::Relaxed) {
                let idx = next_rand() % KEY_CARDINALITY;
                let is_delete = next_rand() % 10 == 0; // ~10% deletes
                if is_delete {
                    match client
                        .delete(format!("{base_url}/v1/kv/{}", url_safe_b64(&key_b64(idx))))
                        .header("Authorization", &auth)
                        .send()
                        .await
                    {
                        Ok(resp) if resp.status() == 429 => {
                            counters.rate_limited_429.fetch_add(1, Ordering::Relaxed);
                        }
                        Ok(resp) if resp.status().is_success() => {
                            if let Ok(body) = resp.json::<serde_json::Value>().await {
                                if let Some(seq) = body["seq"].as_u64() {
                                    models[idx as usize]
                                        .record(Version { seq, value: None }, &last_recorded_seq);
                                    counters.deletes.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                        Ok(_) => {
                            counters.http_errors.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) if e.is_timeout() => {
                            counters.timeouts.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(_) => {
                            counters.http_errors.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                } else {
                    let value = format!("v-{}-{}", w, next_rand()).into_bytes();
                    match client
                        .put(format!("{base_url}/v1/kv"))
                        .header("Authorization", &auth)
                        .json(&serde_json::json!({
                            "key_b64": key_b64(idx),
                            "value_b64": STANDARD.encode(&value),
                        }))
                        .send()
                        .await
                    {
                        Ok(resp) if resp.status() == 429 => {
                            counters.rate_limited_429.fetch_add(1, Ordering::Relaxed);
                        }
                        Ok(resp) if resp.status().is_success() => {
                            if let Ok(body) = resp.json::<serde_json::Value>().await {
                                if let Some(seq) = body["seq"].as_u64() {
                                    models[idx as usize].record(
                                        Version {
                                            seq,
                                            value: Some(value),
                                        },
                                        &last_recorded_seq,
                                    );
                                    counters.writes.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                        Ok(_) => {
                            counters.http_errors.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) if e.is_timeout() => {
                            counters.timeouts.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(_) => {
                            counters.http_errors.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                // Realistic per-writer pacing (~6-12 ops/sec/writer),
                // not a max-speed hammer -- that is the load-sweep
                // harness's job (`api_load_test.rs`). This keeps the
                // soak's aggregate rate under the realistic, default-
                // scale rate limit it intentionally runs with, so 429s
                // stay a real but non-dominant, tracked occurrence
                // rather than swamping every other counter.
                tokio::time::sleep(Duration::from_millis(80 + (next_rand() % 80))).await;
            }
        }));
    }

    for r in 0..reader_count {
        let base_url = base_url.clone();
        let admin_key = admin_key.clone();
        let models = models.clone();
        let last_recorded_seq = last_recorded_seq.clone();
        let counters = counters.clone();
        let stop = stop.clone();
        handles.push(tokio::spawn(async move {
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap();
            let auth = format!("Bearer {admin_key}");
            let mut rng_state: u64 = 0xC2B2AE3D27D4EB4F ^ (r as u64 + 1);
            let mut next_rand = move || {
                rng_state ^= rng_state << 13;
                rng_state ^= rng_state >> 7;
                rng_state ^= rng_state << 17;
                rng_state
            };
            let mut snapshot_countdown: u64 = 50;
            while !stop.load(Ordering::Relaxed) {
                if snapshot_countdown == 0 {
                    snapshot_countdown = 50;
                    // Range/snapshot check.
                    if let Ok(resp) = client
                        .post(format!("{base_url}/v1/snapshots"))
                        .header("Authorization", &auth)
                        .send()
                        .await
                    {
                        if let Ok(body) = resp.json::<serde_json::Value>().await {
                            if let (Some(id), Some(snap_seq)) =
                                (body["id"].as_str(), body["seq"].as_u64())
                            {
                                // `snap_seq` comes from the engine's own
                                // global counter, independent of any
                                // specific writer task's client-side
                                // model `record()` call -- a write whose
                                // HTTP response (seq <= snap_seq)
                                // already arrived server-side may not
                                // yet have run its own local `record()`
                                // by the time this snapshot call
                                // returns. Race-free fix (not a settle
                                // delay): pin the probe at
                                // `min(snap_seq, last_recorded_seq)` --
                                // `last_recorded_seq` is only ever
                                // advanced *after* a `record()` call
                                // completes (`fetch_max`, `Release`), so
                                // by construction the model has already
                                // caught up to any seq <= its current
                                // value. The real held snapshot (pinned
                                // at `snap_seq` >= this probe seq) still
                                // guarantees on-disk retention for
                                // whatever version the probe needs.
                                let probe_seq =
                                    snap_seq.min(last_recorded_seq.load(Ordering::Acquire));
                                for _ in 0..20 {
                                    let idx = next_rand() % KEY_CARDINALITY;
                                    let (expected, gap) =
                                        models[idx as usize].at_or_before(probe_seq);
                                    if gap {
                                        counters
                                            .range_probe_skipped_gap
                                            .fetch_add(1, Ordering::Relaxed);
                                        continue;
                                    }
                                    counters.range_checks.fetch_add(1, Ordering::Relaxed);
                                    let resp = client
                                        .get(format!(
                                            "{base_url}/v1/kv/{}?as_of_seq={probe_seq}",
                                            url_safe_b64(&key_b64(idx))
                                        ))
                                        .header("Authorization", &auth)
                                        .send()
                                        .await;
                                    // Flatten to `Option<Vec<u8>>`: both
                                    // "never written" (outer `None`) and
                                    // "written then deleted" (`Some(v)`
                                    // with `v.value == None`) mean the
                                    // same thing to the API -- expect
                                    // 404 -- and must be handled by the
                                    // same arm below (the bug this
                                    // replaced: matching the outer
                                    // `Option<Version>` directly let a
                                    // correct 404-after-delete fall
                                    // through to the catch-all mismatch
                                    // arm).
                                    let expected_value: Option<Vec<u8>> =
                                        expected.and_then(|v| v.value);
                                    match (resp, &expected_value) {
                                        (Ok(r), None) if r.status() == 404 => {}
                                        (Ok(r), Some(v)) if r.status().is_success() => {
                                            if let Ok(body) = r.json::<serde_json::Value>().await {
                                                let got = body["value_b64"].as_str().unwrap_or("");
                                                if got != STANDARD.encode(v) {
                                                    counters
                                                        .range_probe_mismatches
                                                        .fetch_add(1, Ordering::Relaxed);
                                                }
                                            }
                                        }
                                        (Ok(r), _) if r.status() == 429 => {
                                            counters
                                                .rate_limited_429
                                                .fetch_add(1, Ordering::Relaxed);
                                        }
                                        _ => {
                                            counters
                                                .range_probe_mismatches
                                                .fetch_add(1, Ordering::Relaxed);
                                        }
                                    }
                                }
                                let _ = client
                                    .delete(format!("{base_url}/v1/snapshots/{id}"))
                                    .header("Authorization", &auth)
                                    .send()
                                    .await;
                            }
                        }
                    }
                    continue;
                }
                snapshot_countdown -= 1;

                let idx = next_rand() % KEY_CARDINALITY;
                let latest = models[idx as usize].latest();
                let Some(expected) = latest else { continue };
                counters.point_checks.fetch_add(1, Ordering::Relaxed);
                let resp = client
                    .get(format!(
                        "{base_url}/v1/kv/{}?as_of_seq={}",
                        url_safe_b64(&key_b64(idx)),
                        expected.seq
                    ))
                    .header("Authorization", &auth)
                    .send()
                    .await;
                match (resp, &expected.value) {
                    (Ok(r), None) if r.status() == 404 => {}
                    (Ok(r), Some(v)) if r.status().is_success() => {
                        if let Ok(body) = r.json::<serde_json::Value>().await {
                            let got = body["value_b64"].as_str().unwrap_or("");
                            if got != STANDARD.encode(v) {
                                counters.point_mismatches.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    (Ok(r), _) if r.status() == 429 => {
                        counters.rate_limited_429.fetch_add(1, Ordering::Relaxed);
                    }
                    (Ok(_), _) => {
                        // Any other outcome (wrong status for the
                        // expected outcome) is a genuine mismatch.
                        counters.point_mismatches.fetch_add(1, Ordering::Relaxed);
                    }
                    (Err(e), _) if e.is_timeout() => {
                        counters.timeouts.fetch_add(1, Ordering::Relaxed);
                    }
                    (Err(_), _) => {
                        counters.http_errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
                // Realistic per-reader pacing (~20-50 ops/sec/reader) --
                // see the writer loop's identical rationale above.
                tokio::time::sleep(Duration::from_millis(20 + (next_rand() % 30))).await;
            }
        }));
    }

    // Sampler: periodic progress line to the log/stdout.
    {
        let base_url = base_url.clone();
        let admin_key = admin_key.clone();
        let counters = counters.clone();
        let stop = stop.clone();
        let pid = server.child.id();
        let data_dir = server.data_dir.clone();
        handles.push(tokio::spawn(async move {
            let client = reqwest::Client::new();
            let auth = format!("Bearer {admin_key}");
            log_line(
                &log_file,
                "elapsed_secs,writes,deletes,point_checks,point_mismatches,range_checks,range_mismatches,range_skipped_gap,http_errors,timeouts,rate_limited_429,rss_kb,handles,threads,db_size_bytes,storage_state",
            );
            while !stop.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_secs(sample_interval_secs)).await;
                let (rss, h, t) = sample_process_metrics(pid).unwrap_or((0, 0, 0));
                let db_size = dir_size_bytes(&data_dir);
                let storage_state = client
                    .get(format!("{base_url}/v1/status"))
                    .header("Authorization", &auth)
                    .send()
                    .await
                    .ok();
                let storage_state = match storage_state {
                    Some(r) => match r.json::<serde_json::Value>().await {
                        Ok(b) => b["storage_state"].as_str().unwrap_or("?").to_string(),
                        Err(_) => "?".to_string(),
                    },
                    None => "?".to_string(),
                };
                log_line(
                    &log_file,
                    &format!(
                        "{:.0},{},{},{},{},{},{},{},{},{},{},{rss},{h},{t},{db_size},{storage_state}",
                        start.elapsed().as_secs_f64(),
                        counters.writes.load(Ordering::Relaxed),
                        counters.deletes.load(Ordering::Relaxed),
                        counters.point_checks.load(Ordering::Relaxed),
                        counters.point_mismatches.load(Ordering::Relaxed),
                        counters.range_checks.load(Ordering::Relaxed),
                        counters.range_probe_mismatches.load(Ordering::Relaxed),
                        counters.range_probe_skipped_gap.load(Ordering::Relaxed),
                        counters.http_errors.load(Ordering::Relaxed),
                        counters.timeouts.load(Ordering::Relaxed),
                        counters.rate_limited_429.load(Ordering::Relaxed),
                    ),
                );
            }
        }));
    }

    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    stop.store(true, Ordering::Relaxed);
    for h in handles {
        let _ = tokio::time::timeout(Duration::from_secs(15), h).await;
    }

    let log_file = Mutex::new(
        std::fs::OpenOptions::new()
            .append(true)
            .open(&log_path)
            .unwrap(),
    );
    log_line(
        &log_file,
        &format!(
            "# FINAL after {:.0}s: writes={} deletes={} point_checks={} point_mismatches={} range_checks={} range_mismatches={} range_skipped_gap={} http_errors={} timeouts={} rate_limited_429={}",
            start.elapsed().as_secs_f64(),
            counters.writes.load(Ordering::Relaxed),
            counters.deletes.load(Ordering::Relaxed),
            counters.point_checks.load(Ordering::Relaxed),
            counters.point_mismatches.load(Ordering::Relaxed),
            counters.range_checks.load(Ordering::Relaxed),
            counters.range_probe_mismatches.load(Ordering::Relaxed),
            counters.range_probe_skipped_gap.load(Ordering::Relaxed),
            counters.http_errors.load(Ordering::Relaxed),
            counters.timeouts.load(Ordering::Relaxed),
            counters.rate_limited_429.load(Ordering::Relaxed),
        ),
    );

    // Restart the real process against the same data dir and verify a
    // sample of keys survive, before final teardown -- a cheap extra
    // real-restart check layered on top of the dedicated
    // `api_process_restart_test.rs` harness.
    let data_dir = server.data_dir.clone();
    drop(server);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let restarted = spawn_server_at(18198, &data_dir, &admin_key).await;
    let client = reqwest::Client::new();
    let auth = format!("Bearer {admin_key}");
    let mut post_restart_mismatches = 0u64;
    let mut post_restart_checked = 0u64;
    for i in (0..KEY_CARDINALITY).step_by(37) {
        if let Some(expected) = models[i as usize].latest() {
            post_restart_checked += 1;
            let resp = client
                .get(format!(
                    "{}/v1/kv/{}?as_of_seq={}",
                    restarted.base_url,
                    url_safe_b64(&key_b64(i)),
                    expected.seq
                ))
                .header("Authorization", &auth)
                .send()
                .await;
            let ok = match (resp, &expected.value) {
                (Ok(r), None) => r.status() == 404,
                (Ok(r), Some(v)) if r.status().is_success() => {
                    match r.json::<serde_json::Value>().await {
                        Ok(body) => body["value_b64"].as_str().unwrap_or("") == STANDARD.encode(v),
                        Err(_) => false,
                    }
                }
                _ => false,
            };
            if !ok {
                post_restart_mismatches += 1;
            }
        }
    }
    log_line(
        &log_file,
        &format!(
            "# POST-RESTART verification: checked={post_restart_checked} mismatches={post_restart_mismatches}"
        ),
    );
    drop(restarted);

    println!(
        "\n=== endurance soak complete -- log at {} ===",
        log_path.display()
    );
}

async fn spawn_server_at(port: u16, data_dir: &std::path::Path, admin_key: &str) -> Server {
    let child = Command::new(api_bin_path())
        .env("RUBIXDB_DATA_DIR", data_dir)
        .env("RUBIXDB_LISTEN_ADDR", format!("127.0.0.1:{port}"))
        .env("RUBIXDB_API_KEYS", format!("admin:admin:{admin_key}"))
        .env("RUBIXDB_RATE_LIMIT_RPS", "1000000")
        .env("RUBIXDB_RATE_LIMIT_BURST", "1000000")
        .env("RUBIXDB_COMPACTION_AUTO_TRIGGER", "true")
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn rubixdb-api.exe for post-restart check");
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
            panic!("restarted rubixdb-api did not become healthy within 30s");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Server {
        child,
        base_url,
        admin_key: admin_key.to_string(),
        data_dir: data_dir.to_path_buf(),
    }
}

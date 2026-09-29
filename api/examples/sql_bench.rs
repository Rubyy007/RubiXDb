//! Real, repeatable end-to-end `POST /v1/sql` latency/throughput
//! benchmark against a real running server (release build intended --
//! `cargo run --release -p rubixdb-api --example sql_bench`). Measures
//! whole-request wall-clock latency (network + parse + bind + plan +
//! execute + serialize combined) -- this tool does not break latency
//! down into sub-phases (no such per-phase timing is exposed by the
//! server today; that is a documented limitation of this measurement,
//! not a claim of phase-level numbers this tool doesn't actually
//! produce). `PHASE_RUBIXDB_PERFORMANCE_BASELINE.md` records the
//! results of running this.
//!
//! USAGE:
//!   RUBIXDB_BENCH_URL=http://127.0.0.1:302 \
//!   RUBIXDB_BENCH_KEY=<admin key> \
//!   cargo run --release -p rubixdb-api --example sql_bench

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const READ_ROWS: i64 = 2000;
const READ_ITERATIONS_PER_LEVEL: usize = 200;
const WRITE_ITERATIONS_PER_LEVEL: usize = 100;
const CONCURRENCY_LEVELS: &[usize] = &[1, 2, 4, 8, 16];

struct Client {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl Client {
    async fn exec(&self, sql: &str) -> Result<Value, String> {
        let resp = self
            .http
            .post(format!("{}/v1/sql", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&json!({ "sql": sql, "params": [] }))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = resp.status();
        let body: Value = resp.json().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!("HTTP {status}: {body}"));
        }
        Ok(body)
    }

    async fn timed(&self, sql: &str) -> Result<Duration, String> {
        let start = Instant::now();
        self.exec(sql).await?;
        Ok(start.elapsed())
    }
}

fn percentile(sorted_micros: &[u128], p: f64) -> u128 {
    if sorted_micros.is_empty() {
        return 0;
    }
    let idx = ((sorted_micros.len() as f64 - 1.0) * p).round() as usize;
    sorted_micros[idx.min(sorted_micros.len() - 1)]
}

fn report(label: &str, mut samples: Vec<Duration>, wall: Duration) {
    samples.sort();
    let micros: Vec<u128> = samples.iter().map(|d| d.as_micros()).collect();
    let n = micros.len();
    let p50 = percentile(&micros, 0.50);
    let p95 = percentile(&micros, 0.95);
    let p99 = percentile(&micros, 0.99);
    let max = micros.last().copied().unwrap_or(0);
    let throughput = if wall.as_secs_f64() > 0.0 {
        n as f64 / wall.as_secs_f64()
    } else {
        0.0
    };
    println!(
        "{label:<40} n={n:<5} p50={:>7.2}ms p95={:>7.2}ms p99={:>7.2}ms max={:>7.2}ms throughput={throughput:>8.1} req/s",
        p50 as f64 / 1000.0,
        p95 as f64 / 1000.0,
        p99 as f64 / 1000.0,
        max as f64 / 1000.0,
    );
}

async fn run_concurrent<F, Fut>(
    concurrency: usize,
    iterations: usize,
    make_request: F,
) -> (Vec<Duration>, Duration)
where
    F: Fn(usize) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<Duration, String>> + Send,
{
    let make_request = Arc::new(make_request);
    let per_task = iterations.div_ceil(concurrency);
    let start = Instant::now();
    let mut handles = Vec::new();
    let counter = Arc::new(AtomicI64::new(0));
    for _ in 0..concurrency {
        let make_request = Arc::clone(&make_request);
        let counter = Arc::clone(&counter);
        handles.push(tokio::spawn(async move {
            let mut out = Vec::with_capacity(per_task);
            for _ in 0..per_task {
                let i = counter.fetch_add(1, Ordering::Relaxed) as usize;
                if i >= iterations {
                    break;
                }
                match make_request(i).await {
                    Ok(d) => out.push(d),
                    Err(e) => eprintln!("request error: {e}"),
                }
            }
            out
        }));
    }
    let mut all = Vec::new();
    for h in handles {
        all.extend(h.await.unwrap());
    }
    (all, start.elapsed())
}

#[tokio::main]
async fn main() {
    let base_url =
        std::env::var("RUBIXDB_BENCH_URL").unwrap_or_else(|_| "http://127.0.0.1:302".to_string());
    let api_key = std::env::var("RUBIXDB_BENCH_KEY").expect("RUBIXDB_BENCH_KEY must be set");

    let client = Arc::new(Client {
        http: reqwest::Client::new(),
        base_url,
        api_key,
    });

    println!("=== setup ===");
    client.exec("DROP TABLE IF EXISTS bench_ro").await.ok();
    client
        .exec("CREATE TABLE bench_ro (id INTEGER PRIMARY KEY, grp TEXT, val INTEGER)")
        .await
        .expect("create bench_ro");
    for id in 0..READ_ROWS {
        let grp = format!("g{}", id % 10);
        client
            .exec(&format!(
                "INSERT INTO bench_ro (id, grp, val) VALUES ({id}, '{grp}', {id})"
            ))
            .await
            .expect("seed row");
    }
    client
        .exec("CREATE INDEX idx_bench_ro_grp ON bench_ro (grp)")
        .await
        .expect("create index");
    println!("seeded {READ_ROWS} rows + index\n");

    println!("=== read workloads ===");
    for &c in CONCURRENCY_LEVELS {
        let client = Arc::clone(&client);
        let (samples, wall) = run_concurrent(c, READ_ITERATIONS_PER_LEVEL, move |i| {
            let client = Arc::clone(&client);
            async move {
                let id = (i as i64 * 97) % READ_ROWS;
                client
                    .timed(&format!("SELECT * FROM bench_ro WHERE id = {id}"))
                    .await
            }
        })
        .await;
        report(&format!("pk_lookup concurrency={c}"), samples, wall);
    }
    println!();

    for &c in CONCURRENCY_LEVELS {
        let client = Arc::clone(&client);
        let (samples, wall) = run_concurrent(c, READ_ITERATIONS_PER_LEVEL, move |i| {
            let client = Arc::clone(&client);
            async move {
                let grp = format!("g{}", i % 10);
                client
                    .timed(&format!("SELECT * FROM bench_ro WHERE grp = '{grp}'"))
                    .await
            }
        })
        .await;
        report(&format!("indexed_lookup concurrency={c}"), samples, wall);
    }
    println!();

    for &c in CONCURRENCY_LEVELS {
        let client = Arc::clone(&client);
        let (samples, wall) = run_concurrent(c, READ_ITERATIONS_PER_LEVEL, move |i| {
            let client = Arc::clone(&client);
            async move {
                let lo = (i as i64 * 53) % (READ_ROWS - 50);
                client
                    .timed(&format!(
                        "SELECT * FROM bench_ro WHERE id >= {lo} AND id < {}",
                        lo + 50
                    ))
                    .await
            }
        })
        .await;
        report(&format!("range_scan_50rows concurrency={c}"), samples, wall);
    }
    println!();

    for &c in CONCURRENCY_LEVELS {
        let client = Arc::clone(&client);
        let (samples, wall) = run_concurrent(c, READ_ITERATIONS_PER_LEVEL, move |_i| {
            let client = Arc::clone(&client);
            async move { client.timed("SELECT COUNT(*) FROM bench_ro").await }
        })
        .await;
        report(&format!("seq_scan_count concurrency={c}"), samples, wall);
    }
    println!();

    for &c in CONCURRENCY_LEVELS {
        let client = Arc::clone(&client);
        let (samples, wall) = run_concurrent(c, READ_ITERATIONS_PER_LEVEL, move |_i| {
            let client = Arc::clone(&client);
            async move {
                client
                    .timed("SELECT grp, COUNT(*), SUM(val) FROM bench_ro GROUP BY grp HAVING COUNT(*) > 0")
                    .await
            }
        })
        .await;
        report(&format!("group_by_having concurrency={c}"), samples, wall);
    }
    println!();

    println!("=== write workloads (lower concurrency: each request needs a distinct row, avoided coordinating >8-way to keep the benchmark itself simple and correct) ===");
    client.exec("DROP TABLE IF EXISTS bench_rw").await.ok();
    client
        .exec("CREATE TABLE bench_rw (id INTEGER PRIMARY KEY, v TEXT)")
        .await
        .expect("create bench_rw");

    // A real bug found while first running this benchmark: `bench_rw`
    // was never cleared between concurrency levels here, so every
    // level after the first restarted its id counter at 0 and
    // collided with rows the *previous* level had already inserted --
    // every request beyond concurrency=1 failed with a real (but
    // benchmark-induced, not product) primary-key conflict. Each level
    // now gets a clean table, matching the update/delete loops below.
    let write_levels = [1usize, 2, 4, 8];
    for &c in &write_levels {
        client.exec("DELETE FROM bench_rw").await.ok();
        let client = Arc::clone(&client);
        let counter = Arc::new(AtomicI64::new(0));
        let (samples, wall) = run_concurrent(c, WRITE_ITERATIONS_PER_LEVEL, move |_i| {
            let client = Arc::clone(&client);
            let counter = Arc::clone(&counter);
            async move {
                let id = counter.fetch_add(1, Ordering::Relaxed);
                client
                    .timed(&format!("INSERT INTO bench_rw (id, v) VALUES ({id}, 'x')"))
                    .await
            }
        })
        .await;
        report(&format!("insert concurrency={c}"), samples, wall);
    }
    println!();

    // Re-seed a fixed pool of rows for UPDATE/DELETE so each worker
    // can claim a distinct row via the same atomic counter pattern.
    for &c in &write_levels {
        client.exec("DELETE FROM bench_rw").await.ok();
        for id in 0..WRITE_ITERATIONS_PER_LEVEL as i64 {
            client
                .exec(&format!("INSERT INTO bench_rw (id, v) VALUES ({id}, 'x')"))
                .await
                .expect("seed for update");
        }
        let client2 = Arc::clone(&client);
        let counter = Arc::new(AtomicI64::new(0));
        let (samples, wall) = run_concurrent(c, WRITE_ITERATIONS_PER_LEVEL, move |_i| {
            let client = Arc::clone(&client2);
            let counter = Arc::clone(&counter);
            async move {
                let id = counter.fetch_add(1, Ordering::Relaxed);
                client
                    .timed(&format!(
                        "UPDATE bench_rw SET v = 'updated' WHERE id = {id}"
                    ))
                    .await
            }
        })
        .await;
        report(&format!("update concurrency={c}"), samples, wall);
    }
    println!();

    for &c in &write_levels {
        client.exec("DELETE FROM bench_rw").await.ok();
        for id in 0..WRITE_ITERATIONS_PER_LEVEL as i64 {
            client
                .exec(&format!("INSERT INTO bench_rw (id, v) VALUES ({id}, 'x')"))
                .await
                .expect("seed for delete");
        }
        let client2 = Arc::clone(&client);
        let counter = Arc::new(AtomicI64::new(0));
        let (samples, wall) = run_concurrent(c, WRITE_ITERATIONS_PER_LEVEL, move |_i| {
            let client = Arc::clone(&client2);
            let counter = Arc::clone(&counter);
            async move {
                let id = counter.fetch_add(1, Ordering::Relaxed);
                client
                    .timed(&format!("DELETE FROM bench_rw WHERE id = {id}"))
                    .await
            }
        })
        .await;
        report(&format!("delete concurrency={c}"), samples, wall);
    }

    println!("\n=== done ===");
}

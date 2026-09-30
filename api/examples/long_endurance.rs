//! Blocker 9 (multi-hour chained endurance) workload driver. Extends
//! `endurance.rs` (Increment 13, single 180s in-process run) in two
//! ways the chained-segment design in `PHASE_RUBIXDB_INCREMENT14_
//! BLOCKER9_LONG_DURATION_ENDURANCE.md` requires:
//!
//! 1. **Persistence-aware across process restarts.** `RUBIXDB_
//!    ENDURANCE_FRESH=1` (segment 1 only) creates and seeds the
//!    tables; `RUBIXDB_ENDURANCE_FRESH=0` (segments 2/3) skips
//!    creation entirely and instead reads the real current `MAX(id)`
//!    from the table left behind by the previous segment, so new rows
//!    never collide with old ones and the accumulated database state
//!    is never reset between segments.
//! 2. **A real JOIN**, absent from the Increment 13 driver: a second
//!    table (`long_endurance_grp`) is seeded once and joined against
//!    on every JOIN-op cycle, so this run's operation mix actually
//!    covers SELECT/indexed SELECT/range SELECT/JOIN/GROUP BY/HAVING/
//!    INSERT/UPDATE/DELETE/transactions -- the mission's exact list.
//!
//! Per-operation client-observed max latency is tracked locally
//! (a single `AtomicU64` per op, nanoseconds -- O(1) memory, no
//! per-request retention). p50/p95/p99 come from the server's own
//! already-certified bounded-reservoir route metrics (`/v1/metrics`,
//! `api/src/metrics.rs`), sampled externally by the orchestrating
//! script -- this driver does not recompute percentiles itself.
//!
//! USAGE:
//!   RUBIXDB_BENCH_URL=http://127.0.0.1:302 \
//!   RUBIXDB_BENCH_KEY=<admin key> \
//!   RUBIXDB_ENDURANCE_SECS=7200 \
//!   RUBIXDB_ENDURANCE_FRESH=1 \
//!   cargo run --release -p rubixdb-api --example long_endurance

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

struct Client {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl Client {
    async fn exec(&self, sql: &str, session_id: Option<&str>) -> Result<Value, String> {
        let mut body = json!({ "sql": sql, "params": [] });
        if let Some(sid) = session_id {
            body["session_id"] = json!(sid);
        }
        let resp = self
            .http
            .post(format!("{}/v1/sql", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = resp.status();
        let value: Value = resp.json().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!("HTTP {status}: {value}"));
        }
        Ok(value)
    }
}

#[derive(Default)]
struct OpStat {
    count: AtomicU64,
    errors: AtomicU64,
    sum_nanos: AtomicU64,
    max_nanos: AtomicU64,
    // Bounded to 3 samples -- proves *what* the errors actually are
    // (e.g. the expected snapshot-isolation CONFLICT_ERROR under
    // deliberate contention, per PHASE_RUBIXDB_ENDURANCE.md) rather
    // than trusting an error count alone, without retaining every
    // message over a multi-hour run.
    error_samples: std::sync::Mutex<Vec<String>>,
}

impl OpStat {
    fn record(&self, elapsed: Duration, result: &Result<Value, String>) {
        self.count.fetch_add(1, Ordering::Relaxed);
        if let Err(e) = result {
            self.errors.fetch_add(1, Ordering::Relaxed);
            let mut samples = self.error_samples.lock().unwrap_or_else(|p| p.into_inner());
            if samples.len() < 3 {
                samples.push(e.clone());
            }
        }
        let nanos = elapsed.as_nanos().min(u128::from(u64::MAX)) as u64;
        self.sum_nanos.fetch_add(nanos, Ordering::Relaxed);
        self.max_nanos.fetch_max(nanos, Ordering::Relaxed);
    }

    fn report(&self, name: &str) {
        let count = self.count.load(Ordering::Relaxed);
        let errors = self.errors.load(Ordering::Relaxed);
        let sum = self.sum_nanos.load(Ordering::Relaxed);
        let max = self.max_nanos.load(Ordering::Relaxed);
        let avg_ms = if count > 0 {
            (sum as f64 / count as f64) / 1_000_000.0
        } else {
            0.0
        };
        let max_ms = max as f64 / 1_000_000.0;
        println!(
            "{name:<16} count={count:<10} errors={errors:<8} avg_ms={avg_ms:>9.3} max_ms={max_ms:>10.3}"
        );
        if errors > 0 {
            for sample in self.error_samples.lock().unwrap_or_else(|p| p.into_inner()).iter() {
                println!("  sample error: {sample}");
            }
        }
    }
}

#[derive(Default)]
struct Counters {
    select: OpStat,
    indexed_select: OpStat,
    range_select: OpStat,
    join: OpStat,
    insert: OpStat,
    update: OpStat,
    delete: OpStat,
    group_by: OpStat,
    transactions_committed: AtomicU64,
    transactions_rolled_back: AtomicU64,
    transaction_errors: AtomicU64,
}

async fn timed(client: &Client, sql: &str, session_id: Option<&str>, stat: &OpStat) -> Result<Value, String> {
    let start = Instant::now();
    let result = client.exec(sql, session_id).await;
    stat.record(start.elapsed(), &result);
    result
}

#[tokio::main]
async fn main() {
    let base_url =
        std::env::var("RUBIXDB_BENCH_URL").unwrap_or_else(|_| "http://127.0.0.1:302".to_string());
    let api_key = std::env::var("RUBIXDB_BENCH_KEY").expect("RUBIXDB_BENCH_KEY must be set");
    let duration_secs: u64 = std::env::var("RUBIXDB_ENDURANCE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(7200);
    let fresh: bool = std::env::var("RUBIXDB_ENDURANCE_FRESH")
        .ok()
        .map(|v| v != "0")
        .unwrap_or(true);
    let segment: String =
        std::env::var("RUBIXDB_ENDURANCE_SEGMENT").unwrap_or_else(|_| "1".to_string());

    let client = Arc::new(Client {
        http: reqwest::Client::new(),
        base_url,
        api_key,
    });

    println!("=== segment {segment} setup (fresh={fresh}) ===");

    let next_id = Arc::new(AtomicI64::new(1000));

    if fresh {
        client
            .exec("DROP TABLE IF EXISTS long_endurance_t", None)
            .await
            .ok();
        client
            .exec("DROP TABLE IF EXISTS long_endurance_grp", None)
            .await
            .ok();
        client
            .exec(
                "CREATE TABLE long_endurance_grp (grp TEXT PRIMARY KEY, label TEXT)",
                None,
            )
            .await
            .expect("create grp table");
        for g in 0..10 {
            client
                .exec(
                    &format!("INSERT INTO long_endurance_grp (grp, label) VALUES ('g{g}', 'label-{g}')"),
                    None,
                )
                .await
                .expect("seed grp row");
        }
        // The session/transaction worker below inserts rows with
        // grp='session' -- seeded here too so the JOIN/orphan
        // correctness check at the end has no *expected* mismatches
        // to explain away.
        client
            .exec("INSERT INTO long_endurance_grp (grp, label) VALUES ('session', 'session-label')", None)
            .await
            .expect("seed session grp row");
        client
            .exec(
                "CREATE TABLE long_endurance_t (id INTEGER PRIMARY KEY, grp TEXT, val INTEGER, v TEXT)",
                None,
            )
            .await
            .expect("create table");
        for id in 0..1000i64 {
            client
                .exec(
                    &format!(
                        "INSERT INTO long_endurance_t (id, grp, val, v) VALUES ({id}, 'g{}', {id}, 'seed')",
                        id % 10
                    ),
                    None,
                )
                .await
                .expect("seed row");
        }
        client
            .exec(
                "CREATE INDEX idx_long_endurance_grp ON long_endurance_t (grp)",
                None,
            )
            .await
            .expect("create index");
        println!("fresh: seeded 1000 rows + grp table + index");
    } else {
        let existing = client
            .exec("SELECT COUNT(*) AS n, MAX(id) AS m FROM long_endurance_t", None)
            .await
            .expect("continuing segment must find the prior segment's table");
        println!("continuing: existing state = {existing}");
        let max_val = &existing["result"]["rows"][0][1]["value"];
        let max_id = max_val
            .as_i64()
            .or_else(|| max_val.as_str().and_then(|s| s.parse::<i64>().ok()))
            .unwrap_or(999);
        next_id.store(max_id + 1, Ordering::Relaxed);
    }

    println!("running for {duration_secs}s (segment {segment})\n");

    let counters = Arc::new(Counters::default());
    let deadline = Instant::now() + Duration::from_secs(duration_secs);

    let mut handles = Vec::new();
    for worker in 0..6i64 {
        let client = Arc::clone(&client);
        let counters = Arc::clone(&counters);
        let next_id = Arc::clone(&next_id);
        handles.push(tokio::spawn(async move {
            let mut i: i64 = 0;
            while Instant::now() < deadline {
                let op = (worker * 7 + i) % 8;
                let _ = match op {
                    0 => {
                        timed(
                            &client,
                            &format!("SELECT * FROM long_endurance_t WHERE id = {}", i % 1000),
                            None,
                            &counters.select,
                        )
                        .await
                    }
                    1 => {
                        timed(
                            &client,
                            &format!("SELECT * FROM long_endurance_t WHERE grp = 'g{}'", i % 10),
                            None,
                            &counters.indexed_select,
                        )
                        .await
                    }
                    2 => {
                        let lo = i % 900;
                        timed(
                            &client,
                            &format!(
                                "SELECT * FROM long_endurance_t WHERE id >= {lo} AND id < {}",
                                lo + 50
                            ),
                            None,
                            &counters.range_select,
                        )
                        .await
                    }
                    3 => {
                        let lo = i % 950;
                        timed(
                            &client,
                            &format!(
                                "SELECT t.id, t.val, g.label FROM long_endurance_t t JOIN \
                                 long_endurance_grp g ON t.grp = g.grp WHERE t.id >= {lo} AND t.id < {}",
                                lo + 20
                            ),
                            None,
                            &counters.join,
                        )
                        .await
                    }
                    4 => {
                        let id = next_id.fetch_add(1, Ordering::Relaxed);
                        timed(
                            &client,
                            &format!(
                                "INSERT INTO long_endurance_t (id, grp, val, v) VALUES ({id}, 'g{}', {id}, 'w{worker}')",
                                id % 10
                            ),
                            None,
                            &counters.insert,
                        )
                        .await
                    }
                    5 => {
                        timed(
                            &client,
                            &format!(
                                "UPDATE long_endurance_t SET v = 'updated-{i}' WHERE id = {}",
                                i % 1000
                            ),
                            None,
                            &counters.update,
                        )
                        .await
                    }
                    6 => {
                        timed(
                            &client,
                            "SELECT grp, COUNT(*), SUM(val) FROM long_endurance_t GROUP BY grp HAVING COUNT(*) > 0",
                            None,
                            &counters.group_by,
                        )
                        .await
                    }
                    _ => {
                        let id = i % 1000;
                        timed(
                            &client,
                            &format!("DELETE FROM long_endurance_t WHERE id = {id}"),
                            None,
                            &counters.delete,
                        )
                        .await
                    }
                };
                i += 1;
            }
        }));
    }

    // Session/transaction cycling worker (unchanged from endurance.rs
    // Phases I/J: real BEGIN/write/COMMIT-or-ROLLBACK cycles, with
    // periodic snapshot retention across concurrent writers).
    {
        let client = Arc::clone(&client);
        let counters = Arc::clone(&counters);
        let next_id = Arc::clone(&next_id);
        handles.push(tokio::spawn(async move {
            let mut cycle: i64 = 0;
            while Instant::now() < deadline {
                let begin = client.exec("BEGIN", None).await;
                let Ok(begin_body) = begin else {
                    counters.transaction_errors.fetch_add(1, Ordering::Relaxed);
                    continue;
                };
                let session_id = begin_body["session_id"].as_str().map(|s| s.to_string());

                let id = next_id.fetch_add(1, Ordering::Relaxed);
                let _ = client
                    .exec(
                        &format!(
                            "INSERT INTO long_endurance_t (id, grp, val, v) VALUES ({id}, 'session', {id}, 'txn')"
                        ),
                        session_id.as_deref(),
                    )
                    .await;

                if cycle % 5 == 0 {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    let _ = client
                        .exec("SELECT COUNT(*) FROM long_endurance_t", session_id.as_deref())
                        .await;
                }

                if cycle % 3 == 0 {
                    let _ = client.exec("ROLLBACK", session_id.as_deref()).await;
                    counters.transactions_rolled_back.fetch_add(1, Ordering::Relaxed);
                } else {
                    let _ = client.exec("COMMIT", session_id.as_deref()).await;
                    counters.transactions_committed.fetch_add(1, Ordering::Relaxed);
                }
                cycle += 1;
            }
        }));
    }

    // Liveness heartbeat: prints running counters every 5 minutes so
    // a 2-hour background run's log shows continuous progress, not
    // silence until the end.
    {
        let counters = Arc::clone(&counters);
        let segment = segment.clone();
        handles.push(tokio::spawn(async move {
            // Sleeps in short ticks and checks the real deadline every
            // tick, rather than sleeping a fixed 300s per iteration --
            // a fixed-sleep version overshoots `deadline` by up to
            // 300s before it next wakes to notice, which measurably
            // happened in this driver's own smoke test (a 20s run
            // took 305s to exit). This task must never be the reason
            // a segment overruns its allotted wall-clock budget.
            const TICK: Duration = Duration::from_secs(10);
            let mut since_last_print = Duration::ZERO;
            loop {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                let sleep_for = TICK.min(deadline - now);
                tokio::time::sleep(sleep_for).await;
                since_last_print += sleep_for;
                if since_last_print >= Duration::from_secs(300) {
                    since_last_print = Duration::ZERO;
                    println!(
                        "[segment {segment}] heartbeat select={} insert={} update={} delete={} join={} group_by={} committed={} rolled_back={}",
                        counters.select.count.load(Ordering::Relaxed),
                        counters.insert.count.load(Ordering::Relaxed),
                        counters.update.count.load(Ordering::Relaxed),
                        counters.delete.count.load(Ordering::Relaxed),
                        counters.join.count.load(Ordering::Relaxed),
                        counters.group_by.count.load(Ordering::Relaxed),
                        counters.transactions_committed.load(Ordering::Relaxed),
                        counters.transactions_rolled_back.load(Ordering::Relaxed),
                    );
                }
            }
        }));
    }

    for h in handles {
        let _ = h.await;
    }

    println!("\n=== segment {segment} run complete ===");
    counters.select.report("select");
    counters.indexed_select.report("indexed_select");
    counters.range_select.report("range_select");
    counters.join.report("join");
    counters.insert.report("insert");
    counters.update.report("update");
    counters.delete.report("delete");
    counters.group_by.report("group_by");
    println!(
        "transactions_committed:  {}",
        counters.transactions_committed.load(Ordering::Relaxed)
    );
    println!(
        "transactions_rolled_back:{}",
        counters.transactions_rolled_back.load(Ordering::Relaxed)
    );
    println!(
        "transaction_errors:      {}",
        counters.transaction_errors.load(Ordering::Relaxed)
    );

    // Correctness check that also validates the JOIN foreign-key-like
    // relationship end to end: any row whose grp has no matching
    // long_endurance_grp entry would be a real product bug (a JOIN
    // silently dropping/duplicating), not something to hide.
    let final_count = client
        .exec("SELECT COUNT(*) AS n FROM long_endurance_t", None)
        .await
        .expect("final correctness check must succeed");
    println!("final table state: {final_count}");

    let orphans = client
        .exec(
            "SELECT COUNT(*) AS n FROM long_endurance_t t LEFT JOIN long_endurance_grp g \
             ON t.grp = g.grp WHERE g.grp IS NULL",
            None,
        )
        .await
        .expect("orphan check must succeed");
    println!("rows with no matching grp (expect 0 -- 'session' grp is seeded too): {orphans}");
}

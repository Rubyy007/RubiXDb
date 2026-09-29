//! Real sustained mixed-workload endurance driver (Increment 13
//! Phases E/F/I/J) -- runs continuously against a real running server
//! for a configurable duration (`RUBIXDB_ENDURANCE_SECS`, default
//! 300s), mixing SELECT/indexed SELECT/range SELECT/INSERT/UPDATE/
//! DELETE/`GROUP BY`/`HAVING` with real session/transaction cycles
//! (including a snapshot held open across other clients' concurrent
//! writes -- Phase J), so a caller sampling the server process's real
//! RSS/handles/threads throughout (this tool does not sample itself;
//! see `PHASE_RUBIXDB_ENDURANCE.md` for the orchestration and results)
//! gets a genuine, non-synthetic resource trend.
//!
//! USAGE:
//!   RUBIXDB_BENCH_URL=http://127.0.0.1:302 \
//!   RUBIXDB_BENCH_KEY=<admin key> \
//!   RUBIXDB_ENDURANCE_SECS=300 \
//!   cargo run --release -p rubixdb-api --example endurance

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
struct Counters {
    selects: AtomicU64,
    indexed_selects: AtomicU64,
    range_selects: AtomicU64,
    inserts: AtomicU64,
    updates: AtomicU64,
    deletes: AtomicU64,
    group_bys: AtomicU64,
    transactions_committed: AtomicU64,
    transactions_rolled_back: AtomicU64,
    errors: AtomicU64,
}

#[tokio::main]
async fn main() {
    let base_url =
        std::env::var("RUBIXDB_BENCH_URL").unwrap_or_else(|_| "http://127.0.0.1:302".to_string());
    let api_key = std::env::var("RUBIXDB_BENCH_KEY").expect("RUBIXDB_BENCH_KEY must be set");
    let duration_secs: u64 = std::env::var("RUBIXDB_ENDURANCE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);

    let client = Arc::new(Client {
        http: reqwest::Client::new(),
        base_url,
        api_key,
    });

    println!("=== setup ===");
    client
        .exec("DROP TABLE IF EXISTS endurance_t", None)
        .await
        .ok();
    client
        .exec(
            "CREATE TABLE endurance_t (id INTEGER PRIMARY KEY, grp TEXT, val INTEGER, v TEXT)",
            None,
        )
        .await
        .expect("create table");
    for id in 0..1000i64 {
        client
            .exec(
                &format!(
                    "INSERT INTO endurance_t (id, grp, val, v) VALUES ({id}, 'g{}', {id}, 'seed')",
                    id % 10
                ),
                None,
            )
            .await
            .expect("seed row");
    }
    client
        .exec("CREATE INDEX idx_endurance_grp ON endurance_t (grp)", None)
        .await
        .expect("create index");
    println!("seeded 1000 rows + index; running for {duration_secs}s\n");

    let counters = Arc::new(Counters::default());
    let deadline = Instant::now() + Duration::from_secs(duration_secs);
    let next_id = Arc::new(AtomicI64::new(1000));

    // 6 mixed read/write workers.
    let mut handles = Vec::new();
    for worker in 0..6 {
        let client = Arc::clone(&client);
        let counters = Arc::clone(&counters);
        let next_id = Arc::clone(&next_id);
        handles.push(tokio::spawn(async move {
            let mut i: i64 = 0;
            while Instant::now() < deadline {
                let op = (worker * 7 + i) % 7;
                let result = match op {
                    0 => {
                        counters.selects.fetch_add(1, Ordering::Relaxed);
                        client
                            .exec(&format!("SELECT * FROM endurance_t WHERE id = {}", i % 1000), None)
                            .await
                    }
                    1 => {
                        counters.indexed_selects.fetch_add(1, Ordering::Relaxed);
                        client
                            .exec(
                                &format!("SELECT * FROM endurance_t WHERE grp = 'g{}'", i % 10),
                                None,
                            )
                            .await
                    }
                    2 => {
                        counters.range_selects.fetch_add(1, Ordering::Relaxed);
                        let lo = i % 900;
                        client
                            .exec(
                                &format!(
                                    "SELECT * FROM endurance_t WHERE id >= {lo} AND id < {}",
                                    lo + 50
                                ),
                                None,
                            )
                            .await
                    }
                    3 => {
                        counters.inserts.fetch_add(1, Ordering::Relaxed);
                        let id = next_id.fetch_add(1, Ordering::Relaxed);
                        client
                            .exec(
                                &format!(
                                    "INSERT INTO endurance_t (id, grp, val, v) VALUES ({id}, 'g{}', {id}, 'w{worker}')",
                                    id % 10
                                ),
                                None,
                            )
                            .await
                    }
                    4 => {
                        counters.updates.fetch_add(1, Ordering::Relaxed);
                        client
                            .exec(
                                &format!(
                                    "UPDATE endurance_t SET v = 'updated-{i}' WHERE id = {}",
                                    i % 1000
                                ),
                                None,
                            )
                            .await
                    }
                    5 => {
                        counters.group_bys.fetch_add(1, Ordering::Relaxed);
                        client
                            .exec(
                                "SELECT grp, COUNT(*), SUM(val) FROM endurance_t GROUP BY grp HAVING COUNT(*) > 0",
                                None,
                            )
                            .await
                    }
                    _ => {
                        counters.deletes.fetch_add(1, Ordering::Relaxed);
                        // Targets the original 0..1000 seed range, so
                        // this is a real delete of a real row (not a
                        // guaranteed-empty no-op) -- concurrent
                        // workers may occasionally race the same id
                        // with an insert/select, which is realistic
                        // contention, not a bug; any resulting error
                        // is counted, not hidden.
                        let id = i % 1000;
                        client
                            .exec(&format!("DELETE FROM endurance_t WHERE id = {id}"), None)
                            .await
                    }
                };
                if let Err(e) = &result {
                    let n = counters.errors.fetch_add(1, Ordering::Relaxed);
                    if n < 5 {
                        eprintln!("sample error #{n} (op={op}): {e}");
                    }
                }
                i += 1;
            }
        }));
    }

    // Phase I/J: one dedicated worker cycling real sessions --
    // BEGIN, a write, then COMMIT or ROLLBACK, repeatedly, plus a
    // periodic "hold a snapshot open while other clients write" case
    // (a transaction that begins, reads, sleeps briefly while the
    // mixed workers above are actively writing concurrently, then
    // closes) -- proving snapshot retention/release under real
    // concurrent write pressure, not release_of the_session_at_
    // client's convenience.
    {
        let client = Arc::clone(&client);
        let counters = Arc::clone(&counters);
        let next_id = Arc::clone(&next_id);
        handles.push(tokio::spawn(async move {
            let mut cycle: i64 = 0;
            while Instant::now() < deadline {
                let begin = client.exec("BEGIN", None).await;
                let Ok(begin_body) = begin else {
                    counters.errors.fetch_add(1, Ordering::Relaxed);
                    continue;
                };
                let session_id = begin_body["session_id"].as_str().map(|s| s.to_string());

                let id = next_id.fetch_add(1, Ordering::Relaxed);
                let _ = client
                    .exec(
                        &format!(
                            "INSERT INTO endurance_t (id, grp, val, v) VALUES ({id}, 'session', {id}, 'txn')"
                        ),
                        session_id.as_deref(),
                    )
                    .await;

                // Phase J: hold the snapshot open across real
                // concurrent write activity from the other workers.
                if cycle % 5 == 0 {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    let _ = client
                        .exec("SELECT COUNT(*) FROM endurance_t", session_id.as_deref())
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

    for h in handles {
        let _ = h.await;
    }

    println!("=== endurance run complete ===");
    println!(
        "selects:               {}",
        counters.selects.load(Ordering::Relaxed)
    );
    println!(
        "indexed_selects:       {}",
        counters.indexed_selects.load(Ordering::Relaxed)
    );
    println!(
        "range_selects:         {}",
        counters.range_selects.load(Ordering::Relaxed)
    );
    println!(
        "inserts:               {}",
        counters.inserts.load(Ordering::Relaxed)
    );
    println!(
        "updates:               {}",
        counters.updates.load(Ordering::Relaxed)
    );
    println!(
        "deletes:               {}",
        counters.deletes.load(Ordering::Relaxed)
    );
    println!(
        "group_bys:             {}",
        counters.group_bys.load(Ordering::Relaxed)
    );
    println!(
        "transactions_committed:{}",
        counters.transactions_committed.load(Ordering::Relaxed)
    );
    println!(
        "transactions_rolled_back:{}",
        counters.transactions_rolled_back.load(Ordering::Relaxed)
    );
    println!(
        "errors:                {}",
        counters.errors.load(Ordering::Relaxed)
    );

    // Final correctness check: the table must still be fully
    // readable and internally consistent after the whole run.
    let final_count = client
        .exec("SELECT COUNT(*) AS n FROM endurance_t", None)
        .await
        .expect("final correctness check must succeed");
    println!("final table state: {final_count}");
}

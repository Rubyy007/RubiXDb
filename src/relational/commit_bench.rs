//! Commit-path overhead benchmark for `docs/adr/ADR-DDL-FK-01.md`: foreign-key
//! support adds work to every `Transaction::commit` (reading the foreign-key set,
//! building the lock plan, comparing a generation counter). This measures the cost on
//! tables that have **no** foreign key, which is every table that existed before the
//! feature. `#[ignore]`d; run with
//! `cargo test --release -p rubixdb --lib commit_bench -- --ignored --nocapture`.
//! The same file is run against an unmodified checkout for the comparison.

use std::fs;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::catalog::service::ColumnDef;
use crate::catalog::CatalogService;
use crate::execution::batch_coordinator::BatchCoordinatorConfig;
use crate::lsm::{LsmConfig, LsmEngine};
use crate::relational::value::TYPE_TAG_INTEGER;
use crate::relational::{RelationalValue, TableStore, TransactionManager};
use crate::wal::{SyncMode, WalConfig};

fn run_once(threads: usize, per_thread: usize) -> (f64, Vec<u64>) {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("rubixdb_commit_bench_{nanos}"));
    fs::create_dir_all(&dir).unwrap();
    let engine = Arc::new(
        LsmEngine::open(
            &dir,
            WalConfig {
                sync_mode: SyncMode::GroupCommit {
                    max_wait: Duration::from_millis(1),
                    max_batch_bytes: 256 * 1024,
                },
                ..WalConfig::default()
            },
            BatchCoordinatorConfig {
                queue_capacity: 4096,
                max_queued_bytes: 64 * 1024 * 1024,
                submission_timeout: Duration::from_secs(30),
                shutdown_drain_bound: Duration::from_secs(30),
                await_retry_budget: Duration::from_secs(30),
                max_drain_per_batch: 4096,
            },
            LsmConfig::default(),
        )
        .unwrap(),
    );
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    catalog.bootstrap().unwrap();
    let db = catalog.list_databases().unwrap()[0].database_id;
    let schema = catalog.list_schemas(db).unwrap()[0].schema_id;
    let col = |n: &str, nullable: bool| ColumnDef {
        name: n.to_string(),
        data_type: TYPE_TAG_INTEGER,
        nullable,
        default_value: None,
        type_params: None,
    };
    let table = catalog
        .create_table(schema, "t", &[col("id", false), col("v", true)], &[0])
        .unwrap();
    let store = Arc::new(TableStore::new(Arc::clone(&engine), Arc::clone(&catalog)));
    let txm = TransactionManager::new(Arc::clone(&engine), Arc::clone(&store));

    let started = Instant::now();
    let handles: Vec<_> = (0..threads)
        .map(|w| {
            let txm = txm.clone();
            thread::spawn(move || {
                let mut micros = Vec::with_capacity(per_thread);
                for i in 0..per_thread {
                    let t0 = Instant::now();
                    let mut txn = txm.begin().unwrap();
                    txn.put_row(
                        table,
                        &[
                            Some(RelationalValue::Integer((w * per_thread + i) as i32)),
                            Some(RelationalValue::Integer(7)),
                        ],
                    )
                    .unwrap();
                    txn.commit().unwrap();
                    micros.push(t0.elapsed().as_micros() as u64);
                }
                micros
            })
        })
        .collect();
    let mut all = Vec::new();
    for h in handles {
        all.extend(h.join().unwrap());
    }
    let elapsed = started.elapsed().as_secs_f64();
    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
    all.sort_unstable();
    (((threads * per_thread) as f64) / elapsed, all)
}

#[test]
#[ignore = "benchmark; run explicitly (see the module documentation)"]
fn commit_throughput_on_tables_without_foreign_keys() {
    let runs = 7;
    let (threads, per_thread) = (16, 400);
    let mut rates = Vec::new();
    let mut p50s = Vec::new();
    let mut p99s = Vec::new();
    for _ in 0..runs {
        let (rate, lat) = run_once(threads, per_thread);
        rates.push(rate);
        p50s.push(lat[lat.len() / 2]);
        p99s.push(lat[lat.len() * 99 / 100]);
    }
    rates.sort_by(|a, b| a.partial_cmp(b).unwrap());
    p50s.sort_unstable();
    p99s.sort_unstable();
    println!(
        "COMMIT_BENCH threads={threads} per_thread={per_thread} runs={runs} \
         commits_per_sec: min={:.0} median={:.0} max={:.0} | latency_us p50 median={} | p99 median={}",
        rates[0],
        rates[runs / 2],
        rates[runs - 1],
        p50s[runs / 2],
        p99s[runs / 2]
    );
}

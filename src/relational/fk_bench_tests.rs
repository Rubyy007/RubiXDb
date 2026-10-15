//! Cost of enforcing a foreign key on the commit path (`docs/adr/ADR-DDL-FK-01.md`):
//! the same multi-threaded single-row-insert workload into a child table, with and
//! without a foreign key to a parent table, in one process. `#[ignore]`d; run with
//! `cargo test --release -p rubixdb --lib fk_bench -- --ignored --nocapture`.

use std::fs;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::catalog::schema::FkAction;
use crate::catalog::service::{ColumnDef, NewForeignKey};
use crate::catalog::CatalogService;
use crate::execution::batch_coordinator::BatchCoordinatorConfig;
use crate::lsm::{LsmConfig, LsmEngine};
use crate::relational::index::IndexBuilder;
use crate::relational::value::TYPE_TAG_INTEGER;
use crate::relational::{RelationalValue, TableStore, TransactionManager};
use crate::wal::{SyncMode, WalConfig};

fn run_once(with_fk: bool, threads: usize, per_thread: usize) -> f64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("rubixdb_fk_bench_{nanos}"));
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
    let parent = catalog
        .create_table(schema, "parent", &[col("id", false), col("v", true)], &[0])
        .unwrap();
    let child = catalog
        .create_table(schema, "child", &[col("id", false), col("pid", true)], &[0])
        .unwrap();
    let store = Arc::new(TableStore::new(Arc::clone(&engine), Arc::clone(&catalog)));
    let builder = IndexBuilder::new(
        Arc::clone(&engine),
        Arc::clone(&catalog),
        Arc::clone(&store),
    );
    for p in 0..16 {
        store
            .put_row(
                parent,
                &[
                    Some(RelationalValue::Integer(p)),
                    Some(RelationalValue::Integer(0)),
                ],
            )
            .unwrap();
    }
    if with_fk {
        builder
            .alter_add_foreign_key(&NewForeignKey {
                table_id: child,
                name: "fk".to_string(),
                child_ordinals: vec![1],
                parent_table_id: parent,
                parent_ordinals: vec![0],
                on_delete: FkAction::NoAction,
                on_update: FkAction::NoAction,
            })
            .unwrap();
    }
    let txm = TransactionManager::new(Arc::clone(&engine), Arc::clone(&store));
    let started = Instant::now();
    let handles: Vec<_> = (0..threads)
        .map(|w| {
            let txm = txm.clone();
            thread::spawn(move || {
                for i in 0..per_thread {
                    let mut txn = txm.begin().unwrap();
                    txn.put_row(
                        child,
                        &[
                            Some(RelationalValue::Integer((w * per_thread + i) as i32)),
                            Some(RelationalValue::Integer(((w + i) % 16) as i32)),
                        ],
                    )
                    .unwrap();
                    txn.commit().unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let elapsed = started.elapsed().as_secs_f64();
    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
    ((threads * per_thread) as f64) / elapsed
}

#[test]
#[ignore = "benchmark; run explicitly (see the module documentation)"]
fn child_insert_throughput_with_and_without_a_foreign_key() {
    let (threads, per_thread, runs) = (16, 400, 5);
    let mut without = Vec::new();
    let mut with = Vec::new();
    for _ in 0..runs {
        without.push(run_once(false, threads, per_thread));
        with.push(run_once(true, threads, per_thread));
    }
    without.sort_by(|a, b| a.partial_cmp(b).unwrap());
    with.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "FK_BENCH threads={threads} per_thread={per_thread} runs={runs} commits_per_sec \
         without_fk: min={:.0} median={:.0} max={:.0} | with_fk: min={:.0} median={:.0} max={:.0}",
        without[0],
        without[runs / 2],
        without[runs - 1],
        with[0],
        with[runs / 2],
        with[runs - 1]
    );
}

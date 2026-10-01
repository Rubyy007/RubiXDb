//! Increment 18 comparison benchmarks, written to compile and run
//! **unchanged against both the Increment 17 tree (`bc80c79`) and the
//! Increment 18 tree**: mixed PK-range + secondary-index concurrency, and
//! index DDL timing (statistics overhead on CREATE INDEX / DROP INDEX).
//!
//!   cargo test --release -p rubixdb-sql --lib inc18_compare -- --ignored --nocapture --test-threads=1

use std::sync::Arc;
use std::time::{Duration, Instant};

use rubixdb::catalog::schema::IndexKind;

use crate::exec::ExecMetrics;
use crate::index_read_benchmark::{proc_sample, Env, Lat};

/// 1/2/4/8/16/32 concurrent readers of (a) a wide PK range combined with a
/// selective secondary predicate -- the shape the Increment 17 planner
/// executed as a full walk of the PK range --, (b) a plain index equality and
/// (c) a PK equality control.
#[test]
#[ignore]
fn inc18_compare_concurrency() {
    let n = 100_000;
    let env = Arc::new(Env::new("inc18_conc", n, &[100, 1_000], true));
    for (label, sql, ops_each) in [
        (
            "pk range 20K + idx K=1000",
            "SELECT * FROM ix WHERE id >= 5000 AND id < 25000 AND g1000 = 'g0'".to_string(),
            12usize,
        ),
        (
            "idx eq K=100",
            "SELECT * FROM ix WHERE g100 = 'g0'".to_string(),
            30,
        ),
        (
            "pk eq",
            format!("SELECT * FROM ix WHERE id = {}", n / 2),
            400,
        ),
    ] {
        let plan = Arc::new(env.prepare(&sql));
        println!("\n--- {label} ---");
        for &conc in &[1usize, 2, 4, 8, 16, 32] {
            let cpu0 = proc_sample().cpu_ms;
            let wall0 = Instant::now();
            let handles: Vec<_> = (0..conc)
                .map(|_| {
                    let env = Arc::clone(&env);
                    let plan = Arc::clone(&plan);
                    std::thread::spawn(move || {
                        let mut v = Vec::with_capacity(ops_each);
                        for _ in 0..ops_each {
                            let s = Instant::now();
                            env.exec(&plan, &ExecMetrics::default());
                            v.push(s.elapsed());
                        }
                        v
                    })
                })
                .collect();
            let all: Vec<Duration> = handles
                .into_iter()
                .flat_map(|h| h.join().unwrap())
                .collect();
            let wall = wall0.elapsed();
            let ps = proc_sample();
            let lat = Lat(all);
            println!(
                "conc={conc:<3} p50={:>9.3} p95={:>9.3} p99={:>9.3} max={:>9.3} ms | {:>9.1} op/s cpu={:>9.1}ms rss={:>5.0}MB handles={}",
                lat.pct(0.5),
                lat.pct(0.95),
                lat.pct(0.99),
                lat.max(),
                (conc * ops_each) as f64 / wall.as_secs_f64(),
                ps.cpu_ms - cpu0,
                ps.rss_mb,
                ps.handles,
            );
        }
    }
    if let Ok(env) = Arc::try_unwrap(env) {
        env.cleanup();
    }
}

/// CREATE INDEX (online backfill) and DROP INDEX wall time on a 100K-row
/// table: the statistics machinery observes the row count the backfill
/// already enumerates, so it should add nothing measurable.
#[test]
#[ignore]
fn inc18_compare_index_ddl() {
    println!("\n=== CREATE / DROP INDEX, N=100,000 ===");
    for rep in 0..3 {
        let env = Env::new(&format!("inc18_ddl{rep}"), 100_000, &[], false);
        let t = Instant::now();
        let id = env
            .builder
            .create_index_online(env.table_id, "ddl_idx", IndexKind::NonUnique, &[2 + 3])
            .unwrap();
        let create = t.elapsed();
        let t = Instant::now();
        env.builder.drop_index_online(id).unwrap();
        let drop = t.elapsed();
        println!(
            "rep{rep}: CREATE INDEX {:>9.1} ms | DROP INDEX {:>9.1} ms",
            create.as_secs_f64() * 1e3,
            drop.as_secs_f64() * 1e3
        );
        env.cleanup();
    }
}

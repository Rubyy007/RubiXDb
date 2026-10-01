//! Increment 17 overhead measurements, written to compile and run
//! **unchanged against both the Increment 16 tree (`730ecca`) and the
//! Increment 17 tree**, so before/after are the same test on the same
//! machine: planner overhead, simple-query execution overhead, and write
//! overhead (the runtime-statistics hooks).
//!
//!   cargo test --release -p rubixdb-sql --lib inc17_overhead -- --ignored --nocapture --test-threads=1

use std::sync::Arc;
use std::time::{Duration, Instant};

use rubixdb::relational::RelationalValue;

use crate::auth::AuthContext;
use crate::bind::bind_statement;
use crate::index_read_benchmark::{proc_sample, report, run_query, Env};
use crate::limits::SqlLimits;
use crate::metrics::SqlMetrics;
use crate::parse::parse_statement;
use crate::plan::{build_plan, PlannerLimits, PlannerMetrics};

fn median(mut v: Vec<Duration>) -> f64 {
    v.sort();
    v[v.len() / 2].as_secs_f64() * 1e6
}

fn p99(mut v: Vec<Duration>) -> f64 {
    v.sort();
    v[(v.len() * 99 / 100).min(v.len() - 1)].as_secs_f64() * 1e6
}

/// Parse / bind / plan cost (microseconds) for representative shapes.
#[test]
#[ignore]
fn inc17_overhead_planner() {
    let env = Env::new("inc17_plan", 5_000, &[10, 100], true);
    let shapes: [(&str, String); 6] = [
        ("pk lookup", "SELECT * FROM ix WHERE id = 42".to_string()),
        ("index equality", "SELECT * FROM ix WHERE g100 = 'g0'".to_string()),
        ("index range", "SELECT * FROM ix WHERE r >= 10 AND r < 60".to_string()),
        ("seq scan (unindexed)", "SELECT * FROM ix WHERE pad = 'x'".to_string()),
        (
            "complex predicate",
            "SELECT id, r FROM ix WHERE (g100 = 'g0' OR g10 = 'g1') AND r > 5 AND id >= 3 AND pad <> 'z' ORDER BY id".to_string(),
        ),
        (
            "join",
            "SELECT a.id, b.r FROM ix a JOIN ix b ON a.id = b.id WHERE a.g100 = 'g0'".to_string(),
        ),
    ];
    let limits = SqlLimits::default();
    let auth = AuthContext::admin("bench");
    println!("\n=== planner overhead (us, median of 2,000 / p99) ===");
    for (label, sql) in &shapes {
        let stmt = parse_statement(sql, &limits).unwrap();
        let bound = bind_statement(
            &env.f.catalog,
            &env.f.ctx,
            &auth,
            &SqlMetrics::default(),
            &limits,
            &stmt,
        )
        .unwrap();
        let mut parse = Vec::new();
        let mut bind = Vec::new();
        let mut plan = Vec::new();
        for _ in 0..2_000 {
            let t = Instant::now();
            let _ = parse_statement(sql, &limits).unwrap();
            parse.push(t.elapsed());
            let t = Instant::now();
            let _ = bind_statement(
                &env.f.catalog,
                &env.f.ctx,
                &auth,
                &SqlMetrics::default(),
                &limits,
                &stmt,
            )
            .unwrap();
            bind.push(t.elapsed());
            let t = Instant::now();
            let _ = build_plan(
                &bound,
                &env.f.catalog,
                &PlannerLimits::default(),
                &PlannerMetrics::default(),
            )
            .unwrap();
            plan.push(t.elapsed());
        }
        println!(
            "{label:<22} parse={:>7.2}  bind={:>8.2}  plan={:>8.2} (p99 {:>8.2})",
            median(parse),
            median(bind),
            median(plan.clone()),
            p99(plan)
        );
    }
    env.cleanup();
}

/// End-to-end latency of the simple shapes the cost model must not slow.
#[test]
#[ignore]
fn inc17_overhead_execution() {
    let n = 100_000;
    let env = Env::new("inc17_exec", n, &[10, 100, 1_000], true);
    println!("\n=== execution overhead N={n} (warm; ms) ===");
    for (label, sql) in [
        (
            "pk lookup",
            format!("SELECT * FROM ix WHERE id = {}", n / 2),
        ),
        (
            "index eq K=10",
            "SELECT * FROM ix WHERE g10 = 'g0'".to_string(),
        ),
        (
            "index eq K=100",
            "SELECT * FROM ix WHERE g100 = 'g0'".to_string(),
        ),
        (
            "index eq K=1000",
            "SELECT * FROM ix WHERE g1000 = 'g0'".to_string(),
        ),
        (
            "pk range K=100",
            "SELECT * FROM ix WHERE id >= 5000 AND id < 5100".to_string(),
        ),
        (
            "seq scan (COUNT)",
            "SELECT COUNT(*) FROM ix WHERE pad = 'pad-pad-pad-pad'".to_string(),
        ),
    ] {
        let r = run_query(&env, &sql);
        report(label, &r);
    }
    env.cleanup();
}

fn new_row(id: usize) -> Vec<Option<RelationalValue>> {
    let mut row: Vec<Option<RelationalValue>> = vec![
        Some(RelationalValue::Integer(id as i32)),
        Some(RelationalValue::Integer(id as i32)),
    ];
    for g in [1usize, 10, 100, 1_000, 10_000] {
        row.push(Some(RelationalValue::Text(format!(
            "g{}",
            id % (1000 / g.min(1000)).max(1)
        ))));
    }
    row.push(Some(RelationalValue::Text("pad-pad-pad-pad".to_string())));
    row
}

/// Write-path overhead: the runtime-statistics hook (one relaxed atomic add
/// for a tracked table, a read-lock and hash probe for an untracked one).
#[test]
#[ignore]
fn inc17_overhead_writes() {
    println!("\n=== write overhead (table with 3 secondary indexes) ===");
    for rep in 0..3 {
        let env = Arc::new(Env::new(&format!("inc17_w{rep}"), 1_000, &[10, 100], true));
        let tid = env.table_id;
        let cpu0 = proc_sample().cpu_ms;
        let t = Instant::now();
        for id in 10_000..15_000 {
            env.store.put_row(tid, &new_row(id)).unwrap();
        }
        let put_row = t.elapsed();
        let t = Instant::now();
        for chunk in (20_000..40_000usize).collect::<Vec<_>>().chunks(500) {
            let rows: Vec<_> = chunk.iter().map(|&i| new_row(i)).collect();
            env.store.put_rows(tid, &rows).unwrap();
        }
        let put_rows = t.elapsed();
        let t = Instant::now();
        for id in 50_000..53_000 {
            env.txm.autocommit_put_row(tid, &new_row(id)).unwrap();
        }
        let txn = t.elapsed();
        let t = Instant::now();
        let handles: Vec<_> = (0..8)
            .map(|w| {
                let env = Arc::clone(&env);
                std::thread::spawn(move || {
                    for i in 0..1_500 {
                        env.store
                            .put_row(env.table_id, &new_row(100_000 + w * 10_000 + i))
                            .unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let conc = t.elapsed();
        println!(
            "rep{rep}: put_row x5,000 = {:>7.1} ms ({:>6.1} us/row) | put_rows 20,000 (500/batch) = {:>7.1} ms | txn autocommit x3,000 = {:>7.1} ms ({:>6.1} us/row) | 8 writers x1,500 = {:>7.1} ms | cpu {:>7.1} ms",
            put_row.as_secs_f64() * 1e3,
            put_row.as_secs_f64() * 1e6 / 5_000.0,
            put_rows.as_secs_f64() * 1e3,
            txn.as_secs_f64() * 1e3,
            txn.as_secs_f64() * 1e6 / 3_000.0,
            conc.as_secs_f64() * 1e3,
            proc_sample().cpu_ms - cpu0,
        );
        if let Ok(env) = Arc::try_unwrap(env) {
            env.cleanup();
        }
    }
}

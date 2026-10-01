//! Increment 16 performance evidence (`PHASE_RUBIXDB_INCREMENT16_INDEX_
//! READ_PERFORMANCE.md`): secondary-index read cost, decomposed by
//! pipeline stage and by the two independent variables the Blocker 9
//! `indexed_select` drift conflated -- TOTAL TABLE SIZE (`N`) and
//! MATCHED-ROW COUNT (`K`).
//!
//! Dataset: one table `ix(id INTEGER PK, r INTEGER, g1..g10000 TEXT,
//! pad TEXT)`, `r = id` (unique, for index/PK range queries), and one
//! text column per target cardinality: `g{K}` = `"g{id % (N/K)}"`, so
//! an equality predicate on `g{K}` matches exactly `K` rows for ANY
//! `N` (matching rows are spread evenly across the whole PK domain --
//! the same placement the endurance workload's round-robin `grp`
//! produces). Every `g{K}` column and `r` carries a secondary index.
//!
//! `#[ignore]`d (tens of seconds to minutes each, and honour the
//! `INC16_*` env vars below). Run, with TEMP on a drive that has space:
//!   cargo test --release -p rubixdb-sql --lib index_read_benchmark \
//!       -- --ignored --nocapture --test-threads=1
//!
//! Env: `INC16_SIZES` (comma list, default `1000,10000,100000`),
//! `INC16_KS` (default `1,10,100,1000,10000`).

use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rubixdb::catalog::schema::IndexKind;
use rubixdb::catalog::service::ColumnDef;
use rubixdb::catalog::CatalogService;
use rubixdb::lsm::ReadStats;
use rubixdb::relational::index::IndexBuilder;
use rubixdb::relational::index_key::{
    decode_indexed_columns, encode_indexed_columns, index_entry_prefix_range,
};
use rubixdb::relational::key::decode_composite_key;
use rubixdb::relational::table_store::TableStore;
use rubixdb::relational::value::{RelationalType, TYPE_TAG_INTEGER, TYPE_TAG_TEXT};
use rubixdb::relational::{RelationalValue, TransactionManager};

use crate::auth::AuthContext;
use crate::bind::bind_statement;
use crate::exec::{
    execute_autocommit, CancellationToken, ExecLimits, ExecMetrics, ExecMetricsSnapshot,
};
use crate::limits::SqlLimits;
use crate::metrics::SqlMetrics;
use crate::parse::parse_statement;
use crate::plan::{build_plan, Plan, PlannerLimits, PlannerMetrics};
use crate::test_support::Fixture;

const ALL_KS: [usize; 5] = [1, 10, 100, 1_000, 10_000];

// ---------------------------------------------------------------------
// Process resource sampling (Windows FFI; zeros elsewhere).
// ---------------------------------------------------------------------

#[derive(Clone, Copy, Default, Debug)]
pub struct ProcSample {
    pub cpu_ms: f64,
    pub rss_mb: f64,
    pub peak_rss_mb: f64,
    pub handles: u32,
}

#[cfg(windows)]
pub fn proc_sample() -> ProcSample {
    #[repr(C)]
    #[derive(Default)]
    struct FileTime {
        lo: u32,
        hi: u32,
    }
    #[repr(C)]
    #[derive(Default)]
    struct Pmc {
        cb: u32,
        page_faults: u32,
        peak_ws: usize,
        ws: usize,
        a: usize,
        b: usize,
        c: usize,
        d: usize,
        pagefile: usize,
        peak_pagefile: usize,
    }
    extern "system" {
        fn GetCurrentProcess() -> isize;
        fn GetProcessTimes(
            h: isize,
            c: *mut FileTime,
            e: *mut FileTime,
            k: *mut FileTime,
            u: *mut FileTime,
        ) -> i32;
        fn K32GetProcessMemoryInfo(h: isize, p: *mut Pmc, cb: u32) -> i32;
        fn GetProcessHandleCount(h: isize, n: *mut u32) -> i32;
    }
    let ft = |f: &FileTime| ((f.hi as u64) << 32 | f.lo as u64) as f64 / 10_000.0;
    // SAFETY: plain Win32 queries on the current-process pseudo-handle
    // into correctly-sized, zero-initialised out structs.
    unsafe {
        let h = GetCurrentProcess();
        let (mut c, mut e, mut k, mut u) = (
            FileTime::default(),
            FileTime::default(),
            FileTime::default(),
            FileTime::default(),
        );
        GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u);
        let mut pmc = Pmc {
            cb: std::mem::size_of::<Pmc>() as u32,
            ..Default::default()
        };
        K32GetProcessMemoryInfo(h, &mut pmc, pmc.cb);
        let mut handles = 0u32;
        GetProcessHandleCount(h, &mut handles);
        ProcSample {
            cpu_ms: ft(&k) + ft(&u),
            rss_mb: pmc.ws as f64 / 1_048_576.0,
            peak_rss_mb: pmc.peak_ws as f64 / 1_048_576.0,
            handles,
        }
    }
}

#[cfg(not(windows))]
pub fn proc_sample() -> ProcSample {
    ProcSample::default()
}

fn thread_count() -> u32 {
    #[cfg(windows)]
    {
        let pid = std::process::id();
        if let Ok(out) = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!("(Get-Process -Id {pid}).Threads.Count"),
            ])
            .output()
        {
            return String::from_utf8_lossy(&out.stdout)
                .trim()
                .parse()
                .unwrap_or(0);
        }
    }
    0
}

// ---------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------

pub struct Env {
    pub f: Fixture,
    pub catalog: Arc<CatalogService>,
    pub store: Arc<TableStore>,
    pub builder: Arc<IndexBuilder>,
    pub txm: Arc<TransactionManager>,
    pub n: usize,
    pub table_id: u32,
}

fn env_list(name: &str, default: &[usize]) -> Vec<usize> {
    match std::env::var(name) {
        Ok(v) => v.split(',').filter_map(|s| s.trim().parse().ok()).collect(),
        Err(_) => default.to_vec(),
    }
}

/// `INC16_COMPACT=1` runs the engine with automatic Compaction enabled
/// (the shipped server/CLI default); unset keeps the library default
/// (disabled), under which SSTables accumulate.
fn bench_fixture(tag: &str) -> Fixture {
    if std::env::var("INC16_COMPACT").is_ok_and(|v| v == "1") {
        Fixture::new_with_lsm(
            tag,
            rubixdb::lsm::LsmConfig {
                compaction_auto_trigger: true,
                ..Default::default()
            },
        )
    } else {
        Fixture::new(tag)
    }
}

fn col(name: &str, ty: u8, nullable: bool) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        data_type: ty,
        nullable,
        default_value: None,
        type_params: None,
    }
}

impl Env {
    /// Seeds `n` rows (chunked `put_rows`, no index yet) then builds
    /// every requested index via the real online backfill path.
    pub fn new(tag: &str, n: usize, ks: &[usize], index_r: bool) -> Self {
        let f = bench_fixture(tag);
        let catalog = Arc::new(CatalogService::new(Arc::clone(&f.engine)));
        let mut cols = vec![
            col("id", TYPE_TAG_INTEGER, false),
            col("r", TYPE_TAG_INTEGER, false),
        ];
        for k in ALL_KS {
            cols.push(col(&format!("g{k}"), TYPE_TAG_TEXT, true));
        }
        cols.push(col("pad", TYPE_TAG_TEXT, true));
        let table_id = catalog
            .create_table(f.ctx.default_schema_id, "ix", &cols, &[0])
            .unwrap();
        let store = Arc::new(TableStore::new(Arc::clone(&f.engine), Arc::clone(&catalog)));
        let builder = Arc::new(IndexBuilder::new(
            Arc::clone(&f.engine),
            Arc::clone(&catalog),
            Arc::clone(&store),
        ));
        let txm = Arc::new(TransactionManager::new(
            Arc::clone(&f.engine),
            Arc::clone(&store),
        ));
        let env = Env {
            f,
            catalog,
            store,
            builder,
            txm,
            n,
            table_id,
        };
        env.seed();
        for &k in ks {
            if k <= n {
                env.builder
                    .create_index_online(
                        table_id,
                        &format!("ix_g{k}"),
                        IndexKind::NonUnique,
                        &[2 + ALL_KS.iter().position(|&x| x == k).unwrap() as u16],
                    )
                    .unwrap();
            }
        }
        if index_r {
            env.builder
                .create_index_online(table_id, "ix_r", IndexKind::NonUnique, &[1])
                .unwrap();
        }
        env
    }

    fn seed(&self) {
        let n = self.n;
        let mut chunk = Vec::with_capacity(2000);
        for id in 0..n {
            let mut row: Vec<Option<RelationalValue>> = vec![
                Some(RelationalValue::Integer(id as i32)),
                Some(RelationalValue::Integer(id as i32)),
            ];
            for k in ALL_KS {
                let groups = (n / k.min(n)).max(1);
                row.push(Some(RelationalValue::Text(format!("g{}", id % groups))));
            }
            row.push(Some(RelationalValue::Text("pad-pad-pad-pad".to_string())));
            chunk.push(row);
            if chunk.len() == 2000 {
                self.store.put_rows(self.table_id, &chunk).unwrap();
                chunk.clear();
            }
        }
        if !chunk.is_empty() {
            self.store.put_rows(self.table_id, &chunk).unwrap();
        }
    }

    pub fn index_id(&self, name: &str) -> u32 {
        self.catalog
            .list_indexes(self.table_id)
            .unwrap()
            .into_iter()
            .find(|i| i.name == name)
            .unwrap()
            .index_id
    }

    pub fn prepare(&self, sql: &str) -> Plan {
        let limits = SqlLimits::default();
        let stmt = parse_statement(sql, &limits).unwrap();
        let auth = AuthContext::admin("bench");
        let bound = bind_statement(
            &self.f.catalog,
            &self.f.ctx,
            &auth,
            &SqlMetrics::default(),
            &limits,
            &stmt,
        )
        .unwrap();
        build_plan(
            &bound,
            &self.f.catalog,
            &PlannerLimits::default(),
            &PlannerMetrics::default(),
        )
        .unwrap()
    }

    pub fn exec(&self, plan: &Plan, m: &ExecMetrics) -> crate::exec::QueryResult {
        execute_autocommit(
            plan,
            &self.txm,
            &self.store,
            &self.builder,
            &[],
            &ExecLimits::default(),
            m,
            &CancellationToken::new(),
        )
        .unwrap()
    }

    pub fn cleanup(self) {
        self.f.cleanup();
    }
}

// ---------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------

pub struct Lat(pub Vec<Duration>);

impl Lat {
    pub fn pct(&self, p: f64) -> f64 {
        let mut v = self.0.clone();
        v.sort();
        let idx = ((v.len() as f64 - 1.0) * p).round() as usize;
        v[idx].as_secs_f64() * 1000.0
    }
    pub fn max(&self) -> f64 {
        self.0.iter().max().unwrap().as_secs_f64() * 1000.0
    }
}

pub struct Run {
    pub rows: usize,
    pub cold_ms: f64,
    pub lat: Lat,
    pub exec: ExecMetricsSnapshot,
    pub reads: (u64, u64, u64), // (requests, sstables_consulted, blocks_read) per exec
    pub cpu_ms_per_op: f64,
    pub rss_mb: f64,
    pub throughput: f64,
}

fn rs_delta(a: &ReadStats, b: &ReadStats) -> (u64, u64, u64) {
    (
        b.read_requests - a.read_requests,
        b.sstables_consulted - a.sstables_consulted,
        b.blocks_read - a.blocks_read,
    )
}

/// Cold = first execution after setup (engine-internal state cold; OS
/// page cache is whatever the seed left). Warm = adaptive repeat count.
pub fn run_query(env: &Env, sql: &str) -> Run {
    let plan = env.prepare(sql);
    let t0 = Instant::now();
    let res = env.exec(&plan, &ExecMetrics::default());
    let cold_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let rows = res.rows.len();
    let reps = ((1500.0 / cold_ms.max(0.05)) as usize).clamp(8, 200);

    let m = ExecMetrics::default();
    let rs0 = env.f.engine.read_stats();
    env.exec(&plan, &m);
    let reads = rs_delta(&rs0, &env.f.engine.read_stats());
    let exec = m.snapshot();

    let cpu0 = proc_sample().cpu_ms;
    let wall0 = Instant::now();
    let mut samples = Vec::with_capacity(reps);
    for _ in 0..reps {
        let s = Instant::now();
        env.exec(&plan, &ExecMetrics::default());
        samples.push(s.elapsed());
    }
    let wall = wall0.elapsed();
    let ps = proc_sample();
    Run {
        rows,
        cold_ms,
        lat: Lat(samples),
        exec,
        reads,
        cpu_ms_per_op: (ps.cpu_ms - cpu0) / reps as f64,
        rss_mb: ps.rss_mb,
        throughput: reps as f64 / wall.as_secs_f64(),
    }
}

pub fn report(label: &str, r: &Run) {
    println!(
        "{label:<34} rows={:<6} cold={:>8.2} p50={:>8.3} p95={:>8.3} p99={:>8.3} max={:>8.3} ms | {:>8.1} op/s cpu={:>7.2}ms/op rss={:>6.0}MB | idx_ex={} fetch={} scanned={} reads(req/sst/blk)={}/{}/{}",
        r.rows,
        r.cold_ms,
        r.lat.pct(0.5),
        r.lat.pct(0.95),
        r.lat.pct(0.99),
        r.lat.max(),
        r.throughput,
        r.cpu_ms_per_op,
        r.rss_mb,
        r.exec.index_rows_examined,
        r.exec.table_fetches,
        r.exec.rows_scanned,
        r.reads.0,
        r.reads.1,
        r.reads.2,
    );
}

fn median_ms<F: FnMut()>(reps: usize, mut f: F) -> f64 {
    f(); // warm
    let mut v = Vec::with_capacity(reps);
    for _ in 0..reps {
        let s = Instant::now();
        f();
        v.push(s.elapsed());
    }
    v.sort();
    v[v.len() / 2].as_secs_f64() * 1000.0
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

/// Stage-by-stage decomposition of one indexed-equality read returning
/// `K` rows from an `N`-row table: separates SQL front-end, index
/// traversal, entry decode, per-row fetch (and the catalog-metadata
/// resolution inside it), engine-floor raw point reads, the full
/// relational `index_lookup_as_of`, and the full SQL execution.
#[test]
#[ignore]
fn stage_decomposition() {
    let sizes = env_list("INC16_SIZES", &[100_000]);
    for &n in &sizes {
        let ks = env_list("INC16_KS", &ALL_KS);
        let env = Env::new(&format!("inc16_decomp_{n}"), n, &ks, false);
        println!(
            "\n=== stage decomposition N={n} (sstables={}) ===",
            env.f.engine.sstable_count()
        );
        for &k in ks.iter().filter(|&&k| k <= n) {
            let colname = format!("g{k}");
            let index_id = env.index_id(&format!("ix_{colname}"));
            let sql = format!("SELECT * FROM ix WHERE {colname} = 'g0'");
            let table = env.catalog.get_table(env.table_id).unwrap().unwrap();
            let columns = env.catalog.get_columns(env.table_id).unwrap();
            let index_row = env.catalog.get_index(index_id).unwrap().unwrap();
            let itypes: Vec<RelationalType> = vec![RelationalType::Text];
            let pk_types = vec![RelationalType::Integer];
            let _ = (&table, &columns, &index_row);

            // front-end
            let limits = SqlLimits::default();
            let t_parse = median_ms(200, || {
                parse_statement(&sql, &limits).unwrap();
            });
            let stmt = parse_statement(&sql, &limits).unwrap();
            let auth = AuthContext::admin("bench");
            let t_bind = median_ms(200, || {
                bind_statement(
                    &env.f.catalog,
                    &env.f.ctx,
                    &auth,
                    &SqlMetrics::default(),
                    &limits,
                    &stmt,
                )
                .unwrap();
            });
            let bound = bind_statement(
                &env.f.catalog,
                &env.f.ctx,
                &auth,
                &SqlMetrics::default(),
                &limits,
                &stmt,
            )
            .unwrap();
            let t_plan = median_ms(200, || {
                build_plan(
                    &bound,
                    &env.f.catalog,
                    &PlannerLimits::default(),
                    &PlannerMetrics::default(),
                )
                .unwrap();
            });

            // index traversal only
            let prefix =
                encode_indexed_columns(&[Some(RelationalValue::Text("g0".into()))]).unwrap();
            let (s, e) = index_entry_prefix_range(env.table_id, index_id, &prefix);
            fn bref(b: &Bound<Vec<u8>>) -> Bound<&[u8]> {
                match b {
                    Bound::Included(v) => Bound::Included(v.as_slice()),
                    Bound::Excluded(v) => Bound::Excluded(v.as_slice()),
                    Bound::Unbounded => Bound::Unbounded,
                }
            }
            let eng = &env.f.engine;
            let t_trav = median_ms(30, || {
                let mut c = 0usize;
                for ent in eng.range_scan(bref(&s), bref(&e), u64::MAX) {
                    let _ = ent.unwrap();
                    c += 1;
                }
                assert_eq!(c, k);
            });
            // + decode
            let mut pks: Vec<Vec<RelationalValue>> = Vec::new();
            let t_dec = median_ms(30, || {
                pks.clear();
                for ent in eng.range_scan(bref(&s), bref(&e), u64::MAX) {
                    let (key, _) = ent.unwrap();
                    let body = &key[9..];
                    let (_v, used) = decode_indexed_columns(&itypes, body).unwrap();
                    pks.push(decode_composite_key(&pk_types, &body[used..]).unwrap());
                }
            });
            // metadata resolution x K (what get_row_as_of does per call)
            let t_meta = median_ms(10, || {
                for _ in 0..k {
                    let _ = env.catalog.get_table(env.table_id).unwrap();
                    let _ = env.catalog.get_columns(env.table_id).unwrap();
                }
            });
            // per-row fetch exactly as scan_entries does it
            let t_fetch = median_ms(10, || {
                for pk in &pks {
                    env.store
                        .get_row_as_of(env.table_id, pk, u64::MAX)
                        .unwrap()
                        .unwrap();
                }
            });
            // engine floor: raw point reads, no catalog, no decode
            let keys: Vec<Vec<u8>> = pks
                .iter()
                .map(|pk| {
                    rubixdb::relational::key::table_row_key(
                        env.table_id,
                        &rubixdb::relational::key::encode_composite_key(pk).unwrap(),
                    )
                })
                .collect();
            let t_raw = median_ms(10, || {
                for key in &keys {
                    eng.get_as_of(key, u64::MAX).unwrap().unwrap();
                }
            });
            // full relational op
            let t_rel = median_ms(10, || {
                let rows = env
                    .builder
                    .index_lookup_as_of(
                        index_id,
                        &[Some(RelationalValue::Text("g0".into()))],
                        u64::MAX,
                    )
                    .unwrap();
                assert_eq!(rows.len(), k);
            });
            // full SQL
            let plan = env.prepare(&sql);
            let t_sql = median_ms(10, || {
                let r = env.exec(&plan, &ExecMetrics::default());
                assert_eq!(r.rows.len(), k);
            });
            println!(
                "N={n:<8} K={k:<6} parse={t_parse:>7.3} bind={t_bind:>7.3} plan={t_plan:>7.3} | traverse={t_trav:>8.3} +decode={t_dec:>8.3} | meta_x_K={t_meta:>9.3} fetch_x_K={t_fetch:>9.3} raw_get_x_K={t_raw:>9.3} | index_lookup_as_of={t_rel:>9.3} full_sql={t_sql:>9.3} (ms, median)"
            );
        }
        env.cleanup();
    }
}

/// Mandatory test 1: result cardinality FIXED (K), table size grows.
/// Mandatory test 2: table size FIXED (N), result cardinality grows.
/// Both include the comparison access paths at the same result size:
/// PK equality (K=1), PK range, secondary-index range, selective
/// SeqScan (forced by `id + 0` arithmetic so the planner cannot pick a
/// bounded path).
#[test]
#[ignore]
fn scaling_matrix() {
    let sizes = env_list("INC16_SIZES", &[1_000, 10_000, 100_000]);
    let ks = env_list("INC16_KS", &ALL_KS);
    let mut results: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for &n in &sizes {
        let env = Env::new(&format!("inc16_scale_{n}"), n, &ks, true);
        println!("\n=== N={n} sstables={} ===", env.f.engine.sstable_count());
        for &k in ks.iter().filter(|&&k| k <= n) {
            let r = run_query(&env, &format!("SELECT * FROM ix WHERE g{k} = 'g0'"));
            report(&format!("idx_eq        N={n} K={k}"), &r);
            results
                .entry(format!("idx_eq K={k}"))
                .or_default()
                .push(format!("N={n}: p50={:.3}", r.lat.pct(0.5)));
            let lo = (n / 3) as i64;
            let r = run_query(
                &env,
                &format!("SELECT * FROM ix WHERE r >= {lo} AND r < {}", lo + k as i64),
            );
            report(&format!("idx_range     N={n} K={k}"), &r);
            let r = run_query(
                &env,
                &format!(
                    "SELECT * FROM ix WHERE id >= {lo} AND id < {}",
                    lo + k as i64
                ),
            );
            report(&format!("pk_range      N={n} K={k}"), &r);
            if n <= 100_000 {
                let r = run_query(
                    &env,
                    &format!(
                        "SELECT * FROM ix WHERE id + 0 >= {lo} AND id + 0 < {}",
                        lo + k as i64
                    ),
                );
                report(&format!("seqscan(sel.) N={n} K={k}"), &r);
            }
        }
        let r = run_query(&env, &format!("SELECT * FROM ix WHERE id = {}", n / 2));
        report(&format!("pk_eq         N={n} K=1"), &r);
        env.cleanup();
    }
    println!("\n--- fixed-K summary (idx_eq p50 ms by N) ---");
    for (k, v) in results {
        println!("{k}: {}", v.join("  "));
    }
}

/// Concurrency matrix: 1..32 threads issuing indexed-equality (K=100)
/// and PK-equality reads against a shared table.
#[test]
#[ignore]
fn concurrency_matrix() {
    let n: usize = env_list("INC16_SIZES", &[100_000])[0];
    let env = Arc::new(Env::new("inc16_conc", n, &[100, 1_000], false));
    for (label, sql) in [
        (
            "idx_eq K=100",
            "SELECT * FROM ix WHERE g100 = 'g0'".to_string(),
        ),
        (
            "idx_eq K=1000",
            "SELECT * FROM ix WHERE g1000 = 'g0'".to_string(),
        ),
        ("pk_eq", format!("SELECT * FROM ix WHERE id = {}", n / 2)),
    ] {
        let plan = Arc::new(env.prepare(&sql));
        for &conc in &[1usize, 2, 4, 8, 16, 32] {
            let ops_each = if label == "pk_eq" { 400 } else { 30 };
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
            let threads = thread_count();
            let all: Vec<Duration> = handles
                .into_iter()
                .flat_map(|h| h.join().unwrap())
                .collect();
            let wall = wall0.elapsed();
            let ps = proc_sample();
            let lat = Lat(all);
            println!(
                "{label:<14} conc={conc:<3} p50={:>8.3} p95={:>8.3} p99={:>8.3} max={:>8.3} ms | {:>9.1} op/s cpu={:>8.1}ms rss={:>5.0}MB peak={:>5.0}MB threads={threads} handles={}",
                lat.pct(0.5), lat.pct(0.95), lat.pct(0.99), lat.max(),
                (conc * ops_each) as f64 / wall.as_secs_f64(),
                ps.cpu_ms - cpu0, ps.rss_mb, ps.peak_rss_mb, ps.handles,
            );
        }
    }
    if let Ok(env) = Arc::try_unwrap(env) {
        env.cleanup();
    }
}

/// Candidate-architecture costs (measured, not implemented): disk
/// amplification of the existing index vs. the table, a covering-index
/// design's size and write cost (simulated by writing the full row
/// bytes as the index entry's value -- the exact extra bytes a
/// covering index would persist), its read ceiling (a contiguous
/// decode-only scan of K rows), and a parallel-prefetch fetch of the
/// same K point reads.
#[test]
#[ignore]
fn candidate_costs() {
    use rubixdb::lsm::WriteOp;
    use rubixdb::relational::index_key::index_entry_range;
    use rubixdb::relational::key::table_row_range;
    let n: usize = env_list("INC16_SIZES", &[100_000])[0];
    let env = Env::new("inc16_cand", n, &[1_000], false);
    let eng = Arc::clone(&env.f.engine);
    fn bref(b: &Bound<Vec<u8>>) -> Bound<&[u8]> {
        match b {
            Bound::Included(v) => Bound::Included(v.as_slice()),
            Bound::Excluded(v) => Bound::Excluded(v.as_slice()),
            Bound::Unbounded => Bound::Unbounded,
        }
    }
    // Disk amplification: bytes of table rows vs. one index's entries.
    let (ts, te) = table_row_range(env.table_id);
    let (mut tb, mut tn) = (0usize, 0usize);
    for e in eng.range_scan(bref(&ts), bref(&te), u64::MAX) {
        let (k, v) = e.unwrap();
        tb += k.len() + v.len();
        tn += 1;
    }
    let iid = env.index_id("ix_g1000");
    let (is, ie) = index_entry_range(env.table_id, iid);
    let (mut ib, mut inn) = (0usize, 0usize);
    for e in eng.range_scan(bref(&is), bref(&ie), u64::MAX) {
        let (k, v) = e.unwrap();
        ib += k.len() + v.len();
        inn += 1;
    }
    println!(
        "table: {tn} rows {tb} B ({:.1} B/row) | one secondary index: {inn} entries {ib} B ({:.1} B/entry, {:.0}% of table) | covering (full row as value) would add ~{} B/entry => index {:.0}% of table",
        tb as f64 / tn as f64,
        ib as f64 / inn as f64,
        100.0 * ib as f64 / tb as f64,
        (tb / tn),
        100.0 * (ib as f64 + (tb as f64)) / tb as f64,
    );

    // Write amplification: time + WAL-bound ops for inserting 20K rows
    // into a table with 0 / 1 / 6 secondary indexes vs 1 covering.
    for (label, ks) in [
        ("0 idx", vec![]),
        ("1 idx", vec![1_000usize]),
        ("6 idx", vec![1, 10, 100, 1_000, 10_000]),
    ] {
        let e2 = Env::new(
            &format!("inc16_wa_{}", ks.len()),
            1_000,
            &ks,
            !ks.is_empty(),
        );
        let mut chunk = Vec::new();
        let t0 = Instant::now();
        let cpu0 = proc_sample().cpu_ms;
        for id in 1_000..21_000usize {
            let mut row: Vec<Option<RelationalValue>> = vec![
                Some(RelationalValue::Integer(id as i32)),
                Some(RelationalValue::Integer(id as i32)),
            ];
            for k in ALL_KS {
                row.push(Some(RelationalValue::Text(format!(
                    "g{}",
                    id % (1000 / k.min(1000)).max(1)
                ))));
            }
            row.push(Some(RelationalValue::Text("pad-pad-pad-pad".into())));
            chunk.push(row);
            if chunk.len() == 500 {
                e2.store.put_rows(e2.table_id, &chunk).unwrap();
                chunk.clear();
            }
        }
        println!(
            "write 20K rows, {label}{}: {:.0} ms, cpu {:.0} ms",
            if ks.is_empty() { "" } else { " (+r idx)" },
            t0.elapsed().as_secs_f64() * 1000.0,
            proc_sample().cpu_ms - cpu0
        );
        e2.cleanup();
    }
    // Covering-index write cost: an index entry carrying the row bytes.
    {
        let e2 = Env::new("inc16_wa_cov", 1_000, &[1_000], false);
        let t0 = Instant::now();
        let payload = vec![0u8; (tb / tn).max(1)];
        let (s2, _) = index_entry_range(e2.table_id, e2.index_id("ix_g1000"));
        let base = match s2 {
            Bound::Included(v) => v,
            _ => unreachable!(),
        };
        let mut ops = Vec::new();
        for id in 0..20_000u32 {
            let mut k = base.clone();
            k.extend_from_slice(b"covering-sim-");
            k.extend_from_slice(&id.to_be_bytes());
            ops.push(WriteOp::Put {
                key: k,
                value: payload.clone(),
            });
            if ops.len() == 500 {
                e2.f.engine.write_batch(&ops).unwrap();
                ops.clear();
            }
        }
        println!(
            "write 20K covering-sim index entries ({} B value): {:.0} ms (extra, on top of row+index writes)",
            payload.len(),
            t0.elapsed().as_secs_f64() * 1000.0
        );
        e2.cleanup();
    }

    // Parallel prefetch vs sequential point reads for K=10_000 keys.
    let iid = env.index_id("ix_g1000");
    let _ = iid;
    let ids: Vec<i32> = (0..10_000).map(|i| (i * 7 % n) as i32).collect();
    let keys: Vec<Vec<u8>> = ids
        .iter()
        .map(|&i| {
            rubixdb::relational::key::table_row_key(
                env.table_id,
                &rubixdb::relational::key::encode_composite_key(&[RelationalValue::Integer(i)])
                    .unwrap(),
            )
        })
        .collect();
    let seq = median_ms(7, || {
        for k in &keys {
            eng.get_as_of(k, u64::MAX).unwrap().unwrap();
        }
    });
    for threads in [2usize, 4, 8] {
        let par = median_ms(7, || {
            let chunk = keys.len().div_ceil(threads);
            std::thread::scope(|s| {
                for part in keys.chunks(chunk) {
                    let eng = &eng;
                    s.spawn(move || {
                        for k in part {
                            eng.get_as_of(k, u64::MAX).unwrap().unwrap();
                        }
                    });
                }
            });
        });
        println!("10K point reads: sequential {seq:.1} ms vs {threads}-thread prefetch {par:.1} ms (spawn included)");
    }
    // Covering read ceiling: decode-only contiguous scan of K rows.
    for k in [100usize, 1_000, 10_000] {
        let r = run_query(
            &env,
            &format!("SELECT * FROM ix WHERE id >= 1000 AND id < {}", 1000 + k),
        );
        report(&format!("covering-ceiling(pk contiguous) K={k}"), &r);
    }
    env.cleanup();
}

/// Shared-infrastructure regression matrix (run on the same build before
/// and after the Increment 16 change): PK equality/range, SeqScan, JOIN
/// and aggregation over an index predicate, plus UPDATE and DELETE that
/// locate their targets through a secondary index.
#[test]
#[ignore]
fn regression_matrix() {
    use crate::exec::write::{execute_write_autocommit, WriteMetrics};
    let n: usize = env_list("INC16_SIZES", &[100_000])[0];
    let env = Env::new("inc16_reg", n, &[10, 100, 1_000], true);
    println!("\n=== regression matrix N={n} ===");
    let selects: [(&str, String); 9] = [
        ("pk_eq", format!("SELECT * FROM ix WHERE id = {}", n / 2)),
        (
            "pk_range K=100",
            "SELECT * FROM ix WHERE id >= 5000 AND id < 5100".to_string(),
        ),
        (
            "idx_range K=100",
            "SELECT * FROM ix WHERE r >= 5000 AND r < 5100".to_string(),
        ),
        (
            "seqscan full (COUNT)",
            "SELECT COUNT(*) FROM ix WHERE pad = 'pad-pad-pad-pad'".to_string(),
        ),
        (
            "join idx-pred K=100",
            "SELECT a.id, b.r FROM ix a JOIN ix b ON a.id = b.id WHERE a.g100 = 'g0'".to_string(),
        ),
        (
            "agg COUNT/SUM idx K=1000",
            "SELECT COUNT(*), SUM(r) FROM ix WHERE g1000 = 'g0'".to_string(),
        ),
        (
            "group by/having idx K=1000",
            "SELECT g100, COUNT(*) FROM ix WHERE g1000 = 'g0' GROUP BY g100 HAVING COUNT(*) > 0"
                .to_string(),
        ),
        (
            "idx_eq K=1000 (SELECT *)",
            "SELECT * FROM ix WHERE g1000 = 'g0'".to_string(),
        ),
        (
            "idx_eq K=1000 (id only)",
            "SELECT id FROM ix WHERE g1000 = 'g0'".to_string(),
        ),
    ];
    for (label, sql) in &selects {
        let r = run_query(&env, sql);
        report(label, &r);
    }
    let w = |sql: &str| -> u64 {
        execute_write_autocommit(
            &env.prepare(sql),
            &env.txm,
            &env.store,
            &env.catalog,
            &env.builder,
            &[],
            &ExecLimits::default(),
            &WriteMetrics::default(),
            &CancellationToken::new(),
        )
        .unwrap()
        .rows_affected
    };
    // UPDATE via index predicate (K=100 rows each, indexed column g100 itself updated back and forth)
    let mut lat = Vec::new();
    for i in 0..30 {
        let s = Instant::now();
        let n1 = w(&format!(
            "UPDATE ix SET pad = 'u{i}' WHERE g100 = 'g{}'",
            i % 10
        ));
        lat.push(s.elapsed());
        assert_eq!(n1, 100);
    }
    let l = Lat(lat);
    println!(
        "{:<34} rows=100    p50={:>8.3} p95={:>8.3} p99={:>8.3} max={:>8.3} ms",
        "UPDATE via idx K=100 (non-idx col)",
        l.pct(0.5),
        l.pct(0.95),
        l.pct(0.99),
        l.max()
    );
    let mut lat = Vec::new();
    for i in 0..30 {
        let s = Instant::now();
        let n1 = w(&format!(
            "UPDATE ix SET g100 = 'moved{i}' WHERE g10 = 'g{}'",
            i
        ));
        lat.push(s.elapsed());
        assert_eq!(n1, 10);
    }
    let l = Lat(lat);
    println!(
        "{:<34} rows=10     p50={:>8.3} p95={:>8.3} p99={:>8.3} max={:>8.3} ms",
        "UPDATE indexed col via idx K=10",
        l.pct(0.5),
        l.pct(0.95),
        l.pct(0.99),
        l.max()
    );
    let mut lat = Vec::new();
    for i in 0..30 {
        let s = Instant::now();
        let n1 = w(&format!("DELETE FROM ix WHERE g10 = 'g{}'", 100 + i));
        lat.push(s.elapsed());
        assert_eq!(n1, 10);
    }
    let l = Lat(lat);
    println!(
        "{:<34} rows=10     p50={:>8.3} p95={:>8.3} p99={:>8.3} max={:>8.3} ms",
        "DELETE via idx K=10",
        l.pct(0.5),
        l.pct(0.95),
        l.pct(0.99),
        l.max()
    );
    let mut lat = Vec::new();
    for i in 0..10 {
        let s = Instant::now();
        let n1 = w(&format!("DELETE FROM ix WHERE g1000 = 'g{}'", 50 + i));
        lat.push(s.elapsed());
        assert_eq!(n1, 1000);
    }
    let l = Lat(lat);
    println!(
        "{:<34} rows=1000   p50={:>8.3} p95={:>8.3} p99={:>8.3} max={:>8.3} ms",
        "DELETE via idx K=1000",
        l.pct(0.5),
        l.pct(0.95),
        l.pct(0.99),
        l.max()
    );
    env.cleanup();
}

/// Replay of the Blocker 9 `indexed_select` shape: the endurance
/// schema (`id INTEGER PK, grp TEXT, val INTEGER, v TEXT`), `grp = g{id
/// % 10}` (10 groups => the predicate matches ~1/10 of the table, so the
/// MATCHED-ROW count grows with the table, exactly as in the recorded
/// 277.8ms -> 783.7ms -> 1,148.1ms drift), at the same three table
/// sizes. Run on the same build before/after the change.
#[test]
#[ignore]
fn endurance_shape_replay() {
    let sizes = env_list("INC16_SIZES", &[105_000, 155_000, 206_000]);
    for &n in &sizes {
        let f = bench_fixture(&format!("inc16_replay_{n}"));
        let catalog = Arc::new(CatalogService::new(Arc::clone(&f.engine)));
        let table_id = catalog
            .create_table(
                f.ctx.default_schema_id,
                "rp",
                &[
                    col("id", TYPE_TAG_INTEGER, false),
                    col("grp", TYPE_TAG_TEXT, true),
                    col("val", TYPE_TAG_INTEGER, true),
                    col("v", TYPE_TAG_TEXT, true),
                ],
                &[0],
            )
            .unwrap();
        let store = Arc::new(TableStore::new(Arc::clone(&f.engine), Arc::clone(&catalog)));
        let builder = Arc::new(IndexBuilder::new(
            Arc::clone(&f.engine),
            Arc::clone(&catalog),
            Arc::clone(&store),
        ));
        let txm = Arc::new(TransactionManager::new(
            Arc::clone(&f.engine),
            Arc::clone(&store),
        ));
        let mut chunk = Vec::new();
        for id in 0..n {
            chunk.push(vec![
                Some(RelationalValue::Integer(id as i32)),
                Some(RelationalValue::Text(format!("g{}", id % 10))),
                Some(RelationalValue::Integer(id as i32)),
                Some(RelationalValue::Text("w1".to_string())),
            ]);
            if chunk.len() == 2000 {
                store.put_rows(table_id, &chunk).unwrap();
                chunk.clear();
            }
        }
        if !chunk.is_empty() {
            store.put_rows(table_id, &chunk).unwrap();
        }
        builder
            .create_index_online(table_id, "idx_rp_grp", IndexKind::NonUnique, &[1])
            .unwrap();
        let env = Env {
            f,
            catalog,
            store,
            builder,
            txm,
            n,
            table_id,
        };
        let r = run_query(&env, "SELECT * FROM rp WHERE grp = 'g3'");
        report(&format!("indexed_select replay N={n}"), &r);
        env.cleanup();
    }
}

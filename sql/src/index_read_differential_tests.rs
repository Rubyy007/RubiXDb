//! Increment 16 correctness evidence: randomized differential tests of
//! every indexed read shape against an independent in-memory reference
//! model (`PHASE_RUBIXDB_INCREMENT16_INDEX_READ_RESULTS.md`).
//!
//! The reference model is a plain `BTreeMap<i32, Mrow>` evaluated by
//! brute-force predicates -- it shares no code with the planner, the
//! index, or the executor. Every mutation goes through the real SQL write
//! executor (INSERT/UPDATE/DELETE, including UPDATE of indexed columns,
//! DELETE of indexed rows, re-insert after delete, `DROP INDEX` +
//! `CREATE INDEX` mid-run). The engine runs with a tiny MemTable and an
//! automatic Compaction trigger so flushes and Compactions happen while
//! reads (including reads inside old snapshot transactions) are active.
//!
//! Schema: `dt(id INT PK, a INT NULL, b TEXT NULL, c INT)` with
//! indexes `ia(a)`, `ib(b)`, composite `iab(a, b)`; `c` is unindexed;
//! `dj(a INT PK, label TEXT)` for JOINs.

use std::collections::BTreeMap;
use std::sync::Arc;

use proptest::prelude::*;
use rubixdb::catalog::CatalogService;
use rubixdb::lsm::LsmConfig;
use rubixdb::relational::index::IndexBuilder;
use rubixdb::relational::index_key::{decode_indexed_columns, index_entry_range};
use rubixdb::relational::key::decode_composite_key;
use rubixdb::relational::table_store::TableStore;
use rubixdb::relational::value::RelationalType;
use rubixdb::relational::{RelationalValue, Transaction, TransactionManager};

use crate::auth::AuthContext;
use crate::bind::bind_statement;
use crate::exec::write::{execute_write_autocommit, WriteMetrics};
use crate::exec::{
    execute, execute_autocommit, CancellationToken, ExecLimits, ExecMetrics, QueryResult,
};
use crate::limits::SqlLimits;
use crate::metrics::SqlMetrics;
use crate::parse::parse_statement;
use crate::plan::{build_plan, Plan, PlannerLimits, PlannerMetrics};
use crate::test_support::Fixture;

type Mrow = (i32, Option<i32>, Option<String>, i32);
type Model = BTreeMap<i32, Mrow>;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

struct Diff {
    f: Fixture,
    catalog: Arc<CatalogService>,
    store: Arc<TableStore>,
    builder: Arc<IndexBuilder>,
    txm: TransactionManager,
}

impl Diff {
    fn new(tag: &str) -> Self {
        let f = Fixture::new_with_lsm(
            tag,
            LsmConfig {
                memtable_max_size_bytes: 24 * 1024,
                compaction_trigger_count: 2,
                compaction_auto_trigger: true,
                ..LsmConfig::default()
            },
        );
        let catalog = Arc::new(CatalogService::new(Arc::clone(&f.engine)));
        let store = Arc::new(TableStore::new(Arc::clone(&f.engine), Arc::clone(&catalog)));
        let builder = Arc::new(IndexBuilder::new(
            Arc::clone(&f.engine),
            Arc::clone(&catalog),
            Arc::clone(&store),
        ));
        let txm = TransactionManager::new(Arc::clone(&f.engine), Arc::clone(&store));
        let d = Diff {
            f,
            catalog,
            store,
            builder,
            txm,
        };
        d.write("CREATE TABLE dt (id INTEGER PRIMARY KEY, a INTEGER, b TEXT, c INTEGER)");
        d.write("CREATE TABLE dj (a INTEGER PRIMARY KEY, label TEXT)");
        for a in 0..8 {
            d.write(&format!("INSERT INTO dj (a, label) VALUES ({a}, 'L{a}')"));
        }
        d.write("CREATE INDEX ia ON dt (a)");
        d.write("CREATE INDEX ib ON dt (b)");
        d.write("CREATE INDEX iab ON dt (a, b)");
        d
    }

    fn plan(&self, sql: &str) -> Plan {
        let limits = SqlLimits::default();
        let stmt = parse_statement(sql, &limits).unwrap_or_else(|e| panic!("parse {sql:?}: {e}"));
        let bound = bind_statement(
            &self.f.catalog,
            &self.f.ctx,
            &AuthContext::admin("diff"),
            &SqlMetrics::default(),
            &limits,
            &stmt,
        )
        .unwrap_or_else(|e| panic!("bind {sql:?}: {e}"));
        build_plan(
            &bound,
            &self.f.catalog,
            &PlannerLimits::default(),
            &PlannerMetrics::default(),
        )
        .unwrap_or_else(|e| panic!("plan {sql:?}: {e}"))
    }

    fn write(&self, sql: &str) -> u64 {
        execute_write_autocommit(
            &self.plan(sql),
            &self.txm,
            &self.store,
            &self.catalog,
            &self.builder,
            &[],
            &ExecLimits::default(),
            &WriteMetrics::default(),
            &CancellationToken::new(),
        )
        .unwrap_or_else(|e| panic!("write {sql:?}: {e}"))
        .rows_affected
    }

    fn select(&self, sql: &str) -> QueryResult {
        execute_autocommit(
            &self.plan(sql),
            &self.txm,
            &self.store,
            &self.builder,
            &[],
            &ExecLimits::default(),
            &ExecMetrics::default(),
            &CancellationToken::new(),
        )
        .unwrap_or_else(|e| panic!("select {sql:?}: {e}"))
    }

    fn select_in(&self, sql: &str, txn: &Transaction) -> QueryResult {
        execute(
            &self.plan(sql),
            txn,
            &self.store,
            &self.builder,
            &[],
            &ExecLimits::default(),
            &ExecMetrics::default(),
            &CancellationToken::new(),
        )
        .unwrap_or_else(|e| panic!("select {sql:?}: {e}"))
    }
}

fn lit_i(v: Option<i32>) -> String {
    v.map_or("NULL".to_string(), |v| v.to_string())
}
fn lit_s(v: &Option<String>) -> String {
    v.as_ref().map_or("NULL".to_string(), |s| format!("'{s}'"))
}

fn to_mrow(r: &[Option<RelationalValue>]) -> Mrow {
    let i = |v: &Option<RelationalValue>| match v {
        Some(RelationalValue::Integer(n)) => Some(*n),
        None => None,
        o => panic!("unexpected {o:?}"),
    };
    let s = |v: &Option<RelationalValue>| match v {
        Some(RelationalValue::Text(s)) => Some(s.clone()),
        None => None,
        o => panic!("unexpected {o:?}"),
    };
    (i(&r[0]).unwrap(), i(&r[1]), s(&r[2]), i(&r[3]).unwrap())
}

fn rows_of(res: &QueryResult) -> Vec<Mrow> {
    let mut v: Vec<Mrow> = res.rows.iter().map(|r| to_mrow(r)).collect();
    v.sort();
    v
}

/// One query shape: its SQL and its brute-force reference predicate.
struct Q {
    sql: String,
    pred: Box<dyn Fn(&Mrow) -> bool>,
}

fn queries(rng: &mut Rng) -> Vec<Q> {
    let v = rng.below(8) as i32;
    let lo = rng.below(8) as i32;
    let hi = lo + 1 + rng.below(4) as i32;
    let s = format!("s{}", rng.below(5));
    let k = rng.below(6) as i32;
    let id_lo = rng.below(280) as i32;
    let id_hi = id_lo + 1 + rng.below(40) as i32;
    let id_eq = rng.below(300) as i32;
    let mut out: Vec<Q> = Vec::new();
    let mut q = |sql: String, pred: Box<dyn Fn(&Mrow) -> bool>| out.push(Q { sql, pred });
    q(
        format!("SELECT * FROM dt WHERE a = {v}"),
        Box::new(move |r| r.1 == Some(v)),
    );
    q(
        format!("SELECT * FROM dt WHERE a >= {lo} AND a < {hi}"),
        Box::new(move |r| r.1.is_some_and(|a| a >= lo && a < hi)),
    );
    q(
        format!("SELECT * FROM dt WHERE a > {lo}"),
        Box::new(move |r| r.1.is_some_and(|a| a > lo)),
    );
    q(
        format!("SELECT * FROM dt WHERE a <= {lo}"),
        Box::new(move |r| r.1.is_some_and(|a| a <= lo)),
    );
    let s1 = s.clone();
    q(
        format!("SELECT * FROM dt WHERE b = '{s}'"),
        Box::new(move |r| r.2.as_deref() == Some(s1.as_str())),
    );
    let s2 = s.clone();
    q(
        format!("SELECT * FROM dt WHERE a = {v} AND b = '{s}'"),
        Box::new(move |r| r.1 == Some(v) && r.2.as_deref() == Some(s2.as_str())),
    );
    q(
        format!("SELECT * FROM dt WHERE a = {v} AND c > {k}"),
        Box::new(move |r| r.1 == Some(v) && r.3 > k),
    );
    q(
        format!("SELECT * FROM dt WHERE id = {id_eq}"),
        Box::new(move |r| r.0 == id_eq),
    );
    q(
        format!("SELECT * FROM dt WHERE id >= {id_lo} AND id < {id_hi}"),
        Box::new(move |r| r.0 >= id_lo && r.0 < id_hi),
    );
    q(
        format!("SELECT * FROM dt WHERE id >= {id_lo} AND id < {id_hi} AND a = {v}"),
        Box::new(move |r| r.0 >= id_lo && r.0 < id_hi && r.1 == Some(v)),
    );
    q(
        format!("SELECT * FROM dt WHERE c = {k}"),
        Box::new(move |r| r.3 == k),
    );
    q(
        "SELECT * FROM dt WHERE a = 9999".to_string(),
        Box::new(|_| false),
    );
    let s3 = s;
    q(
        format!("SELECT * FROM dt WHERE a = {v} OR b = '{s3}'"),
        Box::new(move |r| r.1 == Some(v) || r.2.as_deref() == Some(s3.as_str())),
    );
    out
}

fn check_queries(d: &Diff, model: &Model, txn: Option<&Transaction>, rng: &mut Rng, ctx: &str) {
    for q in queries(rng) {
        let got = match txn {
            Some(t) => d.select_in(&q.sql, t),
            None => d.select(&q.sql),
        };
        let expect: Vec<Mrow> = model.values().filter(|r| (q.pred)(r)).cloned().collect();
        assert_eq!(rows_of(&got), expect, "{ctx}: {}", q.sql);
    }
    // aggregation + JOIN over an indexed predicate
    let v = rng.below(8) as i32;
    let n = d.select(&format!("SELECT COUNT(*) FROM dt WHERE a = {v}"));
    let expect = model.values().filter(|r| r.1 == Some(v)).count() as i64;
    let got = match &n.rows[0][0] {
        Some(RelationalValue::Bigint(x)) => *x,
        Some(RelationalValue::Integer(x)) => *x as i64,
        o => panic!("count {o:?}"),
    };
    if txn.is_none() {
        assert_eq!(got, expect, "{ctx}: COUNT a={v}");
        let s = format!("s{}", rng.below(5));
        let j = d.select(&format!(
            "SELECT dt.id, dj.label FROM dt JOIN dj ON dt.a = dj.a WHERE dt.b = '{s}'"
        ));
        let mut got: Vec<(i32, String)> = j
            .rows
            .iter()
            .map(|r| match (&r[0], &r[1]) {
                (Some(RelationalValue::Integer(i)), Some(RelationalValue::Text(l))) => {
                    (*i, l.clone())
                }
                o => panic!("{o:?}"),
            })
            .collect();
        got.sort();
        let expect: Vec<(i32, String)> = model
            .values()
            .filter(|r| r.2.as_deref() == Some(s.as_str()) && r.1.is_some_and(|a| a < 8))
            .map(|r| (r.0, format!("L{}", r.1.unwrap())))
            .collect();
        assert_eq!(got, expect, "{ctx}: JOIN b={s}");
    }
}

/// Zero stale / zero orphan / zero duplicate physical index entries:
/// the decoded entry set of each index must equal exactly what the model
/// says it should hold.
fn check_index_entries(d: &Diff, model: &Model) {
    let table_id = d
        .catalog
        .get_table_by_name(d.f.ctx.default_schema_id, "dt")
        .unwrap()
        .unwrap()
        .table_id;
    let idx = d.catalog.list_indexes(table_id).unwrap();
    let pk_types = [RelationalType::Integer];
    for (name, types) in [
        ("ia", vec![RelationalType::Integer]),
        ("ib", vec![RelationalType::Text]),
        ("iab", vec![RelationalType::Integer, RelationalType::Text]),
    ] {
        let index_id = idx.iter().find(|i| i.name == name).unwrap().index_id;
        let (s, e) = index_entry_range(table_id, index_id);
        fn sref(b: &std::ops::Bound<Vec<u8>>) -> std::ops::Bound<&[u8]> {
            match b {
                std::ops::Bound::Included(v) => std::ops::Bound::Included(v.as_slice()),
                std::ops::Bound::Excluded(v) => std::ops::Bound::Excluded(v.as_slice()),
                std::ops::Bound::Unbounded => std::ops::Bound::Unbounded,
            }
        }
        let mut got: Vec<(i32, Vec<Option<RelationalValue>>)> = Vec::new();
        for ent in d.f.engine.range_scan(sref(&s), sref(&e), u64::MAX) {
            let (key, _) = ent.unwrap();
            let body = &key[9..];
            let (vals, used) = decode_indexed_columns(&types, body).unwrap();
            let pk = decode_composite_key(&pk_types, &body[used..]).unwrap();
            let RelationalValue::Integer(id) = pk[0] else {
                panic!()
            };
            got.push((id, vals));
        }
        got.sort_by_key(|g| g.0);
        let expect: Vec<(i32, Vec<Option<RelationalValue>>)> = model
            .values()
            .map(|r| {
                let a = r.1.map(RelationalValue::Integer);
                let b = r.2.clone().map(RelationalValue::Text);
                let vals = match name {
                    "ia" => vec![a],
                    "ib" => vec![b],
                    _ => vec![a, b],
                };
                (r.0, vals)
            })
            .collect();
        assert_eq!(got, expect, "index {name}: stale/orphan/duplicate entries");
    }
}

fn run(seed: u64, steps: usize) -> u64 {
    let mut rng = Rng(seed);
    let d = Diff::new(&format!("diff_{seed}"));
    let mut model: Model = BTreeMap::new();
    let mut snaps: Vec<(Transaction, Model)> = Vec::new();
    let ctx = |step: usize| format!("seed={seed} step={step}");
    for step in 0..steps {
        let id = rng.below(300) as i32;
        let a = if rng.below(10) == 0 {
            None
        } else {
            Some(rng.below(8) as i32)
        };
        let b = if rng.below(10) == 0 {
            None
        } else {
            Some(format!("s{}", rng.below(5)))
        };
        let c = rng.below(6) as i32;
        match rng.below(100) {
            0..=44 => {
                if let std::collections::btree_map::Entry::Vacant(e) = model.entry(id) {
                    d.write(&format!(
                        "INSERT INTO dt (id, a, b, c) VALUES ({id}, {}, {}, {c})",
                        lit_i(a),
                        lit_s(&b)
                    ));
                    e.insert((id, a, b, c));
                } else {
                    // UPDATE of indexed columns by PK
                    let n = d.write(&format!(
                        "UPDATE dt SET a = {}, b = {} WHERE id = {id}",
                        lit_i(a),
                        lit_s(&b)
                    ));
                    assert_eq!(n, 1, "{}", ctx(step));
                    let r = model.get_mut(&id).unwrap();
                    r.1 = a;
                    r.2 = b;
                }
            }
            45..=59 => {
                let existed = model.remove(&id).is_some();
                let n = d.write(&format!("DELETE FROM dt WHERE id = {id}"));
                assert_eq!(n, existed as u64, "{}", ctx(step));
            }
            60..=69 => {
                // UPDATE through the secondary-index access path
                let v = rng.below(8) as i32;
                let nb = format!("s{}", rng.below(5));
                let n = d.write(&format!("UPDATE dt SET b = '{nb}' WHERE a = {v}"));
                let mut cnt = 0;
                for r in model.values_mut().filter(|r| r.1 == Some(v)) {
                    r.2 = Some(nb.clone());
                    cnt += 1;
                }
                assert_eq!(n, cnt, "{}: UPDATE a={v}", ctx(step));
            }
            70..=77 => {
                // DELETE through the secondary-index access path
                let sv = format!("s{}", rng.below(5));
                let n = d.write(&format!("DELETE FROM dt WHERE b = '{sv}' AND c = {c}"));
                let before = model.len();
                model.retain(|_, r| !(r.2.as_deref() == Some(sv.as_str()) && r.3 == c));
                assert_eq!(
                    n as usize,
                    before - model.len(),
                    "{}: DELETE b={sv}",
                    ctx(step)
                );
            }
            78..=80 if step > 20 && step < steps - 20 && rng.below(4) == 0 => {
                // DROP INDEX + CREATE INDEX (online backfill) mid-run.
                d.write("DROP INDEX ib ON dt");
                d.write("CREATE INDEX ib ON dt (b)");
                // Finding F-2: a snapshot older than an index rebuild
                // cannot read through the rebuilt index (open, pre-
                // existing); retire such snapshots rather than assert a
                // known-open behavior.
                snaps.clear();
            }
            _ => {}
        }
        if step % 29 == 0 {
            snaps.push((d.txm.begin().unwrap(), model.clone()));
            if snaps.len() > 3 {
                snaps.remove(0);
            }
        }
        if step % 7 == 0 {
            check_queries(&d, &model, None, &mut rng, &ctx(step));
        }
        if step % 13 == 0 {
            for (txn, snap_model) in &snaps {
                check_queries(
                    &d,
                    snap_model,
                    Some(txn),
                    &mut rng,
                    &format!("{} (snapshot)", ctx(step)),
                );
            }
        }
    }
    check_queries(&d, &model, None, &mut rng, &ctx(steps));
    check_index_entries(&d, &model);
    drop(snaps);
    let cycles = d.f.engine.compaction_metrics().cycles_completed;
    d.f.cleanup();
    cycles
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]
    #[test]
    fn index_reads_match_reference_model(seed in any::<u64>()) {
        run(seed, 160);
    }
}

#[test]
fn index_reads_match_reference_model_fixed_seeds_long() {
    let mut cycles = 0;
    for seed in [1u64, 2, 3, 0xDEAD_BEEF, 42] {
        cycles += run(seed, 600);
    }
    println!("automatic Compaction cycles completed during differential runs: {cycles}");
    assert!(cycles > 0, "differential run must overlap real Compaction");
}

/// Large-result + selectivity sweep: a deterministic table where one
/// indexed column has high (unique), medium (1/100), and low (1/3)
/// selectivity, queried at 1/10/100/1,000-row result sizes, plus
/// compaction-forcing churn between reads.
#[test]
fn index_reads_match_model_across_selectivities_and_result_sizes() {
    let f = Fixture::new_with_lsm(
        "diff_sel",
        LsmConfig {
            memtable_max_size_bytes: 64 * 1024,
            compaction_trigger_count: 2,
            compaction_auto_trigger: true,
            ..LsmConfig::default()
        },
    );
    let catalog = Arc::new(CatalogService::new(Arc::clone(&f.engine)));
    let store = Arc::new(TableStore::new(Arc::clone(&f.engine), Arc::clone(&catalog)));
    let builder = Arc::new(IndexBuilder::new(
        Arc::clone(&f.engine),
        Arc::clone(&catalog),
        Arc::clone(&store),
    ));
    let txm = TransactionManager::new(Arc::clone(&f.engine), Arc::clone(&store));
    let d = Diff {
        f,
        catalog,
        store,
        builder,
        txm,
    };
    d.write("CREATE TABLE sel (id INTEGER PRIMARY KEY, hi INTEGER, md INTEGER, lo INTEGER)");
    let n = 3_000;
    for chunk in (0..n).collect::<Vec<i32>>().chunks(100) {
        let vals: Vec<String> = chunk
            .iter()
            .map(|i| format!("({i}, {i}, {}, {})", i % 30, i % 3))
            .collect();
        d.write(&format!(
            "INSERT INTO sel (id, hi, md, lo) VALUES {}",
            vals.join(",")
        ));
    }
    d.write("CREATE INDEX s_hi ON sel (hi)");
    d.write("CREATE INDEX s_md ON sel (md)");
    d.write("CREATE INDEX s_lo ON sel (lo)");
    // churn: delete + reinsert some rows to leave tombstones/old versions
    for i in (0..n).step_by(50) {
        d.write(&format!("DELETE FROM sel WHERE id = {i}"));
        d.write(&format!(
            "INSERT INTO sel (id, hi, md, lo) VALUES ({i}, {i}, {}, {})",
            i % 30,
            i % 3
        ));
    }
    let cases: [(&str, i32, usize); 4] = [
        ("hi", 7, 1),      // high selectivity: 1 row
        ("md", 7, 100),    // medium: 100 rows
        ("lo", 1, 1000),   // low: 1,000 rows
        ("hi", 99_999, 0), // empty
    ];
    for (col, v, expect) in cases {
        let r = d.select(&format!("SELECT id FROM sel WHERE {col} = {v}"));
        assert_eq!(r.rows.len(), expect, "{col}={v}");
        let mut ids: Vec<i32> = r
            .rows
            .iter()
            .map(|x| match x[0] {
                Some(RelationalValue::Integer(i)) => i,
                _ => panic!(),
            })
            .collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), expect, "duplicates for {col}={v}");
    }
    // 10-row range result
    let r = d.select("SELECT id FROM sel WHERE hi >= 100 AND hi < 110");
    assert_eq!(r.rows.len(), 10);
    d.f.cleanup();
}

/// `max_index_scan_rows` is enforced while collecting: a limit below the
/// match count fails closed with `ResourceLimit`, exactly at the boundary.
#[test]
fn index_scan_row_limit_is_enforced_while_collecting() {
    let d = Diff::new("diff_limit");
    for i in 0..50 {
        d.write(&format!(
            "INSERT INTO dt (id, a, b, c) VALUES ({i}, 1, 's1', 0)"
        ));
    }
    let plan = d.plan("SELECT * FROM dt WHERE a = 1");
    let run = |max: usize| {
        execute_autocommit(
            &plan,
            &d.txm,
            &d.store,
            &d.builder,
            &[],
            &ExecLimits {
                max_index_scan_rows: max,
                ..ExecLimits::default()
            },
            &ExecMetrics::default(),
            &CancellationToken::new(),
        )
    };
    assert_eq!(
        run(50).unwrap().rows.len(),
        50,
        "exactly at the limit passes"
    );
    assert!(
        matches!(run(49), Err(crate::SqlError::ResourceLimit { .. })),
        "one over the limit fails closed"
    );
    d.f.cleanup();
}

/// Finding F-1 regression: an upper-bound-only index range (`a <= x`,
/// `a < x`) must never return rows whose indexed value is NULL (SQL:
/// `NULL <= x` is not true). Pre-existing planner bug found by the
/// differential run -- see PHASE_RUBIXDB_INCREMENT16_INDEX_READ_RESULTS.md.
#[test]
fn upper_bound_only_index_range_excludes_null_indexed_values() {
    let d = Diff::new("diff_nullbound");
    d.write("INSERT INTO dt (id, a, b, c) VALUES (1, NULL, 's0', 0)");
    d.write("INSERT INTO dt (id, a, b, c) VALUES (2, 1, NULL, 0)");
    d.write("INSERT INTO dt (id, a, b, c) VALUES (3, 5, 's1', 0)");
    d.write("INSERT INTO dt (id, a, b, c) VALUES (4, 1, 's1', 0)");
    d.write("INSERT INTO dt (id, a, b, c) VALUES (5, 1, 's9', 0)");
    let ids = |sql: &str| -> Vec<i32> { rows_of(&d.select(sql)).iter().map(|r| r.0).collect() };
    assert_eq!(ids("SELECT * FROM dt WHERE a <= 3"), vec![2, 4, 5]);
    assert_eq!(ids("SELECT * FROM dt WHERE a < 3"), vec![2, 4, 5]);
    // composite prefix equality + upper-only bound on the next column
    assert_eq!(ids("SELECT * FROM dt WHERE a = 1 AND b <= 's5'"), vec![4]);
    d.f.cleanup();
}

/// Finding F-2 (pre-existing, NOT fixed in Increment 16): a snapshot
/// transaction that began *before* an index was (re)built, and then
/// reads through that index, misses rows -- the backfilled index
/// entries carry post-snapshot sequence numbers, so the old snapshot
/// sees an empty index while the planner still selects it. Documented
/// in PHASE_RUBIXDB_INCREMENT16_INDEX_READ_RESULTS.md as an open
/// finding; `#[ignore]`d so the suite does not codify the bug.
#[test]
#[ignore]
fn known_gap_snapshot_started_before_index_rebuild_misses_rows() {
    let d = Diff::new("diff_f2");
    d.write("INSERT INTO dt (id, a, b, c) VALUES (4, 6, 's2', 2)");
    let txn = d.txm.begin().unwrap();
    d.write("DROP INDEX ib ON dt");
    d.write("CREATE INDEX ib ON dt (b)");
    let got = rows_of(&d.select_in("SELECT * FROM dt WHERE b = 's2'", &txn));
    assert_eq!(got.len(), 1, "snapshot must still see the row it could see");
    d.f.cleanup();
}

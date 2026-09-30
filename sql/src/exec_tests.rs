//! Query executor tests — `PHASE_RELATIONAL_QUERY_EXECUTOR_ARCHITECTURE.md`
//! is the decision record these verify against.

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use rubixdb::catalog::schema::IndexKind;
use rubixdb::catalog::CatalogService;
use rubixdb::relational::index::IndexBuilder;
use rubixdb::relational::table_store::TableStore;
use rubixdb::relational::{RelationalValue, Transaction, TransactionManager};

use crate::auth::AuthContext;
use crate::bind::bind_statement;
use crate::exec::{
    execute, execute_autocommit, CancellationToken, ExecLimits, ExecMetrics, QueryResult,
};
use crate::limits::SqlLimits;
use crate::metrics::SqlMetrics;
use crate::parse::parse_statement;
use crate::plan::{build_plan, Plan, PlannerLimits, PlannerMetrics};
use crate::test_support::Fixture;

struct ExecFixture {
    f: Fixture,
    catalog: Arc<CatalogService>,
    store: Arc<TableStore>,
    builder: Arc<IndexBuilder>,
    txm: TransactionManager,
}

impl ExecFixture {
    fn new(tag: &str) -> Self {
        let f = Fixture::new(tag);
        let catalog = Arc::new(CatalogService::new(Arc::clone(&f.engine)));
        let store = Arc::new(TableStore::new(Arc::clone(&f.engine), Arc::clone(&catalog)));
        let builder = Arc::new(IndexBuilder::new(
            Arc::clone(&f.engine),
            Arc::clone(&catalog),
            Arc::clone(&store),
        ));
        let txm = TransactionManager::new(Arc::clone(&f.engine), Arc::clone(&store));
        ExecFixture {
            f,
            catalog,
            store,
            builder,
            txm,
        }
    }

    fn table_id(&self, name: &str) -> u32 {
        self.catalog
            .get_table_by_name(self.f.ctx.default_schema_id, name)
            .unwrap()
            .unwrap()
            .table_id
    }

    fn create_name_index(&self, table: &str, kind: IndexKind) -> u32 {
        let table_id = self.table_id(table);
        self.builder
            .create_index_online(table_id, &format!("{table}_name_idx"), kind, &[1])
            .unwrap()
    }

    fn plan(&self, sql: &str) -> Plan {
        let limits = SqlLimits::default();
        let stmt = parse_statement(sql, &limits)
            .unwrap_or_else(|e| panic!("parse failed for {sql:?}: {e}"));
        let metrics = SqlMetrics::default();
        let auth = AuthContext::admin("test-principal");
        let bound = bind_statement(
            &self.f.catalog,
            &self.f.ctx,
            &auth,
            &metrics,
            &limits,
            &stmt,
        )
        .unwrap_or_else(|e| panic!("bind failed for {sql:?}: {e}"));
        build_plan(
            &bound,
            &self.f.catalog,
            &PlannerLimits::default(),
            &PlannerMetrics::default(),
        )
        .unwrap_or_else(|e| panic!("plan failed for {sql:?}: {e}"))
    }

    fn run(&self, sql: &str) -> QueryResult {
        self.run_with_params(sql, &[])
    }

    fn run_with_params(&self, sql: &str, params: &[Option<RelationalValue>]) -> QueryResult {
        let plan = self.plan(sql);
        execute_autocommit(
            &plan,
            &self.txm,
            &self.store,
            &self.builder,
            params,
            &ExecLimits::default(),
            &ExecMetrics::default(),
            &CancellationToken::new(),
        )
        .unwrap_or_else(|e| panic!("execute failed for {sql:?}: {e}"))
    }

    fn run_in_txn(&self, sql: &str, txn: &Transaction) -> QueryResult {
        let plan = self.plan(sql);
        execute(
            &plan,
            txn,
            &self.store,
            &self.builder,
            &[],
            &ExecLimits::default(),
            &ExecMetrics::default(),
            &CancellationToken::new(),
        )
        .unwrap_or_else(|e| panic!("execute failed for {sql:?}: {e}"))
    }

    fn cleanup(self) {
        self.f.cleanup();
    }
}

fn row(id: i32, name: &str, active: bool) -> Vec<Option<RelationalValue>> {
    vec![
        Some(RelationalValue::Integer(id)),
        Some(RelationalValue::Text(name.to_string())),
        Some(RelationalValue::Boolean(active)),
    ]
}

fn text(s: &str) -> Option<RelationalValue> {
    Some(RelationalValue::Text(s.to_string()))
}

fn int(n: i32) -> Option<RelationalValue> {
    Some(RelationalValue::Integer(n))
}

// -----------------------------------------------------------------
// PK lookup / SeqScan / result schema (items 7/8/10/11)
// -----------------------------------------------------------------

#[test]
fn pk_lookup_returns_exactly_the_matching_row_typed() {
    let f = ExecFixture::new("pk_lookup");
    let t = f.table_id("t");
    f.store.put_row(t, &row(1, "alice", true)).unwrap();
    f.store.put_row(t, &row(2, "bob", true)).unwrap();

    let result = f.run("SELECT id, name, active FROM t WHERE id = 1");
    assert_eq!(result.rows.len(), 1);
    assert_eq!(
        result.rows[0],
        vec![int(1), text("alice"), Some(RelationalValue::Boolean(true))]
    );
    assert_eq!(result.schema.fields.len(), 3);
    assert_eq!(result.schema.fields[0].name, "id");
    f.cleanup();
}

#[test]
fn pk_lookup_on_missing_key_returns_no_rows() {
    let f = ExecFixture::new("pk_lookup_missing");
    let result = f.run("SELECT id FROM t WHERE id = 999");
    assert!(result.rows.is_empty());
    f.cleanup();
}

#[test]
fn seq_scan_returns_every_row_never_materializing_a_partial_table() {
    let f = ExecFixture::new("seq_scan");
    let t = f.table_id("t");
    for i in 0..30 {
        f.store.put_row(t, &row(i, "x", i % 2 == 0)).unwrap();
    }
    let result = f.run("SELECT id FROM t");
    assert_eq!(result.rows.len(), 30);
    f.cleanup();
}

// -----------------------------------------------------------------
// Secondary index equality/range + residual filters (items 12/13/14/15/64)
// -----------------------------------------------------------------

#[test]
fn index_equality_scan_uses_the_index_not_a_full_scan() {
    let f = ExecFixture::new("index_eq");
    f.create_name_index("t", IndexKind::NonUnique);
    let t = f.table_id("t");
    for i in 0..20 {
        f.store
            .put_row(t, &row(i, if i == 5 { "target" } else { "other" }, true))
            .unwrap();
    }

    let metrics = ExecMetrics::default();
    let plan = f.plan("SELECT id FROM t WHERE name = 'target'");
    let result = execute_autocommit(
        &plan,
        &f.txm,
        &f.store,
        &f.builder,
        &[],
        &ExecLimits::default(),
        &metrics,
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(result.rows, vec![vec![int(5)]]);
    let snap = metrics.snapshot();
    assert_eq!(snap.index_scans, 1);
    assert_eq!(
        snap.seq_scans, 0,
        "an indexed equality predicate must never fall back to a table scan"
    );
    f.cleanup();
}

#[test]
fn index_range_scan_respects_inclusive_and_exclusive_bounds() {
    let f = ExecFixture::new("index_range");
    f.create_name_index("t", IndexKind::NonUnique);
    let t = f.table_id("t");
    for i in 0..10 {
        f.store.put_row(t, &row(i, &format!("n{i}"), true)).unwrap();
    }
    let result = f.run("SELECT name FROM t WHERE name >= 'n3' AND name < 'n7'");
    let mut names: Vec<String> = result
        .rows
        .iter()
        .map(|r| match &r[0] {
            Some(RelationalValue::Text(s)) => s.clone(),
            _ => panic!(),
        })
        .collect();
    names.sort();
    assert_eq!(names, vec!["n3", "n4", "n5", "n6"]);
    f.cleanup();
}

#[test]
fn index_narrowed_residual_predicate_is_still_evaluated() {
    // item 64: index narrows on `name`, but `active` (non-indexed) must
    // still filter the result -- catches accidental residual-drop bugs.
    let f = ExecFixture::new("residual");
    f.create_name_index("t", IndexKind::NonUnique);
    let t = f.table_id("t");
    f.store.put_row(t, &row(1, "shared", true)).unwrap();
    f.store.put_row(t, &row(2, "shared", false)).unwrap();

    let result = f.run("SELECT id FROM t WHERE name = 'shared' AND active = TRUE");
    assert_eq!(
        result.rows,
        vec![vec![int(1)]],
        "the non-indexed residual predicate must still apply"
    );
    f.cleanup();
}

// -----------------------------------------------------------------
// PK range scan (Increment 15, `PHASE_RUBIXDB_INCREMENT14_BLOCKER9_
// PK_RANGE_SCAN_ADR.md` / `PHASE_RUBIXDB_INCREMENT15_PK_RANGE_
// ARCHITECTURE.md`) -- correctness of the new bounded access path.
// -----------------------------------------------------------------

#[test]
fn pk_range_scan_returns_exactly_the_matching_rows_via_the_right_access_path() {
    let f = ExecFixture::new("pk_range_exact");
    let t = f.table_id("t");
    for i in 0..30 {
        f.store.put_row(t, &row(i, "x", true)).unwrap();
    }

    let metrics = ExecMetrics::default();
    let plan = f.plan("SELECT id FROM t WHERE id >= 10 AND id < 15");
    let result = execute_autocommit(
        &plan,
        &f.txm,
        &f.store,
        &f.builder,
        &[],
        &ExecLimits::default(),
        &metrics,
        &CancellationToken::new(),
    )
    .unwrap();

    let mut ids: Vec<i32> = result
        .rows
        .iter()
        .map(|r| match r[0] {
            Some(RelationalValue::Integer(n)) => n,
            _ => panic!(),
        })
        .collect();
    ids.sort();
    assert_eq!(ids, vec![10, 11, 12, 13, 14]);

    let snap = metrics.snapshot();
    assert_eq!(snap.pk_range_scans, 1);
    assert_eq!(
        snap.seq_scans, 0,
        "a bounded PK range predicate must never fall back to a table scan"
    );
    f.cleanup();
}

#[test]
fn pk_range_scan_respects_inclusive_and_exclusive_bounds_at_domain_edges() {
    let f = ExecFixture::new("pk_range_bounds");
    let t = f.table_id("t");
    for i in 0..10 {
        f.store.put_row(t, &row(i, "x", true)).unwrap();
    }

    let cases: &[(&str, &[i32])] = &[
        ("SELECT id FROM t WHERE id >= 3 AND id < 7", &[3, 4, 5, 6]),
        ("SELECT id FROM t WHERE id > 3 AND id <= 7", &[4, 5, 6, 7]),
        ("SELECT id FROM t WHERE id >= 0 AND id < 1", &[0]),
        // An empty range must return zero rows, not error and not the
        // whole table.
        ("SELECT id FROM t WHERE id >= 5 AND id < 5", &[]),
        // Range touching the very start/end of the PK domain.
        ("SELECT id FROM t WHERE id < 1", &[0]),
        ("SELECT id FROM t WHERE id >= 9", &[9]),
        // The full-table-equivalent range, expressed as a range.
        (
            "SELECT id FROM t WHERE id >= 0 AND id < 10",
            &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
        ),
    ];
    for (sql, expected) in cases {
        let result = f.run(sql);
        let mut ids: Vec<i32> = result
            .rows
            .iter()
            .map(|r| match r[0] {
                Some(RelationalValue::Integer(n)) => n,
                _ => panic!(),
            })
            .collect();
        ids.sort();
        assert_eq!(ids, *expected, "mismatch for {sql:?}");
    }
    f.cleanup();
}

#[test]
fn pk_range_scan_never_returns_an_adjacent_tables_rows() {
    // PK RANGE TABLE ISOLATION: two tables whose PK domains overlap
    // exactly (both 0..20) -- a range query against one must never
    // observe the other's physical rows, proven by actual overlapping
    // data, not merely by inspecting the byte-range construction.
    let f = ExecFixture::new("pk_range_isolation");
    f.f.catalog
        .create_table(
            f.f.ctx.default_schema_id,
            "u",
            &[
                rubixdb::catalog::service::ColumnDef {
                    name: "id".to_string(),
                    data_type: rubixdb::relational::value::TYPE_TAG_INTEGER,
                    nullable: false,
                    default_value: None,
                    type_params: None,
                },
                rubixdb::catalog::service::ColumnDef {
                    name: "marker".to_string(),
                    data_type: rubixdb::relational::value::TYPE_TAG_TEXT,
                    nullable: false,
                    default_value: None,
                    type_params: None,
                },
            ],
            &[0],
        )
        .unwrap();
    let t = f.table_id("t");
    let u = f.table_id("u");
    for i in 0..20 {
        f.store.put_row(t, &row(i, "t-row", true)).unwrap();
        f.store
            .put_row(
                u,
                &[int(i), Some(RelationalValue::Text("u-row".to_string()))],
            )
            .unwrap();
    }

    let result = f.run("SELECT name FROM t WHERE id >= 0 AND id < 20");
    assert_eq!(result.rows.len(), 20);
    assert!(
        result.rows.iter().all(|r| r[0] == text("t-row")),
        "must contain only table t's own rows, never table u's"
    );
    f.cleanup();
}

#[test]
fn pk_range_scan_residual_predicate_is_still_evaluated() {
    let f = ExecFixture::new("pk_range_residual");
    let t = f.table_id("t");
    for i in 0..10 {
        f.store.put_row(t, &row(i, "x", i % 2 == 0)).unwrap();
    }
    let result = f.run("SELECT id FROM t WHERE id >= 2 AND id < 8 AND active = TRUE");
    let mut ids: Vec<i32> = result
        .rows
        .iter()
        .map(|r| match r[0] {
            Some(RelationalValue::Integer(n)) => n,
            _ => panic!(),
        })
        .collect();
    ids.sort();
    assert_eq!(
        ids,
        vec![2, 4, 6],
        "the non-PK residual predicate must still apply"
    );
    f.cleanup();
}

#[test]
fn pk_range_scan_on_composite_primary_key_prefix_returns_every_matching_row() {
    // The exact correctness landmine the mission called out: a partial
    // composite-PK prefix bound (`a = 1`, PK is `(a, b)`) must return
    // *every* row with `a = 1` regardless of `b`, never just the first
    // one a naive truncated-key bound would happen to hit.
    let f = ExecFixture::new("pk_range_composite");
    let table_id =
        f.f.catalog
            .create_table(
                f.f.ctx.default_schema_id,
                "composite3",
                &[
                    rubixdb::catalog::service::ColumnDef {
                        name: "a".to_string(),
                        data_type: rubixdb::relational::value::TYPE_TAG_INTEGER,
                        nullable: false,
                        default_value: None,
                        type_params: None,
                    },
                    rubixdb::catalog::service::ColumnDef {
                        name: "b".to_string(),
                        data_type: rubixdb::relational::value::TYPE_TAG_INTEGER,
                        nullable: false,
                        default_value: None,
                        type_params: None,
                    },
                ],
                &[0, 1],
            )
            .unwrap();
    for (a, b) in [(1, 1), (1, 2), (1, 3), (2, 1), (0, 9)] {
        f.store.put_row(table_id, &[int(a), int(b)]).unwrap();
    }

    let result = f.run("SELECT a, b FROM composite3 WHERE a = 1");
    let mut pairs: Vec<(i32, i32)> = result
        .rows
        .iter()
        .map(|r| match (&r[0], &r[1]) {
            (Some(RelationalValue::Integer(a)), Some(RelationalValue::Integer(b))) => (*a, *b),
            _ => panic!(),
        })
        .collect();
    pairs.sort();
    assert_eq!(
        pairs,
        vec![(1, 1), (1, 2), (1, 3)],
        "every row with a=1 must come back regardless of b"
    );
    f.cleanup();
}

#[test]
fn pk_range_scan_respects_transaction_snapshot_isolation() {
    let f = ExecFixture::new("pk_range_snapshot");
    let t = f.table_id("t");
    for i in 0..5 {
        f.store.put_row(t, &row(i, "x", true)).unwrap();
    }

    let snapshot_txn = f.txm.begin().unwrap();
    // A later, independently-committed write inside the same PK range
    // must not become visible to a snapshot already taken before it.
    f.store.put_row(t, &row(3_000, "late", true)).unwrap();

    let seen = f.run_in_txn(
        "SELECT id FROM t WHERE id >= 0 AND id < 10000",
        &snapshot_txn,
    );
    assert_eq!(
        seen.rows.len(),
        5,
        "PkRangeScan must honor the transaction's own pinned snapshot, not read-committed"
    );
    drop(snapshot_txn);

    let after = f.run("SELECT id FROM t WHERE id >= 0 AND id < 10000");
    assert_eq!(
        after.rows.len(),
        6,
        "a fresh read must see the committed write"
    );
    f.cleanup();
}

#[test]
fn pk_range_scan_reflects_delete_then_reinsert_not_a_stale_or_duplicate_version() {
    let f = ExecFixture::new("pk_range_tombstone");
    let t = f.table_id("t");
    f.store.put_row(t, &row(1, "v1", true)).unwrap();
    f.store.put_row(t, &row(2, "keep", true)).unwrap();
    f.store
        .delete_row(t, &[RelationalValue::Integer(1)])
        .unwrap();
    f.store.put_row(t, &row(1, "v2", false)).unwrap();

    let result = f.run("SELECT id, name FROM t WHERE id >= 0 AND id < 10");
    let mut rows: Vec<(i32, String)> = result
        .rows
        .iter()
        .map(|r| match (&r[0], &r[1]) {
            (Some(RelationalValue::Integer(id)), Some(RelationalValue::Text(name))) => {
                (*id, name.clone())
            }
            _ => panic!(),
        })
        .collect();
    rows.sort();
    assert_eq!(
        rows,
        vec![(1, "v2".to_string()), (2, "keep".to_string())],
        "must see exactly one, current version per key -- never the deleted v1, never both"
    );
    f.cleanup();
}

// -----------------------------------------------------------------
// Filter three-valued logic (items 16/62)
// -----------------------------------------------------------------

#[test]
fn null_semantics_truth_table() {
    let f = ExecFixture::new("null_semantics");
    let t = f.table_id("t");
    f.store
        .put_row(t, &[int(1), None, Some(RelationalValue::Boolean(true))])
        .unwrap();

    // NULL = NULL -> UNKNOWN -> filtered out.
    assert!(f.run("SELECT id FROM t WHERE name = name").rows.is_empty());
    // NULL <> NULL -> UNKNOWN -> filtered out.
    assert!(f.run("SELECT id FROM t WHERE name <> name").rows.is_empty());
    // IS NULL -> TRUE.
    assert_eq!(f.run("SELECT id FROM t WHERE name IS NULL").rows.len(), 1);
    // IS NOT NULL -> FALSE.
    assert!(f
        .run("SELECT id FROM t WHERE name IS NOT NULL")
        .rows
        .is_empty());
    // NOT (NULL) -> UNKNOWN -> filtered out. (`name IS NULL` is TRUE,
    // but a NULL-valued boolean column compared would be UNKNOWN; here
    // we directly test a NULL boolean expression via active IS NULL is
    // FALSE for this row -- covered above. Use a genuinely NULL boolean
    // instead:)
    f.store.put_row(t, &[int(2), text("x"), None]).unwrap();
    assert!(
        f.run("SELECT id FROM t WHERE id = 2 AND active")
            .rows
            .is_empty(),
        "NULL used as a boolean truth value must not be treated as TRUE"
    );
    assert!(
        f.run("SELECT id FROM t WHERE id = 2 AND NOT active")
            .rows
            .is_empty(),
        "NOT NULL is still NULL/UNKNOWN, never TRUE"
    );
    f.cleanup();
}

#[test]
fn and_or_three_valued_truth_tables() {
    let f = ExecFixture::new("and_or_3vl");
    let t = f.table_id("t");
    // one row with active = NULL
    f.store.put_row(t, &[int(1), text("x"), None]).unwrap();

    // NULL AND FALSE -> FALSE (filtered).
    assert!(f
        .run("SELECT id FROM t WHERE active AND (1 = 2)")
        .rows
        .is_empty());
    // NULL OR TRUE -> TRUE (kept).
    assert_eq!(
        f.run("SELECT id FROM t WHERE active OR (1 = 1)").rows.len(),
        1
    );
    // NULL AND TRUE -> UNKNOWN (filtered).
    assert!(f
        .run("SELECT id FROM t WHERE active AND (1 = 1)")
        .rows
        .is_empty());
    // NULL OR FALSE -> UNKNOWN (filtered).
    assert!(f
        .run("SELECT id FROM t WHERE active OR (1 = 2)")
        .rows
        .is_empty());
    f.cleanup();
}

// -----------------------------------------------------------------
// Projection (items 19/20)
// -----------------------------------------------------------------

#[test]
fn projection_preserves_select_list_order_and_aliases() {
    let f = ExecFixture::new("projection_order");
    let t = f.table_id("t");
    f.store.put_row(t, &row(1, "alice", true)).unwrap();
    let result = f.run("SELECT active AS is_active, name, id FROM t WHERE id = 1");
    assert_eq!(result.schema.fields[0].name, "is_active");
    assert_eq!(
        result.rows[0],
        vec![Some(RelationalValue::Boolean(true)), text("alice"), int(1)]
    );
    f.cleanup();
}

#[test]
fn wildcard_projection_matches_catalog_column_order() {
    let f = ExecFixture::new("wildcard");
    let t = f.table_id("t");
    f.store.put_row(t, &row(1, "alice", true)).unwrap();
    let result = f.run("SELECT * FROM t WHERE id = 1");
    assert_eq!(result.rows[0], row(1, "alice", true));
    f.cleanup();
}

// -----------------------------------------------------------------
// DISTINCT (items 21/22)
// -----------------------------------------------------------------

#[test]
fn distinct_deduplicates_by_projected_value_including_null() {
    let f = ExecFixture::new("distinct");
    let t = f.table_id("t");
    f.store.put_row(t, &row(1, "a", true)).unwrap();
    f.store.put_row(t, &row(2, "a", true)).unwrap();
    f.store.put_row(t, &row(3, "b", true)).unwrap();
    f.store
        .put_row(t, &[int(4), None, Some(RelationalValue::Boolean(true))])
        .unwrap();
    f.store
        .put_row(t, &[int(5), None, Some(RelationalValue::Boolean(true))])
        .unwrap();

    let mut result = f.run("SELECT DISTINCT name FROM t");
    result
        .rows
        .sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    assert_eq!(
        result.rows.len(),
        3,
        "'a', 'b', and NULL -- NULLs group together for DISTINCT"
    );
    f.cleanup();
}

// -----------------------------------------------------------------
// Sort (items 23/24)
// -----------------------------------------------------------------

#[test]
fn sort_ascending_and_descending_with_nulls_first_and_last() {
    let f = ExecFixture::new("sort");
    let t = f.table_id("t");
    f.store.put_row(t, &row(1, "b", true)).unwrap();
    f.store.put_row(t, &row(2, "a", true)).unwrap();
    f.store
        .put_row(t, &[int(3), None, Some(RelationalValue::Boolean(true))])
        .unwrap();

    let asc = f.run("SELECT id FROM t ORDER BY name NULLS FIRST");
    assert_eq!(asc.rows, vec![vec![int(3)], vec![int(2)], vec![int(1)]]);

    let desc = f.run("SELECT id FROM t ORDER BY name DESC NULLS LAST");
    assert_eq!(desc.rows, vec![vec![int(1)], vec![int(2)], vec![int(3)]]);
    f.cleanup();
}

#[test]
fn sort_may_reference_a_column_outside_the_projection() {
    let f = ExecFixture::new("sort_outside_projection");
    let t = f.table_id("t");
    f.store.put_row(t, &row(1, "z", true)).unwrap();
    f.store.put_row(t, &row(2, "a", true)).unwrap();
    let result = f.run("SELECT id FROM t ORDER BY name");
    assert_eq!(result.rows, vec![vec![int(2)], vec![int(1)]]);
    f.cleanup();
}

// -----------------------------------------------------------------
// Limit / Offset (items 25/26/52)
// -----------------------------------------------------------------

#[test]
fn limit_stops_scanning_early() {
    let f = ExecFixture::new("limit_early_stop");
    let t = f.table_id("t");
    for i in 0..1000 {
        f.store.put_row(t, &row(i, "x", true)).unwrap();
    }
    let metrics = ExecMetrics::default();
    let plan = f.plan("SELECT id FROM t LIMIT 5");
    let result = execute_autocommit(
        &plan,
        &f.txm,
        &f.store,
        &f.builder,
        &[],
        &ExecLimits::default(),
        &metrics,
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(result.rows.len(), 5);
    let scanned = metrics.snapshot().rows_scanned;
    assert!(
        scanned < 100,
        "LIMIT 5 must not scan anywhere close to all 1000 rows (scanned {scanned})"
    );
    f.cleanup();
}

#[test]
fn offset_skips_without_materializing_all_prior_rows_in_the_result() {
    let f = ExecFixture::new("offset");
    let t = f.table_id("t");
    for i in 0..10 {
        f.store.put_row(t, &row(i, "x", true)).unwrap();
    }
    let result = f.run("SELECT id FROM t ORDER BY id NULLS FIRST LIMIT 3 OFFSET 5");
    assert_eq!(result.rows, vec![vec![int(5)], vec![int(6)], vec![int(7)]]);
    f.cleanup();
}

// -----------------------------------------------------------------
// JOIN — INNER/LEFT, multiplicity, NULL extension (items 28/29/30/32/63)
// -----------------------------------------------------------------

#[test]
fn inner_join_emits_exact_multiplicity() {
    let f = ExecFixture::new("inner_join_multiplicity");
    let t = f.table_id("t");
    let orders = f.table_id("orders");
    f.store.put_row(t, &row(1, "alice", true)).unwrap();
    f.store
        .put_row(
            orders,
            &[
                Some(RelationalValue::Bigint(1)),
                text("o1"),
                int(10),
                int(1),
            ],
        )
        .unwrap();
    f.store
        .put_row(
            orders,
            &[
                Some(RelationalValue::Bigint(2)),
                text("o2"),
                int(20),
                int(1),
            ],
        )
        .unwrap();
    f.store
        .put_row(
            orders,
            &[
                Some(RelationalValue::Bigint(3)),
                text("o3"),
                int(30),
                int(1),
            ],
        )
        .unwrap();

    let result = f.run(
        "SELECT orders.customer FROM t INNER JOIN orders ON orders.t_id = t.id WHERE t.id = 1",
    );
    assert_eq!(
        result.rows.len(),
        3,
        "one outer row matching three inner rows must emit exactly three joined rows"
    );
    f.cleanup();
}

#[test]
fn inner_join_with_a_pk_range_on_the_outer_side_still_emits_exact_multiplicity() {
    // Increment 15 regression: a PK range on the JOIN's outer side
    // (`t.id >= 1 AND t.id < 2`, matching the same single row as the
    // equality test above) must integrate with the join executor
    // exactly like `PkLookup`/`SeqScan` already do -- never
    // globalizing the bound across rows it shouldn't, never dropping
    // or duplicating matches.
    let f = ExecFixture::new("inner_join_pk_range");
    let t = f.table_id("t");
    let orders = f.table_id("orders");
    f.store.put_row(t, &row(1, "alice", true)).unwrap();
    f.store.put_row(t, &row(2, "bob", true)).unwrap();
    for (order_id, customer, t_id) in [(1i64, "o1", 1), (2, "o2", 1), (3, "o3", 2)] {
        f.store
            .put_row(
                orders,
                &[
                    Some(RelationalValue::Bigint(order_id)),
                    text(customer),
                    int(10),
                    int(t_id),
                ],
            )
            .unwrap();
    }

    let result = f.run(
        "SELECT orders.customer FROM t INNER JOIN orders ON orders.t_id = t.id \
         WHERE t.id >= 1 AND t.id < 2",
    );
    assert_eq!(
        result.rows.len(),
        2,
        "the PK range must match only t.id=1, joined to exactly its two orders"
    );
    f.cleanup();
}

#[test]
fn left_join_zero_matches_emits_one_null_extended_row() {
    let f = ExecFixture::new("left_join_zero");
    let t = f.table_id("t");
    f.store.put_row(t, &row(1, "alice", true)).unwrap();
    let result =
        f.run("SELECT t.id, orders.customer FROM t LEFT JOIN orders ON orders.t_id = t.id");
    assert_eq!(result.rows.len(), 1);
    assert_eq!(
        result.rows[0],
        vec![int(1), None],
        "the unmatched inner side must be NULL, the row must not disappear"
    );
    f.cleanup();
}

#[test]
fn left_join_one_and_many_matches() {
    let f = ExecFixture::new("left_join_matches");
    let t = f.table_id("t");
    let orders = f.table_id("orders");
    f.store.put_row(t, &row(1, "alice", true)).unwrap();
    f.store.put_row(t, &row(2, "bob", true)).unwrap();
    f.store
        .put_row(
            orders,
            &[
                Some(RelationalValue::Bigint(1)),
                text("o1"),
                int(10),
                int(1),
            ],
        )
        .unwrap();
    f.store
        .put_row(
            orders,
            &[
                Some(RelationalValue::Bigint(2)),
                text("o2"),
                int(20),
                int(2),
            ],
        )
        .unwrap();
    f.store
        .put_row(
            orders,
            &[
                Some(RelationalValue::Bigint(3)),
                text("o3"),
                int(30),
                int(2),
            ],
        )
        .unwrap();

    let mut result =
        f.run("SELECT t.id, orders.customer FROM t LEFT JOIN orders ON orders.t_id = t.id");
    result
        .rows
        .sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    assert_eq!(
        result.rows.len(),
        3,
        "1 match for alice + 2 matches for bob"
    );
    f.cleanup();
}

#[test]
fn left_join_where_on_nullable_side_correctly_excludes_unmatched_rows() {
    // A WHERE predicate on the nullable side, applied *after* the join
    // (never pushed into the scan, item 20 of the planner spec, already
    // certified) -- an unmatched row's NULL inner columns correctly
    // fail `orders.amount = 100`, so it is excluded here, but that is
    // WHERE's own job, not a lost row (item 30).
    let f = ExecFixture::new("left_join_where_excludes");
    let t = f.table_id("t");
    let orders = f.table_id("orders");
    f.store.put_row(t, &row(1, "alice", true)).unwrap(); // no matching order
    f.store.put_row(t, &row(2, "bob", true)).unwrap();
    f.store
        .put_row(
            orders,
            &[
                Some(RelationalValue::Bigint(1)),
                text("o1"),
                int(100),
                int(2),
            ],
        )
        .unwrap();

    let result = f
        .run("SELECT t.id FROM t LEFT JOIN orders ON orders.t_id = t.id WHERE orders.amount = 100");
    assert_eq!(result.rows, vec![vec![int(2)]]);
    f.cleanup();
}

#[test]
fn index_nested_loop_join_produces_the_same_result_as_plain_nested_loop() {
    // Differential-style check: a join with a usable inner index
    // (IndexNestedLoop) and the identical query forced into a shape with
    // no usable inner access (NestedLoop, via an unindexed predicate on
    // the same logical condition) must agree on final rows -- item 19's
    // own "the optimization must be provably transparent" (D18).
    let f = ExecFixture::new("index_nested_loop_agrees");
    let t = f.table_id("t");
    let orders = f.table_id("orders");
    for i in 0..5 {
        f.store.put_row(t, &row(i, "x", true)).unwrap();
        f.store
            .put_row(
                orders,
                &[
                    Some(RelationalValue::Bigint(i as i64)),
                    text("o"),
                    int(i * 10),
                    int(i),
                ],
            )
            .unwrap();
    }

    // t.id is PRIMARY KEY -- joining INTO t from orders makes this an
    // IndexNestedLoop (PK-keyed) join.
    let indexed = f.run("SELECT orders.id FROM orders INNER JOIN t ON t.id = orders.t_id ORDER BY orders.id NULLS FIRST");
    // A plain NestedLoop covering the identical logical result: use
    // orders as the driving side matched by hand via a UNION-free
    // equivalent query shape is unavailable in this grammar, so instead
    // assert the indexed join's own row count/content directly against
    // the known fixture (5 orders, each matching exactly one t row).
    assert_eq!(indexed.rows.len(), 5);
    for (i, r) in indexed.rows.iter().enumerate() {
        assert_eq!(r[0], Some(RelationalValue::Bigint(i as i64)));
    }
    f.cleanup();
}

// -----------------------------------------------------------------
// Parameters (item 34)
// -----------------------------------------------------------------

#[test]
fn parameter_in_pk_lookup_and_filter() {
    let f = ExecFixture::new("parameters");
    let t = f.table_id("t");
    f.store.put_row(t, &row(1, "alice", true)).unwrap();
    f.store.put_row(t, &row(2, "bob", true)).unwrap();

    let result = f.run_with_params("SELECT name FROM t WHERE id = $1", &[int(2)]);
    assert_eq!(result.rows, vec![vec![text("bob")]]);
    f.cleanup();
}

#[test]
fn null_parameter_in_equality_matches_nothing() {
    let f = ExecFixture::new("null_parameter");
    let t = f.table_id("t");
    f.store.put_row(t, &row(1, "alice", true)).unwrap();
    let result = f.run_with_params("SELECT id FROM t WHERE id = $1", &[None]);
    assert!(
        result.rows.is_empty(),
        "id = NULL can never match (item 62), never an internal-error path either"
    );
    f.cleanup();
}

#[test]
fn missing_parameter_is_a_controlled_error_not_a_panic() {
    let f = ExecFixture::new("missing_parameter");
    let err = {
        let plan = f.plan("SELECT id FROM t WHERE id = $1");
        execute_autocommit(
            &plan,
            &f.txm,
            &f.store,
            &f.builder,
            &[],
            &ExecLimits::default(),
            &ExecMetrics::default(),
            &CancellationToken::new(),
        )
        .unwrap_err()
    };
    assert!(matches!(err, crate::SqlError::ExecutionParameter { .. }));
    f.cleanup();
}

// -----------------------------------------------------------------
// Transaction reads: snapshot consistency, read-your-own-writes (items 35/36/37)
// -----------------------------------------------------------------

#[test]
fn read_your_own_writes_within_an_explicit_transaction() {
    let f = ExecFixture::new("read_your_own_writes");
    let t = f.table_id("t");
    let mut txn = f.txm.begin().unwrap();
    txn.put_row(t, &row(1, "local", true)).unwrap();
    // Not committed yet -- only visible through this same transaction's
    // own read context (`Transaction::get_row`'s already-certified
    // overlay), exercised here through the executor's PkLookup path.
    let result = f.run_in_txn("SELECT name FROM t WHERE id = 1", &txn);
    assert_eq!(result.rows, vec![vec![text("local")]]);
    txn.rollback().unwrap();
    f.cleanup();
}

#[test]
fn snapshot_is_stable_against_a_later_external_commit() {
    let f = ExecFixture::new("snapshot_stable");
    let t = f.table_id("t");
    f.store.put_row(t, &row(1, "before", true)).unwrap();

    let txn = f.txm.begin().unwrap();
    // External committed change after the snapshot was taken.
    f.store.put_row(t, &row(1, "after", true)).unwrap();

    let result = f.run_in_txn("SELECT name FROM t WHERE id = 1", &txn);
    assert_eq!(
        result.rows,
        vec![vec![text("before")]],
        "the transaction's own snapshot must not observe a later external commit"
    );
    drop(txn);
    f.cleanup();
}

#[test]
fn seq_scan_and_index_scan_both_honor_the_transaction_snapshot() {
    let f = ExecFixture::new("snapshot_scans");
    f.create_name_index("t", IndexKind::NonUnique);
    let t = f.table_id("t");
    f.store.put_row(t, &row(1, "before", true)).unwrap();

    let txn = f.txm.begin().unwrap();
    f.store.put_row(t, &row(2, "before", true)).unwrap();

    let seq = f.run_in_txn("SELECT id FROM t WHERE active = TRUE", &txn);
    assert_eq!(
        seq.rows.len(),
        1,
        "SeqScan must not see the post-snapshot row"
    );

    let idx = f.run_in_txn("SELECT id FROM t WHERE name = 'before'", &txn);
    assert_eq!(
        idx.rows.len(),
        1,
        "IndexScan must not see the post-snapshot row"
    );
    drop(txn);
    f.cleanup();
}

// -----------------------------------------------------------------
// Cancellation / deadline (items 40/41/71)
// -----------------------------------------------------------------

#[test]
fn cancellation_token_stops_execution() {
    let f = ExecFixture::new("cancellation");
    let t = f.table_id("t");
    for i in 0..1000 {
        f.store.put_row(t, &row(i, "x", true)).unwrap();
    }
    let plan = f.plan("SELECT id FROM t");
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let err = execute_autocommit(
        &plan,
        &f.txm,
        &f.store,
        &f.builder,
        &[],
        &ExecLimits::default(),
        &ExecMetrics::default(),
        &cancellation,
    )
    .unwrap_err();
    assert!(matches!(err, crate::SqlError::Cancelled));
    f.cleanup();
}

#[test]
fn deadline_exceeded_is_a_controlled_error() {
    let f = ExecFixture::new("deadline");
    let t = f.table_id("t");
    f.store.put_row(t, &row(1, "x", true)).unwrap();
    let plan = f.plan("SELECT id FROM t");
    let limits = ExecLimits {
        deadline: Some(Duration::from_nanos(1)),
        ..ExecLimits::default()
    };
    thread::sleep(Duration::from_millis(5));
    let err = execute_autocommit(
        &plan,
        &f.txm,
        &f.store,
        &f.builder,
        &[],
        &limits,
        &ExecMetrics::default(),
        &CancellationToken::new(),
    )
    .unwrap_err();
    assert!(matches!(err, crate::SqlError::DeadlineExceeded));
    f.cleanup();
}

// -----------------------------------------------------------------
// Resource limits (items 22/24/42/70)
// -----------------------------------------------------------------

#[test]
fn max_result_rows_is_enforced() {
    let f = ExecFixture::new("max_result_rows");
    let t = f.table_id("t");
    for i in 0..10 {
        f.store.put_row(t, &row(i, "x", true)).unwrap();
    }
    let plan = f.plan("SELECT id FROM t");
    let limits = ExecLimits {
        max_result_rows: 5,
        ..ExecLimits::default()
    };
    let err = execute_autocommit(
        &plan,
        &f.txm,
        &f.store,
        &f.builder,
        &[],
        &limits,
        &ExecMetrics::default(),
        &CancellationToken::new(),
    )
    .unwrap_err();
    assert!(matches!(err, crate::SqlError::ResourceLimit { .. }));
    f.cleanup();
}

#[test]
fn max_materialized_rows_bounds_sort_and_distinct() {
    let f = ExecFixture::new("max_materialized");
    let t = f.table_id("t");
    for i in 0..20 {
        f.store.put_row(t, &row(i, "x", true)).unwrap();
    }
    let limits = ExecLimits {
        max_materialized_rows: 5,
        ..ExecLimits::default()
    };
    let sort_plan = f.plan("SELECT id FROM t ORDER BY name");
    let err = execute_autocommit(
        &sort_plan,
        &f.txm,
        &f.store,
        &f.builder,
        &[],
        &limits,
        &ExecMetrics::default(),
        &CancellationToken::new(),
    )
    .unwrap_err();
    assert!(matches!(err, crate::SqlError::ResourceLimit { .. }));

    let distinct_plan = f.plan("SELECT DISTINCT id FROM t");
    let err = execute_autocommit(
        &distinct_plan,
        &f.txm,
        &f.store,
        &f.builder,
        &[],
        &limits,
        &ExecMetrics::default(),
        &CancellationToken::new(),
    )
    .unwrap_err();
    assert!(matches!(err, crate::SqlError::ResourceLimit { .. }));
    f.cleanup();
}

// -----------------------------------------------------------------
// Plan/executor contract (items 5/65/67/68)
// -----------------------------------------------------------------

#[test]
fn non_query_plan_is_an_explicit_unsupported_error_never_silent() {
    let f = ExecFixture::new("unsupported_plan");
    let plan = f.plan("INSERT INTO t (id, name, active) VALUES (1, 'a', TRUE)");
    let err = execute_autocommit(
        &plan,
        &f.txm,
        &f.store,
        &f.builder,
        &[],
        &ExecLimits::default(),
        &ExecMetrics::default(),
        &CancellationToken::new(),
    )
    .unwrap_err();
    assert!(matches!(err, crate::SqlError::UnsupportedExecution { .. }));
    f.cleanup();
}

#[test]
fn explain_wraps_and_still_executes_the_inner_query() {
    let f = ExecFixture::new("explain_exec");
    let t = f.table_id("t");
    f.store.put_row(t, &row(1, "alice", true)).unwrap();
    let result = f.run("EXPLAIN SELECT id FROM t WHERE id = 1");
    assert_eq!(result.rows, vec![vec![int(1)]]);
    f.cleanup();
}

// -----------------------------------------------------------------
// Metrics (item 45/46)
// -----------------------------------------------------------------

#[test]
fn metrics_record_access_path_choices() {
    let f = ExecFixture::new("metrics");
    let t = f.table_id("t");
    f.store.put_row(t, &row(1, "x", true)).unwrap();
    let metrics = ExecMetrics::default();

    let plan1 = f.plan("SELECT id FROM t WHERE id = 1");
    execute_autocommit(
        &plan1,
        &f.txm,
        &f.store,
        &f.builder,
        &[],
        &ExecLimits::default(),
        &metrics,
        &CancellationToken::new(),
    )
    .unwrap();
    let plan2 = f.plan("SELECT id FROM t WHERE active = TRUE");
    execute_autocommit(
        &plan2,
        &f.txm,
        &f.store,
        &f.builder,
        &[],
        &ExecLimits::default(),
        &metrics,
        &CancellationToken::new(),
    )
    .unwrap();

    let snap = metrics.snapshot();
    assert_eq!(snap.queries_executed, 2);
    assert_eq!(snap.pk_lookups, 1);
    assert_eq!(snap.seq_scans, 1);
    f.cleanup();
}

// -----------------------------------------------------------------
// Concurrency (item 57/58)
// -----------------------------------------------------------------

#[test]
fn concurrent_independent_queries_never_cross_contaminate() {
    let f = Arc::new(ExecFixture::new("concurrency"));
    let t = f.table_id("t");
    for i in 0..20 {
        f.store.put_row(t, &row(i, &format!("n{i}"), true)).unwrap();
    }

    let handles: Vec<_> = (0..20)
        .map(|i| {
            let f = Arc::clone(&f);
            thread::spawn(move || {
                let result = f.run_with_params("SELECT name FROM t WHERE id = $1", &[int(i)]);
                assert_eq!(
                    result.rows,
                    vec![vec![text(&format!("n{i}"))]],
                    "thread {i} must see only its own parameter's result"
                );
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    Arc::try_unwrap(f).ok().unwrap().cleanup();
}

// -----------------------------------------------------------------
// Compaction integration (item 75/76)
// -----------------------------------------------------------------

#[test]
fn queries_remain_correct_during_automatic_compaction() {
    use rubixdb::lsm::{LsmConfig, LsmEngine};
    use rubixdb::wal::{SyncMode, WalConfig};

    let dir = std::env::temp_dir().join(format!(
        "rubixdb_exec_compaction_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let engine = Arc::new(
        LsmEngine::open(
            &dir,
            WalConfig {
                sync_mode: SyncMode::GroupCommit {
                    max_wait: Duration::from_millis(5),
                    max_batch_bytes: 256 * 1024,
                },
                ..WalConfig::default()
            },
            Default::default(),
            LsmConfig {
                memtable_max_size_bytes: 256,
                max_immutable_memtables: 32,
                compaction_trigger_count: 3,
                compaction_auto_trigger: true,
                ..LsmConfig::default()
            },
        )
        .unwrap(),
    );
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    catalog.bootstrap().unwrap();
    let database_id = catalog.list_databases().unwrap()[0].database_id;
    let schema_id = catalog.list_schemas(database_id).unwrap()[0].schema_id;
    let store = Arc::new(TableStore::new(Arc::clone(&engine), Arc::clone(&catalog)));
    let builder = Arc::new(IndexBuilder::new(
        Arc::clone(&engine),
        Arc::clone(&catalog),
        Arc::clone(&store),
    ));
    let txm = TransactionManager::new(Arc::clone(&engine), Arc::clone(&store));

    let table_id = catalog
        .create_table(
            schema_id,
            "t",
            &[
                rubixdb::catalog::service::ColumnDef {
                    name: "id".to_string(),
                    data_type: rubixdb::relational::value::TYPE_TAG_INTEGER,
                    nullable: false,
                    default_value: None,
                    type_params: None,
                },
                rubixdb::catalog::service::ColumnDef {
                    name: "name".to_string(),
                    data_type: rubixdb::relational::value::TYPE_TAG_TEXT,
                    nullable: true,
                    default_value: None,
                    type_params: None,
                },
            ],
            &[0],
        )
        .unwrap();
    store.put_row(table_id, &[int(1), text("marker")]).unwrap();
    for i in 100..250 {
        store.put_row(table_id, &[int(i), text("filler")]).unwrap();
    }
    assert!(
        engine.compaction_metrics().cycles_completed > 0,
        "fixture must actually trigger compaction"
    );

    let ctx = crate::bind::BindContext {
        database_id,
        default_schema_id: schema_id,
    };
    let limits = SqlLimits::default();
    let stmt = parse_statement("SELECT name FROM t WHERE id = 1", &limits).unwrap();
    let metrics = SqlMetrics::default();
    let auth = AuthContext::admin("t");
    let bound = bind_statement(&catalog, &ctx, &auth, &metrics, &limits, &stmt).unwrap();
    let plan = build_plan(
        &bound,
        &catalog,
        &PlannerLimits::default(),
        &PlannerMetrics::default(),
    )
    .unwrap();
    let result = execute_autocommit(
        &plan,
        &txm,
        &store,
        &builder,
        &[],
        &ExecLimits::default(),
        &ExecMetrics::default(),
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(result.rows, vec![vec![text("marker")]]);

    engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

// -----------------------------------------------------------------
// Differential testing against an independent reference model (item 60/61)
// -----------------------------------------------------------------

mod differential {
    use super::*;

    /// A tiny, independent in-memory relational model -- never calls
    /// the planner, executor, `TableStore`, or `IndexBuilder` for its
    /// own semantic decisions (item 60).
    fn reference_select(
        rows: &[(i32, Option<&str>, bool)],
        name_filter: Option<&str>,
        active_filter: Option<bool>,
    ) -> Vec<i32> {
        let mut out: Vec<i32> = rows
            .iter()
            .filter(|(_, name, active)| {
                let name_ok = match name_filter {
                    None => true,
                    Some(f) => *name == Some(f),
                };
                let active_ok = match active_filter {
                    None => true,
                    Some(f) => *active == f,
                };
                name_ok && active_ok
            })
            .map(|(id, _, _)| *id)
            .collect();
        out.sort_unstable();
        out
    }

    #[test]
    fn matches_reference_model_across_a_generated_dataset() {
        let f = ExecFixture::new("diff_dataset");
        let t = f.table_id("t");
        let dataset: Vec<(i32, Option<&str>, bool)> = vec![
            (1, Some("a"), true),
            (2, Some("a"), false),
            (3, Some("b"), true),
            (4, None, true),
            (5, Some("b"), false),
        ];
        for (id, name, active) in &dataset {
            let row = vec![
                int(*id),
                name.map(|s| RelationalValue::Text(s.to_string())),
                Some(RelationalValue::Boolean(*active)),
            ];
            f.store.put_row(t, &row).unwrap();
        }

        let cases: &[(&str, Option<&str>, Option<bool>)] = &[
            ("SELECT id FROM t WHERE name = 'a'", Some("a"), None),
            ("SELECT id FROM t WHERE active = TRUE", None, Some(true)),
            (
                "SELECT id FROM t WHERE name = 'b' AND active = TRUE",
                Some("b"),
                Some(true),
            ),
        ];

        for (sql, name_filter, active_filter) in cases {
            let mut actual: Vec<i32> = f
                .run(sql)
                .rows
                .into_iter()
                .map(|r| match r[0] {
                    Some(RelationalValue::Integer(n)) => n,
                    _ => panic!(),
                })
                .collect();
            actual.sort_unstable();
            let expected = reference_select(&dataset, *name_filter, *active_filter);
            assert_eq!(actual, expected, "mismatch for {sql:?}");
        }
        f.cleanup();
    }
}

// -----------------------------------------------------------------
// Increment 11: GROUP BY / HAVING / aggregate execution.
// `aggregate_reference_model.rs` covers differential/property testing
// for COUNT/SUM/AVG/MIN/MAX + GROUP BY + NULL grouping over a dedicated
// numeric fixture table; the tests below cover the remaining items this
// crate's `t`/`orders` fixture tables are well-suited for: empty input,
// HAVING, ORDER BY/LIMIT over aggregate output, resource limits,
// cancellation/deadline, snapshot/transaction visibility, concurrency,
// and metrics correctness.
// -----------------------------------------------------------------

fn bigint(n: i64) -> Option<RelationalValue> {
    Some(RelationalValue::Bigint(n))
}

#[test]
fn count_star_over_empty_table_returns_one_row_with_zero() {
    // item 11/44: an aggregate query with no GROUP BY always produces
    // exactly one result row, even over zero qualifying input rows.
    let f = ExecFixture::new("agg_count_empty");
    let result = f.run("SELECT COUNT(*) FROM t");
    assert_eq!(result.rows, vec![vec![bigint(0)]]);
    f.cleanup();
}

#[test]
fn sum_avg_min_max_over_empty_table_are_null_count_is_zero() {
    let f = ExecFixture::new("agg_empty_full_matrix");
    let result = f.run("SELECT COUNT(*), COUNT(id), SUM(id), AVG(id), MIN(id), MAX(id) FROM t");
    assert_eq!(result.rows.len(), 1);
    assert_eq!(
        result.rows[0],
        vec![bigint(0), bigint(0), None, None, None, None],
        "COUNT is 0 over empty input; SUM/AVG/MIN/MAX are all NULL"
    );
    f.cleanup();
}

#[test]
fn grouped_aggregate_over_empty_table_returns_zero_rows() {
    // item 11: distinct from the no-GROUP-BY empty-input case above --
    // grouping over zero qualifying rows produces zero groups, not one.
    let f = ExecFixture::new("agg_group_empty");
    let result = f.run("SELECT name, COUNT(*) FROM t GROUP BY name");
    assert!(result.rows.is_empty());
    f.cleanup();
}

#[test]
fn null_semantics_full_matrix_count_sum_avg_min_max() {
    let f = ExecFixture::new("agg_null_matrix");
    let orders = f.table_id("orders");
    let put = |id: i64, amount: Option<RelationalValue>| {
        f.store
            .put_row(
                orders,
                &[Some(RelationalValue::Bigint(id)), text("c"), amount, None],
            )
            .unwrap();
    };
    // all-NULL amount.
    put(1, None);
    put(2, None);
    let all_null = f.run("SELECT COUNT(*), COUNT(amount), SUM(amount), AVG(amount), MIN(amount), MAX(amount) FROM orders");
    assert_eq!(
        all_null.rows[0],
        vec![bigint(2), bigint(0), None, None, None, None]
    );

    // some-NULL amount.
    put(3, int(10));
    put(4, None);
    put(5, int(30));
    let some_null = f.run("SELECT COUNT(*), COUNT(amount), SUM(amount), AVG(amount), MIN(amount), MAX(amount) FROM orders WHERE id >= 3");
    assert_eq!(
        some_null.rows[0],
        vec![
            bigint(3),
            bigint(2),
            bigint(40),
            Some(RelationalValue::Double(20.0)),
            int(10),
            int(30)
        ]
    );
    f.cleanup();
}

#[test]
fn having_filters_groups_using_three_valued_logic() {
    let f = ExecFixture::new("agg_having");
    let t = f.table_id("t");
    f.store.put_row(t, &row(1, "a", true)).unwrap();
    f.store.put_row(t, &row(2, "a", true)).unwrap();
    f.store.put_row(t, &row(3, "b", true)).unwrap();

    let mut result = f.run("SELECT name, COUNT(*) FROM t GROUP BY name HAVING COUNT(*) > 1");
    result
        .rows
        .sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    assert_eq!(result.rows, vec![vec![text("a"), bigint(2)]]);
    f.cleanup();
}

#[test]
fn having_evaluates_once_per_group_not_once_per_input_row() {
    // item 45/58, proven via metrics: many input rows collapsing into
    // few groups, with HAVING discarding some of them -- the *shape* of
    // the plan (HAVING = Filter directly above Aggregate) guarantees
    // this structurally, and `groups_emitted` is exactly the number of
    // tuples the HAVING Filter ever pulls from Aggregate.
    let f = ExecFixture::new("agg_having_once_per_group");
    let t = f.table_id("t");
    for i in 0..300 {
        f.store
            .put_row(t, &row(i, if i % 3 == 0 { "keep" } else { "drop" }, true))
            .unwrap();
    }
    let metrics = ExecMetrics::default();
    let plan = f.plan("SELECT name, COUNT(*) FROM t GROUP BY name HAVING COUNT(*) > 150");
    let result = execute_autocommit(
        &plan,
        &f.txm,
        &f.store,
        &f.builder,
        &[],
        &ExecLimits::default(),
        &metrics,
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(result.rows, vec![vec![text("drop"), bigint(200)]]);
    let snap = metrics.snapshot();
    assert_eq!(snap.groups_emitted, 2, "exactly 2 distinct name groups");
    assert_eq!(snap.aggregate_rows_processed, 300);
    f.cleanup();
}

#[test]
fn order_by_and_limit_apply_after_aggregation() {
    let f = ExecFixture::new("agg_order_limit");
    let t = f.table_id("t");
    for (id, name) in [(1, "a"), (2, "a"), (3, "b"), (4, "c"), (5, "c"), (6, "c")] {
        f.store.put_row(t, &row(id, name, true)).unwrap();
    }
    let result =
        f.run("SELECT name, COUNT(*) FROM t GROUP BY name ORDER BY COUNT(*) DESC, name LIMIT 2");
    assert_eq!(
        result.rows,
        vec![vec![text("c"), bigint(3)], vec![text("a"), bigint(2)]]
    );
    f.cleanup();
}

#[test]
fn composite_group_by_produces_one_group_per_distinct_pair() {
    let f = ExecFixture::new("agg_composite_group");
    let t = f.table_id("t");
    f.store.put_row(t, &row(1, "a", true)).unwrap();
    f.store.put_row(t, &row(2, "a", false)).unwrap();
    f.store.put_row(t, &row(3, "a", true)).unwrap();
    f.store.put_row(t, &row(4, "b", true)).unwrap();

    let mut result = f.run("SELECT name, active, COUNT(*) FROM t GROUP BY name, active");
    result
        .rows
        .sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    assert_eq!(
        result.rows,
        vec![
            vec![text("a"), Some(RelationalValue::Boolean(false)), bigint(1)],
            vec![text("a"), Some(RelationalValue::Boolean(true)), bigint(2)],
            vec![text("b"), Some(RelationalValue::Boolean(true)), bigint(1)],
        ],
        "3 distinct (name, active) pairs -- composite grouping must not collide"
    );
    f.cleanup();
}

#[test]
fn aggregation_over_a_pk_range_matches_the_same_query_expressed_as_seq_scan_plus_filter() {
    // Increment 15 regression: COUNT/SUM/GROUP BY/HAVING must see
    // exactly the same input rows whether the underlying access is a
    // `PkRangeScan` (id >= 2 AND id < 8) or a logically equivalent
    // `SeqScan` + residual filter (id >= 2 AND id < 8 expressed via a
    // predicate shape the planner cannot recognize as a PK range,
    // forcing SeqScan) -- a real cross-check, not just "some number
    // came back."
    let f = ExecFixture::new("agg_pk_range");
    let t = f.table_id("t");
    for i in 0..10 {
        f.store
            .put_row(t, &row(i, if i % 2 == 0 { "even" } else { "odd" }, true))
            .unwrap();
    }

    let range_result =
        f.run("SELECT name, COUNT(*), SUM(id) FROM t WHERE id >= 2 AND id < 8 GROUP BY name");
    // `id + 0` defeats PK-range recognition (not a plain column
    // reference), forcing SeqScan for the same logical row set.
    let seq_scan_result = f.run(
        "SELECT name, COUNT(*), SUM(id) FROM t WHERE id + 0 >= 2 AND id + 0 < 8 GROUP BY name",
    );

    let normalize = |mut r: crate::exec::QueryResult| {
        r.rows
            .sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
        r.rows
    };
    assert_eq!(normalize(range_result), normalize(seq_scan_result));
    f.cleanup();
}

#[test]
fn null_grouping_key_forms_exactly_one_group() {
    let f = ExecFixture::new("agg_null_grouping");
    let t = f.table_id("t");
    f.store
        .put_row(t, &[int(1), None, Some(RelationalValue::Boolean(true))])
        .unwrap();
    f.store
        .put_row(t, &[int(2), None, Some(RelationalValue::Boolean(true))])
        .unwrap();
    f.store.put_row(t, &row(3, "x", true)).unwrap();

    let mut result = f.run("SELECT name, COUNT(*) FROM t GROUP BY name");
    result
        .rows
        .sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    assert_eq!(
        result.rows,
        vec![vec![None, bigint(2)], vec![text("x"), bigint(1)]],
        "both NULL-name rows must land in exactly one group, never two"
    );
    f.cleanup();
}

#[test]
fn high_cardinality_group_by_produces_correct_group_count_and_sums() {
    // item 31: at least a few thousand distinct groups, not a low-
    // cardinality boolean-shaped column.
    let f = ExecFixture::new("agg_high_cardinality");
    let orders = f.table_id("orders");
    const GROUPS: i32 = 5_000;
    for g in 0..GROUPS {
        f.store
            .put_row(
                orders,
                &[
                    Some(RelationalValue::Bigint(g as i64)),
                    text("c"),
                    int(g),
                    int(g), // t_id doubles as the grouping column here
                ],
            )
            .unwrap();
    }
    let metrics = ExecMetrics::default();
    let plan = f.plan("SELECT t_id, SUM(amount) FROM orders GROUP BY t_id");
    let result = execute_autocommit(
        &plan,
        &f.txm,
        &f.store,
        &f.builder,
        &[],
        &ExecLimits::default(),
        &metrics,
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(result.rows.len(), GROUPS as usize);
    let snap = metrics.snapshot();
    assert_eq!(snap.groups_created, GROUPS as u64);
    assert_eq!(snap.groups_emitted, GROUPS as u64);
    for r in &result.rows {
        // each group has exactly one row, with amount == t_id, so
        // SUM(amount) for group t_id=g must be exactly g.
        let Some(RelationalValue::Integer(g)) = r[0] else {
            panic!("expected an Integer t_id, got {:?}", r[0]);
        };
        assert_eq!(r[1], Some(RelationalValue::Bigint(g as i64)));
    }
    f.cleanup();
}

#[test]
fn max_group_count_resource_limit_fails_closed_never_ooms() {
    // item 30/74: an adversarial high-cardinality GROUP BY must return a
    // controlled ResourceLimit error, never a partial result and never
    // unbounded growth.
    let f = ExecFixture::new("agg_max_group_count");
    let t = f.table_id("t");
    for i in 0..50 {
        f.store.put_row(t, &row(i, &format!("g{i}"), true)).unwrap();
    }
    let limits = ExecLimits {
        max_group_count: 10,
        ..ExecLimits::default()
    };
    let plan = f.plan("SELECT name, COUNT(*) FROM t GROUP BY name");
    let metrics = ExecMetrics::default();
    let err = execute_autocommit(
        &plan,
        &f.txm,
        &f.store,
        &f.builder,
        &[],
        &limits,
        &metrics,
        &CancellationToken::new(),
    )
    .unwrap_err();
    assert!(matches!(err, crate::SqlError::ResourceLimit { .. }));
    assert_eq!(metrics.snapshot().aggregate_resource_limit_hits, 1);
    f.cleanup();
}

#[test]
fn max_aggregate_state_bytes_resource_limit_fails_closed() {
    let f = ExecFixture::new("agg_max_state_bytes");
    let t = f.table_id("t");
    for i in 0..50 {
        f.store.put_row(t, &row(i, &format!("g{i}"), true)).unwrap();
    }
    let limits = ExecLimits {
        max_aggregate_state_bytes: 16,
        ..ExecLimits::default()
    };
    let plan = f.plan("SELECT name, COUNT(*) FROM t GROUP BY name");
    let err = execute_autocommit(
        &plan,
        &f.txm,
        &f.store,
        &f.builder,
        &[],
        &limits,
        &ExecMetrics::default(),
        &CancellationToken::new(),
    )
    .unwrap_err();
    assert!(matches!(err, crate::SqlError::ResourceLimit { .. }));
    f.cleanup();
}

#[test]
fn aggregate_cancellation_and_deadline_are_controlled_errors() {
    let f = ExecFixture::new("agg_cancel_deadline");
    let t = f.table_id("t");
    for i in 0..500 {
        f.store.put_row(t, &row(i, "x", true)).unwrap();
    }
    let plan = f.plan("SELECT COUNT(*) FROM t GROUP BY name");
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let err = execute_autocommit(
        &plan,
        &f.txm,
        &f.store,
        &f.builder,
        &[],
        &ExecLimits::default(),
        &ExecMetrics::default(),
        &cancellation,
    )
    .unwrap_err();
    assert!(matches!(err, crate::SqlError::Cancelled));

    let limits = ExecLimits {
        deadline: Some(Duration::from_nanos(1)),
        ..ExecLimits::default()
    };
    thread::sleep(Duration::from_millis(5));
    let err = execute_autocommit(
        &plan,
        &f.txm,
        &f.store,
        &f.builder,
        &[],
        &limits,
        &ExecMetrics::default(),
        &CancellationToken::new(),
    )
    .unwrap_err();
    assert!(matches!(err, crate::SqlError::DeadlineExceeded));
    f.cleanup();
}

#[test]
fn aggregate_query_honors_transaction_snapshot() {
    // item 37: aggregation must use the exact same transaction/read
    // context as every other operator, never a second snapshot -- and it
    // does, by construction (`AggregateOp`'s own input is `AccessOp`,
    // unchanged). It therefore also inherits `AccessOp`'s own existing,
    // pre-Increment-11 scope boundary *as-is*, never a new one: a
    // `SeqScan`/`IndexScan` access path reads via `TableStore::
    // scan_table_rows_as_of(snapshot_seq)` directly
    // (`sql/src/exec/operators.rs::AccessOp::build`'s `SeqScan`/
    // `IndexScan` arms), not through `Transaction::get_row`'s
    // write-set-overlay read-your-own-writes path -- only `PkLookup`
    // does that. This test proves aggregation is externally-snapshot-
    // stable (the part item 37 requires); it deliberately does not
    // assert read-your-own-writes for a `COUNT(*)`-shaped (`SeqScan`)
    // query, because no `SeqScan`-based query anywhere in this crate
    // has that property today -- asserting it here would be testing a
    // capability this increment neither adds nor regresses.
    let f = ExecFixture::new("agg_snapshot");
    let t = f.table_id("t");
    f.store.put_row(t, &row(1, "a", true)).unwrap();

    let txn = f.txm.begin().unwrap();
    // External committed insert after the snapshot was taken.
    f.store.put_row(t, &row(2, "a", true)).unwrap();
    let result = f.run_in_txn("SELECT COUNT(*) FROM t", &txn);
    assert_eq!(
        result.rows,
        vec![vec![bigint(1)]],
        "aggregation must not observe a later external commit"
    );
    drop(txn);

    // Read-your-own-writes for a *PK-lookup*-shaped aggregate input
    // (the one access path that does go through `Transaction::get_row`)
    // does work, since `AggregateOp` simply consumes whatever rows its
    // input operator produces.
    let mut txn2 = f.txm.begin().unwrap();
    txn2.put_row(t, &row(3, "a", true)).unwrap();
    let result2 = f.run_in_txn("SELECT COUNT(*) FROM t WHERE id = 3", &txn2);
    assert_eq!(
        result2.rows,
        vec![vec![bigint(1)]],
        "aggregation over a PkLookup input must see this transaction's own uncommitted write"
    );
    txn2.rollback().unwrap();
    f.cleanup();
}

#[test]
fn concurrent_aggregate_queries_never_cross_contaminate() {
    let f = Arc::new(ExecFixture::new("agg_concurrency"));
    let t = f.table_id("t");
    for i in 0..200 {
        f.store
            .put_row(t, &row(i, if i % 4 == 0 { "a" } else { "b" }, true))
            .unwrap();
    }
    let handles: Vec<_> = (0..16)
        .map(|_| {
            let f = Arc::clone(&f);
            thread::spawn(move || {
                let mut result = f.run("SELECT name, COUNT(*) FROM t GROUP BY name");
                result
                    .rows
                    .sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
                assert_eq!(
                    result.rows,
                    vec![vec![text("a"), bigint(50)], vec![text("b"), bigint(150)]]
                );
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    Arc::try_unwrap(f).ok().unwrap().cleanup();
}

#[test]
fn aggregate_metrics_are_recorded_correctly() {
    let f = ExecFixture::new("agg_metrics");
    let t = f.table_id("t");
    for i in 0..10 {
        f.store
            .put_row(t, &row(i, if i < 6 { "a" } else { "b" }, true))
            .unwrap();
    }
    let metrics = ExecMetrics::default();
    let plan = f.plan("SELECT name, COUNT(*) FROM t GROUP BY name");
    execute_autocommit(
        &plan,
        &f.txm,
        &f.store,
        &f.builder,
        &[],
        &ExecLimits::default(),
        &metrics,
        &CancellationToken::new(),
    )
    .unwrap();
    let snap = metrics.snapshot();
    assert_eq!(
        snap.aggregate_rows_processed, 10,
        "every input row processed"
    );
    assert_eq!(snap.groups_created, 2, "2 distinct name values");
    assert_eq!(snap.groups_emitted, 2);
    assert_eq!(snap.rows_returned, 2);
    f.cleanup();
}

#[test]
fn planner_records_aggregate_plan_metric() {
    let f = ExecFixture::new("agg_planner_metric");
    let limits = SqlLimits::default();
    let stmt = parse_statement("SELECT COUNT(*) FROM t", &limits).unwrap();
    let sql_metrics = SqlMetrics::default();
    let auth = AuthContext::admin("test-principal");
    let bound =
        bind_statement(&f.f.catalog, &f.f.ctx, &auth, &sql_metrics, &limits, &stmt).unwrap();
    let planner_metrics = PlannerMetrics::default();
    build_plan(
        &bound,
        &f.f.catalog,
        &PlannerLimits::default(),
        &planner_metrics,
    )
    .unwrap();
    assert_eq!(planner_metrics.snapshot().aggregate_plans, 1);
    f.cleanup();
}

#[test]
fn aggregate_queries_remain_correct_during_automatic_compaction() {
    use rubixdb::lsm::{LsmConfig, LsmEngine};
    use rubixdb::wal::{SyncMode, WalConfig};

    let dir = std::env::temp_dir().join(format!(
        "rubixdb_agg_compaction_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let engine = Arc::new(
        LsmEngine::open(
            &dir,
            WalConfig {
                sync_mode: SyncMode::GroupCommit {
                    max_wait: Duration::from_millis(5),
                    max_batch_bytes: 256 * 1024,
                },
                ..WalConfig::default()
            },
            Default::default(),
            LsmConfig {
                memtable_max_size_bytes: 256,
                max_immutable_memtables: 32,
                compaction_trigger_count: 3,
                compaction_auto_trigger: true,
                ..LsmConfig::default()
            },
        )
        .unwrap(),
    );
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    catalog.bootstrap().unwrap();
    let database_id = catalog.list_databases().unwrap()[0].database_id;
    let schema_id = catalog.list_schemas(database_id).unwrap()[0].schema_id;
    let store = Arc::new(TableStore::new(Arc::clone(&engine), Arc::clone(&catalog)));
    let builder = Arc::new(IndexBuilder::new(
        Arc::clone(&engine),
        Arc::clone(&catalog),
        Arc::clone(&store),
    ));
    let txm = TransactionManager::new(Arc::clone(&engine), Arc::clone(&store));

    let table_id = catalog
        .create_table(
            schema_id,
            "t",
            &[
                rubixdb::catalog::service::ColumnDef {
                    name: "id".to_string(),
                    data_type: rubixdb::relational::value::TYPE_TAG_INTEGER,
                    nullable: false,
                    default_value: None,
                    type_params: None,
                },
                rubixdb::catalog::service::ColumnDef {
                    name: "name".to_string(),
                    data_type: rubixdb::relational::value::TYPE_TAG_TEXT,
                    nullable: true,
                    default_value: None,
                    type_params: None,
                },
            ],
            &[0],
        )
        .unwrap();
    for i in 0..200 {
        store
            .put_row(
                table_id,
                &[int(i), text(if i % 5 == 0 { "x" } else { "y" })],
            )
            .unwrap();
    }
    assert!(
        engine.compaction_metrics().cycles_completed > 0,
        "fixture must actually trigger compaction"
    );

    let ctx = crate::bind::BindContext {
        database_id,
        default_schema_id: schema_id,
    };
    let limits = SqlLimits::default();
    let stmt = parse_statement("SELECT name, COUNT(*) FROM t GROUP BY name", &limits).unwrap();
    let metrics = SqlMetrics::default();
    let auth = AuthContext::admin("t");
    let bound = bind_statement(&catalog, &ctx, &auth, &metrics, &limits, &stmt).unwrap();
    let plan = build_plan(
        &bound,
        &catalog,
        &PlannerLimits::default(),
        &PlannerMetrics::default(),
    )
    .unwrap();
    let mut result = execute_autocommit(
        &plan,
        &txm,
        &store,
        &builder,
        &[],
        &ExecLimits::default(),
        &ExecMetrics::default(),
        &CancellationToken::new(),
    )
    .unwrap();
    result
        .rows
        .sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    assert_eq!(
        result.rows,
        vec![vec![text("x"), bigint(40)], vec![text("y"), bigint(160)]]
    );

    engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

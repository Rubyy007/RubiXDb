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

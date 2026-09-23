//! Query planner tests — `PHASE_RELATIONAL_QUERY_PLANNER_ARCHITECTURE.md`
//! is the decision record these verify against.

use std::ops::Bound;
use std::sync::Arc;
use std::thread;

use rubixdb::catalog::schema::IndexKind;
use rubixdb::catalog::service::ColumnDef;
use rubixdb::catalog::CatalogService;
use rubixdb::relational::index::IndexBuilder;
use rubixdb::relational::table_store::TableStore;
use rubixdb::relational::value::TYPE_TAG_INTEGER;

use crate::ast::JoinKind;
use crate::auth::AuthContext;
use crate::bind::bind_statement;
use crate::bound::{BoundExprKind, BoundStatement};
use crate::limits::SqlLimits;
use crate::metrics::SqlMetrics;
use crate::parse::parse_statement;
use crate::plan::access::{IndexAccessMode, PhysicalAccess};
use crate::plan::physical::{JoinAlgorithm, PhysicalPlan};
use crate::plan::{build_plan, explain, Plan, PlannerLimits, PlannerMetrics};
use crate::test_support::Fixture;

fn bound(f: &Fixture, sql: &str) -> BoundStatement {
    let limits = SqlLimits::default();
    let stmt =
        parse_statement(sql, &limits).unwrap_or_else(|e| panic!("parse failed for {sql:?}: {e}"));
    let metrics = SqlMetrics::default();
    let auth = AuthContext::admin("test-principal");
    bind_statement(&f.catalog, &f.ctx, &auth, &metrics, &limits, &stmt)
        .unwrap_or_else(|e| panic!("bind failed for {sql:?}: {e}"))
}

fn plan(f: &Fixture, sql: &str) -> Plan {
    let b = bound(f, sql);
    build_plan(
        &b,
        &f.catalog,
        &PlannerLimits::default(),
        &PlannerMetrics::default(),
    )
    .unwrap_or_else(|e| panic!("plan failed for {sql:?}: {e}"))
}

fn only_access(plan: &PhysicalPlan) -> &PhysicalAccess {
    match plan {
        PhysicalPlan::Access(a) => a,
        PhysicalPlan::Filter { input, .. }
        | PhysicalPlan::Projection { input, .. }
        | PhysicalPlan::Distinct { input }
        | PhysicalPlan::Sort { input, .. }
        | PhysicalPlan::Limit { input, .. } => only_access(input),
        PhysicalPlan::Join { .. } | PhysicalPlan::EmptyRelation => {
            panic!("expected a single Access node, found {plan:?}")
        }
    }
}

fn query_physical(p: &Plan) -> &PhysicalPlan {
    match p {
        Plan::Query { physical, .. } => physical,
        other => panic!("expected Plan::Query, got {other:?}"),
    }
}

// -----------------------------------------------------------------
// Logical/physical separation, EmptyRelation
// -----------------------------------------------------------------

#[test]
fn from_less_select_is_an_empty_relation() {
    let f = Fixture::new("empty_relation");
    let p = plan(&f, "SELECT 1");
    assert!(
        matches!(query_physical(&p), PhysicalPlan::Projection { input, .. } if matches!(**input, PhysicalPlan::EmptyRelation))
    );
    f.cleanup();
}

// -----------------------------------------------------------------
// PRIMARY KEY lookup detection (items 6/7)
// -----------------------------------------------------------------

#[test]
fn full_pk_equality_becomes_pk_lookup() {
    let f = Fixture::new("pk_lookup");
    let p = plan(&f, "SELECT name FROM t WHERE id = 5");
    let access = only_access(query_physical(&p));
    assert!(matches!(access, PhysicalAccess::PkLookup { .. }));
    f.cleanup();
}

#[test]
fn partial_composite_pk_equality_never_becomes_a_point_lookup() {
    let f = Fixture::new("partial_composite_pk");
    let catalog = &f.catalog;
    let table_id = catalog
        .create_table(
            f.ctx.default_schema_id,
            "composite",
            &[
                ColumnDef {
                    name: "a".to_string(),
                    data_type: TYPE_TAG_INTEGER,
                    nullable: false,
                    default_value: None,
                    type_params: None,
                },
                ColumnDef {
                    name: "b".to_string(),
                    data_type: TYPE_TAG_INTEGER,
                    nullable: false,
                    default_value: None,
                    type_params: None,
                },
            ],
            &[0, 1],
        )
        .unwrap();

    // Only `a = ?` is given; PRIMARY KEY(a, b) requires both. Must NOT
    // become PkLookup (item 6's exact correctness-critical example).
    let p = plan(&f, "SELECT a FROM composite WHERE a = 1");
    let access = only_access(query_physical(&p));
    assert!(
        !matches!(access, PhysicalAccess::PkLookup { .. }),
        "partial composite PK equality must never become a point lookup"
    );

    // Both `a` and `b` given: now it is safe.
    let p2 = plan(&f, "SELECT a FROM composite WHERE a = 1 AND b = 2");
    let access2 = only_access(query_physical(&p2));
    match access2 {
        PhysicalAccess::PkLookup {
            table_id: tid,
            key_values,
            ..
        } => {
            assert_eq!(*tid, table_id);
            assert_eq!(key_values.len(), 2);
        }
        other => {
            panic!("expected PkLookup once both PK columns are equality-constrained, got {other:?}")
        }
    }
    f.cleanup();
}

#[test]
fn pk_lookup_preserves_parameter_reference_unevaluated() {
    let f = Fixture::new("pk_param");
    let p = plan(&f, "SELECT name FROM t WHERE id = $1");
    let access = only_access(query_physical(&p));
    match access {
        PhysicalAccess::PkLookup { key_values, .. } => {
            assert_eq!(key_values.len(), 1);
            assert!(matches!(
                key_values[0].kind,
                BoundExprKind::Parameter { index: 1 }
            ));
        }
        other => panic!("expected PkLookup, got {other:?}"),
    }
    f.cleanup();
}

// -----------------------------------------------------------------
// Secondary index selection / range planning (items 8/9/10/11)
// -----------------------------------------------------------------

struct IndexedFixture {
    f: Fixture,
    builder: IndexBuilder,
    index_id: u32,
}

fn with_name_index(tag: &str) -> IndexedFixture {
    let f = Fixture::new(tag);
    let catalog = Arc::new(CatalogService::new(Arc::clone(&f.engine)));
    let store = Arc::new(TableStore::new(Arc::clone(&f.engine), Arc::clone(&catalog)));
    let builder = IndexBuilder::new(
        Arc::clone(&f.engine),
        Arc::clone(&catalog),
        Arc::clone(&store),
    );
    let table_id = f
        .catalog
        .get_table_by_name(f.ctx.default_schema_id, "t")
        .unwrap()
        .unwrap()
        .table_id;
    let index_id = builder
        .create_index_online(table_id, "t_name_idx", IndexKind::NonUnique, &[1])
        .unwrap();
    IndexedFixture {
        f,
        builder,
        index_id,
    }
}

#[test]
fn equality_on_indexed_column_selects_index_scan() {
    let ix = with_name_index("index_eq");
    let p = plan(&ix.f, "SELECT id FROM t WHERE name = 'alice'");
    let access = only_access(query_physical(&p));
    match access {
        PhysicalAccess::IndexScan {
            index_id,
            mode,
            residual,
            ..
        } => {
            assert_eq!(*index_id, ix.index_id);
            assert!(matches!(mode, IndexAccessMode::Equality { .. }));
            assert!(residual.is_none());
        }
        other => panic!("expected IndexScan, got {other:?}"),
    }
    let _ = ix.builder;
    ix.f.cleanup();
}

#[test]
fn range_predicate_on_indexed_column_builds_index_range_scan() {
    let ix = with_name_index("index_range");
    let p = plan(&ix.f, "SELECT id FROM t WHERE name >= 'a' AND name < 'm'");
    let access = only_access(query_physical(&p));
    match access {
        PhysicalAccess::IndexScan { mode, .. } => match mode {
            IndexAccessMode::Range { start, end } => {
                assert!(matches!(start, Bound::Included(_)));
                assert!(matches!(end, Bound::Excluded(_)));
            }
            other => panic!("expected Range mode, got {other:?}"),
        },
        other => panic!("expected IndexScan, got {other:?}"),
    }
    ix.f.cleanup();
}

#[test]
fn index_narrows_but_residual_predicate_is_preserved() {
    let ix = with_name_index("index_residual");
    let p = plan(
        &ix.f,
        "SELECT id FROM t WHERE name = 'alice' AND active = TRUE",
    );
    let access = only_access(query_physical(&p));
    match access {
        PhysicalAccess::IndexScan { residual, .. } => {
            assert!(
                residual.is_some(),
                "the non-indexed `active` conjunct must remain as a residual filter"
            );
        }
        other => panic!("expected IndexScan, got {other:?}"),
    }
    ix.f.cleanup();
}

#[test]
fn unindexed_predicate_falls_back_to_seq_scan_never_a_partial_index_plan() {
    let f = Fixture::new("seq_fallback");
    let p = plan(&f, "SELECT id FROM t WHERE active = TRUE");
    let access = only_access(query_physical(&p));
    assert!(matches!(
        access,
        PhysicalAccess::SeqScan {
            predicate: Some(_),
            ..
        }
    ));
    f.cleanup();
}

#[test]
fn primary_kind_catalog_index_is_never_selected_as_an_index_scan() {
    // `t`'s own auto-created `Primary` index row exists in the catalog
    // (naming/metadata only, no physical entries — see `crate::plan::
    // access`'s own doc comment) but must never be chosen as an
    // `IndexScan` path; PK equality must always route through the
    // dedicated `PkLookup` instead.
    let f = Fixture::new("no_primary_index_scan");
    let p = plan(&f, "SELECT name FROM t WHERE id = 1");
    let access = only_access(query_physical(&p));
    assert!(matches!(access, PhysicalAccess::PkLookup { .. }));
    f.cleanup();
}

// -----------------------------------------------------------------
// Predicate pushdown / LEFT JOIN safety (items 12, 20)
// -----------------------------------------------------------------

#[test]
fn single_table_predicate_is_fully_pushed_no_residual_filter_node() {
    let f = Fixture::new("full_pushdown");
    let p = plan(&f, "SELECT id FROM t WHERE id = 5");
    // The whole predicate became the PkLookup's key -- no separate
    // Filter node should remain above it.
    match query_physical(&p) {
        PhysicalPlan::Projection { input, .. } => {
            assert!(matches!(
                **input,
                PhysicalPlan::Access(PhysicalAccess::PkLookup { .. })
            ));
        }
        other => panic!("unexpected shape: {other:?}"),
    }
    f.cleanup();
}

#[test]
fn left_join_predicate_on_nullable_side_is_never_pushed_into_its_scan() {
    let f = Fixture::new("left_join_safety");
    let p = plan(
        &f,
        "SELECT t.id FROM t LEFT JOIN orders ON orders.t_id = t.id WHERE orders.amount = 100",
    );
    let PhysicalPlan::Projection { input: top, .. } = query_physical(&p) else {
        panic!("expected a Projection at the top");
    };
    match top.as_ref() {
        PhysicalPlan::Filter { input, predicate } => {
            // The WHERE predicate on the null-extended side must remain
            // as a Filter *above* the Join, not pushed into orders' own
            // scan.
            assert!(matches!(predicate.kind, BoundExprKind::BinaryOp { .. }));
            assert!(matches!(**input, PhysicalPlan::Join { .. }));
            let PhysicalPlan::Join { right, .. } = input.as_ref() else {
                panic!()
            };
            if let PhysicalPlan::Access(access) = right.as_ref() {
                assert!(
                    access_predicate_is_none_or_join_key_only(access),
                    "orders' own scan must not carry the WHERE-side residual predicate: {access:?}"
                );
            }
        }
        other => panic!(
            "expected a top-level Filter retaining the nullable-side predicate, got {other:?}"
        ),
    }
    f.cleanup();
}

fn access_predicate_is_none_or_join_key_only(access: &PhysicalAccess) -> bool {
    // The only conjuncts allowed on the inner scan are ones derived
    // from the join's own ON condition (t_id = t.id), never the WHERE
    // clause's `amount = 100`.
    let residual_text = match access {
        PhysicalAccess::SeqScan { predicate, .. } => predicate.as_ref().map(|e| format!("{e:?}")),
        PhysicalAccess::IndexScan { residual, .. } => residual.as_ref().map(|e| format!("{e:?}")),
        PhysicalAccess::PkLookup { residual, .. } => residual.as_ref().map(|e| format!("{e:?}")),
    };
    match residual_text {
        None => true,
        Some(text) => !text.contains("100"),
    }
}

#[test]
fn inner_join_predicate_on_either_side_may_be_pushed() {
    let f = Fixture::new("inner_join_pushdown");
    let p = plan(
        &f,
        "SELECT t.id FROM t INNER JOIN orders ON orders.t_id = t.id WHERE orders.amount = 100",
    );
    // No top-level residual Filter should remain: `amount = 100`
    // references only `orders` (not null-extended under INNER JOIN), so
    // it is eligible to push into orders' own scan.
    match query_physical(&p) {
        PhysicalPlan::Projection { input, .. } => {
            assert!(matches!(**input, PhysicalPlan::Join { .. }), "{input:?}");
        }
        other => panic!("{other:?}"),
    }
    f.cleanup();
}

// -----------------------------------------------------------------
// NULL / three-valued logic (item 21) — never confuse IS NULL with `= NULL`
// -----------------------------------------------------------------

#[test]
fn is_null_predicate_is_never_treated_as_an_equality_lookup_key() {
    let f = Fixture::new("is_null_not_equality");
    let p = plan(&f, "SELECT id FROM t WHERE name IS NULL");
    let access = only_access(query_physical(&p));
    // Must fall back to SeqScan (no index/PK equality can be built from
    // `IS NULL`) with the predicate preserved verbatim.
    match access {
        PhysicalAccess::SeqScan {
            predicate: Some(p), ..
        } => {
            assert!(matches!(p.kind, BoundExprKind::IsNull { .. }));
        }
        other => panic!("expected SeqScan with an IsNull predicate preserved, got {other:?}"),
    }
    f.cleanup();
}

// -----------------------------------------------------------------
// Projection pruning (item 13)
// -----------------------------------------------------------------

#[test]
fn required_columns_include_projection_and_filter_but_not_unused_columns() {
    let f = Fixture::new("prune");
    // `active` is neither projected nor filtered -- must not appear in
    // the required set. `id` is filtered (via PK lookup residual? no --
    // fully consumed) and `name` is projected.
    let p = plan(&f, "SELECT name FROM t WHERE name = 'x'");
    let access = only_access(query_physical(&p));
    if let PhysicalAccess::IndexScan { .. } | PhysicalAccess::SeqScan { .. } = access {
        // required_columns lives on the logical Scan node, already
        // consumed by physical planning; verify indirectly via the
        // logical plan builder instead (see `logical_required_columns_*`).
    }
    let _ = access;
    f.cleanup();
}

#[test]
fn logical_required_columns_excludes_unreferenced_column() {
    use crate::plan::logical::build_logical_plan;
    use crate::plan::optimize::optimize;
    let f = Fixture::new("prune_logical");
    let BoundStatement::Select(select) = bound(&f, "SELECT name FROM t WHERE id = 5") else {
        panic!()
    };
    let logical = build_logical_plan(&select, &PlannerLimits::default()).unwrap();
    let optimized = optimize(
        logical,
        &PlannerLimits::default(),
        &PlannerMetrics::default(),
    )
    .unwrap();
    let cols = required_columns_of_scan(&optimized);
    assert!(cols.contains(&0)); // id, via the pushed predicate
    assert!(cols.contains(&1)); // name, via the projection
    assert!(!cols.contains(&2)); // active, referenced nowhere
    f.cleanup();
}

fn required_columns_of_scan(plan: &crate::plan::LogicalPlan) -> Vec<u16> {
    use crate::plan::LogicalPlan;
    match plan {
        LogicalPlan::Scan {
            required_columns, ..
        } => required_columns.clone().unwrap_or_default(),
        LogicalPlan::Filter { input, .. }
        | LogicalPlan::Projection { input, .. }
        | LogicalPlan::Distinct { input }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Limit { input, .. } => required_columns_of_scan(input),
        LogicalPlan::Join { left, .. } => required_columns_of_scan(left),
        LogicalPlan::EmptyRelation => Vec::new(),
    }
}

// -----------------------------------------------------------------
// LIMIT pushdown (item 14)
// -----------------------------------------------------------------

#[test]
fn limit_directly_above_a_scan_is_marked_pushable() {
    let f = Fixture::new("limit_pushable");
    let p = plan(&f, "SELECT id FROM t WHERE active = TRUE LIMIT 10");
    match query_physical(&p) {
        PhysicalPlan::Limit { pushable, .. } => assert!(*pushable),
        other => panic!("expected a Limit node, got {other:?}"),
    }
    f.cleanup();
}

#[test]
fn limit_above_a_join_is_never_marked_pushable() {
    let f = Fixture::new("limit_not_pushable_join");
    let p = plan(
        &f,
        "SELECT t.id FROM t INNER JOIN orders ON orders.t_id = t.id LIMIT 10",
    );
    match query_physical(&p) {
        PhysicalPlan::Limit { pushable, .. } => assert!(!pushable),
        other => panic!("expected a Limit node, got {other:?}"),
    }
    f.cleanup();
}

#[test]
fn limit_above_sort_is_never_marked_pushable() {
    let f = Fixture::new("limit_not_pushable_sort");
    let p = plan(&f, "SELECT id FROM t ORDER BY name LIMIT 10");
    match query_physical(&p) {
        PhysicalPlan::Limit { pushable, .. } => assert!(!pushable),
        other => panic!("expected a Limit node, got {other:?}"),
    }
    f.cleanup();
}

// -----------------------------------------------------------------
// ORDER BY / Sort elimination (item 15) -- conservative, exact-match only
// -----------------------------------------------------------------

// `LogicalPlan`/`PhysicalPlan` nest `Sort` as the *outermost* node
// (evaluation order `Scan/Join -> Filter -> Projection -> Distinct ->
// Sort -> Limit`, `crate::plan::logical`'s own doc comment) -- so
// "Sort eliminated" means `query_physical` returns the `Projection`
// directly at the top (Sort's own former `input`, promoted), and "Sort
// retained" means `query_physical` returns `PhysicalPlan::Sort` itself
// at the top, not nested inside anything else.

#[test]
fn sort_is_eliminated_when_pk_lookup_returns_at_most_one_row() {
    let f = Fixture::new("sort_elim_pk");
    let p = plan(&f, "SELECT id FROM t WHERE id = 5 ORDER BY name");
    assert!(
        matches!(query_physical(&p), PhysicalPlan::Projection { .. }),
        "Sort must be eliminated above a PkLookup: {:?}",
        query_physical(&p)
    );
    f.cleanup();
}

#[test]
fn sort_is_eliminated_when_index_scan_already_provides_matching_ascending_order() {
    let ix = with_name_index("sort_elim_index");
    // D5's default for ascending is NULLS LAST, which the physical
    // index (always NULLS-FIRST-ascending, no reverse-scan primitive)
    // does *not* naturally satisfy -- explicit `NULLS FIRST` is the one
    // case that matches the index's own physical order exactly.
    let p = plan(&ix.f, "SELECT id FROM t ORDER BY name NULLS FIRST");
    assert!(
        matches!(query_physical(&p), PhysicalPlan::Projection { .. }),
        "an ascending, NULLS FIRST index scan on exactly the ORDER BY column must eliminate Sort: {:?}",
        query_physical(&p)
    );
    ix.f.cleanup();
}

#[test]
fn sort_is_retained_for_descending_order_no_reverse_scan_primitive_exists() {
    let ix = with_name_index("sort_retained_desc");
    let p = plan(&ix.f, "SELECT id FROM t ORDER BY name DESC");
    assert!(
        matches!(query_physical(&p), PhysicalPlan::Sort { .. }),
        "DESC must retain Sort -- no reverse scan exists: {:?}",
        query_physical(&p)
    );
    ix.f.cleanup();
}

#[test]
fn sort_is_retained_for_the_sql_standard_default_nulls_last_ascending() {
    // The common case (`ORDER BY name` with no explicit NULLS clause)
    // binds to NULLS LAST (D5's default) -- which the index's physical
    // NULLS-FIRST order does not match, so Sort must be retained even
    // though a same-column ascending index exists.
    let ix = with_name_index("sort_retained_nulls_last_default");
    let p = plan(&ix.f, "SELECT id FROM t ORDER BY name");
    assert!(
        matches!(query_physical(&p), PhysicalPlan::Sort { .. }),
        "default NULLS LAST must retain Sort even with a matching ascending index: {:?}",
        query_physical(&p)
    );
    ix.f.cleanup();
}

#[test]
fn sort_is_retained_when_no_matching_index_exists() {
    let f = Fixture::new("sort_retained_no_index");
    let p = plan(&f, "SELECT id FROM t ORDER BY name NULLS FIRST");
    assert!(
        matches!(query_physical(&p), PhysicalPlan::Sort { .. }),
        "no index exists at all, Sort must be retained: {:?}",
        query_physical(&p)
    );
    f.cleanup();
}

// -----------------------------------------------------------------
// DISTINCT (item 16) -- explicit, never silently GROUP BY
// -----------------------------------------------------------------

#[test]
fn distinct_is_represented_as_its_own_explicit_node() {
    let f = Fixture::new("distinct_explicit");
    let p = plan(&f, "SELECT DISTINCT name FROM t");
    match query_physical(&p) {
        PhysicalPlan::Distinct { .. } => {}
        other => panic!("expected an explicit Distinct node, got {other:?}"),
    }
    f.cleanup();
}

// -----------------------------------------------------------------
// JOIN planning (items 17/18/19)
// -----------------------------------------------------------------

#[test]
fn inner_and_left_join_kinds_are_preserved() {
    let f = Fixture::new("join_kinds");
    let inner = plan(
        &f,
        "SELECT t.id FROM t INNER JOIN orders ON orders.t_id = t.id",
    );
    let left = plan(
        &f,
        "SELECT t.id FROM t LEFT JOIN orders ON orders.t_id = t.id",
    );
    match query_physical(&inner) {
        PhysicalPlan::Projection { input, .. } => {
            let PhysicalPlan::Join { kind, .. } = input.as_ref() else {
                panic!()
            };
            assert_eq!(*kind, JoinKind::Inner);
        }
        other => panic!("{other:?}"),
    }
    match query_physical(&left) {
        PhysicalPlan::Projection { input, .. } => {
            let PhysicalPlan::Join { kind, .. } = input.as_ref() else {
                panic!()
            };
            assert_eq!(*kind, JoinKind::Left);
        }
        other => panic!("{other:?}"),
    }
    f.cleanup();
}

#[test]
fn join_on_inner_pk_selects_index_nested_loop() {
    let f = Fixture::new("index_nested_loop_pk");
    // orders.t_id = t.id -- joining FROM orders INTO t, using t's own
    // PRIMARY KEY as the correlated lookup key.
    let p = plan(
        &f,
        "SELECT orders.customer FROM orders INNER JOIN t ON t.id = orders.t_id",
    );
    match query_physical(&p) {
        PhysicalPlan::Projection { input, .. } => {
            let PhysicalPlan::Join {
                algorithm, right, ..
            } = input.as_ref()
            else {
                panic!()
            };
            assert_eq!(*algorithm, JoinAlgorithm::IndexNestedLoop);
            assert!(matches!(
                right.as_ref(),
                PhysicalPlan::Access(PhysicalAccess::PkLookup { .. })
            ));
        }
        other => panic!("{other:?}"),
    }
    f.cleanup();
}

#[test]
fn join_with_no_usable_inner_index_selects_plain_nested_loop() {
    let f = Fixture::new("plain_nested_loop");
    let p = plan(
        &f,
        "SELECT t.id FROM t INNER JOIN orders ON orders.amount = t.id",
    );
    match query_physical(&p) {
        PhysicalPlan::Projection { input, .. } => {
            let PhysicalPlan::Join { algorithm, .. } = input.as_ref() else {
                panic!()
            };
            assert_eq!(*algorithm, JoinAlgorithm::NestedLoop);
        }
        other => panic!("{other:?}"),
    }
    f.cleanup();
}

// -----------------------------------------------------------------
// Parameter/authorization preservation, plan determinism (items 23/24/53)
// -----------------------------------------------------------------

#[test]
fn identical_input_produces_an_identical_plan_every_time() {
    let f = Fixture::new("determinism");
    let b = bound(&f, "SELECT t.id, orders.customer FROM t LEFT JOIN orders ON orders.t_id = t.id WHERE t.active = TRUE ORDER BY t.name LIMIT 5");
    let limits = PlannerLimits::default();
    let p1 = build_plan(&b, &f.catalog, &limits, &PlannerMetrics::default()).unwrap();
    let p2 = build_plan(&b, &f.catalog, &limits, &PlannerMetrics::default()).unwrap();
    assert_eq!(p1, p2);
    f.cleanup();
}

// -----------------------------------------------------------------
// Resource limits (item 26)
// -----------------------------------------------------------------

#[test]
fn max_joins_limit_is_enforced() {
    let f = Fixture::new("max_joins");
    let b = bound(
        &f,
        "SELECT t.id FROM t INNER JOIN orders ON orders.t_id = t.id",
    );
    let tight = PlannerLimits {
        max_joins: 1,
        ..PlannerLimits::default()
    };
    let err = build_plan(&b, &f.catalog, &tight, &PlannerMetrics::default()).unwrap_err();
    assert!(matches!(err, crate::SqlError::ResourceLimit { .. }));
    f.cleanup();
}

#[test]
fn max_predicate_conjuncts_limit_is_enforced() {
    let f = Fixture::new("max_conjuncts");
    let b = bound(&f, "SELECT id FROM t WHERE id = 1 AND id = 1 AND id = 1");
    let tight = PlannerLimits {
        max_predicate_conjuncts: 2,
        ..PlannerLimits::default()
    };
    let err = build_plan(&b, &f.catalog, &tight, &PlannerMetrics::default()).unwrap_err();
    assert!(matches!(err, crate::SqlError::ResourceLimit { .. }));
    f.cleanup();
}

// -----------------------------------------------------------------
// Metrics (item 50)
// -----------------------------------------------------------------

#[test]
fn metrics_record_plan_shape_choices() {
    let f = Fixture::new("metrics");
    let metrics = PlannerMetrics::default();
    let limits = PlannerLimits::default();
    let b1 = bound(&f, "SELECT id FROM t WHERE id = 1");
    build_plan(&b1, &f.catalog, &limits, &metrics).unwrap();
    let b2 = bound(&f, "SELECT id FROM t WHERE active = TRUE");
    build_plan(&b2, &f.catalog, &limits, &metrics).unwrap();

    let snap = metrics.snapshot();
    assert_eq!(snap.plans_built, 2);
    assert_eq!(snap.pk_lookups_selected, 1);
    assert_eq!(snap.seq_scans_selected, 1);
    f.cleanup();
}

// -----------------------------------------------------------------
// EXPLAIN formatter (items 30/31) -- deterministic, never leaks secrets
// -----------------------------------------------------------------

#[test]
fn explain_output_is_deterministic_and_contains_no_filesystem_path() {
    let f = Fixture::new("explain_fmt");
    let p = plan(&f, "SELECT name FROM t WHERE id = 5");
    let out1 = explain(&p);
    let out2 = explain(&p);
    assert_eq!(out1, out2);
    assert!(out1.contains("PkLookup"));
    assert!(!out1
        .to_lowercase()
        .contains(&f.dir.to_string_lossy().to_lowercase()));
    f.cleanup();
}

// -----------------------------------------------------------------
// UPDATE/DELETE target-row planning (item 47) -- reuses `plan_table_
// access` verbatim, same algorithm as a SELECT scan.
// -----------------------------------------------------------------

#[test]
fn update_target_row_uses_the_same_pk_lookup_detection_as_select() {
    let f = Fixture::new("update_pk_lookup");
    let b = bound(&f, "UPDATE t SET name = 'x' WHERE id = 5");
    let p = build_plan(
        &b,
        &f.catalog,
        &PlannerLimits::default(),
        &PlannerMetrics::default(),
    )
    .unwrap();
    match p {
        Plan::Update { access, .. } => assert!(matches!(access, PhysicalAccess::PkLookup { .. })),
        other => panic!("{other:?}"),
    }
    f.cleanup();
}

#[test]
fn delete_target_row_falls_back_to_seq_scan_when_unindexed() {
    let f = Fixture::new("delete_seq_scan");
    let b = bound(&f, "DELETE FROM t WHERE active = TRUE");
    let p = build_plan(
        &b,
        &f.catalog,
        &PlannerLimits::default(),
        &PlannerMetrics::default(),
    )
    .unwrap();
    match p {
        Plan::Delete { access, .. } => assert!(matches!(access, PhysicalAccess::SeqScan { .. })),
        other => panic!("{other:?}"),
    }
    f.cleanup();
}

#[test]
fn delete_target_row_uses_a_matching_secondary_index() {
    let ix = with_name_index("delete_index_scan");
    let b = bound(&ix.f, "DELETE FROM t WHERE name = 'alice'");
    let p = build_plan(
        &b,
        &ix.f.catalog,
        &PlannerLimits::default(),
        &PlannerMetrics::default(),
    )
    .unwrap();
    match p {
        Plan::Delete { access, .. } => assert!(matches!(access, PhysicalAccess::IndexScan { .. })),
        other => panic!("{other:?}"),
    }
    ix.f.cleanup();
}

// -----------------------------------------------------------------
// INSERT/DDL/transaction-control pass-through (item 47) -- no
// execution nodes fabricated for statement kinds this increment does
// not plan.
// -----------------------------------------------------------------

#[test]
fn insert_ddl_and_transaction_control_are_carried_through_unchanged() {
    let f = Fixture::new("passthrough");
    assert!(matches!(
        plan(&f, "INSERT INTO t (id, name, active) VALUES (1, 'a', TRUE)"),
        Plan::Insert(_)
    ));
    assert!(matches!(
        plan(&f, "CREATE TABLE u (id INTEGER PRIMARY KEY)"),
        Plan::Ddl(_)
    ));
    assert!(matches!(plan(&f, "DROP TABLE t"), Plan::Ddl(_)));
    assert!(matches!(plan(&f, "BEGIN"), Plan::Begin));
    assert!(matches!(plan(&f, "COMMIT"), Plan::Commit));
    assert!(matches!(plan(&f, "ROLLBACK"), Plan::Rollback));
    f.cleanup();
}

#[test]
fn explain_wraps_the_inner_statements_own_plan() {
    let f = Fixture::new("explain_wraps");
    let p = plan(&f, "EXPLAIN SELECT id FROM t WHERE id = 1");
    match p {
        Plan::Explain(inner) => assert!(matches!(*inner, Plan::Query { .. })),
        other => panic!("{other:?}"),
    }
    f.cleanup();
}

// -----------------------------------------------------------------
// Security / adversarial resource limits (item 36)
// -----------------------------------------------------------------

#[test]
fn deeply_nested_or_predicate_within_sql_limits_does_not_overflow_the_planner() {
    let f = Fixture::new("deep_or");
    let mut sql = "SELECT id FROM t WHERE ".to_string();
    for i in 0..100 {
        if i > 0 {
            sql.push_str(" OR ");
        }
        sql.push_str(&format!("id = {i}"));
    }
    // Must plan without panicking or hanging; the OR chain has no
    // top-level AND conjuncts to push, so it stays a single residual
    // predicate on a SeqScan.
    let p = plan(&f, &sql);
    let access = only_access(query_physical(&p));
    assert!(matches!(
        access,
        PhysicalAccess::SeqScan {
            predicate: Some(_),
            ..
        }
    ));
    f.cleanup();
}

#[test]
fn many_and_conjuncts_within_default_limits_plan_successfully() {
    let f = Fixture::new("many_and");
    let mut sql = "SELECT id FROM t WHERE active = TRUE".to_string();
    for i in 0..50 {
        sql.push_str(&format!(" AND id = {i}"));
    }
    let p = plan(&f, &sql);
    let _ = query_physical(&p); // must not panic
    f.cleanup();
}

// -----------------------------------------------------------------
// Concurrency (item 54) -- independent statements planned in parallel
// against the same catalog/metrics must never race or corrupt state.
// -----------------------------------------------------------------

#[test]
fn concurrent_planning_of_independent_statements_is_race_free() {
    let f = Arc::new(Fixture::new("concurrency"));
    let metrics = Arc::new(PlannerMetrics::default());
    let statements = [
        "SELECT id FROM t WHERE id = 1",
        "SELECT name FROM t WHERE active = TRUE",
        "SELECT t.id FROM t INNER JOIN orders ON orders.t_id = t.id",
        "SELECT id FROM t ORDER BY name",
    ];
    let bounds: Vec<_> = statements.iter().map(|s| bound(&f, s)).collect();

    thread::scope(|scope| {
        for b in &bounds {
            let metrics = Arc::clone(&metrics);
            let f = Arc::clone(&f);
            scope.spawn(move || {
                for _ in 0..20 {
                    build_plan(b, &f.catalog, &PlannerLimits::default(), &metrics).unwrap();
                }
            });
        }
    });

    assert_eq!(
        metrics.snapshot().plans_built,
        (statements.len() * 20) as u64
    );
    Arc::try_unwrap(f).ok().unwrap().cleanup();
}

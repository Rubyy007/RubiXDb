//! Differential/property testing for the planner's two simplest, most
//! safety-critical rules — items 37/38: primary-key lookup detection
//! and index eligibility — against an independent reference model built
//! from scratch on plain data (never calling into `crate::plan::access`
//! itself, never using the production planner as its own oracle).

use std::collections::BTreeSet;
use std::sync::Arc;

use proptest::prelude::*;
use rubixdb::catalog::schema::IndexKind;
use rubixdb::catalog::CatalogService;
use rubixdb::relational::index::IndexBuilder;
use rubixdb::relational::table_store::TableStore;

use crate::auth::AuthContext;
use crate::bind::bind_statement;
use crate::bound::BoundStatement;
use crate::limits::SqlLimits;
use crate::metrics::SqlMetrics;
use crate::parse::parse_statement;
use crate::plan::access::PhysicalAccess;
use crate::plan::physical::PhysicalPlan;
use crate::plan::{build_plan, Plan, PlannerLimits, PlannerMetrics};
use crate::test_support::Fixture;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefAccess {
    PkLookup,
    IndexScan(u32),
    SeqScan,
    /// Increment 15 (`PHASE_RUBIXDB_INCREMENT14_BLOCKER9_PK_RANGE_
    /// SCAN_ADR.md`): a PK range too narrow for `PkLookup` (not every
    /// PK ordinal has an equality conjunct) but wide enough to form a
    /// leading-column prefix range.
    PkRangeScan,
}

/// Independently scores a PK-range candidate, mirroring `reference_
/// choose`'s own secondary-index scoring shape (prefix of leading
/// equality ordinals, plus one more for a range on the very next
/// ordinal) but never calling into `crate::plan::access` — this is a
/// from-scratch re-derivation, the same discipline this whole module
/// already applies to `IndexScan`/`PkLookup`. Returns `None` when no
/// PK ordinal has a usable predicate, or when every PK ordinal already
/// has equality (that case is `PkLookup`, handled by the caller
/// before this is ever consulted).
fn pk_range_score(
    pk_ordinals: &[u16],
    equalities: &BTreeSet<u16>,
    ranges: &BTreeSet<u16>,
) -> usize {
    if pk_ordinals.is_empty() {
        return 0;
    }
    let mut prefix_len = 0usize;
    for &ord in pk_ordinals {
        if equalities.contains(&ord) {
            prefix_len += 1;
        } else {
            break;
        }
    }
    if prefix_len == pk_ordinals.len() {
        return 0; // full equality is PkLookup, not this path
    }
    let next = pk_ordinals[prefix_len];
    let has_range = ranges.contains(&next);
    if prefix_len == 0 && !has_range {
        return 0;
    }
    prefix_len + usize::from(has_range)
}

/// Independently re-implemented (not shared code with `crate::plan::
/// access::plan_table_access`): given the table's PK ordinals, the
/// `Ready`, non-`Primary` indexes available (`(index_id, column_
/// ordinals)`, already in ascending `index_id` order — matching
/// `CatalogService::list_indexes`'s own real ordering guarantee), and
/// which ordinals carry an equality vs. a comparison conjunct, predicts
/// the same classification `plan_table_access` should reach.
fn reference_choose(
    pk_ordinals: &[u16],
    indexes: &[(u32, Vec<u16>)],
    equalities: &BTreeSet<u16>,
    ranges: &BTreeSet<u16>,
) -> RefAccess {
    if !pk_ordinals.is_empty() && pk_ordinals.iter().all(|o| equalities.contains(o)) {
        return RefAccess::PkLookup;
    }
    // Increment 15: PK-range is seeded first, so it wins ties against
    // a secondary index of equal score -- the same deterministic
    // tie-break `candidate_pk_range_access`'s caller in `crate::plan::
    // access` applies (PK-range is seeded into `best` before the
    // secondary-index loop there too).
    let mut best: Option<(RefAccess, usize)> = None;
    let pk_score = pk_range_score(pk_ordinals, equalities, ranges);
    if pk_score > 0 {
        best = Some((RefAccess::PkRangeScan, pk_score));
    }
    for (id, cols) in indexes {
        let mut k = 0usize;
        for &c in cols {
            if equalities.contains(&c) {
                k += 1;
            } else {
                break;
            }
        }
        let score = if k == cols.len() && k > 0 {
            k
        } else if let Some(&next) = cols.get(k) {
            if ranges.contains(&next) {
                k + 1
            } else {
                k
            }
        } else {
            k
        };
        if score == 0 {
            continue;
        }
        match &best {
            Some((_, best_score)) if *best_score >= score => {}
            _ => best = Some((RefAccess::IndexScan(*id), score)),
        }
    }
    match best {
        Some((access, _)) => access,
        None => RefAccess::SeqScan,
    }
}

fn classify(plan: &Plan) -> RefAccess {
    fn find(p: &PhysicalPlan) -> &PhysicalAccess {
        match p {
            PhysicalPlan::Access(a) => a,
            PhysicalPlan::Filter { input, .. }
            | PhysicalPlan::Projection { input, .. }
            | PhysicalPlan::Distinct { input }
            | PhysicalPlan::Sort { input, .. }
            | PhysicalPlan::Limit { input, .. } => find(input),
            _ => panic!("expected a single-table access plan"),
        }
    }
    let Plan::Query { physical, .. } = plan else {
        panic!()
    };
    match find(physical) {
        PhysicalAccess::PkLookup { .. } => RefAccess::PkLookup,
        PhysicalAccess::IndexScan { index_id, .. } => RefAccess::IndexScan(*index_id),
        PhysicalAccess::SeqScan { .. } => RefAccess::SeqScan,
        PhysicalAccess::PkRangeScan { .. } => RefAccess::PkRangeScan,
    }
}

fn plan_for(f: &Fixture, sql: &str) -> Plan {
    let limits = SqlLimits::default();
    let stmt = parse_statement(sql, &limits).unwrap();
    let metrics = SqlMetrics::default();
    let auth = AuthContext::admin("test");
    let b = bind_statement(&f.catalog, &f.ctx, &auth, &metrics, &limits, &stmt).unwrap();
    let BoundStatement::Select(_) = &b else {
        panic!()
    };
    build_plan(
        &b,
        &f.catalog,
        &PlannerLimits::default(),
        &PlannerMetrics::default(),
    )
    .unwrap()
}

// -----------------------------------------------------------------
// Fixed-scenario differential matrix
// -----------------------------------------------------------------

#[test]
fn matches_reference_model_across_a_fixed_scenario_matrix() {
    let f = Fixture::new("plan_diff_fixed");
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
    let name_idx = builder
        .create_index_online(table_id, "t_name_idx", IndexKind::NonUnique, &[1])
        .unwrap();

    let indexes = vec![(name_idx, vec![1u16])];
    let pk = vec![0u16];

    let scenarios: &[(&str, &[u16], &[u16])] = &[
        ("SELECT id FROM t WHERE id = 1", &[0], &[]),
        ("SELECT id FROM t WHERE name = 'a'", &[1], &[]),
        ("SELECT id FROM t WHERE name >= 'a'", &[], &[1]),
        ("SELECT id FROM t WHERE active = TRUE", &[], &[]),
        ("SELECT id FROM t WHERE id = 1 AND name = 'a'", &[0, 1], &[]),
        // Increment 15: a PK range (not a full-PK equality) must
        // predict PkRangeScan, not SeqScan.
        ("SELECT id FROM t WHERE id >= 1 AND id < 5", &[], &[0]),
        ("SELECT id FROM t WHERE id > 1", &[], &[0]),
    ];

    for (sql, eq_ords, range_ords) in scenarios {
        let equalities: BTreeSet<u16> = eq_ords.iter().copied().collect();
        let ranges: BTreeSet<u16> = range_ords.iter().copied().collect();
        let expected = reference_choose(&pk, &indexes, &equalities, &ranges);
        let actual = classify(&plan_for(&f, sql));
        assert_eq!(actual, expected, "mismatch for {sql:?}");
    }
    f.cleanup();
}

// -----------------------------------------------------------------
// Randomized differential property test
// -----------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, .. ProptestConfig::default() })]

    #[test]
    fn matches_reference_model_for_randomized_predicate_and_index_shapes(
        id_eq in any::<bool>(),
        id_range in any::<bool>(),
        name_eq in any::<bool>(),
        name_range in any::<bool>(),
        active_eq in any::<bool>(),
        with_name_index in any::<bool>(),
    ) {
        // `name_eq`/`name_range` are mutually exclusive predicate shapes
        // on the same column in one generated statement; likewise
        // `id_eq`/`id_range` (Increment 15: exercises `PkRangeScan`
        // against the reference model too, not just secondary-index
        // range predicates).
        let name_range = name_range && !name_eq;
        let id_range = id_range && !id_eq;

        let f = Fixture::new("plan_diff_property");
        let catalog = Arc::new(CatalogService::new(Arc::clone(&f.engine)));
        let store = Arc::new(TableStore::new(Arc::clone(&f.engine), Arc::clone(&catalog)));
        let builder = IndexBuilder::new(Arc::clone(&f.engine), Arc::clone(&catalog), Arc::clone(&store));
        let table_id = f.catalog.get_table_by_name(f.ctx.default_schema_id, "t").unwrap().unwrap().table_id;

        let mut indexes = Vec::new();
        if with_name_index {
            let id = builder
                .create_index_online(table_id, "t_name_idx_p", IndexKind::NonUnique, &[1])
                .unwrap();
            indexes.push((id, vec![1u16]));
        }

        let mut conjuncts = Vec::new();
        let mut equalities = BTreeSet::new();
        let mut ranges = BTreeSet::new();
        if id_eq {
            conjuncts.push("id = 1".to_string());
            equalities.insert(0u16);
        } else if id_range {
            conjuncts.push("id >= 1".to_string());
            ranges.insert(0u16);
        }
        if name_eq {
            conjuncts.push("name = 'a'".to_string());
            equalities.insert(1u16);
        } else if name_range {
            conjuncts.push("name >= 'a'".to_string());
            ranges.insert(1u16);
        }
        if active_eq {
            conjuncts.push("active = TRUE".to_string());
            equalities.insert(2u16);
        }

        prop_assume!(!conjuncts.is_empty());
        let sql = format!("SELECT id FROM t WHERE {}", conjuncts.join(" AND "));

        let expected = reference_choose(&[0], &indexes, &equalities, &ranges);
        let actual = classify(&plan_for(&f, &sql));
        prop_assert_eq!(actual, expected, "mismatch for {}", sql);

        f.cleanup();
    }
}

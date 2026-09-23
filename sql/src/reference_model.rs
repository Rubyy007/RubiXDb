//! An independent reference model for column resolution — D30's own
//! "never the production algorithm as its own oracle" principle, applied
//! to the binder (item 40). `expected_resolution` is a from-scratch
//! reimplementation of "which (table, column) a reference names," never
//! calling into `crate::bind::scope`, compared against the real
//! `Scope::resolve_column` for a table-driven matrix of qualified/
//! unqualified/ambiguous/wildcard cases plus a `proptest`-generated
//! fuzz of synthetic scopes.

use proptest::prelude::*;

use crate::ast::{ColumnRef, Ident};
use crate::bind::bind_statement;
use crate::bind::scope::{ResolvedTable, Scope};
use crate::bound::BoundStatement;
use crate::error::SqlError;

/// A minimal, standalone description of one FROM-scope table — just
/// enough for this independent model to reason about resolution, with
/// **no** dependency on `crate::bind::scope`'s own types.
#[derive(Debug, Clone)]
struct ModelTable {
    effective_name: String,
    columns: Vec<&'static str>,
}

#[derive(Debug, PartialEq, Eq)]
enum ModelOutcome {
    Resolved {
        table_index: usize,
        column_index: usize,
    },
    Unknown,
    Ambiguous,
}

/// Reimplements SQL's own standard column-resolution rule from
/// scratch: an unqualified name must match exactly one column across
/// every table in scope; a qualified name (`alias.column`) must match
/// the named table exactly, then that column within it.
fn expected_resolution(tables: &[ModelTable], reference: &[&str]) -> ModelOutcome {
    match reference.len() {
        1 => {
            let name = reference[0];
            let mut hits = Vec::new();
            for (ti, t) in tables.iter().enumerate() {
                for (ci, c) in t.columns.iter().enumerate() {
                    if *c == name {
                        hits.push((ti, ci));
                    }
                }
            }
            match hits.len() {
                0 => ModelOutcome::Unknown,
                1 => ModelOutcome::Resolved {
                    table_index: hits[0].0,
                    column_index: hits[0].1,
                },
                _ => ModelOutcome::Ambiguous,
            }
        }
        2 => {
            let (qualifier, name) = (reference[0], reference[1]);
            let Some(ti) = tables.iter().position(|t| t.effective_name == qualifier) else {
                return ModelOutcome::Unknown;
            };
            match tables[ti].columns.iter().position(|c| *c == name) {
                Some(ci) => ModelOutcome::Resolved {
                    table_index: ti,
                    column_index: ci,
                },
                None => ModelOutcome::Unknown,
            }
        }
        _ => ModelOutcome::Unknown,
    }
}

fn model_from_scope(scope: &Scope) -> Vec<ModelTable> {
    scope
        .entries
        .iter()
        .map(|e| ModelTable {
            effective_name: e.effective_name.clone(),
            columns: e
                .resolved
                .columns
                .iter()
                .map(|c| Box::leak(c.name.clone().into_boxed_str()) as &'static str)
                .collect(),
        })
        .collect()
}

fn make_scope(resolved: Vec<(ResolvedTable, Option<&str>)>) -> Scope {
    let mut scope = Scope::default();
    for (r, alias) in resolved {
        let alias_ident = alias.map(|a| Ident::new(a, false));
        scope.push(r, alias_ident.as_ref(), false);
    }
    scope
}

fn column_ref(parts: &[&str]) -> ColumnRef {
    ColumnRef {
        parts: parts.iter().map(|p| Ident::new(*p, false)).collect(),
    }
}

#[test]
fn matrix_of_qualified_unqualified_ambiguous_and_unknown_references() {
    let f = crate::test_support::Fixture::new("reference_model_matrix");
    let database_id = f.catalog.list_databases().unwrap()[0].database_id;
    let schema_id = f.catalog.list_schemas(database_id).unwrap()[0].schema_id;
    let t_row = f
        .catalog
        .get_table_by_name(schema_id, "t")
        .unwrap()
        .unwrap();
    let orders_row = f
        .catalog
        .get_table_by_name(schema_id, "orders")
        .unwrap()
        .unwrap();
    let t_resolved = ResolvedTable {
        database_id,
        schema_id,
        table_id: t_row.table_id,
        table: t_row.clone(),
        columns: f.catalog.get_columns(t_row.table_id).unwrap(),
    };
    let orders_resolved = ResolvedTable {
        database_id,
        schema_id,
        table_id: orders_row.table_id,
        table: orders_row.clone(),
        columns: f.catalog.get_columns(orders_row.table_id).unwrap(),
    };

    // Single-table scope.
    let scope = make_scope(vec![(t_resolved.clone(), None)]);
    let model = model_from_scope(&scope);
    for reference in [
        vec!["name"],
        vec!["t", "name"],
        vec!["nope"],
        vec!["x", "name"],
    ] {
        let refs: Vec<&str> = reference.to_vec();
        let expected = expected_resolution(&model, &refs);
        let actual = scope.resolve_column(&column_ref(&refs));
        match expected {
            ModelOutcome::Resolved {
                table_index,
                column_index,
            } => {
                let (entry, col) =
                    actual.unwrap_or_else(|e| panic!("expected Resolved for {refs:?}, got {e}"));
                assert_eq!(entry.table_ref_id as usize, table_index);
                assert_eq!(col.ordinal as usize, column_index);
            }
            ModelOutcome::Unknown => {
                assert!(
                    matches!(actual, Err(SqlError::UnknownObject { .. })),
                    "{refs:?}"
                );
            }
            ModelOutcome::Ambiguous => {
                assert!(
                    matches!(actual, Err(SqlError::AmbiguousColumn { .. })),
                    "{refs:?}"
                );
            }
        }
    }

    // Two-table scope with an overlapping column name (`id` in both).
    let scope = make_scope(vec![(t_resolved, Some("t")), (orders_resolved, Some("o"))]);
    let model = model_from_scope(&scope);
    for reference in [
        vec!["id"],       // ambiguous: both tables have it
        vec!["name"],     // only t
        vec!["customer"], // only orders
        vec!["t", "id"],  // qualified, resolves to t
        vec!["o", "id"],  // qualified, resolves to orders
        vec!["z", "id"],  // unknown qualifier
    ] {
        let refs: Vec<&str> = reference.to_vec();
        let expected = expected_resolution(&model, &refs);
        let actual = scope.resolve_column(&column_ref(&refs));
        match expected {
            ModelOutcome::Resolved {
                table_index,
                column_index,
            } => {
                let (entry, col) =
                    actual.unwrap_or_else(|e| panic!("expected Resolved for {refs:?}, got {e}"));
                assert_eq!(entry.table_ref_id as usize, table_index, "{refs:?}");
                assert_eq!(col.ordinal as usize, column_index, "{refs:?}");
            }
            ModelOutcome::Unknown => {
                assert!(
                    matches!(actual, Err(SqlError::UnknownObject { .. })),
                    "{refs:?}"
                );
            }
            ModelOutcome::Ambiguous => {
                assert!(
                    matches!(actual, Err(SqlError::AmbiguousColumn { .. })),
                    "{refs:?}"
                );
            }
        }
    }
    f.cleanup();
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// For every generated `SELECT <ref> FROM t [JOIN orders o ON ...]`
    /// shape, the production binder's success/failure class must match
    /// this independent model's own prediction.
    #[test]
    fn production_binder_matches_independent_model_for_generated_queries(
        use_join in any::<bool>(),
        column_name in prop_oneof![Just("id"), Just("name"), Just("active"), Just("customer"), Just("amount"), Just("ghost")],
        qualify_with in prop_oneof![Just(None), Just(Some("t")), Just(Some("o")), Just(Some("bogus"))],
    ) {
        let f = crate::test_support::Fixture::new("reference_model_prop");
        let sql = match qualify_with {
            None => format!("SELECT {column_name} FROM t{}", if use_join { " JOIN orders o ON t.id = o.t_id" } else { "" }),
            Some(q) => format!(
                "SELECT {q}.{column_name} FROM t{}",
                if use_join { " JOIN orders o ON t.id = o.t_id" } else { "" }
            ),
        };

        let limits = crate::limits::SqlLimits::default();
        let metrics = crate::metrics::SqlMetrics::default();
        let auth = crate::auth::AuthContext::admin("p");
        let parsed = crate::parse::parse_statement(&sql, &limits);
        let Ok(stmt) = parsed else {
            f.cleanup();
            return Ok(());
        };
        let actual = bind_statement(&f.catalog, &f.ctx, &auth, &metrics, &limits, &stmt);

        // Build the independent model's own table list matching the
        // query's actual FROM clause.
        let t_cols = ["id", "name", "active"];
        let o_cols = ["id", "customer", "amount"];
        let mut model_tables = vec![ModelTable { effective_name: "t".to_string(), columns: t_cols.to_vec() }];
        if use_join {
            model_tables.push(ModelTable { effective_name: "o".to_string(), columns: o_cols.to_vec() });
        }
        let refs: Vec<&str> = match qualify_with {
            None => vec![column_name],
            Some(q) => vec![q, column_name],
        };
        let expected = expected_resolution(&model_tables, &refs);

        match (&expected, &actual) {
            (ModelOutcome::Resolved { .. }, Ok(BoundStatement::Select(_))) => {}
            (ModelOutcome::Unknown, Err(SqlError::UnknownObject { .. })) => {}
            (ModelOutcome::Ambiguous, Err(SqlError::AmbiguousColumn { .. })) => {}
            (e, a) => prop_assert!(false, "sql={sql:?} expected={e:?} actual={a:?}"),
        }
        f.cleanup();
    }
}

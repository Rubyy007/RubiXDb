//! Binder integration tests against a real `CatalogService` — item 24
//! ("do not let invalid SQL reach planner/executor"), item 15/32
//! (authorization), item 18/19/20 (wildcard/alias/column resolution).

use rubixdb::relational::{RelationalType, RelationalValue};

use crate::ast::JoinKind;
use crate::auth::AuthContext;
use crate::bind::bind_statement;
use crate::bound::*;
use crate::error::SqlError;
use crate::limits::SqlLimits;
use crate::metrics::SqlMetrics;
use crate::parse::parse_statement;
use crate::test_support::Fixture;

fn bind(f: &Fixture, sql: &str) -> crate::Result<BoundStatement> {
    bind_with_auth(f, sql, &AuthContext::admin("test-principal"))
}

fn bind_with_auth(f: &Fixture, sql: &str, auth: &AuthContext) -> crate::Result<BoundStatement> {
    let limits = SqlLimits::default();
    let stmt =
        parse_statement(sql, &limits).unwrap_or_else(|e| panic!("parse failed for {sql:?}: {e}"));
    let metrics = SqlMetrics::default();
    bind_statement(&f.catalog, &f.ctx, auth, &metrics, &limits, &stmt)
}

/// Runs the full parse → bind pipeline, propagating either stage's own
/// error rather than panicking on a parse-stage failure (`bind`'s own
/// helper deliberately panics there, since every other test in this file
/// wants parsing itself to always succeed) — for cases this increment
/// rejects as early as AST conversion (`crate::convert`), not binding.
fn parse_then_bind(f: &Fixture, sql: &str) -> crate::Result<BoundStatement> {
    let limits = SqlLimits::default();
    let stmt = parse_statement(sql, &limits)?;
    let metrics = SqlMetrics::default();
    bind_statement(
        &f.catalog,
        &f.ctx,
        &AuthContext::admin("test-principal"),
        &metrics,
        &limits,
        &stmt,
    )
}

// -----------------------------------------------------------------
// SELECT: column resolution, wildcard, type resolution
// -----------------------------------------------------------------

#[test]
fn select_plain_column_resolves_correct_ordinal_and_type() {
    let f = Fixture::new("select_plain");
    let BoundStatement::Select(s) = bind(&f, "SELECT name FROM t").unwrap() else {
        panic!()
    };
    assert_eq!(s.projection.len(), 1);
    let BoundExprKind::Column(col) = &s.projection[0].expr.kind else {
        panic!()
    };
    assert_eq!(col.ordinal, 1); // t(id=0, name=1, active=2)
    assert_eq!(s.projection[0].expr.ty, Some(RelationalType::Text));
    assert_eq!(s.projection[0].output_name, "name");
    f.cleanup();
}

#[test]
fn select_wildcard_expands_every_column_in_ordinal_order() {
    let f = Fixture::new("wildcard");
    let BoundStatement::Select(s) = bind(&f, "SELECT * FROM t").unwrap() else {
        panic!()
    };
    assert_eq!(s.projection.len(), 3);
    assert_eq!(s.projection[0].output_name, "id");
    assert_eq!(s.projection[1].output_name, "name");
    assert_eq!(s.projection[2].output_name, "active");
    f.cleanup();
}

#[test]
fn select_qualified_wildcard_on_join_expands_only_that_table() {
    let f = Fixture::new("qualified_wildcard");
    let BoundStatement::Select(s) =
        bind(&f, "SELECT o.* FROM t INNER JOIN orders o ON t.id = o.t_id").unwrap()
    else {
        panic!()
    };
    assert_eq!(s.projection.len(), 4); // orders(id, customer, amount, t_id)
    assert_eq!(s.projection[0].output_name, "id");
    assert_eq!(s.projection[1].output_name, "customer");
    f.cleanup();
}

#[test]
fn select_unknown_column_is_rejected() {
    let f = Fixture::new("unknown_col");
    let err = bind(&f, "SELECT nope FROM t").unwrap_err();
    assert!(matches!(
        err,
        SqlError::UnknownObject { kind: "column", .. }
    ));
    f.cleanup();
}

#[test]
fn select_unknown_table_is_rejected() {
    let f = Fixture::new("unknown_table");
    let err = bind(&f, "SELECT id FROM nope").unwrap_err();
    assert!(matches!(err, SqlError::UnknownObject { kind: "table", .. }));
    f.cleanup();
}

// -----------------------------------------------------------------
// JOIN binding — ambiguity, LEFT JOIN nullability
// -----------------------------------------------------------------

#[test]
fn join_binds_two_tables_and_resolves_qualified_columns() {
    let f = Fixture::new("join_basic");
    let BoundStatement::Select(s) = bind(
        &f,
        "SELECT t.id, o.customer FROM t INNER JOIN orders o ON t.id = o.t_id",
    )
    .unwrap() else {
        panic!()
    };
    assert_eq!(s.from.len(), 2);
    assert!(s.from[0].join.is_none());
    assert!(matches!(s.from[1].join, Some((JoinKind::Inner, _))));
    f.cleanup();
}

#[test]
fn ambiguous_unqualified_column_across_a_join_is_rejected() {
    let f = Fixture::new("ambiguous");
    // Both `t` and `orders` have an `id` column.
    let err = bind(&f, "SELECT id FROM t INNER JOIN orders o ON t.id = o.t_id").unwrap_err();
    assert!(matches!(err, SqlError::AmbiguousColumn { .. }));
    f.cleanup();
}

#[test]
fn left_join_marks_the_right_side_nullable_even_for_not_null_columns() {
    let f = Fixture::new("left_join_nullable");
    // orders.customer is NOT NULL in the catalog, but under a LEFT JOIN
    // it must bind as nullable (D18: unmatched left rows -> NULL).
    let BoundStatement::Select(s) = bind(
        &f,
        "SELECT o.customer FROM t LEFT JOIN orders o ON t.id = o.t_id",
    )
    .unwrap() else {
        panic!()
    };
    assert!(
        s.projection[0].expr.nullable,
        "LEFT JOIN right side must be nullable"
    );
    f.cleanup();
}

#[test]
fn inner_join_preserves_the_underlying_not_null_flag() {
    let f = Fixture::new("inner_join_not_null");
    let BoundStatement::Select(s) = bind(
        &f,
        "SELECT o.customer FROM t INNER JOIN orders o ON t.id = o.t_id",
    )
    .unwrap() else {
        panic!()
    };
    assert!(!s.projection[0].expr.nullable);
    f.cleanup();
}

// -----------------------------------------------------------------
// Type resolution / expression binding
// -----------------------------------------------------------------

#[test]
fn integer_literal_compared_against_bigint_column_binds_as_bigint() {
    let f = Fixture::new("literal_context");
    let BoundStatement::Select(s) = bind(&f, "SELECT id FROM orders WHERE id = 5").unwrap() else {
        panic!()
    };
    let Some(BoundExprKind::BinaryOp { right, .. }) = s.selection.as_ref().map(|e| &e.kind) else {
        panic!()
    };
    assert_eq!(right.ty, Some(RelationalType::Bigint));
    assert_eq!(
        right.kind,
        BoundExprKind::Literal(Some(RelationalValue::Bigint(5)))
    );
    f.cleanup();
}

#[test]
fn mismatched_comparison_types_are_rejected() {
    let f = Fixture::new("type_mismatch");
    let err = bind(&f, "SELECT id FROM t WHERE id = 'not a number'").unwrap_err();
    assert!(matches!(err, SqlError::TypeMismatch { .. }));
    f.cleanup();
}

#[test]
fn function_call_binds_and_unknown_function_is_rejected() {
    let f = Fixture::new("functions");
    let BoundStatement::Select(s) = bind(&f, "SELECT length(name) FROM t").unwrap() else {
        panic!()
    };
    assert_eq!(s.projection[0].expr.ty, Some(RelationalType::Integer));

    let err = bind(&f, "SELECT not_a_real_fn(name) FROM t").unwrap_err();
    assert!(matches!(
        err,
        SqlError::UnknownObject {
            kind: "function",
            ..
        }
    ));
    f.cleanup();
}

#[test]
fn between_and_in_list_require_a_shared_type() {
    let f = Fixture::new("between_in");
    assert!(bind(&f, "SELECT id FROM t WHERE id BETWEEN 1 AND 10").is_ok());
    let err = bind(&f, "SELECT id FROM t WHERE id BETWEEN 1 AND 'x'").unwrap_err();
    assert!(matches!(err, SqlError::TypeMismatch { .. }));

    assert!(bind(&f, "SELECT id FROM t WHERE id IN (1, 2, 3)").is_ok());
    let err = bind(&f, "SELECT id FROM t WHERE id IN (1, 'x')").unwrap_err();
    assert!(matches!(err, SqlError::TypeMismatch { .. }));
    f.cleanup();
}

#[test]
fn null_is_assignable_to_any_typed_context() {
    let f = Fixture::new("null_assignable");
    assert!(bind(&f, "SELECT id FROM t WHERE name = NULL OR id = NULL").is_ok());
    f.cleanup();
}

#[test]
fn order_by_nulls_default_matches_d5() {
    let f = Fixture::new("order_by_nulls");
    let BoundStatement::Select(s) =
        bind(&f, "SELECT id FROM t ORDER BY name ASC, id DESC").unwrap()
    else {
        panic!()
    };
    assert_eq!(s.order_by[0].nulls, NullsOrder::Last); // ASC default: NULLS LAST
    assert_eq!(s.order_by[1].nulls, NullsOrder::First); // DESC default: NULLS FIRST
    f.cleanup();
}

#[test]
fn parameter_index_exceeding_max_parameters_is_rejected() {
    let f = Fixture::new("param_limit");
    let limits = SqlLimits {
        max_parameters: 2,
        ..SqlLimits::default()
    };
    let stmt = parse_statement("SELECT id FROM t WHERE id = $2", &limits).unwrap();
    let metrics = SqlMetrics::default();
    assert!(bind_statement(
        &f.catalog,
        &f.ctx,
        &AuthContext::admin("p"),
        &metrics,
        &limits,
        &stmt
    )
    .is_ok());

    let stmt = parse_statement("SELECT id FROM t WHERE id = $3", &limits).unwrap();
    let err = bind_statement(
        &f.catalog,
        &f.ctx,
        &AuthContext::admin("p"),
        &metrics,
        &limits,
        &stmt,
    )
    .unwrap_err();
    assert!(matches!(err, SqlError::InvalidParameter { .. }));
    f.cleanup();
}

// -----------------------------------------------------------------
// INSERT / UPDATE / DELETE binding
// -----------------------------------------------------------------

#[test]
fn insert_with_explicit_columns_binds_one_row_per_catalog_column() {
    let f = Fixture::new("insert_explicit");
    let BoundStatement::Insert(ins) = bind(&f, "INSERT INTO t (id, name) VALUES (1, 'a')").unwrap()
    else {
        panic!()
    };
    assert_eq!(ins.rows.len(), 1);
    assert_eq!(ins.rows[0].len(), 3); // id, name, active(defaulted to NULL)
    assert_eq!(ins.rows[0][2].kind, BoundExprKind::Literal(None));
    assert!(ins.rows[0][2].nullable);
    f.cleanup();
}

#[test]
fn insert_missing_not_null_column_without_default_is_rejected() {
    let f = Fixture::new("insert_missing_not_null");
    let err = bind(&f, "INSERT INTO orders (id) VALUES (1)").unwrap_err(); // customer is NOT NULL
    assert!(matches!(err, SqlError::TypeMismatch { .. }));
    f.cleanup();
}

#[test]
fn insert_explicit_null_into_not_null_column_is_rejected() {
    let f = Fixture::new("insert_explicit_null");
    let err = bind(&f, "INSERT INTO orders (id, customer) VALUES (1, NULL)").unwrap_err();
    assert!(matches!(err, SqlError::TypeMismatch { .. }));
    f.cleanup();
}

#[test]
fn insert_multi_row_values_binds_every_row() {
    let f = Fixture::new("insert_multi_row");
    let BoundStatement::Insert(ins) = bind(
        &f,
        "INSERT INTO t (id, name) VALUES (1, 'a'), (2, 'b'), (3, 'c')",
    )
    .unwrap() else {
        panic!()
    };
    assert_eq!(ins.rows.len(), 3);
    f.cleanup();
}

#[test]
fn update_binds_assignment_ordinals_and_where() {
    let f = Fixture::new("update_basic");
    let BoundStatement::Update(u) = bind(&f, "UPDATE t SET name = 'x' WHERE id = 1").unwrap()
    else {
        panic!()
    };
    assert_eq!(u.assignments.len(), 1);
    assert_eq!(u.assignments[0].ordinal, 1); // name
    assert!(u.selection.is_some());
    f.cleanup();
}

#[test]
fn update_of_primary_key_column_is_rejected() {
    let f = Fixture::new("update_pk");
    let err = bind(&f, "UPDATE t SET id = 2 WHERE id = 1").unwrap_err();
    assert!(matches!(err, SqlError::Unsupported { .. }));
    f.cleanup();
}

#[test]
fn delete_binds_table_and_where() {
    let f = Fixture::new("delete_basic");
    let BoundStatement::Delete(d) = bind(&f, "DELETE FROM t WHERE id = 1").unwrap() else {
        panic!()
    };
    assert_eq!(d.table.table_id, {
        let db = f.catalog.list_databases().unwrap()[0].database_id;
        let schema = f.catalog.list_schemas(db).unwrap()[0].schema_id;
        f.catalog
            .get_table_by_name(schema, "t")
            .unwrap()
            .unwrap()
            .table_id
    });
    f.cleanup();
}

// -----------------------------------------------------------------
// DDL binding
// -----------------------------------------------------------------

#[test]
fn create_table_binds_columns_and_primary_key() {
    let f = Fixture::new("create_table");
    let BoundStatement::CreateTable(ct) = bind(
        &f,
        "CREATE TABLE foo (id INTEGER PRIMARY KEY, label TEXT NOT NULL)",
    )
    .unwrap() else {
        panic!()
    };
    assert_eq!(ct.columns.len(), 2);
    assert_eq!(ct.pk_ordinals, vec![0]);
    assert!(!ct.columns[1].nullable);
    f.cleanup();
}

#[test]
fn create_table_composite_primary_key() {
    let f = Fixture::new("composite_pk");
    let BoundStatement::CreateTable(ct) = bind(
        &f,
        "CREATE TABLE foo (a INTEGER, b INTEGER, c TEXT, PRIMARY KEY (a, b))",
    )
    .unwrap() else {
        panic!()
    };
    assert_eq!(ct.pk_ordinals, vec![0, 1]);
    f.cleanup();
}

#[test]
fn create_table_without_primary_key_is_rejected() {
    let f = Fixture::new("no_pk");
    let err = bind(&f, "CREATE TABLE foo (a INTEGER)").unwrap_err();
    assert!(matches!(err, SqlError::TypeMismatch { .. }));
    f.cleanup();
}

#[test]
fn drop_table_if_exists_on_missing_table_is_a_clean_noop() {
    let f = Fixture::new("drop_if_exists");
    let BoundStatement::DropTable(dt) = bind(&f, "DROP TABLE IF EXISTS does_not_exist").unwrap()
    else {
        panic!()
    };
    assert!(dt.table_id.is_none());
    f.cleanup();
}

#[test]
fn drop_table_without_if_exists_on_missing_table_errors() {
    let f = Fixture::new("drop_no_if_exists");
    let err = bind(&f, "DROP TABLE does_not_exist").unwrap_err();
    assert!(matches!(err, SqlError::UnknownObject { .. }));
    f.cleanup();
}

#[test]
fn create_index_binds_column_ordinals() {
    let f = Fixture::new("create_index");
    let BoundStatement::CreateIndex(ci) = bind(&f, "CREATE INDEX idx ON t (name)").unwrap() else {
        panic!()
    };
    assert_eq!(ci.column_ordinals, vec![1]);
    assert_eq!(ci.name, "idx");
    f.cleanup();
}

#[test]
fn drop_index_resolves_by_table_scoped_name() {
    let f = Fixture::new("drop_index");
    // `bind` never executes anything (this crate's own hard boundary) --
    // the catalog fixture state must be set up directly, exactly as
    // `IndexBuilder::create_index_online` would, not by binding
    // `CREATE INDEX` (which only *validates* against the catalog, never
    // writes to it).
    let database_id = f.catalog.list_databases().unwrap()[0].database_id;
    let schema_id = f.catalog.list_schemas(database_id).unwrap()[0].schema_id;
    let table_id = f
        .catalog
        .get_table_by_name(schema_id, "t")
        .unwrap()
        .unwrap()
        .table_id;
    f.catalog
        .create_index(
            table_id,
            "idx",
            rubixdb::catalog::schema::IndexKind::NonUnique,
            &[1],
        )
        .unwrap();

    let BoundStatement::DropIndex(di) = bind(&f, "DROP INDEX idx ON t").unwrap() else {
        panic!()
    };
    assert!(di.index_id.is_some());
    f.cleanup();
}

#[test]
fn explain_binds_the_inner_statement() {
    let f = Fixture::new("explain");
    let BoundStatement::Explain(inner) = bind(&f, "EXPLAIN SELECT id FROM t").unwrap() else {
        panic!()
    };
    assert!(matches!(*inner, BoundStatement::Select(_)));
    f.cleanup();
}

#[test]
fn transaction_statements_bind_as_trivial_markers() {
    let f = Fixture::new("txn");
    assert_eq!(bind(&f, "BEGIN").unwrap(), BoundStatement::Begin);
    assert_eq!(bind(&f, "COMMIT").unwrap(), BoundStatement::Commit);
    assert_eq!(bind(&f, "ROLLBACK").unwrap(), BoundStatement::Rollback);
    f.cleanup();
}

// -----------------------------------------------------------------
// Authorization (D25) — the same-pass, indistinguishable-error property
// -----------------------------------------------------------------

#[test]
fn reader_cannot_insert() {
    let f = Fixture::new("reader_insert_denied");
    let err = bind_with_auth(
        &f,
        "INSERT INTO t (id) VALUES (1)",
        &AuthContext::reader("r"),
    )
    .unwrap_err();
    assert!(matches!(err, SqlError::UnknownObject { kind: "table", .. }));
    f.cleanup();
}

#[test]
fn nonexistent_table_and_forbidden_table_produce_the_identical_error_shape() {
    let f = Fixture::new("indistinguishable");
    let no_default_access = AuthContext::none("stranger");

    let err_forbidden = bind_with_auth(&f, "SELECT id FROM t", &no_default_access).unwrap_err();
    let err_missing = bind_with_auth(
        &f,
        "SELECT id FROM this_table_does_not_exist",
        &no_default_access,
    )
    .unwrap_err();

    let (SqlError::UnknownObject { kind: k1, .. }, SqlError::UnknownObject { kind: k2, .. }) =
        (&err_forbidden, &err_missing)
    else {
        panic!("both must be UnknownObject, got {err_forbidden:?} / {err_missing:?}");
    };
    assert_eq!(k1, k2);
    assert_eq!(k1, &"table");
    f.cleanup();
}

#[test]
fn explicit_grant_authorizes_an_otherwise_default_access_none_principal() {
    let f = Fixture::new("explicit_grant");
    let database_id = f.catalog.list_databases().unwrap()[0].database_id;
    let schema_id = f.catalog.list_schemas(database_id).unwrap()[0].schema_id;
    let table = f
        .catalog
        .get_table_by_name(schema_id, "t")
        .unwrap()
        .unwrap();
    f.catalog
        .grant(
            "limited",
            rubixdb::catalog::schema::ObjectKind::Table,
            table.table_id,
            rubixdb::catalog::schema::Privilege::Select,
        )
        .unwrap();

    let auth = AuthContext::none("limited");
    assert!(bind_with_auth(&f, "SELECT id FROM t", &auth).is_ok());
    let err = bind_with_auth(&f, "INSERT INTO t (id) VALUES (1)", &auth).unwrap_err();
    assert!(matches!(err, SqlError::UnknownObject { .. }));
    f.cleanup();
}

#[test]
fn non_admin_cannot_create_database() {
    let f = Fixture::new("create_db_denied");
    let err = bind_with_auth(&f, "CREATE DATABASE d", &AuthContext::reader("r")).unwrap_err();
    assert!(matches!(err, SqlError::AuthorizationDenied { .. }));
    f.cleanup();
}

#[test]
fn metrics_record_bind_success_and_denial() {
    let f = Fixture::new("metrics");
    let limits = SqlLimits::default();
    let metrics = SqlMetrics::default();
    let stmt = parse_statement("SELECT id FROM t", &limits).unwrap();
    bind_statement(
        &f.catalog,
        &f.ctx,
        &AuthContext::admin("p"),
        &metrics,
        &limits,
        &stmt,
    )
    .unwrap();
    let stmt = parse_statement("SELECT id FROM t", &limits).unwrap();
    let _ = bind_statement(
        &f.catalog,
        &f.ctx,
        &AuthContext::none("nobody"),
        &metrics,
        &limits,
        &stmt,
    );
    let snap = metrics.snapshot();
    assert_eq!(snap.bound_statements, 1);
    assert_eq!(snap.bind_errors, 1);
    assert_eq!(snap.authorization_denials, 1);
    f.cleanup();
}

// -----------------------------------------------------------------
// Regression: `bind_shared`'s exponential-time re-bind (found by
// `plan_tests::deeply_nested_or_predicate_within_sql_limits_does_not_
// overflow_the_planner` while building Increment 8's query planner,
// not by inspection).
// -----------------------------------------------------------------

/// `bind_shared` used to unconditionally re-bind *every* operand
/// against the newly-determined shared type, including operands that
/// were already rigidly typed (a nested `BinaryOp`, a column, ...) and
/// whose type could never change on a second pass. Because `bind_
/// shared` sits on `bind`'s own recursive path (`bind_binary` calls it
/// for every binary operator, including nested ones), that unconditional
/// re-bind doubled the work at every nesting level -- `O(2^depth)`, not
/// `O(depth)`, for a long chain of binary operators. A 20-term chain
/// took ~4s; unfixed, this test's 100-term chain would have taken
/// (extrapolating from the measured ~1.92x-per-term growth) on the
/// order of `10^17` seconds -- a real, exploitable CPU-exhaustion
/// vector reachable with an ordinary, resource-limit-compliant `WHERE`
/// clause (`SqlLimits::max_expression_depth` alone does not stop it: a
/// 100-term chain is well within the default 128-deep limit). This test
/// asserts the fixed, linear-time behavior directly, with a real wall-
/// clock bound generous enough to never flake on a slow CI machine but
/// tight enough that any reintroduction of the `O(2^depth)` behavior
/// fails it immediately (100 terms would need to take under ~85,000
/// years at the old growth rate to slip under this bound by accident).
#[test]
fn long_or_chain_binds_in_linear_not_exponential_time() {
    let f = Fixture::new("or_chain_linear");
    let mut sql = "SELECT id FROM t WHERE ".to_string();
    for i in 0..100 {
        if i > 0 {
            sql.push_str(" OR ");
        }
        sql.push_str(&format!("id = {i}"));
    }
    let start = std::time::Instant::now();
    bind(&f, &sql).unwrap();
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "100-term OR chain took {elapsed:?} -- exponential re-bind regression"
    );
    f.cleanup();
}

/// The fix must not change *correctness*: two different rigid types on
/// either side of a shared-type unification (`BIGINT` vs. `INTEGER`,
/// D21's "exact match only") must still be rejected, and a flexible
/// literal must still be re-typed to conform to whichever rigid type is
/// found, exactly as before the fix.
#[test]
fn shared_type_unification_still_rejects_mismatched_rigid_types() {
    let f = Fixture::new("shared_type_still_correct");
    // orders.id is BIGINT, t.id is INTEGER (test_support::Fixture's own
    // documented, deliberate type mismatch for exactly this purpose).
    let err = bind(&f, "SELECT 1 FROM t INNER JOIN orders ON orders.id = t.id").unwrap_err();
    assert!(matches!(err, SqlError::TypeMismatch { .. }));
    f.cleanup();
}

#[test]
fn shared_type_unification_still_conforms_a_flexible_literal_to_a_rigid_column_type() {
    let f = Fixture::new("shared_type_literal_conforms");
    let BoundStatement::Select(s) = bind(&f, "SELECT 1 FROM orders WHERE orders.id = 5").unwrap()
    else {
        panic!()
    };
    let BoundExprKind::BinaryOp { right, .. } = &s.selection.as_ref().unwrap().kind else {
        panic!()
    };
    // `orders.id` is BIGINT -- the literal `5` must have been re-bound
    // to `Bigint`, not left at its own context-free `Integer` default.
    assert_eq!(right.ty, Some(RelationalType::Bigint));
    f.cleanup();
}

// -----------------------------------------------------------------
// Regression: an omitted column with a declared `DEFAULT` must bind to
// that default's own value, never to a `NULL` literal indistinguishable
// from an explicit `NULL` -- found while building Increment 10's write
// executor (`crate::bind::dml::bind_insert`'s own "an omitted column
// with a declared DEFAULT..." comment has the full account). Before
// this fix, `BoundInsert.rows` had no way to tell "the caller omitted
// this column" apart from "the caller wrote NULL explicitly" once both
// produced the identical `Literal(None)` -- a real correctness gap: any
// DEFAULT-bearing column omitted from an INSERT column list would have
// silently stored NULL instead of its declared default.
// -----------------------------------------------------------------

#[test]
fn insert_omitted_column_with_default_binds_to_the_defaults_own_value() {
    let f = Fixture::new("insert_default_substitution");
    let default_bytes =
        rubixdb::relational::value::encode_row(1, &[Some(RelationalValue::Integer(42))]);
    f.catalog
        .create_table(
            f.ctx.default_schema_id,
            "with_default",
            &[
                rubixdb::catalog::service::ColumnDef {
                    name: "id".to_string(),
                    data_type: rubixdb::relational::value::TYPE_TAG_INTEGER,
                    nullable: false,
                    default_value: None,
                    type_params: None,
                },
                rubixdb::catalog::service::ColumnDef {
                    name: "score".to_string(),
                    data_type: rubixdb::relational::value::TYPE_TAG_INTEGER,
                    nullable: true,
                    default_value: Some(default_bytes),
                    type_params: None,
                },
            ],
            &[0],
        )
        .unwrap();

    // `score` omitted entirely -- must bind to the DEFAULT's own value.
    let BoundStatement::Insert(ins) = bind(&f, "INSERT INTO with_default (id) VALUES (1)").unwrap()
    else {
        panic!()
    };
    assert_eq!(
        ins.rows[0][1].kind,
        BoundExprKind::Literal(Some(RelationalValue::Integer(42)))
    );

    // An *explicit* NULL on the same (nullable, defaulted) column must
    // still bind to NULL, never silently upgraded to the default.
    let BoundStatement::Insert(ins) =
        bind(&f, "INSERT INTO with_default (id, score) VALUES (2, NULL)").unwrap()
    else {
        panic!()
    };
    assert_eq!(ins.rows[0][1].kind, BoundExprKind::Literal(None));
    f.cleanup();
}

// -----------------------------------------------------------------
// Increment 11: GROUP BY / HAVING / aggregate binding
// -----------------------------------------------------------------

#[test]
fn group_by_binds_and_aggregate_calls_are_extracted_to_a_shared_indexed_list() {
    let f = Fixture::new("agg_bind_basic");
    let BoundStatement::Select(s) =
        bind(&f, "SELECT name, COUNT(*), SUM(id) FROM t GROUP BY name").unwrap()
    else {
        panic!()
    };
    assert_eq!(s.group_by.len(), 1);
    assert!(matches!(s.group_by[0].kind, BoundExprKind::Column(_)));
    assert_eq!(s.aggregates.len(), 2, "COUNT(*) and SUM(id)");
    assert_eq!(s.aggregates[0].func, crate::aggregate::AggregateFunc::Count);
    assert!(matches!(
        s.aggregates[0].arg,
        crate::aggregate::AggregateArg::Wildcard
    ));
    assert_eq!(s.aggregates[1].func, crate::aggregate::AggregateFunc::Sum);

    // Projection items 1/2 must now be AggregateRef(0)/AggregateRef(1),
    // never a raw Aggregate node (item 25's own "the bound tree never
    // carries an unextracted Aggregate node into the plan").
    assert_eq!(s.projection.len(), 3);
    assert_eq!(s.projection[1].expr.kind, BoundExprKind::AggregateRef(0));
    assert_eq!(s.projection[2].expr.kind, BoundExprKind::AggregateRef(1));
    f.cleanup();
}

#[test]
fn identical_aggregate_calls_are_deduplicated_to_one_shared_index() {
    let f = Fixture::new("agg_bind_dedup");
    let BoundStatement::Select(s) = bind(&f, "SELECT COUNT(*), COUNT(*) + 1 FROM t").unwrap()
    else {
        panic!()
    };
    assert_eq!(
        s.aggregates.len(),
        1,
        "two syntactically identical COUNT(*) calls must share one aggregate slot"
    );
    assert_eq!(s.projection[0].expr.kind, BoundExprKind::AggregateRef(0));
    f.cleanup();
}

#[test]
fn count_star_and_count_expr_are_distinguished() {
    let f = Fixture::new("agg_bind_count_kinds");
    let BoundStatement::Select(s) = bind(&f, "SELECT COUNT(*), COUNT(name) FROM t").unwrap() else {
        panic!()
    };
    assert_eq!(s.aggregates.len(), 2);
    assert!(matches!(
        s.aggregates[0].arg,
        crate::aggregate::AggregateArg::Wildcard
    ));
    assert!(matches!(
        s.aggregates[1].arg,
        crate::aggregate::AggregateArg::Expr(_)
    ));
    f.cleanup();
}

#[test]
fn aggregate_without_group_by_is_a_single_implicit_group() {
    let f = Fixture::new("agg_bind_implicit_group");
    let BoundStatement::Select(s) = bind(&f, "SELECT COUNT(*) FROM t").unwrap() else {
        panic!()
    };
    assert!(s.group_by.is_empty());
    assert_eq!(s.aggregates.len(), 1);
    f.cleanup();
}

#[test]
fn having_binds_aggregate_calls_and_is_boolean_typed() {
    let f = Fixture::new("agg_bind_having");
    let BoundStatement::Select(s) = bind(
        &f,
        "SELECT name, COUNT(*) FROM t GROUP BY name HAVING COUNT(*) > 1",
    )
    .unwrap() else {
        panic!()
    };
    assert_eq!(s.having.as_ref().unwrap().ty, Some(RelationalType::Boolean));
    // HAVING's own COUNT(*) must share the same aggregate slot as the
    // projection's COUNT(*) (item 25's dedup, applied across clauses).
    assert_eq!(s.aggregates.len(), 1);
    f.cleanup();
}

#[test]
fn order_by_may_reference_an_aggregate_result_once_aggregated() {
    let f = Fixture::new("agg_bind_order_by_agg");
    let BoundStatement::Select(s) = bind(
        &f,
        "SELECT name, COUNT(*) FROM t GROUP BY name ORDER BY COUNT(*)",
    )
    .unwrap() else {
        panic!()
    };
    assert_eq!(s.aggregates.len(), 1);
    assert_eq!(
        s.order_by[0].expr.kind,
        BoundExprKind::AggregateRef(0),
        "ORDER BY's own COUNT(*) must reuse the projection's aggregate slot"
    );
    f.cleanup();
}

#[test]
fn ungrouped_column_in_an_aggregated_select_list_is_rejected() {
    let f = Fixture::new("agg_bind_ungrouped_rejected");
    let err = bind(&f, "SELECT name, active, COUNT(*) FROM t GROUP BY name").unwrap_err();
    assert!(
        matches!(err, SqlError::TypeMismatch { .. }),
        "`active` is neither grouped nor aggregated -- must be rejected, never silently selected from an arbitrary row of the group, got {err:?}"
    );
    f.cleanup();
}

#[test]
fn ungrouped_column_with_no_group_by_at_all_is_rejected() {
    let f = Fixture::new("agg_bind_ungrouped_no_group_by");
    let err = bind(&f, "SELECT name, COUNT(*) FROM t").unwrap_err();
    assert!(matches!(err, SqlError::TypeMismatch { .. }));
    f.cleanup();
}

#[test]
fn group_by_expression_itself_may_be_selected_directly() {
    // The whole GROUP BY expression, not just a bare column, is group-
    // compatible when it exactly matches (item 17's "functionally
    // dependent" allowance via exact structural match).
    let f = Fixture::new("agg_bind_group_expr_selected");
    let BoundStatement::Select(s) =
        bind(&f, "SELECT active, COUNT(*) FROM t GROUP BY active").unwrap()
    else {
        panic!()
    };
    assert_eq!(s.projection.len(), 2);
    f.cleanup();
}

#[test]
fn group_by_without_any_aggregate_is_supported() {
    let f = Fixture::new("agg_bind_group_by_no_agg");
    let BoundStatement::Select(s) = bind(&f, "SELECT name FROM t GROUP BY name").unwrap() else {
        panic!()
    };
    assert!(s.aggregates.is_empty());
    assert_eq!(s.group_by.len(), 1);
    f.cleanup();
}

#[test]
fn composite_group_by_binds_every_expression() {
    let f = Fixture::new("agg_bind_composite_group_by");
    let BoundStatement::Select(s) = bind(
        &f,
        "SELECT name, active, COUNT(*) FROM t GROUP BY name, active",
    )
    .unwrap() else {
        panic!()
    };
    assert_eq!(s.group_by.len(), 2);
    f.cleanup();
}

#[test]
fn aggregate_in_where_is_rejected() {
    let f = Fixture::new("agg_bind_where_rejected");
    let err = bind(&f, "SELECT id FROM t WHERE COUNT(*) > 1").unwrap_err();
    assert!(
        matches!(err, SqlError::TypeMismatch { .. }),
        "item 19: aggregate expressions are never legal in WHERE, got {err:?}"
    );
    f.cleanup();
}

#[test]
fn nested_aggregate_is_rejected() {
    let f = Fixture::new("agg_bind_nested_rejected");
    let err = bind(&f, "SELECT SUM(COUNT(*)) FROM t").unwrap_err();
    assert!(
        matches!(err, SqlError::TypeMismatch { .. }),
        "item 18: aggregate-inside-aggregate must be rejected deterministically, got {err:?}"
    );
    f.cleanup();
}

#[test]
fn unknown_function_name_is_rejected_not_treated_as_an_aggregate() {
    let f = Fixture::new("agg_bind_unknown_function");
    let err = bind(&f, "SELECT MEDIAN(id) FROM t").unwrap_err();
    assert!(matches!(
        err,
        SqlError::UnknownObject {
            kind: "function",
            ..
        }
    ));
    f.cleanup();
}

#[test]
fn sum_of_text_column_is_a_type_mismatch() {
    let f = Fixture::new("agg_bind_sum_text");
    let err = bind(&f, "SELECT SUM(name) FROM t").unwrap_err();
    assert!(matches!(err, SqlError::TypeMismatch { .. }));
    f.cleanup();
}

#[test]
fn count_star_does_not_support_other_wildcards_and_min_max_reject_wildcard() {
    let f = Fixture::new("agg_bind_wildcard_rules");
    assert!(matches!(
        parse_then_bind(&f, "SELECT SUM(*) FROM t").unwrap_err(),
        SqlError::TypeMismatch { .. }
    ));
    assert!(matches!(
        parse_then_bind(&f, "SELECT MIN(*) FROM t").unwrap_err(),
        SqlError::TypeMismatch { .. }
    ));
    f.cleanup();
}

#[test]
fn avg_of_decimal_is_a_controlled_unsupported_error() {
    let f = Fixture::new("agg_bind_avg_decimal");
    f.catalog
        .create_table(
            f.ctx.default_schema_id,
            "priced",
            &[
                rubixdb::catalog::service::ColumnDef {
                    name: "id".to_string(),
                    data_type: rubixdb::relational::value::TYPE_TAG_INTEGER,
                    nullable: false,
                    default_value: None,
                    type_params: None,
                },
                rubixdb::catalog::service::ColumnDef {
                    name: "price".to_string(),
                    data_type: rubixdb::relational::value::TYPE_TAG_DECIMAL,
                    nullable: true,
                    default_value: None,
                    type_params: Some(vec![10, 2]),
                },
            ],
            &[0],
        )
        .unwrap();
    let err = bind(&f, "SELECT AVG(price) FROM priced").unwrap_err();
    assert!(
        matches!(err, SqlError::Unsupported { .. }),
        "AVG(DECIMAL) must be a controlled, documented unsupported error, never a silently wrong rescale, got {err:?}"
    );
    f.cleanup();
}

#[test]
fn min_max_and_sum_and_avg_on_orders_amount_resolve_expected_types() {
    let f = Fixture::new("agg_bind_numeric_types");
    let BoundStatement::Select(s) = bind(
        &f,
        "SELECT MIN(amount), MAX(amount), SUM(amount), AVG(amount), COUNT(amount) FROM orders",
    )
    .unwrap() else {
        panic!()
    };
    assert_eq!(s.projection[0].expr.ty, Some(RelationalType::Integer)); // MIN preserves input type
    assert_eq!(s.projection[1].expr.ty, Some(RelationalType::Integer)); // MAX preserves input type
    assert_eq!(s.projection[2].expr.ty, Some(RelationalType::Bigint)); // SUM(INTEGER) -> BIGINT
    assert_eq!(s.projection[3].expr.ty, Some(RelationalType::Double)); // AVG(INTEGER) -> DOUBLE
    assert_eq!(s.projection[4].expr.ty, Some(RelationalType::Bigint)); // COUNT -> BIGINT
    assert!(!s.projection[4].expr.nullable, "COUNT is never NULL");
    assert!(
        s.projection[2].expr.nullable,
        "SUM over zero/all-NULL input is NULL"
    );
    f.cleanup();
}

#[test]
fn group_by_all_and_rollup_remain_explicitly_unsupported() {
    let f = Fixture::new("agg_bind_rollup_unsupported");
    assert!(matches!(
        parse_then_bind(&f, "SELECT id FROM t GROUP BY ALL").unwrap_err(),
        SqlError::Unsupported { .. }
    ));
    assert!(matches!(
        parse_then_bind(&f, "SELECT id, COUNT(*) FROM t GROUP BY ROLLUP(id)").unwrap_err(),
        SqlError::Unsupported { .. }
    ));
    f.cleanup();
}

#[test]
fn count_distinct_remains_explicitly_unsupported() {
    let f = Fixture::new("agg_bind_count_distinct_unsupported");
    let err = parse_then_bind(&f, "SELECT COUNT(DISTINCT name) FROM t").unwrap_err();
    assert!(matches!(err, SqlError::Unsupported { .. }));
    f.cleanup();
}

#[test]
fn authorization_denial_on_an_aggregated_query_is_the_same_unknown_object_shape() {
    // item 48: aggregation must not bypass binder authorization -- a
    // principal without SELECT on `t` gets the identical existence-
    // hiding error shape whether or not the query aggregates.
    let f = Fixture::new("agg_bind_auth");
    let no_access = AuthContext::none("no-access-principal");
    let err =
        bind_with_auth(&f, "SELECT name, COUNT(*) FROM t GROUP BY name", &no_access).unwrap_err();
    assert!(matches!(err, SqlError::UnknownObject { .. }));
    f.cleanup();
}

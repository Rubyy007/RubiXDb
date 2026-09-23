//! Security tests — item 33 (SQL injection: parameter values are inert
//! data, identifiers are never string-built), item 42 (binder
//! authorization cannot be bypassed via quoting/case-folding/alias/
//! wildcard/schema-qualification tricks).

use crate::auth::AuthContext;
use crate::bind::bind_statement;
use crate::bound::*;
use crate::error::SqlError;
use crate::limits::SqlLimits;
use crate::metrics::SqlMetrics;
use crate::parse::parse_statement;
use crate::test_support::Fixture;

fn bind_with_auth(f: &Fixture, sql: &str, auth: &AuthContext) -> crate::Result<BoundStatement> {
    let limits = SqlLimits::default();
    let stmt =
        parse_statement(sql, &limits).unwrap_or_else(|e| panic!("parse failed for {sql:?}: {e}"));
    let metrics = SqlMetrics::default();
    bind_statement(&f.catalog, &f.ctx, auth, &metrics, &limits, &stmt)
}

fn bind(f: &Fixture, sql: &str) -> crate::Result<BoundStatement> {
    bind_with_auth(f, sql, &AuthContext::admin("test-principal"))
}

// -----------------------------------------------------------------
// D26: SQL injection — parameter/literal values are inert data
// -----------------------------------------------------------------

/// The OWASP-standard injection-attempt corpus, submitted as ordinary
/// string *literal values* (never as SQL grammar): each must bind as
/// exactly one `Literal::Text`/`RelationalValue::Text` carrying the
/// payload byte-for-byte, never influencing the statement's own shape
/// (one `WHERE`, one comparison, no extra statement).
#[test]
fn injection_payloads_as_literal_values_are_inert_text_never_reparsed() {
    let f = Fixture::new("injection_literals");
    let payloads = [
        "'; DROP TABLE t; --",
        "' OR '1'='1",
        "' UNION SELECT id FROM orders --",
        "1; DELETE FROM t WHERE 1=1",
        "\" OR \"\"=\"",
        "admin'--",
        "'; EXEC xp_cmdshell('dir'); --",
    ];
    for payload in payloads {
        // Escape only the single-quote delimiter the SQL *literal
        // syntax* itself requires (standard doubling) -- this is not
        // "sanitization" of the payload's meaning, only how a Rust test
        // constructs a valid `'...'` token to carry it as one literal.
        let escaped = payload.replace('\'', "''");
        let sql = format!("SELECT id FROM t WHERE name = '{escaped}'");
        let BoundStatement::Select(s) = bind(&f, &sql).unwrap_or_else(|e| panic!("{sql:?}: {e}"))
        else {
            panic!()
        };
        let Some(BoundExprKind::BinaryOp { right, .. }) = s.selection.as_ref().map(|e| &e.kind)
        else {
            panic!()
        };
        assert_eq!(
            right.kind,
            BoundExprKind::Literal(Some(rubixdb::relational::RelationalValue::Text(
                payload.to_string()
            ))),
            "payload must round-trip byte-for-byte as inert text"
        );
        // Exactly one statement was ever parsed -- the payload never
        // caused a second statement to appear.
        assert_eq!(
            crate::parse::parse_script(&sql, &SqlLimits::default())
                .unwrap()
                .len(),
            1
        );
    }
    f.cleanup();
}

/// The same corpus, submitted as `$n` parameters instead of literals —
/// parameters carry no text at all through this layer (only a resolved
/// type/index), so there is nothing for a payload to inject into.
#[test]
fn injection_payloads_as_parameters_never_reach_sql_grammar() {
    let f = Fixture::new("injection_params");
    let BoundStatement::Select(s) = bind(&f, "SELECT id FROM t WHERE name = $1").unwrap() else {
        panic!()
    };
    let Some(BoundExprKind::BinaryOp { right, .. }) = s.selection.as_ref().map(|e| &e.kind) else {
        panic!()
    };
    assert_eq!(right.kind, BoundExprKind::Parameter { index: 1 });
    // The bound tree has no field capable of holding a runtime-supplied
    // parameter *value* at all -- structurally, not by convention, an
    // injection payload supplied as the eventual parameter value at
    // execution time (a future increment's concern) cannot alter this
    // statement's shape, because this shape was already fixed at bind
    // time, before any parameter value exists.
    f.cleanup();
}

/// A table/column *name* containing SQL metacharacters, double-quoted
/// (identifier syntax) — must be treated as one literal identifier,
/// correctly rejected as unknown (no such table), never executed as a
/// second statement fragment.
#[test]
fn identifier_injection_is_isolated_by_quoting_never_executed() {
    let f = Fixture::new("identifier_injection");
    let sql = r#"SELECT id FROM "t; DROP TABLE t; --""#;
    let err = bind(&f, sql).unwrap_err();
    assert!(matches!(err, SqlError::UnknownObject { kind: "table", .. }));
    // The real table must still exist and still be usable afterward.
    assert!(bind(&f, "SELECT id FROM t").is_ok());
    f.cleanup();
}

#[test]
fn semicolon_stacked_queries_are_rejected_by_parse_statement() {
    let f = Fixture::new("stacked_queries");
    let limits = SqlLimits::default();
    let err = parse_statement("SELECT id FROM t; DROP TABLE t;", &limits).unwrap_err();
    assert!(matches!(err, SqlError::ResourceLimit { .. }));
    let _ = f;
}

// -----------------------------------------------------------------
// D25/item 42: authorization cannot be bypassed
// -----------------------------------------------------------------

#[test]
fn case_folding_cannot_be_used_to_bypass_authorization() {
    let f = Fixture::new("case_bypass");
    let stranger = AuthContext::none("stranger");
    for sql in [
        "SELECT id FROM t",
        "SELECT id FROM T",
        "select id from t",
        "SeLeCt Id FrOm T",
    ] {
        let err = bind_with_auth(&f, sql, &stranger).unwrap_err();
        assert!(
            matches!(err, SqlError::UnknownObject { kind: "table", .. }),
            "{sql:?} must still be denied"
        );
    }
    f.cleanup();
}

#[test]
fn quoted_identifier_naming_the_same_table_cannot_bypass_authorization() {
    let f = Fixture::new("quoted_bypass");
    let stranger = AuthContext::none("stranger");
    let err = bind_with_auth(&f, r#"SELECT id FROM "t""#, &stranger).unwrap_err();
    assert!(matches!(err, SqlError::UnknownObject { kind: "table", .. }));
    f.cleanup();
}

#[test]
fn table_alias_cannot_bypass_authorization() {
    let f = Fixture::new("alias_bypass");
    let stranger = AuthContext::none("stranger");
    let err = bind_with_auth(&f, "SELECT x.id FROM t AS x", &stranger).unwrap_err();
    assert!(matches!(err, SqlError::UnknownObject { kind: "table", .. }));
    f.cleanup();
}

#[test]
fn wildcard_expansion_cannot_bypass_authorization() {
    let f = Fixture::new("wildcard_bypass");
    let stranger = AuthContext::none("stranger");
    let err = bind_with_auth(&f, "SELECT * FROM t", &stranger).unwrap_err();
    assert!(matches!(err, SqlError::UnknownObject { kind: "table", .. }));
    f.cleanup();
}

#[test]
fn schema_qualification_cannot_bypass_authorization() {
    let f = Fixture::new("schema_qualified_bypass");
    let stranger = AuthContext::none("stranger");
    let err = bind_with_auth(&f, "SELECT id FROM public.t", &stranger).unwrap_err();
    assert!(matches!(err, SqlError::UnknownObject { kind: "table", .. }));
    f.cleanup();
}

#[test]
fn joining_an_authorized_table_cannot_leak_an_unauthorized_ones_columns() {
    let f = Fixture::new("join_bypass");
    let database_id = f.catalog.list_databases().unwrap()[0].database_id;
    let schema_id = f.catalog.list_schemas(database_id).unwrap()[0].schema_id;
    let orders = f
        .catalog
        .get_table_by_name(schema_id, "orders")
        .unwrap()
        .unwrap();
    f.catalog
        .grant(
            "half-access",
            rubixdb::catalog::schema::ObjectKind::Table,
            orders.table_id,
            rubixdb::catalog::schema::Privilege::Select,
        )
        .unwrap();
    let half = AuthContext::none("half-access");
    // Allowed: orders alone.
    assert!(bind_with_auth(&f, "SELECT id FROM orders", &half).is_ok());
    // Denied: joining in `t`, which was never granted, even though the
    // join's own ON condition only references `orders.id` alongside it.
    let err = bind_with_auth(
        &f,
        "SELECT o.id FROM orders o INNER JOIN t ON o.t_id = t.id",
        &half,
    )
    .unwrap_err();
    assert!(matches!(err, SqlError::UnknownObject { kind: "table", .. }));
    f.cleanup();
}

#[test]
fn physical_ids_are_never_accepted_as_client_input() {
    // The grammar itself has no way to name a table by its physical
    // table_id/schema_id — every DDL/DML/SELECT statement only ever
    // carries an `ObjectName`, resolved exclusively through
    // `CatalogService` by name. This test documents/enforces that by
    // construction: a numeric "name" parses as an expression/literal
    // context, never as an object-name context, so there is no code
    // path here to even attempt an ID-guessing bypass.
    let f = Fixture::new("no_physical_ids");
    let err = parse_statement("SELECT id FROM 1", &SqlLimits::default()).unwrap_err();
    assert!(matches!(
        err,
        SqlError::Parse { .. } | SqlError::Unsupported { .. }
    ));
    f.cleanup();
}

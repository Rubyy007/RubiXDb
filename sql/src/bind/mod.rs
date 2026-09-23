//! The binder — D15: parsed `crate::ast::Statement` -> authorized,
//! typed `crate::bound::BoundStatement`. Every table/column/index
//! resolution goes through `scope`'s helpers, which check D25
//! authorization in the same pass (item 15) and never let "does not
//! exist" and "exists but forbidden" be told apart (item 32).

pub mod ddl;
pub mod dml;
pub mod expr;
pub mod scope;
pub mod select;

pub use scope::BindContext;

use rubixdb::catalog::CatalogService;

use crate::ast;
use crate::auth::AuthContext;
use crate::bound::BoundStatement;
use crate::error::Result;
use crate::limits::SqlLimits;
use crate::metrics::SqlMetrics;

/// Binds one parsed statement. `ctx` supplies the "current database/
/// schema" an unqualified name resolves against (item 17); `auth`
/// supplies the principal and its default access level (D25); `metrics`
/// records bounded-cardinality parse/bind counters (item 44) — callers
/// that don't care may pass a fresh, throwaway `SqlMetrics::default()`.
pub fn bind_statement(
    catalog: &CatalogService,
    ctx: &BindContext,
    auth: &AuthContext,
    metrics: &SqlMetrics,
    limits: &SqlLimits,
    stmt: &ast::Statement,
) -> Result<BoundStatement> {
    let result = bind_statement_inner(catalog, ctx, auth, metrics, limits, stmt);
    match &result {
        Ok(_) => metrics.record_bind_success(),
        Err(crate::error::SqlError::Unsupported { .. }) => metrics.record_unsupported(),
        Err(_) => metrics.record_bind_error(),
    }
    result
}

fn bind_statement_inner(
    catalog: &CatalogService,
    ctx: &BindContext,
    auth: &AuthContext,
    metrics: &SqlMetrics,
    limits: &SqlLimits,
    stmt: &ast::Statement,
) -> Result<BoundStatement> {
    Ok(match stmt {
        ast::Statement::Select(s) => BoundStatement::Select(select::bind_select(catalog, ctx, auth, metrics, limits, s)?),
        ast::Statement::Insert(s) => BoundStatement::Insert(dml::bind_insert(catalog, ctx, auth, metrics, limits, s)?),
        ast::Statement::Update(s) => BoundStatement::Update(dml::bind_update(catalog, ctx, auth, metrics, limits, s)?),
        ast::Statement::Delete(s) => BoundStatement::Delete(dml::bind_delete(catalog, ctx, auth, metrics, limits, s)?),
        ast::Statement::CreateDatabase(s) => {
            BoundStatement::CreateDatabase(ddl::bind_create_database(catalog, auth, metrics, s)?)
        }
        ast::Statement::CreateSchema(s) => {
            BoundStatement::CreateSchema(ddl::bind_create_schema(catalog, ctx, auth, metrics, s)?)
        }
        ast::Statement::CreateTable(s) => {
            BoundStatement::CreateTable(ddl::bind_create_table(catalog, ctx, auth, metrics, limits, s)?)
        }
        ast::Statement::DropTable(s) => BoundStatement::DropTable(ddl::bind_drop_table(catalog, ctx, auth, metrics, s)?),
        ast::Statement::CreateIndex(s) => {
            BoundStatement::CreateIndex(ddl::bind_create_index(catalog, ctx, auth, metrics, limits, s)?)
        }
        ast::Statement::DropIndex(s) => BoundStatement::DropIndex(ddl::bind_drop_index(catalog, ctx, auth, metrics, s)?),
        ast::Statement::Explain(e) => {
            let inner = bind_statement_inner(catalog, ctx, auth, metrics, limits, &e.statement)?;
            BoundStatement::Explain(Box::new(inner))
        }
        ast::Statement::Begin => BoundStatement::Begin,
        ast::Statement::Commit => BoundStatement::Commit,
        ast::Statement::Rollback => BoundStatement::Rollback,
    })
}

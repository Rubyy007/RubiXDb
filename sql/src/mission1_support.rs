#![allow(dead_code)]
//! Shared harness for the SQL-mission-1 test modules (subqueries, CTEs, set
//! operations): a real `LsmEngine`, catalog, `TableStore`, `IndexBuilder` and
//! `TransactionManager`, driven through the real parse, bind, plan and
//! execute pipeline. `#[cfg(test)]`-only.

use std::sync::Arc;

use rubixdb::catalog::CatalogService;
use rubixdb::relational::index::IndexBuilder;
use rubixdb::relational::table_store::TableStore;
use rubixdb::relational::{RelationalValue, Transaction, TransactionManager};

use crate::auth::AuthContext;
use crate::bind::bind_statement;
use crate::bound::BoundStatement;
use crate::exec::write::{execute_write_autocommit, WriteMetrics, WriteResult};
use crate::exec::{
    execute, execute_autocommit, CancellationToken, ExecLimits, ExecMetrics, QueryResult,
};
use crate::limits::SqlLimits;
use crate::metrics::SqlMetrics;
use crate::parse::parse_statement;
use crate::plan::{build_plan, Plan, PlannerLimits, PlannerMetrics};
use crate::test_support::Fixture;

pub(crate) type V = Option<RelationalValue>;
pub(crate) type Row = Vec<V>;

pub(crate) fn int(n: i32) -> V {
    Some(RelationalValue::Integer(n))
}
pub(crate) fn big(n: i64) -> V {
    Some(RelationalValue::Bigint(n))
}
pub(crate) fn text(s: &str) -> V {
    Some(RelationalValue::Text(s.to_string()))
}
pub(crate) fn boolean(b: bool) -> V {
    Some(RelationalValue::Boolean(b))
}
pub(crate) const NULL: V = None;

/// `t(id INTEGER PK, name TEXT NULL, active BOOLEAN NULL)` and
/// `orders(id BIGINT PK, customer TEXT NOT NULL, amount INTEGER NULL,
/// t_id INTEGER NULL)` -- the shared `Fixture` schema.
pub(crate) struct Db {
    pub f: Fixture,
    pub catalog: Arc<CatalogService>,
    pub store: Arc<TableStore>,
    pub builder: Arc<IndexBuilder>,
    pub txm: TransactionManager,
}

impl Db {
    pub fn new(tag: &str) -> Self {
        let f = Fixture::new(tag);
        let catalog = Arc::new(CatalogService::new(Arc::clone(&f.engine)));
        let store = Arc::new(TableStore::new(Arc::clone(&f.engine), Arc::clone(&catalog)));
        let builder = Arc::new(IndexBuilder::new(
            Arc::clone(&f.engine),
            Arc::clone(&catalog),
            Arc::clone(&store),
        ));
        let txm = TransactionManager::new(Arc::clone(&f.engine), Arc::clone(&store));
        Db {
            f,
            catalog,
            store,
            builder,
            txm,
        }
    }

    pub fn table_id(&self, name: &str) -> u32 {
        self.catalog
            .get_table_by_name(self.f.ctx.default_schema_id, name)
            .unwrap()
            .unwrap()
            .table_id
    }

    /// `(id, name, active)` rows.
    pub fn seed_t(&self, rows: &[(i32, Option<&str>, Option<bool>)]) {
        let t = self.table_id("t");
        for (id, name, active) in rows {
            self.store
                .put_row(
                    t,
                    &[int(*id), name.map(text).unwrap_or(NULL), active.map(boolean).unwrap_or(NULL)],
                )
                .unwrap();
        }
    }

    /// `(id, customer, amount, t_id)` rows.
    pub fn seed_orders(&self, rows: &[(i64, &str, Option<i32>, Option<i32>)]) {
        let o = self.table_id("orders");
        for (id, customer, amount, t_id) in rows {
            self.store
                .put_row(
                    o,
                    &[
                        big(*id),
                        text(customer),
                        amount.map(int).unwrap_or(NULL),
                        t_id.map(int).unwrap_or(NULL),
                    ],
                )
                .unwrap();
        }
    }

    pub fn try_bind(&self, sql: &str) -> crate::Result<BoundStatement> {
        self.try_bind_with(sql, &AuthContext::admin("test-principal"), &SqlLimits::default())
    }

    pub fn try_bind_with(
        &self,
        sql: &str,
        auth: &AuthContext,
        limits: &SqlLimits,
    ) -> crate::Result<BoundStatement> {
        let stmt = parse_statement(sql, limits)?;
        bind_statement(
            &self.f.catalog,
            &self.f.ctx,
            auth,
            &SqlMetrics::default(),
            limits,
            &stmt,
        )
    }

    pub fn try_plan(&self, sql: &str) -> crate::Result<Plan> {
        self.try_plan_with(sql, &SqlLimits::default())
    }

    pub fn try_plan_with(&self, sql: &str, limits: &SqlLimits) -> crate::Result<Plan> {
        let bound = self.try_bind_with(sql, &AuthContext::admin("test-principal"), limits)?;
        build_plan(
            &bound,
            &self.f.catalog,
            &PlannerLimits::default(),
            &PlannerMetrics::default(),
        )
    }

    pub fn plan(&self, sql: &str) -> Plan {
        self.try_plan(sql)
            .unwrap_or_else(|e| panic!("bind/plan failed for {sql:?}: {e}"))
    }

    pub fn try_run(&self, sql: &str) -> crate::Result<QueryResult> {
        self.try_run_with(sql, &ExecLimits::default(), &ExecMetrics::default(), &[])
    }

    pub fn try_run_with(
        &self,
        sql: &str,
        limits: &ExecLimits,
        metrics: &ExecMetrics,
        params: &[V],
    ) -> crate::Result<QueryResult> {
        let plan = self.try_plan(sql)?;
        execute_autocommit(
            &plan,
            &self.txm,
            &self.store,
            &self.builder,
            params,
            limits,
            metrics,
            &CancellationToken::new(),
        )
    }

    pub fn run(&self, sql: &str) -> QueryResult {
        self.try_run(sql)
            .unwrap_or_else(|e| panic!("query failed for {sql:?}: {e}"))
    }

    /// Result rows only.
    pub fn rows(&self, sql: &str) -> Vec<Row> {
        self.run(sql).rows
    }

    /// Result rows, sorted by their `Debug` form -- for statements without
    /// an `ORDER BY`, whose row order is unspecified.
    pub fn rows_sorted(&self, sql: &str) -> Vec<Row> {
        let mut rows = self.rows(sql);
        rows.sort_by_key(|r| format!("{r:?}"));
        rows
    }

    pub fn run_in_txn(&self, sql: &str, txn: &Transaction) -> crate::Result<QueryResult> {
        let plan = self.try_plan(sql)?;
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
    }

    pub fn try_write(&self, sql: &str) -> crate::Result<WriteResult> {
        self.try_write_with(sql, &ExecLimits::default())
    }

    pub fn try_write_with(&self, sql: &str, limits: &ExecLimits) -> crate::Result<WriteResult> {
        let plan = self.try_plan(sql)?;
        execute_write_autocommit(
            &plan,
            &self.txm,
            &self.store,
            &self.catalog,
            &self.builder,
            &[],
            limits,
            &WriteMetrics::default(),
            &CancellationToken::new(),
        )
    }

    pub fn write(&self, sql: &str) -> WriteResult {
        self.try_write(sql)
            .unwrap_or_else(|e| panic!("write failed for {sql:?}: {e}"))
    }

    pub fn cleanup(self) {
        self.f.cleanup();
    }
}

//! `POST /v1/sql` — `PHASE_RELATIONAL_SQL_API_ARCHITECTURE.md`. The
//! **one** SQL execution path this product exposes: `authenticate →
//! parse → bind → authorize → plan → execute → serialize`, entirely
//! inside `rubixdb_sql`/`rubixdb::relational` — this handler is a thin
//! translation layer (JSON ↔ typed request/response) and never
//! reimplements any part of that pipeline itself (item 4/105).

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Extension, State};
use axum::Json;
use rubixdb::relational::{RelationalType, RelationalValue};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use rubixdb_sql::auth::AuthContext;
use rubixdb_sql::bind::bind_statement;
use rubixdb_sql::bound::BoundStatement;
use rubixdb_sql::exec::write::{execute_write, execute_write_autocommit, WriteStatementKind};
use rubixdb_sql::exec::{execute, execute_autocommit, CancellationToken, ExecLimits, QueryResult};
use rubixdb_sql::parse::parse_statement;
use rubixdb_sql::plan::{build_plan, explain, Plan, PlannerLimits};
use rubixdb_sql::{Result as SqlResult, SqlError, SqlLimits};

use crate::auth::{to_sql_auth_context, Principal};
use crate::error::ApiError;
use crate::sql_params::{value_to_json, SqlParam, SqlValueJson};
use crate::sql_session::SessionLookupError;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct SqlRequest {
    pub sql: String,
    #[serde(default)]
    pub params: Vec<SqlParam>,
    #[serde(default)]
    pub session_id: Option<Uuid>,
}

#[derive(Debug, Serialize)]
pub struct ColumnMeta {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: Option<&'static str>,
    pub nullable: bool,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SqlResultBody {
    /// A `SELECT` result — item 49/96: column order/types/`NULL`
    /// preserved exactly as the executor produced them, never
    /// reshaped.
    Rows {
        columns: Vec<ColumnMeta>,
        rows: Vec<Vec<SqlValueJson>>,
        row_count: usize,
    },
    /// `INSERT`/`UPDATE`/`DELETE` — item 49/51: returned only after the
    /// write has passed its approved commit/durability boundary (this
    /// handler never constructs this variant before that point).
    Write {
        statement: &'static str,
        rows_affected: u64,
    },
    /// DDL — item 49: `CatalogService`'s own DDL methods are already
    /// atomic/durable the instant they return `Ok` (`PHASE_RELATIONAL_
    /// WRITE_EXECUTOR_ARCHITECTURE.md`), so "success" here already means
    /// durable, exactly like `Write`.
    Ddl,
    /// item 135: the actual structured plan, rendered by `rubixdb_sql::
    /// plan::explain`. Never fabricated text, and (a deliberate choice
    /// this crate's own architecture doc documents) never executes the
    /// inner statement, unlike `rubixdb_sql::exec::execute`'s own
    /// `Plan::Explain` handling.
    Explain { plan_text: String },
    /// item 50: a new session now holds an open transaction — `session_
    /// id` on the outer response is the one to reuse for every
    /// following statement in it.
    Begin,
    /// item 50/51: the session's transaction passed its commit/
    /// durability boundary; the session no longer exists.
    Commit,
    /// item 50: the session's transaction's buffered write-set was
    /// discarded; the session no longer exists.
    Rollback,
}

#[derive(Debug, Serialize)]
pub struct SqlResponse {
    /// `None` whenever this statement never touched the session
    /// registry (item 22's own "the common case is completely
    /// stateless" design, `PHASE_RELATIONAL_SQL_API_ARCHITECTURE.md`
    /// §3) — `Some` for `BEGIN` (a new session) and for any statement
    /// run against an existing `session_id` that is *still open*
    /// afterward (everything except `COMMIT`/`ROLLBACK`, which return
    /// `None` because the session no longer exists).
    pub session_id: Option<Uuid>,
    pub result: SqlResultBody,
}

fn relational_type_name(ty: RelationalType) -> &'static str {
    match ty {
        RelationalType::Boolean => "boolean",
        RelationalType::Integer => "integer",
        RelationalType::Bigint => "bigint",
        RelationalType::Real => "real",
        RelationalType::Double => "double",
        RelationalType::Decimal { .. } => "decimal",
        RelationalType::Text => "text",
        RelationalType::Blob => "blob",
        RelationalType::Date => "date",
        RelationalType::Time => "time",
        RelationalType::Timestamp => "timestamp",
    }
}

fn query_result_to_json(
    result: QueryResult,
    metrics: &crate::sql_metrics::SqlApiMetrics,
) -> SqlResultBody {
    let columns: Vec<ColumnMeta> = result
        .schema
        .fields
        .iter()
        .map(|f| ColumnMeta {
            name: f.name.clone(),
            ty: f.ty.map(relational_type_name),
            nullable: f.nullable,
        })
        .collect();
    let row_count = result.rows.len();
    metrics.record_rows_returned(row_count as u64);
    let rows: Vec<Vec<SqlValueJson>> = result
        .rows
        .into_iter()
        .map(|row| row.iter().map(value_to_json).collect())
        .collect();
    SqlResultBody::Rows {
        columns,
        rows,
        row_count,
    }
}

fn write_statement_name(kind: WriteStatementKind) -> &'static str {
    match kind {
        WriteStatementKind::Insert => "INSERT",
        WriteStatementKind::Update => "UPDATE",
        WriteStatementKind::Delete => "DELETE",
        WriteStatementKind::Ddl => "DDL",
    }
}

fn write_result_to_json(
    result: rubixdb_sql::exec::write::WriteResult,
    is_ddl: bool,
) -> SqlResultBody {
    if is_ddl {
        SqlResultBody::Ddl
    } else {
        SqlResultBody::Write {
            statement: write_statement_name(result.kind),
            rows_affected: result.rows_affected,
        }
    }
}

/// item 10/11: runs `body` (the actual, already-`parse`d/`bind`-ed
/// execution call) on a blocking-pool thread so a potentially long-
/// running `SELECT`/aggregate/write never occupies a tokio async-
/// runtime worker thread; races it against `deadline` (a backstop
/// *above* the executor's own internal `ExecCtx::check` deadline
/// enforcement, covering the narrow case where a single underlying
/// storage call runs long enough that the internal check never gets a
/// chance to fire); and cancels `body`'s own `CancellationToken` the
/// instant this future itself is dropped (a client disconnect drops the
/// handler's future, which drops this function's `CancelOnDrop` guard,
/// which sets the token the abandoned blocking task polls at its own
/// next `ExecCtx::check()` call — item 11's "when a client disconnects,
/// the running read query should stop as soon as safely possible").
///
/// `body` receives an owned clone of `state` — every SQL call needs
/// `Arc<AppState>`'s own long-lived fields (`table_store`/
/// `index_builder`/`catalog`/`txm`/metrics), and cloning the one `Arc`
/// once here is simpler and safer than extracting several individual
/// borrows that would otherwise need an unsound lifetime cast to satisfy
/// `spawn_blocking`'s own `'static` bound.
async fn run_with_cancellation_and_deadline<F, T>(
    state: &Arc<AppState>,
    deadline: Duration,
    body: F,
) -> Result<T, ApiError>
where
    F: FnOnce(Arc<AppState>, CancellationToken) -> SqlResult<T> + Send + 'static,
    T: Send + 'static,
{
    struct CancelOnDrop(CancellationToken);
    impl Drop for CancelOnDrop {
        fn drop(&mut self) {
            self.0.cancel();
        }
    }

    let cancellation = CancellationToken::new();
    let _guard = CancelOnDrop(cancellation.clone());
    let task_cancellation = cancellation.clone();
    let task_state = Arc::clone(state);
    let handle = tokio::task::spawn_blocking(move || body(task_state, task_cancellation));

    // A backstop above the executor's own internal deadline check --
    // generous slack so the internal check is always the one that fires
    // first in the ordinary case.
    match tokio::time::timeout(deadline + Duration::from_secs(2), handle).await {
        Ok(Ok(inner)) => inner.map_err(ApiError::from),
        Ok(Err(_join_err)) => Err(ApiError::Validation(
            "internal: SQL execution task failed unexpectedly".to_string(),
        )),
        Err(_elapsed) => {
            cancellation.cancel();
            Err(ApiError::SqlDeadlineExceeded)
        }
    }
}

fn exec_limits(state: &AppState) -> ExecLimits {
    ExecLimits {
        deadline: Some(Duration::from_secs(
            state.config.sql_statement_deadline_secs,
        )),
        ..ExecLimits::default()
    }
}

pub async fn sql(
    State(state): State<Arc<AppState>>,
    Extension(principal): Extension<Principal>,
    Json(req): Json<SqlRequest>,
) -> Result<Json<SqlResponse>, ApiError> {
    state.sql.api_metrics.record_request();
    let result = handle(&state, &principal, req).await;
    match &result {
        Ok(_) => state.sql.api_metrics.record_success(),
        Err(ApiError::Sql(SqlError::Cancelled)) => state.sql.api_metrics.record_cancellation(),
        Err(ApiError::SqlDeadlineExceeded) => state.sql.api_metrics.record_deadline_exceeded(),
        Err(_) => state.sql.api_metrics.record_error(),
    }
    result.map(Json)
}

async fn handle(
    state: &Arc<AppState>,
    principal: &Principal,
    req: SqlRequest,
) -> Result<SqlResponse, ApiError> {
    let sql_limits = SqlLimits::default();
    let stmt = parse_statement(&req.sql, &sql_limits)?;

    let bind_context = state.sql.bind_context()?;
    let auth_ctx: AuthContext = to_sql_auth_context(principal);
    let bound = bind_statement(
        &state.sql.catalog,
        &bind_context,
        &auth_ctx,
        &state.sql.sql_metrics,
        &sql_limits,
        &stmt,
    )?;

    let plan = build_plan(
        &bound,
        &state.sql.catalog,
        &PlannerLimits::default(),
        &state.sql.planner_metrics,
    )?;

    // item 135: EXPLAIN never executes -- rendered structurally and
    // returned immediately, regardless of session/params.
    if matches!(plan, Plan::Explain(_)) {
        return Ok(SqlResponse {
            session_id: req.session_id,
            result: SqlResultBody::Explain {
                plan_text: explain(&plan),
            },
        });
    }

    let params: Vec<Option<RelationalValue>> = req
        .params
        .into_iter()
        .map(SqlParam::into_relational_value)
        .collect::<Result<_, _>>()?;

    match &bound {
        BoundStatement::Begin => handle_begin(state, principal, req.session_id).await,
        BoundStatement::Commit => handle_commit(state, principal, req.session_id).await,
        BoundStatement::Rollback => handle_rollback(state, principal, req.session_id).await,
        BoundStatement::Select(_) => {
            handle_read(state, principal, req.session_id, plan, params).await
        }
        BoundStatement::Insert(_)
        | BoundStatement::Update(_)
        | BoundStatement::Delete(_)
        | BoundStatement::CreateDatabase(_)
        | BoundStatement::CreateSchema(_)
        | BoundStatement::CreateTable(_)
        | BoundStatement::DropTable(_)
        | BoundStatement::CreateIndex(_)
        | BoundStatement::DropIndex(_) => {
            handle_write(state, principal, req.session_id, plan, params).await
        }
        BoundStatement::Explain(_) => {
            unreachable!(
                "Plan::Explain was already handled above for every BoundStatement::Explain"
            )
        }
    }
}

// =======================================================================
// BEGIN / COMMIT / ROLLBACK — the only statements this endpoint ever
// touches the session registry for (`sql_session`'s own doc comment).
// =======================================================================

async fn handle_begin(
    state: &Arc<AppState>,
    principal: &Principal,
    session_id: Option<Uuid>,
) -> Result<SqlResponse, ApiError> {
    if let Some(existing) = session_id {
        // A session id was supplied alongside BEGIN -- only valid if it
        // does *not* currently resolve (this principal's own prior
        // session already ended). If it resolves, a transaction is
        // already open on it: nested BEGIN is rejected, and the
        // transaction is put back untouched (never silently discarded).
        match state.sql.sessions.take(existing, &principal.name) {
            Ok(txn) => {
                let created_at = Instant::now();
                state
                    .sql
                    .sessions
                    .put_back(existing, principal.name.clone(), txn, created_at);
                return Err(ApiError::Validation(
                    "a transaction is already open on this session_id; COMMIT or ROLLBACK it before issuing BEGIN again"
                        .to_string(),
                ));
            }
            Err(SessionLookupError::NotFound) => {
                // Fall through -- treat exactly like no session_id was
                // given at all, a fresh session is created below.
            }
        }
    }
    let txn = state.sql.txm.begin().map_err(SqlError::from)?;
    let new_id = state
        .sql
        .sessions
        .create(&principal.name, txn)
        .map_err(|_| ApiError::SqlTooManySessions)?;
    Ok(SqlResponse {
        session_id: Some(new_id),
        result: SqlResultBody::Begin,
    })
}

async fn handle_commit(
    state: &Arc<AppState>,
    principal: &Principal,
    session_id: Option<Uuid>,
) -> Result<SqlResponse, ApiError> {
    let Some(id) = session_id else {
        return Err(ApiError::Validation(
            "COMMIT requires session_id (no transaction is open)".to_string(),
        ));
    };
    let txn = state
        .sql
        .sessions
        .take(id, &principal.name)
        .map_err(|_| ApiError::SqlSessionNotFound)?;
    txn.commit().map_err(SqlError::from)?;
    Ok(SqlResponse {
        session_id: None,
        result: SqlResultBody::Commit,
    })
}

async fn handle_rollback(
    state: &Arc<AppState>,
    principal: &Principal,
    session_id: Option<Uuid>,
) -> Result<SqlResponse, ApiError> {
    let Some(id) = session_id else {
        return Err(ApiError::Validation(
            "ROLLBACK requires session_id (no transaction is open)".to_string(),
        ));
    };
    let txn = state
        .sql
        .sessions
        .take(id, &principal.name)
        .map_err(|_| ApiError::SqlSessionNotFound)?;
    txn.rollback().map_err(SqlError::from)?;
    Ok(SqlResponse {
        session_id: None,
        result: SqlResultBody::Rollback,
    })
}

// =======================================================================
// Reads / writes — session path (statement runs inside an existing
// caller-held transaction) vs. autocommit path (no session touched at
// all, item 22's own stateless-by-default design).
// =======================================================================

async fn handle_read(
    state: &Arc<AppState>,
    principal: &Principal,
    session_id: Option<Uuid>,
    plan: Plan,
    params: Vec<Option<RelationalValue>>,
) -> Result<SqlResponse, ApiError> {
    let limits = exec_limits(state);
    let deadline = Duration::from_secs(state.config.sql_statement_deadline_secs);

    match session_id {
        None => {
            let result =
                run_with_cancellation_and_deadline(state, deadline, move |state, cancellation| {
                    execute_autocommit(
                        &plan,
                        &state.sql.txm,
                        &state.sql.table_store,
                        &state.sql.index_builder,
                        &params,
                        &limits,
                        &state.sql.exec_metrics,
                        &cancellation,
                    )
                })
                .await?;
            Ok(SqlResponse {
                session_id: None,
                result: query_result_to_json(result, &state.sql.api_metrics),
            })
        }
        Some(id) => {
            let txn = state
                .sql
                .sessions
                .take(id, &principal.name)
                .map_err(|_| ApiError::SqlSessionNotFound)?;
            let created_at = Instant::now();
            let outcome =
                run_with_cancellation_and_deadline(state, deadline, move |state, cancellation| {
                    let out = execute(
                        &plan,
                        &txn,
                        &state.sql.table_store,
                        &state.sql.index_builder,
                        &params,
                        &limits,
                        &state.sql.exec_metrics,
                        &cancellation,
                    );
                    Ok((out, txn))
                })
                .await;
            match outcome {
                Ok((Ok(result), txn)) => {
                    state
                        .sql
                        .sessions
                        .put_back(id, principal.name.clone(), txn, created_at);
                    Ok(SqlResponse {
                        session_id: Some(id),
                        result: query_result_to_json(result, &state.sql.api_metrics),
                    })
                }
                Ok((Err(e), txn)) => {
                    // The statement failed, but the transaction itself
                    // remains open (SQL semantics: one failed statement
                    // does not implicitly roll back the whole
                    // transaction in this architecture) -- put it back.
                    state
                        .sql
                        .sessions
                        .put_back(id, principal.name.clone(), txn, created_at);
                    Err(e.into())
                }
                Err(api_err) => Err(api_err),
            }
        }
    }
}

async fn handle_write(
    state: &Arc<AppState>,
    principal: &Principal,
    session_id: Option<Uuid>,
    plan: Plan,
    params: Vec<Option<RelationalValue>>,
) -> Result<SqlResponse, ApiError> {
    let limits = exec_limits(state);
    let deadline = Duration::from_secs(state.config.sql_statement_deadline_secs);
    let is_ddl = matches!(plan, Plan::Ddl(_));

    match session_id {
        None => {
            let result =
                run_with_cancellation_and_deadline(state, deadline, move |state, cancellation| {
                    execute_write_autocommit(
                        &plan,
                        &state.sql.txm,
                        &state.sql.table_store,
                        &state.sql.catalog,
                        &state.sql.index_builder,
                        &params,
                        &limits,
                        &state.sql.write_metrics,
                        &cancellation,
                    )
                })
                .await?;
            state
                .sql
                .api_metrics
                .record_rows_affected(result.rows_affected);
            Ok(SqlResponse {
                session_id: None,
                result: write_result_to_json(result, is_ddl),
            })
        }
        Some(id) => {
            let mut txn = state
                .sql
                .sessions
                .take(id, &principal.name)
                .map_err(|_| ApiError::SqlSessionNotFound)?;
            let created_at = Instant::now();
            let outcome =
                run_with_cancellation_and_deadline(state, deadline, move |state, cancellation| {
                    let out = execute_write(
                        &plan,
                        &mut txn,
                        &state.sql.table_store,
                        &state.sql.catalog,
                        &state.sql.index_builder,
                        &params,
                        &limits,
                        &state.sql.write_metrics,
                        &cancellation,
                    );
                    Ok((out, txn))
                })
                .await;
            match outcome {
                Ok((Ok(result), txn)) => {
                    state
                        .sql
                        .sessions
                        .put_back(id, principal.name.clone(), txn, created_at);
                    state
                        .sql
                        .api_metrics
                        .record_rows_affected(result.rows_affected);
                    Ok(SqlResponse {
                        session_id: Some(id),
                        result: write_result_to_json(result, is_ddl),
                    })
                }
                Ok((Err(e), txn)) => {
                    state
                        .sql
                        .sessions
                        .put_back(id, principal.name.clone(), txn, created_at);
                    Err(e.into())
                }
                Err(api_err) => Err(api_err),
            }
        }
    }
}

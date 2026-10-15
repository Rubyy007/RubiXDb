//! `POST /v1/sql` — `PHASE_RELATIONAL_SQL_API_ARCHITECTURE.md`. The
//! **one** SQL execution path this product exposes: `authenticate →
//! parse → bind → authorize → plan → execute → serialize`, entirely
//! inside `rubixdb_sql`/`rubixdb::relational` — this handler is a thin
//! translation layer (JSON ↔ typed request/response) and never
//! reimplements any part of that pipeline itself (item 4/105).

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, FromRequest, Request, State};
use axum::http::StatusCode;
use axum::Json;
use rubixdb::relational::{RelationalType, RelationalValue};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use rubixdb_sql::ast::Statement;
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
    /// The schema a prior `USE SCHEMA` selected. The server keeps no per-client schema (a request stays
    /// self-contained): the client sends the name back with each statement, and unqualified names resolve
    /// in it. Absent means the instance's first schema (`public`).
    #[serde(default)]
    pub schema: Option<String>,
}

/// `Json<T>` whose failures use this API's own error body (code, message, hint) instead of axum's
/// plain-text rejection, so a malformed or mistyped request is a clean 4xx a client can read.
pub struct ApiJson<T>(pub T);

#[axum::async_trait]
impl<S, T> FromRequest<S> for ApiJson<T>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(v)) => Ok(ApiJson(v)),
            Err(rejection) => Err(json_rejection(rejection)),
        }
    }
}

fn json_rejection(rejection: JsonRejection) -> ApiError {
    let (status, code, message) = match rejection {
        JsonRejection::MissingJsonContentType(_) => (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "UNSUPPORTED_MEDIA_TYPE",
            "send the request with the header Content-Type: application/json",
        ),
        JsonRejection::JsonSyntaxError(_) => (
            StatusCode::BAD_REQUEST,
            "VALIDATION_ERROR",
            "the request body is not valid JSON",
        ),
        JsonRejection::JsonDataError(_) => (
            StatusCode::BAD_REQUEST,
            "VALIDATION_ERROR",
            "the request body is JSON but not a SQL request: expected {\"sql\": \"...\"} with optional \"params\", \"session_id\" and \"schema\"",
        ),
        other if other.status() == StatusCode::PAYLOAD_TOO_LARGE => (
            StatusCode::PAYLOAD_TOO_LARGE,
            "PAYLOAD_TOO_LARGE",
            "the request body is larger than the server accepts",
        ),
        _ => (
            StatusCode::BAD_REQUEST,
            "VALIDATION_ERROR",
            "the request body could not be read",
        ),
    };
    ApiError::Admin {
        status,
        code,
        message: message.to_string(),
    }
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
        /// A sentence for people: "INSERT completed: 3 rows affected."
        message: String,
    },
    /// DDL — item 49: `CatalogService`'s own DDL methods are already
    /// atomic/durable the instant they return `Ok` (`PHASE_RELATIONAL_
    /// WRITE_EXECUTOR_ARCHITECTURE.md`), so "success" here already means
    /// durable, exactly like `Write`.
    Ddl {
        /// "CREATE SCHEMA", "DROP TABLE", ...
        statement: &'static str,
        /// The object the statement named, fully qualified where that is known (`default.analytics.users`).
        #[serde(skip_serializing_if = "Option::is_none")]
        object: Option<String>,
        /// False when the statement was a no-op (`IF NOT EXISTS` on an existing object, `IF EXISTS` on a
        /// missing one): it succeeded and changed nothing.
        changed: bool,
        message: String,
    },
    /// `USE SCHEMA`: the schema unqualified names resolve in from now on. Nothing is stored server-side;
    /// the client sends `schema` with each following request.
    UseSchema {
        database: String,
        schema: String,
        message: String,
    },
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

/// What a DDL statement is about, captured before it runs (a dropped object cannot be looked up
/// afterwards).
struct DdlSummary {
    statement: &'static str,
    object: Option<String>,
}

impl DdlSummary {
    fn noun(&self) -> &'static str {
        match self.statement {
            "CREATE SCHEMA" => "Schema",
            "CREATE TABLE" | "DROP TABLE" => "Table",
            "CREATE INDEX" | "DROP INDEX" => "Index",
            _ => "Object",
        }
    }

    fn message(&self, changed: bool) -> String {
        let noun = self.noun();
        let name = self
            .object
            .as_deref()
            .map(|o| format!(" {o}"))
            .unwrap_or_default();
        match (self.statement.starts_with("CREATE"), changed) {
            (true, true) => format!("{noun}{name} created."),
            (true, false) => format!("{noun}{name} already exists; nothing changed."),
            (false, true) => format!("{noun}{name} dropped."),
            (false, false) => format!("{noun}{name} does not exist; nothing dropped."),
        }
    }
}

/// `database.schema` for a schema id, when the catalog can say; names only, never ids.
fn qualified_schema(state: &AppState, schema_id: u32) -> Option<String> {
    let catalog = &state.sql.catalog;
    let schema = catalog.get_schema(schema_id).ok().flatten()?;
    let database = catalog.get_database(schema.database_id).ok().flatten()?;
    Some(format!("{}.{}", database.name, schema.name))
}

fn ddl_summary(state: &AppState, bound: &BoundStatement) -> Option<DdlSummary> {
    let catalog = &state.sql.catalog;
    let database_name = |id: u32| catalog.get_database(id).ok().flatten().map(|d| d.name);
    Some(match bound {
        BoundStatement::CreateDatabase(_) => DdlSummary {
            statement: "CREATE DATABASE",
            object: None,
        },
        BoundStatement::CreateSchema(cs) => DdlSummary {
            statement: "CREATE SCHEMA",
            object: Some(match database_name(cs.database_id) {
                Some(db) => format!("{db}.{}", cs.name),
                None => cs.name.clone(),
            }),
        },
        BoundStatement::CreateTable(ct) => DdlSummary {
            statement: "CREATE TABLE",
            object: Some(match qualified_schema(state, ct.schema_id) {
                Some(s) => format!("{s}.{}", ct.name),
                None => ct.name.clone(),
            }),
        },
        BoundStatement::DropTable(dt) => DdlSummary {
            statement: "DROP TABLE",
            object: dt.table_id.and_then(|id| {
                let t = catalog.get_table(id).ok().flatten()?;
                Some(match qualified_schema(state, t.schema_id) {
                    Some(s) => format!("{s}.{}", t.name),
                    None => t.name,
                })
            }),
        },
        BoundStatement::CreateIndex(ci) => DdlSummary {
            statement: "CREATE INDEX",
            object: Some(ci.name.clone()),
        },
        BoundStatement::AlterTable(at) => DdlSummary {
            statement: "ALTER TABLE",
            object: at.table_id.and_then(|id| {
                let t = catalog.get_table(id).ok().flatten()?;
                Some(match qualified_schema(state, t.schema_id) {
                    Some(s) => format!("{s}.{}", t.name),
                    None => t.name,
                })
            }),
        },
        BoundStatement::DropIndex(di) => DdlSummary {
            statement: "DROP INDEX",
            object: di
                .index_id
                .and_then(|id| catalog.get_index(id).ok().flatten())
                .map(|i| i.name),
        },
        _ => return None,
    })
}

fn write_result_to_json(
    result: rubixdb_sql::exec::write::WriteResult,
    ddl: Option<DdlSummary>,
) -> SqlResultBody {
    // `INSERT` / `UPDATE` / `DELETE ... RETURNING`: the rows the statement changed
    // are the result, shaped exactly like a `SELECT`'s (`row_count` is the number of
    // rows changed). The statement's write is already durable when this is built --
    // the returned rows come out of `execute_write_autocommit` only after its
    // commit, and out of a session statement only after it was buffered.
    if let Some(returned) = result.returning {
        let columns: Vec<ColumnMeta> = returned
            .schema
            .fields
            .iter()
            .map(|f| ColumnMeta {
                name: f.name.clone(),
                ty: f.ty.map(relational_type_name),
                nullable: f.nullable,
            })
            .collect();
        let row_count = returned.rows.len();
        let rows: Vec<Vec<SqlValueJson>> = returned
            .rows
            .iter()
            .map(|row| row.iter().map(value_to_json).collect())
            .collect();
        return SqlResultBody::Rows {
            columns,
            rows,
            row_count,
        };
    }
    let changed = result.rows_affected > 0;
    match ddl {
        Some(summary) => SqlResultBody::Ddl {
            statement: summary.statement,
            message: summary.message(changed),
            object: summary.object,
            changed,
        },
        None => {
            let statement = write_statement_name(result.kind);
            let n = result.rows_affected;
            SqlResultBody::Write {
                statement,
                rows_affected: n,
                message: format!(
                    "{statement} completed: {n} row{} affected.",
                    if n == 1 { "" } else { "s" }
                ),
            }
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

/// Statement class label (closed set) for the query registry.
fn statement_class(stmt: &Statement) -> &'static str {
    use crate::observability::queries::class;
    match stmt {
        Statement::Select(_) => class::SELECT,
        Statement::Insert(_) => class::INSERT,
        Statement::Update(_) => class::UPDATE,
        Statement::Delete(_) => class::DELETE,
        Statement::CreateDatabase(_)
        | Statement::CreateSchema(_)
        | Statement::CreateTable(_)
        | Statement::DropTable(_)
        | Statement::CreateIndex(_)
        | Statement::AlterTable(_)
        | Statement::DropIndex(_) => class::DDL,
        Statement::Begin | Statement::Commit | Statement::Rollback => class::TRANSACTION,
        Statement::UseSchema(_) => class::OTHER,
        Statement::Explain(_) => class::EXPLAIN,
    }
}

pub async fn sql(
    State(state): State<Arc<AppState>>,
    Extension(principal): Extension<Principal>,
    request_id: Option<Extension<crate::observability::RequestId>>,
    ApiJson(req): ApiJson<SqlRequest>,
) -> Result<Json<SqlResponse>, ApiError> {
    use crate::observability::events::{kind, severity, Event};
    state.sql.api_metrics.record_request();
    let query = state
        .obs
        .queries
        .start()
        .log_disconnect_to(&state.obs.events);
    let request_id = request_id.map(|Extension(r)| r.0);
    let session_id = req.session_id;
    let started = Instant::now();
    let mut ddl: Option<DdlAudit> = None;
    let result = handle(&state, &principal, req, &mut ddl, &query).await;
    if let Some(d) = ddl {
        // Phase 7 SG-3b: catalog DDL create/drop. Statement kind and the
        // target identifier come from the parsed AST -- the SQL text, the
        // parameters and any row data are never passed here.
        let outcome = match &result {
            Ok(_) => "ok",
            Err(ApiError::Sql(SqlError::AuthorizationDenied { .. })) => "denied",
            Err(_) => "failed",
        };
        crate::security_log::emit(&crate::security_log::SecurityEvent {
            code: d.code,
            principal: Some(&principal.name),
            method: Some("POST"),
            route: Some("/v1/sql"),
            outcome,
            object_kind: Some(d.kind),
            object: Some(&d.object),
            ..Default::default()
        });
    }
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    match &result {
        Ok(resp) => {
            state.sql.api_metrics.record_success();
            let (returned, affected) = match &resp.result {
                SqlResultBody::Rows { row_count, .. } => (Some(*row_count as u64), None),
                SqlResultBody::Write { rows_affected, .. } => (None, Some(*rows_affected)),
                _ => (None, None),
            };
            query.succeeded(returned, affected);
        }
        Err(ApiError::Sql(SqlError::Cancelled)) => {
            state.sql.api_metrics.record_cancellation();
            query.cancelled();
            state.obs.events.push_operational(
                Event::new(
                    kind::QUERY_CANCELLED,
                    severity::WARNING,
                    "query",
                    "cancelled",
                )
                .request(request_id)
                .session(session_id)
                .duration(elapsed_ms)
                .error_class("CANCELLED"),
            );
        }
        Err(ApiError::SqlDeadlineExceeded) => {
            state.sql.api_metrics.record_deadline_exceeded();
            query.timed_out();
            state.obs.events.push_operational(
                Event::new(kind::QUERY_TIMEOUT, severity::WARNING, "query", "timeout")
                    .request(request_id)
                    .session(session_id)
                    .duration(elapsed_ms)
                    .error_class("SQL_TIMEOUT"),
            );
        }
        Err(e) => {
            state.sql.api_metrics.record_error();
            let code = e.code();
            query.failed(code);
            state.obs.events.push_operational(
                Event::new(kind::QUERY_FAILED, severity::ERROR, "query", "failed")
                    .request(request_id)
                    .session(session_id)
                    .duration(elapsed_ms)
                    .error_class(code),
            );
        }
    }
    result.map(Json)
}

/// What a DDL statement is, for the security event log.
struct DdlAudit {
    code: &'static str,
    kind: &'static str,
    object: String,
}

/// DDL create/drop statements only; everything else is `None`. The object is
/// the statement's most specific identifier (bounded and escaped by the log).
fn ddl_audit(stmt: &Statement) -> Option<DdlAudit> {
    use crate::security_log::code::{CATALOG_ALTER, CATALOG_CREATE, CATALOG_DROP};
    let (code, kind, object) = match stmt {
        Statement::CreateDatabase(s) => (CATALOG_CREATE, "database", s.name.value.clone()),
        Statement::CreateSchema(s) => (CATALOG_CREATE, "schema", s.name.last().value.clone()),
        Statement::CreateTable(s) => (CATALOG_CREATE, "table", s.name.last().value.clone()),
        Statement::DropTable(s) => (CATALOG_DROP, "table", s.name.last().value.clone()),
        Statement::CreateIndex(s) => (
            CATALOG_CREATE,
            "index",
            s.name.as_ref().map(|n| n.value.clone()).unwrap_or_default(),
        ),
        Statement::DropIndex(s) => (CATALOG_DROP, "index", s.name.value.clone()),
        Statement::AlterTable(s) => (CATALOG_ALTER, "table", s.name.last().value.clone()),
        _ => return None,
    };
    Some(DdlAudit { code, kind, object })
}

async fn handle(
    state: &Arc<AppState>,
    principal: &Principal,
    req: SqlRequest,
    ddl: &mut Option<DdlAudit>,
    query: &crate::observability::queries::QueryGuard<'_>,
) -> Result<SqlResponse, ApiError> {
    let sql_limits = SqlLimits::default();
    let stmt = match parse_statement(&req.sql, &sql_limits) {
        Ok(stmt) => stmt,
        Err(e) => {
            // A mistyped keyword gets a "did you mean"; any other error keeps its code's generic hint.
            let hint = match &e {
                SqlError::Parse { .. } | SqlError::Unsupported { .. } => {
                    crate::sql_hints::statement_hint(&req.sql)
                }
                _ => None,
            };
            return Err(match hint {
                Some(h) => ApiError::Hinted(Box::new(ApiError::from(e)), h),
                None => e.into(),
            });
        }
    };
    query.set_class(statement_class(&stmt));
    *ddl = ddl_audit(&stmt);

    let mut bind_context = state.sql.bind_context()?;
    // `USE SCHEMA` is how a client leaves a schema that no longer exists, so it never resolves the old one.
    let selected = req
        .schema
        .as_deref()
        .filter(|n| !n.is_empty() && !matches!(stmt, Statement::UseSchema(_)));
    if let Some(name) = selected {
        if name.len() > sql_limits.max_identifier_len {
            return Err(ApiError::Validation("schema name is too long".to_string()));
        }
        bind_context.default_schema_id = rubixdb_sql::bind::scope::resolve_schema_in_database(
            &state.sql.catalog,
            bind_context.database_id,
            name,
        )
        .map_err(|e| {
            ApiError::Hinted(
                Box::new(ApiError::from(e)),
                "The selected schema does not exist (it may have been removed). Run USE SCHEMA <schema_name> to pick one that does."
                    .to_string(),
            )
        })?;
    }

    if let Statement::UseSchema(u) = &stmt {
        let (_, schema) = rubixdb_sql::bind::scope::resolve_use_schema(
            &state.sql.catalog,
            &bind_context,
            &u.name,
        )?;
        let database = state
            .sql
            .catalog
            .get_database(bind_context.database_id)
            .ok()
            .flatten()
            .map(|d| d.name)
            .unwrap_or_else(|| "default".to_string());
        return Ok(SqlResponse {
            session_id: req.session_id,
            result: SqlResultBody::UseSchema {
                message: format!("Now using schema {database}.{schema}."),
                database,
                schema,
            },
        });
    }

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

    let summary = ddl_summary(state, &bound);
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
        | BoundStatement::AlterTable(_)
        | BoundStatement::DropIndex(_) => {
            handle_write(state, principal, req.session_id, plan, params, summary).await
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
        .map_err(|_| {
            state
                .obs
                .counters
                .sessions_rejected
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            state.obs.events.push_operational(
                crate::observability::events::Event::new(
                    crate::observability::events::kind::SESSION_REJECTED,
                    crate::observability::events::severity::WARNING,
                    "session",
                    "refused",
                )
                .error_class("TOO_MANY_SESSIONS"),
            );
            ApiError::SqlTooManySessions
        })?;
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
    state.sql.sessions.finish(id);
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
    state.sql.sessions.finish(id);
    txn.rollback().map_err(SqlError::from)?;
    Ok(SqlResponse {
        session_id: None,
        result: SqlResultBody::Rollback,
    })
}

/// Owns the "executing" state of a session whose transaction was taken out of the registry for the
/// duration of one statement. The statement future can be dropped at any `.await` (the client
/// disconnected): the transaction then goes with the blocking task (an implicit rollback, the
/// existing `Transaction` drop path) and nothing would call `put_back` or `finish`, leaving a
/// stale entry that the reaper never sees. Dropping an unresolved lease closes the session's
/// observation entry and leaves a `session.closed` event (the existing event shape).
struct SessionLease {
    state: Arc<AppState>,
    id: Uuid,
    resolved: bool,
}

impl SessionLease {
    fn new(state: &Arc<AppState>, id: Uuid) -> Self {
        SessionLease {
            state: Arc::clone(state),
            id,
            resolved: false,
        }
    }

    /// The statement finished and the session was put back or ended through the normal path.
    fn resolve(&mut self) {
        self.resolved = true;
    }
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        if self.resolved {
            return;
        }
        use crate::observability::events::{kind, severity, Event};
        self.state.sql.sessions.finish(self.id);
        self.state.obs.events.push_operational(
            Event::new(
                kind::SESSION_CLOSED,
                severity::WARNING,
                "session",
                "cancelled",
            )
            .error_class("CLIENT_DISCONNECTED")
            .session(Some(self.id)),
        );
    }
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
            let mut lease = SessionLease::new(state, id);
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
                    lease.resolve();
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
                    lease.resolve();
                    Err(e.into())
                }
                Err(api_err) => {
                    // The transaction was dropped with the failed statement (deadline /
                    // cancellation): the session is gone.
                    state.sql.sessions.finish(id);
                    lease.resolve();
                    Err(api_err)
                }
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
    ddl: Option<DdlSummary>,
) -> Result<SqlResponse, ApiError> {
    let limits = exec_limits(state);
    let deadline = Duration::from_secs(state.config.sql_statement_deadline_secs);

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
                result: write_result_to_json(result, ddl),
            })
        }
        Some(id) => {
            let mut txn = state
                .sql
                .sessions
                .take(id, &principal.name)
                .map_err(|_| ApiError::SqlSessionNotFound)?;
            let created_at = Instant::now();
            let mut lease = SessionLease::new(state, id);
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
                    lease.resolve();
                    state
                        .sql
                        .api_metrics
                        .record_rows_affected(result.rows_affected);
                    Ok(SqlResponse {
                        session_id: Some(id),
                        result: write_result_to_json(result, ddl),
                    })
                }
                Ok((Err(e), txn)) => {
                    state
                        .sql
                        .sessions
                        .put_back(id, principal.name.clone(), txn, created_at);
                    lease.resolve();
                    Err(e.into())
                }
                Err(api_err) => {
                    // The transaction was dropped with the failed statement (deadline /
                    // cancellation): the session is gone.
                    state.sql.sessions.finish(id);
                    lease.resolve();
                    Err(api_err)
                }
            }
        }
    }
}

#[cfg(test)]
mod serialization_benchmark {
    use super::*;
    use rubixdb::relational::RelationalValue;
    use rubixdb_sql::exec::{ResultField, ResultSchema};
    use std::time::Instant;

    /// Increment 16: the HTTP-layer serialization stage of an indexed
    /// `SELECT *` result (`query_result_to_json` + `serde_json` encode),
    /// measured on a synthetic result with the benchmark table's shape
    /// (2 INTEGER + 6 TEXT columns). `#[ignore]`d measurement, not a
    /// correctness test:
    ///   cargo test --release -p rubixdb-api --lib serialization_benchmark -- --ignored --nocapture
    #[test]
    #[ignore]
    fn serialization_cost_by_result_size() {
        let fields: Vec<ResultField> = (0..8)
            .map(|i| ResultField {
                name: format!("c{i}"),
                ty: Some(if i < 2 {
                    rubixdb::relational::RelationalType::Integer
                } else {
                    rubixdb::relational::RelationalType::Text
                }),
                nullable: true,
            })
            .collect();
        for k in [1usize, 10, 100, 1_000, 10_000] {
            let make = || {
                let rows: Vec<Vec<Option<RelationalValue>>> = (0..k)
                    .map(|i| {
                        let mut r = vec![
                            Some(RelationalValue::Integer(i as i32)),
                            Some(RelationalValue::Integer(i as i32)),
                        ];
                        for c in 0..6 {
                            r.push(Some(RelationalValue::Text(format!("g{}", (i + c) % 1000))));
                        }
                        r
                    })
                    .collect();
                QueryResult {
                    schema: ResultSchema {
                        fields: fields.clone(),
                    },
                    rows,
                }
            };
            let metrics = crate::sql_metrics::SqlApiMetrics::default();
            let mut samples = Vec::new();
            let mut bytes = 0;
            for _ in 0..15 {
                let res = make();
                let s = Instant::now();
                let body = query_result_to_json(res, &metrics);
                let out = serde_json::to_vec(&body).unwrap();
                samples.push(s.elapsed());
                bytes = out.len();
            }
            samples.sort();
            println!(
                "serialize K={k:<6} p50={:>8.3} ms  max={:>8.3} ms  json={bytes} B",
                samples[samples.len() / 2].as_secs_f64() * 1000.0,
                samples.last().unwrap().as_secs_f64() * 1000.0
            );
        }
    }
}

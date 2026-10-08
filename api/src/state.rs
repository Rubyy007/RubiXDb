//! Shared application state — one `Arc<AppState>` handed to every
//! route via axum's `State` extractor.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use rubixdb::catalog::CatalogService;
use rubixdb::lsm::{LsmConfig, LsmEngine};
use rubixdb::relational::index::IndexBuilder;
use rubixdb::relational::table_store::TableStore;
use rubixdb::relational::TransactionManager;
use uuid::Uuid;

use crate::auth::AuthProvider;
use crate::config::Config;
use crate::error::ApiError;
use crate::metrics::ServiceMetrics;
use crate::rate_limit::RateLimiter;
use crate::sql_metrics::SqlApiMetrics;
use crate::sql_session::{SqlSessionLimits, SqlSessionRegistry};

/// A `Snapshot` the service is holding open on a client's behalf —
/// `PHASE_API_ARCHITECTURE.md` §2.1. Dropping the `Snapshot` value
/// (when the entry is removed from `AppState::snapshots`) releases it
/// from the engine's own `SnapshotRegistry`.
pub struct HeldSnapshot {
    pub snapshot: rubixdb::lsm::Snapshot,
    pub created_at: SystemTime,
}

/// `PHASE_RELATIONAL_SQL_API_ARCHITECTURE.md` §2: RubiXDB has exactly
/// one bootstrapped database/schema today (`CatalogService::bootstrap`
/// creates `"default"`/`"public"` once, idempotently, and `CREATE
/// DATABASE` has no execution primitive — Increment 10's own already-
/// documented boundary, unchanged here). Every SQL statement binds
/// against this one fixed `(database_id, default_schema_id)` pair.
///
/// **Bootstrap is deliberately lazy** (`bind_context()`, first call
/// only, cached thereafter) rather than eager in `AppState::new` —
/// item 115/116's own backward-compatibility requirement, found by a
/// real regression: `CatalogService` persists `system.*` rows into the
/// *same flat keyspace* `/v1/kv`/`/v1/range` already scan (there is no
/// separate catalog storage area — `PHASE_API_ARCHITECTURE.md` §0's own
/// "single flat binary-key/binary-value keyspace" describes the one
/// physical space both product surfaces share). Eagerly bootstrapping
/// on every process start injected catalog rows into every deployment's
/// keyspace whether or not SQL was ever used, breaking `api_
/// integration.rs`'s own pre-existing, certified `/v1/range` tests
/// (which assume a pristine keyspace) and silently falsifying `/v1/
/// metadata`'s own "no tables, no schema, no SQL" claim. Deferring
/// bootstrap until the SQL/catalog surface is actually first touched
/// keeps every pure-KV deployment's keyspace exactly as before.
pub struct SqlContext {
    pub catalog: Arc<CatalogService>,
    pub table_store: Arc<TableStore>,
    pub index_builder: Arc<IndexBuilder>,
    pub txm: TransactionManager,
    /// `(database_id, default_schema_id)`, resolved and cached on first
    /// use by `bind_context()` — never read directly.
    bind_ids: Mutex<Option<(u32, u32)>>,
    pub sessions: Arc<SqlSessionRegistry>,
    pub api_metrics: SqlApiMetrics,
    pub sql_metrics: rubixdb_sql::SqlMetrics,
    pub planner_metrics: rubixdb_sql::plan::PlannerMetrics,
    pub exec_metrics: rubixdb_sql::exec::ExecMetrics,
    pub write_metrics: rubixdb_sql::exec::write::WriteMetrics,
}

impl SqlContext {
    /// Bootstraps the catalog (idempotent, a no-op after the first
    /// successful call anywhere in the process's lifetime, including
    /// across restarts against the same data directory) and resolves
    /// the one `(database_id, default_schema_id)` pair every SQL
    /// statement binds against, caching it after the first call so
    /// every later request is a single uncontended lock check, never a
    /// repeated catalog round-trip.
    pub fn bind_context(&self) -> Result<rubixdb_sql::BindContext, ApiError> {
        let mut guard = self.bind_ids.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((database_id, default_schema_id)) = *guard {
            return Ok(rubixdb_sql::BindContext {
                database_id,
                default_schema_id,
            });
        }
        self.catalog
            .bootstrap()
            .map_err(rubixdb_sql::SqlError::from)?;
        let database_id = self
            .catalog
            .list_databases()
            .map_err(rubixdb_sql::SqlError::from)?[0]
            .database_id;
        let default_schema_id = self
            .catalog
            .list_schemas(database_id)
            .map_err(rubixdb_sql::SqlError::from)?[0]
            .schema_id;
        *guard = Some((database_id, default_schema_id));
        Ok(rubixdb_sql::BindContext {
            database_id,
            default_schema_id,
        })
    }
}

/// One-at-a-time guards and last-result memory for administrative
/// operations (`/v1/admin/*`). Bounded: a fixed number of flags, counters and
/// two small result records — nothing grows with request volume.
#[derive(Default)]
pub struct AdminOps {
    pub backup_running: std::sync::atomic::AtomicBool,
    pub check_running: std::sync::atomic::AtomicBool,
    pub maintenance_running: std::sync::atomic::AtomicBool,
    pub backups_ok: std::sync::atomic::AtomicU64,
    pub backups_failed: std::sync::atomic::AtomicU64,
    pub checks_run: std::sync::atomic::AtomicU64,
    pub last_backup: Mutex<Option<serde_json::Value>>,
    pub last_check: Mutex<Option<serde_json::Value>>,
}

pub struct AppState {
    pub engine: Arc<LsmEngine>,
    pub config: Config,
    /// The exact `LsmConfig` the engine was opened with -- echoed back
    /// by `/v1/metadata` verbatim, never re-derived from `LsmConfig::
    /// default()` (which could silently drift from what `main.rs`
    /// actually passed to `LsmEngine::open`).
    pub lsm_config: LsmConfig,
    pub snapshots: Mutex<HashMap<Uuid, HeldSnapshot>>,
    pub auth: AuthProvider,
    pub rate_limiter: RateLimiter,
    pub metrics: ServiceMetrics,
    pub sql: SqlContext,
    pub started_at: SystemTime,
    pub admin: AdminOps,
    /// Progress of the startup recovery of interrupted index operations
    /// (`crate::recovery`); reported by `GET /readyz`.
    pub index_recovery: crate::recovery::IndexRecovery,
    /// State and findings of the background SSTable data-block verification (ADR-SST-01, F-08); `disabled` until
    /// the host starts the pass. Reported by `GET /readyz` and `GET /v1/status`; never part of `ready`.
    pub sstable_integrity: rubixdb::ops::sstable_integrity::SstableIntegrity,
    /// Background sampler output, bounded time series, query / event registries and the
    /// counters added by the observability layer.
    pub obs: crate::observability::Observability,
}

impl AppState {
    /// `engine` is `Arc`-owned (rather than the bare-`LsmEngine` shape
    /// this struct held before the SQL layer existed) because
    /// `CatalogService`/`TableStore`/`IndexBuilder`/`TransactionManager`
    /// each need their own `Arc<LsmEngine>` clone — the identical
    /// pattern `rubixdb-sql`'s own test fixtures
    /// (`sql/src/test_support.rs::Fixture`) already establish, reused
    /// verbatim rather than inventing a second engine-sharing
    /// convention. Every existing `state.engine.method(...)` call site
    /// elsewhere in this crate is unaffected: `Arc<LsmEngine>` derefs
    /// to `&LsmEngine` transparently.
    pub fn new(engine: Arc<LsmEngine>, lsm_config: LsmConfig, config: Config) -> Self {
        let auth = AuthProvider::from_config(&config.api_keys);
        let rate_limiter = RateLimiter::new(config.rate_limit_rps, config.rate_limit_burst);

        let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
        let table_store = Arc::new(TableStore::new(Arc::clone(&engine), Arc::clone(&catalog)));
        let index_builder = Arc::new(IndexBuilder::new(
            Arc::clone(&engine),
            Arc::clone(&catalog),
            Arc::clone(&table_store),
        ));
        let txm = TransactionManager::new(Arc::clone(&engine), Arc::clone(&table_store));

        let session_limits = SqlSessionLimits {
            max_sessions_per_principal: config.sql_max_sessions_per_principal,
            idle_timeout: Duration::from_secs(config.sql_session_idle_timeout_secs),
            max_lifetime: Duration::from_secs(config.sql_session_max_lifetime_secs),
        };

        let sql = SqlContext {
            catalog,
            table_store,
            index_builder,
            txm,
            bind_ids: Mutex::new(None),
            sessions: Arc::new(SqlSessionRegistry::new(session_limits)),
            api_metrics: SqlApiMetrics::default(),
            sql_metrics: rubixdb_sql::SqlMetrics::default(),
            planner_metrics: rubixdb_sql::plan::PlannerMetrics::default(),
            exec_metrics: rubixdb_sql::exec::ExecMetrics::default(),
            write_metrics: rubixdb_sql::exec::write::WriteMetrics::default(),
        };

        AppState {
            engine,
            config,
            lsm_config,
            snapshots: Mutex::new(HashMap::new()),
            auth,
            rate_limiter,
            metrics: ServiceMetrics::default(),
            sql,
            started_at: SystemTime::now(),
            admin: AdminOps::default(),
            index_recovery: crate::recovery::IndexRecovery::default(),
            sstable_integrity: rubixdb::ops::sstable_integrity::SstableIntegrity::default(),
            obs: crate::observability::Observability::default(),
        }
    }
}

pub fn uptime(state: &AppState) -> Duration {
    state.started_at.elapsed().unwrap_or_default()
}

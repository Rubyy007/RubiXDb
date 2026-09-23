//! Shared test fixtures — real `LsmEngine` + `CatalogService`, mirroring
//! the core crate's own `relational::tests`/`relational::index_tests`
//! fixture conventions one crate over. `#[cfg(test)]`-only.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rubixdb::catalog::service::ColumnDef;
use rubixdb::catalog::CatalogService;
use rubixdb::execution::batch_coordinator::BatchCoordinatorConfig;
use rubixdb::lsm::{LsmConfig, LsmEngine};
use rubixdb::relational::value::{
    TYPE_TAG_BIGINT, TYPE_TAG_BOOLEAN, TYPE_TAG_INTEGER, TYPE_TAG_TEXT,
};
use rubixdb::wal::{SyncMode, WalConfig};

use crate::bind::BindContext;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("rubixdb_sql_test_{tag}_{nanos}_{n}"));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn test_wal_config() -> WalConfig {
    WalConfig {
        sync_mode: SyncMode::GroupCommit {
            max_wait: Duration::from_millis(5),
            max_batch_bytes: 256 * 1024,
        },
        ..WalConfig::default()
    }
}

fn small_pool_config() -> BatchCoordinatorConfig {
    BatchCoordinatorConfig {
        queue_capacity: 256,
        max_queued_bytes: 8 * 1024 * 1024,
        submission_timeout: Duration::from_secs(5),
        shutdown_drain_bound: Duration::from_secs(10),
        await_retry_budget: Duration::from_secs(5),
        max_drain_per_batch: 4096,
    }
}

fn open(dir: &Path) -> Arc<LsmEngine> {
    Arc::new(
        LsmEngine::open(
            dir,
            test_wal_config(),
            small_pool_config(),
            LsmConfig::default(),
        )
        .unwrap(),
    )
}

pub struct Fixture {
    pub dir: PathBuf,
    pub engine: Arc<LsmEngine>,
    pub catalog: CatalogService,
    pub ctx: BindContext,
}

impl Fixture {
    /// Bootstraps a fresh catalog and creates:
    /// `t(id INTEGER PRIMARY KEY, name TEXT, active BOOLEAN)`
    /// `orders(id BIGINT PRIMARY KEY, customer TEXT, amount INTEGER,
    /// t_id INTEGER)` — `t_id` is the type-compatible "foreign key" to
    /// `t.id` (`orders.id` is deliberately a *different* type, `BIGINT`,
    /// so tests can exercise cross-type comparisons; it is never itself
    /// a valid join key against `t.id`, by design, matching D21's "no
    /// implicit coercion" rule) — both tables live in the default
    /// (`public`) schema.
    pub fn new(tag: &str) -> Self {
        let dir = temp_dir(tag);
        let engine = open(&dir);
        let catalog = CatalogService::new(Arc::clone(&engine));
        catalog.bootstrap().unwrap();
        let database_id = catalog.list_databases().unwrap()[0].database_id;
        let default_schema_id = catalog.list_schemas(database_id).unwrap()[0].schema_id;

        catalog
            .create_table(
                default_schema_id,
                "t",
                &[
                    ColumnDef {
                        name: "id".to_string(),
                        data_type: TYPE_TAG_INTEGER,
                        nullable: false,
                        default_value: None,
                        type_params: None,
                    },
                    ColumnDef {
                        name: "name".to_string(),
                        data_type: TYPE_TAG_TEXT,
                        nullable: true,
                        default_value: None,
                        type_params: None,
                    },
                    ColumnDef {
                        name: "active".to_string(),
                        data_type: TYPE_TAG_BOOLEAN,
                        nullable: true,
                        default_value: None,
                        type_params: None,
                    },
                ],
                &[0],
            )
            .unwrap();

        catalog
            .create_table(
                default_schema_id,
                "orders",
                &[
                    ColumnDef {
                        name: "id".to_string(),
                        data_type: TYPE_TAG_BIGINT,
                        nullable: false,
                        default_value: None,
                        type_params: None,
                    },
                    ColumnDef {
                        name: "customer".to_string(),
                        data_type: TYPE_TAG_TEXT,
                        nullable: false,
                        default_value: None,
                        type_params: None,
                    },
                    ColumnDef {
                        name: "amount".to_string(),
                        data_type: TYPE_TAG_INTEGER,
                        nullable: true,
                        default_value: None,
                        type_params: None,
                    },
                    ColumnDef {
                        name: "t_id".to_string(),
                        data_type: TYPE_TAG_INTEGER,
                        nullable: true,
                        default_value: None,
                        type_params: None,
                    },
                ],
                &[0],
            )
            .unwrap();

        Fixture {
            dir,
            engine,
            catalog,
            ctx: BindContext {
                database_id,
                default_schema_id,
            },
        }
    }

    pub fn cleanup(self) {
        self.engine.shutdown();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

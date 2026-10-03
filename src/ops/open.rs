//! Opens an engine with the same WAL / pool configuration the product
//! (`rubixdb-api` `main.rs`, `rubixdb` CLI host) uses, so restore and the
//! offline check exercise the production durability settings, not a test
//! configuration.

use std::path::Path;
use std::time::Duration;

use crate::execution::batch_coordinator::BatchCoordinatorConfig;
use crate::lsm::{LsmConfig, LsmEngine};
use crate::ops::OpsError;
use crate::wal::{SyncMode, WalConfig};

pub fn product_wal_config() -> WalConfig {
    WalConfig {
        sync_mode: SyncMode::GroupCommit {
            max_wait: Duration::from_millis(5),
            max_batch_bytes: 256 * 1024,
        },
        ..WalConfig::default()
    }
}

pub fn product_pool_config() -> BatchCoordinatorConfig {
    BatchCoordinatorConfig {
        queue_capacity: 4096,
        max_queued_bytes: 64 * 1024 * 1024,
        submission_timeout: Duration::from_secs(10),
        shutdown_drain_bound: Duration::from_secs(60),
        await_retry_budget: Duration::from_secs(10),
        max_drain_per_batch: 65536,
    }
}

/// Opens (creating if absent) the engine at `dir` with production
/// durability settings. `compaction_auto_trigger` is off: restore and check
/// are bounded, one-shot operations that must not start background work.
pub fn open_engine_for_ops(dir: &Path) -> Result<LsmEngine, OpsError> {
    // Refuse a directory written by an incompatible build before anything
    // (including recovery) can touch it.
    let format_state = crate::ops::format::startup_guard(dir)?;
    std::fs::create_dir_all(dir)?;
    let engine = LsmEngine::open(
        dir,
        product_wal_config(),
        product_pool_config(),
        LsmConfig {
            compaction_auto_trigger: false,
            ..LsmConfig::default()
        },
    )?;
    crate::ops::format::stamp_if_fresh(dir, format_state)?;
    Ok(engine)
}

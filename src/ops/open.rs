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
    crate::persist::create_dir_all_durable(dir)?;
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

/// An engine opened for `rubixdb check`'s logical pass. When recovery would modify the directory in place (a torn WAL
/// tail: it is truncated), the engine is opened on a private temporary copy so that the inspected directory stays
/// byte-for-byte as it was (F-07 check path); otherwise it is opened in place, exactly as before. The copy is removed when
/// this value is dropped (the engine, declared first, is dropped before it).
pub struct InspectionEngine {
    pub engine: LsmEngine,
    copy: Option<TempCopy>,
}

impl InspectionEngine {
    /// `true` when the engine runs on a private copy and the inspected directory was not opened.
    pub fn on_copy(&self) -> bool {
        self.copy.is_some()
    }
}

struct TempCopy(std::path::PathBuf);

impl Drop for TempCopy {
    fn drop(&mut self) {
        let _ = crate::persist::remove_dir_all(&self.0);
    }
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    crate::persist::create_dir_all(to)?;
    for entry in crate::persist::read_dir(from)? {
        let entry = entry?;
        let (src, dst) = (entry.path(), to.join(entry.file_name()));
        if entry.file_type()?.is_dir() {
            copy_tree(&src, &dst)?;
        } else {
            crate::persist::copy(&src, &dst)?;
        }
    }
    Ok(())
}

/// Like [`open_engine_for_ops`] for the offline logical check, but never modifies `dir` when its WAL has a torn tail.
/// Fails closed: if the private copy cannot be made (for example no space) the inspected directory is left untouched
/// and an error is returned.
pub fn open_engine_for_inspection(dir: &Path) -> Result<InspectionEngine, OpsError> {
    let torn = matches!(crate::ops::wal_tail::plan_tail(dir), Ok(p) if p.tail.is_some());
    if !torn {
        return Ok(InspectionEngine {
            engine: open_engine_for_ops(dir)?,
            copy: None,
        });
    }
    let copy_dir =
        std::env::temp_dir().join(format!("rubixdb_check_{}", crate::ops::unique_id_hex()));
    let guard = TempCopy(copy_dir.clone());
    copy_tree(dir, &copy_dir).map_err(|e| {
        OpsError::new(
            crate::ops::codes::IO,
            format!(
                "the WAL has a torn tail that opening would truncate in place, and the private copy that avoids that could not be made ({e}); {} was not modified",
                dir.display()
            ),
        )
    })?;
    Ok(InspectionEngine {
        engine: open_engine_for_ops(&copy_dir)?,
        copy: Some(guard),
    })
}

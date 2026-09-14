//! Shared helpers for the Phase 1 (Group Commit) integration test suite
//! under `tests/group_commit/`. Included as `mod support;` from
//! `tests/group_commit.rs` — see that file's doc comment for why this
//! directory needs a single top-level integration-test binary at all
//! (Cargo only auto-discovers files directly under `tests/`, not files in
//! subdirectories).
#![allow(dead_code)]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rubixdb::wal::{GroupCommitter, SyncMode, WalConfig};
use rubixdb::EngineError;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A fresh, unique temp directory for one test's WAL — never shared
/// between tests (avoids any cross-test directory-lock contention, since
/// `FileWal::open_for_recovery` takes an exclusive OS lock per directory).
pub fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("rubixdb_group_commit_it_{tag}_{nanos}_{n}"));
    fs::create_dir_all(&path).expect("creating a fresh temp dir must succeed");
    path
}

/// The brief's own literal defaults: `min(200 µs, EMA / 10)` leader window,
/// 256 KiB batch-payload threshold.
pub fn group_commit_config() -> WalConfig {
    WalConfig {
        sync_mode: SyncMode::GroupCommit {
            max_wait: Duration::from_micros(200),
            max_batch_bytes: 256 * 1024,
        },
        ..WalConfig::default()
    }
}

/// `EngineError::Timeout` is a *recoverable* outcome of `await_durable` —
/// it means "this bounded wait expired," not "this record failed."
/// Durability itself is never in doubt: a leader batch that has not
/// finished yet is not a leader batch that has failed. Under real OS
/// thread-scheduling contention (many threads, a shared/virtualized
/// machine), a single `condvar.wait_timeout` call can legitimately expire
/// without a `notify_all()` landing in that exact window even while the
/// system makes steady forward progress — the correct response, exactly
/// like a real caller's, is to call `await_durable` again for the *same*
/// `seq` (never to re-`append`, which would assign a brand new one). See
/// `PROCESS.md`'s M0 milestone entry for the concurrency bug this pattern
/// was extracted from fixing.
pub fn await_durable_retrying_on_timeout(committer: &GroupCommitter, seq: u64) {
    loop {
        match committer.await_durable(seq) {
            Ok(()) => return,
            Err(EngineError::Timeout { .. }) => continue,
            Err(e) => panic!("unexpected error awaiting durability for seq={seq}: {e}"),
        }
    }
}

/// `append` then `await_durable`, retrying only the wait half on
/// `Timeout` — the shape every throughput test in this suite drives many
/// threads through.
pub fn append_durable_retrying(committer: &GroupCommitter, op: rubixdb::wal::WalOp<'_>) -> u64 {
    let position = committer
        .append(op)
        .expect("append must not fail: no fault is injected in this path");
    await_durable_retrying_on_timeout(committer, position.seq);
    position.seq
}

pub fn median_nanos(samples: &[u128]) -> u128 {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    sorted[sorted.len() / 2]
}

pub fn percentile_nanos(samples: &[u128], pct: f64) -> u128 {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let idx = ((sorted.len() as f64) * pct) as usize;
    sorted[idx.min(sorted.len() - 1)]
}

pub fn ms(nanos: u128) -> f64 {
    (nanos as f64) / 1_000_000.0
}

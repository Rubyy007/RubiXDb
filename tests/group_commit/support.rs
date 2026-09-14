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

use rubixdb::wal::{GroupCommitter, SyncMode, Wal, WalConfig};
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

/// `max_wait = 5ms` (not the brief's original literal `200µs`) combined
/// with `WINDOW_EMA_DIVISOR = 1` (`src/wal/group_commit.rs`) — the
/// post-window-size-sweep defaults. See `PHASE1_TEST_RESULTS.md`'s
/// window-size sweep section and `PHASE1_ADR.md` ADR-12 for the data this
/// is derived from: the original `200µs`/`/10` formula left most of this
/// machine's available batching headroom on the table.
pub fn group_commit_config() -> WalConfig {
    WalConfig {
        sync_mode: SyncMode::GroupCommit {
            max_wait: Duration::from_millis(5),
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
///
/// Bounded at `MAX_RETRIES`, not an unconditional `loop`: an unbounded
/// retry would turn a genuine regression into a hung test process rather
/// than a clean, fast failure (see `PROCESS.md`'s M0.1 milestone entry).
pub fn await_durable_retrying_on_timeout(committer: &GroupCommitter, seq: u64) {
    const MAX_RETRIES: u32 = 1_000;
    let mut last_timeout_detail = String::new();
    for _ in 0..MAX_RETRIES {
        match committer.await_durable(seq) {
            Ok(()) => return,
            Err(EngineError::Timeout { detail }) => last_timeout_detail = detail,
            Err(e) => panic!("unexpected error awaiting durability for seq={seq}: {e}"),
        }
    }
    panic!(
        "await_durable(seq={seq}) still not satisfied after {MAX_RETRIES} retries; \
         last timeout: {last_timeout_detail}"
    );
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

pub struct ThroughputResult {
    pub elapsed: Duration,
    pub total_records: usize,
    pub dir: PathBuf,
}

/// Spawns `threads` OS threads, each doing `per_thread` `append` +
/// (retrying) `await_durable` calls through one shared `GroupCommitter`,
/// and returns the wall-clock time for the whole run plus the WAL
/// directory (left on disk — not cleaned up — so the caller can reopen and
/// verify recoverability before removing it). Used by M1.2 and M1.3, which
/// differ only in `threads`/`per_thread`/the throughput target.
pub fn run_throughput_scenario(tag: &str, threads: usize, per_thread: usize) -> ThroughputResult {
    let dir = temp_dir(tag);
    let (wal, _) = rubixdb::wal::FileWal::open_for_recovery(&dir, group_commit_config())
        .expect("opening a fresh WAL must succeed");
    let committer = std::sync::Arc::new(
        GroupCommitter::new(wal).expect("wal was opened with SyncMode::GroupCommit"),
    );

    let started = std::time::Instant::now();
    let handles: Vec<_> = (0..threads)
        .map(|t| {
            let committer = std::sync::Arc::clone(&committer);
            std::thread::spawn(move || {
                for i in 0..per_thread {
                    let key = format!("t{t}-{i}");
                    append_durable_retrying(
                        &committer,
                        rubixdb::wal::WalOp::Put {
                            key: key.as_bytes(),
                            value: b"v",
                        },
                    );
                }
            })
        })
        .collect();
    for h in handles {
        h.join().expect("writer thread must not panic");
    }
    let elapsed = started.elapsed();

    let committer =
        std::sync::Arc::into_inner(committer).expect("no outstanding Arc clones remain");
    let wal = committer.into_inner();
    drop(wal);

    ThroughputResult {
        elapsed,
        total_records: threads * per_thread,
        dir,
    }
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

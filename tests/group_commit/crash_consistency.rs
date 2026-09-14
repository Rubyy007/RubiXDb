//! M1.6: real cross-process crash consistency for `GroupCommitter`, at
//! each of the WAL's four `AbortPoint`s, under concurrent load from 100
//! writer threads. Only compiled with the `test-util` feature (gated in
//! `tests/group_commit.rs`) — see `FileWal::set_abort_hook`, the sole
//! reason this exists, and `tests/crash_consistency.rs`'s own doc comment
//! for what a `std::process::abort()`-based test proves and does not
//! prove (not a real power-loss simulation — see that file for the full
//! argument, which applies identically here).
//!
//! # Why `BeforeSync`/`AfterSync` fire from `GroupCommitter`'s own leader
//! path, not (only) from `FileWal::sync()`
//!
//! `GroupCommitter`'s leader deliberately never calls `FileWal::sync()` —
//! it `fsync`s a cloned `std::fs::File` handle directly, outside the `wal`
//! lock (Shape B — see `group_commit.rs`'s module doc comment). The
//! pre-existing `AbortPoint::BeforeSync`/`AfterSync` hooks, fired only
//! inside `FileWal::sync()`, would therefore never fire on this path at
//! all. `GroupCommitter::run_as_leader` now fires the same two hooks
//! around its own `fsync` call (not around `GroupCommitter::new`'s
//! one-time warm-up probe — deliberately: firing there would abort during
//! construction, before any of this test's 100 writer threads even start,
//! testing a far less interesting "crash before any real work happened"
//! scenario instead of a crash during genuine concurrent leader activity).
//!
//! # How "the recovered records are exactly the acknowledged prefix" is
//! verified without anything surviving `abort()` in memory
//!
//! `std::process::abort()` kills every thread instantly — no code can run
//! afterward to report what happened. Each of the 100 writer threads
//! therefore appends its own acknowledged `seq` to a dedicated per-thread
//! file (`ack-{thread}.txt`) via a single unbuffered `write_all` (a real
//! `write()` syscall, no `BufWriter`) immediately after `await_durable`
//! returns `Ok` — per this crate's own established reasoning (`tests/
//! crash_consistency.rs`'s doc comment): a `write()` that has returned
//! successfully is visible in the OS's view of the file to any process on
//! this machine regardless of `fsync`, `abort()` or not. The parent reads
//! every ack file after reopening the WAL, tolerating (skipping) exactly
//! one possibly-torn final line per file — the one write that could
//! genuinely have raced the abort — and checks every remaining
//! acknowledged `seq` against the recovered set.

#![cfg(feature = "test-util")]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rubixdb::wal::{AbortPoint, FileWal, GroupCommitter, SyncMode, Wal, WalConfig, WalOp};

const ENV_WAL_DIR: &str = "RUBIXDB_GC_CRASH_WAL_DIR";
const ENV_ACK_DIR: &str = "RUBIXDB_GC_CRASH_ACK_DIR";
const ENV_ABORT_POINT: &str = "RUBIXDB_GC_CRASH_ABORT_POINT";

const WRITERS: usize = 100;
const PER_WRITER: usize = 10;

/// Every `AbortPoint` this test exercises: the four pre-Phase-1 WAL-level
/// points (still real boundaries `GroupCommitter::append`/`rotate` reach
/// via `FileWal`) plus all seven Phase 1 group-commit-specific points
/// added alongside this test (`wal::mod`'s `AbortPoint` doc comment names
/// the exact call site for each — every one is a real, reachable code
/// boundary that was actually instrumented, not an aspirational one).
const ALL_ABORT_POINTS: [AbortPoint; 11] = [
    AbortPoint::AfterHeader,
    AbortPoint::MidAppend,
    AbortPoint::BeforeLeader,
    AbortPoint::AfterLeaderElection,
    AbortPoint::DuringBatchWaitPre,
    AbortPoint::DuringBatchWaitPost,
    AbortPoint::BeforeSync,
    AbortPoint::AfterSync,
    AbortPoint::AfterWatermarkBeforeWake,
    AbortPoint::DuringRotationPre,
    AbortPoint::DuringRotationPost,
];

fn abort_point_name(p: AbortPoint) -> &'static str {
    match p {
        AbortPoint::AfterHeader => "AfterHeader",
        AbortPoint::MidAppend => "MidAppend",
        AbortPoint::BeforeSync => "BeforeSync",
        AbortPoint::AfterSync => "AfterSync",
        AbortPoint::BeforeLeader => "BeforeLeader",
        AbortPoint::AfterLeaderElection => "AfterLeaderElection",
        AbortPoint::DuringBatchWaitPre => "DuringBatchWaitPre",
        AbortPoint::DuringBatchWaitPost => "DuringBatchWaitPost",
        AbortPoint::AfterWatermarkBeforeWake => "AfterWatermarkBeforeWake",
        AbortPoint::DuringRotationPre => "DuringRotationPre",
        AbortPoint::DuringRotationPost => "DuringRotationPost",
    }
}

fn parse_abort_point(name: &str) -> AbortPoint {
    match name {
        "AfterHeader" => AbortPoint::AfterHeader,
        "MidAppend" => AbortPoint::MidAppend,
        "BeforeSync" => AbortPoint::BeforeSync,
        "AfterSync" => AbortPoint::AfterSync,
        "BeforeLeader" => AbortPoint::BeforeLeader,
        "AfterLeaderElection" => AbortPoint::AfterLeaderElection,
        "DuringBatchWaitPre" => AbortPoint::DuringBatchWaitPre,
        "DuringBatchWaitPost" => AbortPoint::DuringBatchWaitPost,
        "AfterWatermarkBeforeWake" => AbortPoint::AfterWatermarkBeforeWake,
        "DuringRotationPre" => AbortPoint::DuringRotationPre,
        "DuringRotationPost" => AbortPoint::DuringRotationPost,
        other => panic!("unknown abort point {other:?}"),
    }
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("rubixdb_gc_crash_{tag}_{nanos}_{n}"));
    fs::create_dir_all(&path).unwrap();
    path
}

fn group_commit_config() -> WalConfig {
    WalConfig {
        sync_mode: SyncMode::GroupCommit {
            max_wait: Duration::from_micros(200),
            max_batch_bytes: 256 * 1024,
        },
        ..WalConfig::default()
    }
}

/// Reads `ack_dir`'s per-thread files, returning every `seq` that can be
/// robustly parsed. Tolerates a possibly-torn *final* line per file (the
/// one acknowledgment write that could have genuinely raced the abort) by
/// simply skipping any line that fails to parse as `u64` — every other
/// line in a file that has ever had `write_all` return successfully is a
/// complete, valid write (per this file's module doc comment), so a
/// parse failure can only ever be that one racing tail write.
fn read_acknowledged_seqs(ack_dir: &Path) -> Vec<u64> {
    let mut seqs = Vec::new();
    let Ok(entries) = fs::read_dir(ack_dir) else {
        return seqs;
    };
    for entry in entries.flatten() {
        let Ok(contents) = fs::read_to_string(entry.path()) else {
            continue;
        };
        for line in contents.lines() {
            if let Ok(seq) = line.trim().parse::<u64>() {
                seqs.push(seq);
            }
        }
    }
    seqs
}

/// The parent-side test: for every `AbortPoint`, spawns this same test
/// binary with `m1_6_crash_consistency::child_worker` as an exact
/// test-name filter, plus environment variables telling that invocation
/// which point to target and which directories to use.
#[test]
fn crash_consistency_across_abort_points() {
    let exe = std::env::current_exe().expect("current_exe must be resolvable under cargo test");

    for point in ALL_ABORT_POINTS {
        let name = abort_point_name(point);
        let wal_dir = unique_dir(&format!("{name}_wal"));
        let ack_dir = unique_dir(&format!("{name}_ack"));

        let status = Command::new(&exe)
            .arg("m1_6_crash_consistency::child_worker")
            .arg("--exact")
            .arg("--nocapture")
            .env(ENV_WAL_DIR, &wal_dir)
            .env(ENV_ACK_DIR, &ack_dir)
            .env(ENV_ABORT_POINT, name)
            .status()
            .expect("failed to spawn child test process");
        assert!(
            !status.success(),
            "child process for abort point {name} was expected to abort(), \
             but exited successfully"
        );

        let (_wal, replay) = FileWal::open_for_recovery(&wal_dir, WalConfig::default())
            .unwrap_or_else(|e| panic!("recovery after abort point {name} failed: {e}"));
        assert!(
            replay.corrupted_segments.is_empty(),
            "abort point {name} must never leave corruption behind, got {:?}",
            replay.corrupted_segments
        );
        for (i, (seq, _)) in replay.records.iter().enumerate() {
            assert_eq!(
                *seq,
                (i as u64) + 1,
                "gap or reordering in recovered records at abort point {name}"
            );
        }
        let max_recovered_seq = replay.records.last().map(|(seq, _)| *seq).unwrap_or(0);

        let acknowledged = read_acknowledged_seqs(&ack_dir);
        for seq in &acknowledged {
            assert!(
                *seq <= max_recovered_seq,
                "abort point {name}: seq {seq} was acknowledged durable but is NOT part of \
                 the recovered prefix (max recovered seq = {max_recovered_seq}) — a false \
                 acknowledgment, the exact durability lie this test exists to catch"
            );
        }

        let _ = fs::remove_dir_all(&wal_dir);
        let _ = fs::remove_dir_all(&ack_dir);
    }
}

/// The child-side worker. Registered as an ordinary `#[test]` so it can be
/// selected by name via `--exact` from the parent's `Command`; under a
/// normal, un-parented `cargo test` sweep (the environment variables below
/// are absent) it returns immediately and passes trivially.
#[test]
fn child_worker() {
    let Ok(wal_dir) = std::env::var(ENV_WAL_DIR) else {
        return;
    };
    let Ok(ack_dir) = std::env::var(ENV_ACK_DIR) else {
        return;
    };
    let Ok(point_name) = std::env::var(ENV_ABORT_POINT) else {
        return;
    };
    let target = parse_abort_point(&point_name);

    // `FileWal::set_abort_hook` takes a plain `fn(AbortPoint)` (no closure
    // capture), so the target point is stashed in a process-wide
    // `OnceLock` the hook function reads from — this process exists for
    // exactly one child run, so this is fine.
    static TARGET: OnceLock<AbortPoint> = OnceLock::new();
    TARGET
        .set(target)
        .expect("set once, at the very start of this process");

    fn hook(point: AbortPoint) {
        if TARGET.get() == Some(&point) {
            std::process::abort();
        }
    }
    FileWal::set_abort_hook(hook);

    let (wal, _) = FileWal::open_for_recovery(Path::new(&wal_dir), group_commit_config()).unwrap();
    let committer = std::sync::Arc::new(GroupCommitter::new(wal).unwrap());
    let ack_dir = std::sync::Arc::new(ack_dir);

    // A dedicated rotator thread, mirroring M1.5's design, so
    // DuringRotationPre/DuringRotationPost are real, reachable boundaries
    // in this scenario (not just in the single-threaded unit tests) — a
    // short, fixed delay is enough here: the abort fires on the *first*
    // process-wide occurrence of the target point, so rotate() only needs
    // to run at all, not race precisely against the 100 writers.
    {
        let rotator = std::sync::Arc::clone(&committer);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_micros(200));
            let _ = rotator.rotate();
        });
    }

    let handles: Vec<_> = (0..WRITERS)
        .map(|t| {
            let committer = std::sync::Arc::clone(&committer);
            let ack_dir = std::sync::Arc::clone(&ack_dir);
            std::thread::spawn(move || {
                let ack_path = Path::new(ack_dir.as_str()).join(format!("ack-{t}.txt"));
                let mut ack_file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&ack_path)
                    .expect("creating this thread's own ack file must succeed");
                for i in 0..PER_WRITER {
                    let key = format!("t{t}-{i}");
                    let position = match committer.append(WalOp::Put {
                        key: key.as_bytes(),
                        value: b"v",
                    }) {
                        Ok(p) => p,
                        Err(_) => return, // process may abort mid-append; fine either way
                    };
                    if committer.await_durable(position.seq).is_ok() {
                        // A single unbuffered write() — see this file's
                        // module doc comment for why this is safe to rely
                        // on without an fsync here.
                        let _ = ack_file.write_all(format!("{}\n", position.seq).as_bytes());
                    }
                }
            })
        })
        .collect();
    for h in handles {
        let _ = h.join();
    }
    // Reaching here without aborting is only unreachable-by-design for
    // AfterHeader (fires inside open_for_recovery, above) — for the other
    // three points it depends on timing, exactly like tests/
    // crash_consistency.rs's own child_worker; no assertion forces an
    // abort to have actually happened.
}

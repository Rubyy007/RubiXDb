//! Group 7.2: real cross-process crash-consistency testing. Only compiled
//! with the `test-util` feature (`cargo test --features test-util`) — see
//! `FileWal::set_abort_hook`, which this test is the sole reason for.
//!
//! # What this actually proves, and what it doesn't
//!
//! A child process opens a `FileWal`, appends records, and calls
//! `std::process::abort()` at a configurable [`AbortPoint`] reached via an
//! installed hook. The parent then reopens the same directory and checks
//! that recovery finds a safe, fully-explainable state.
//!
//! `std::process::abort()` on a live machine is **not** a power-loss
//! simulation: once a `write()` syscall has returned successfully, its
//! bytes are visible in the OS's own view of the file to *any* process on
//! that machine, `fsync`ed or not — killing the process that issued the
//! write does not undo it. This test therefore cannot exercise the
//! torn-write recovery path itself (a write straddling a real power cut,
//! with bytes physically incomplete on disk) — that is exactly what
//! `wal::fuzz_tests` and `wal::testing::FaultInjectingIo` are for, working
//! at the byte and syscall level respectively. What this test *does* prove,
//! and is still valuable for, is that the real filesystem path — real
//! `std::fs::File`, real `OpenOptions`, real directory listing — behaves
//! exactly like the in-memory model: after an abort at any of these
//! points, recovery reports zero corruption and an exact, gap-free prefix
//! of what was durably appended, and `next_seq` resumes correctly.

#![cfg(feature = "test-util")]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use rubixdb::wal::{AbortPoint, FileWal, Wal, WalConfig};

const ENV_DIR: &str = "RUBIXDB_CRASH_WAL_DIR";
const ENV_ABORT_POINT: &str = "RUBIXDB_CRASH_ABORT_POINT";

fn abort_point_name(p: AbortPoint) -> &'static str {
    match p {
        AbortPoint::AfterHeader => "AfterHeader",
        AbortPoint::MidAppend => "MidAppend",
        AbortPoint::BeforeSync => "BeforeSync",
        AbortPoint::AfterSync => "AfterSync",
        // Phase 1 (Group Commit) points: not exercised by this WAL-only
        // test (see tests/group_commit/crash_consistency.rs for those) —
        // named here only so this match stays exhaustive as the enum
        // grows, per this crate's own "never silently drop a variant from
        // an exhaustive match" convention.
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
    let path = std::env::temp_dir().join(format!("rubixdb_crash_consistency_{tag}_{nanos}_{n}"));
    std::fs::create_dir_all(&path).unwrap();
    path
}

/// The parent-side test: for every `AbortPoint`, spawns this same test
/// binary with `child_worker` as an exact test-name filter (the standard
/// "re-exec myself to get a fresh process" trick for testing real crash
/// behavior), plus environment variables telling that invocation which
/// point to abort at and which directory to use. `child_worker` no-ops
/// under a normal `cargo test` sweep (no env vars set) — see its own doc
/// comment.
#[test]
fn crash_consistency_across_abort_points() {
    let exe = std::env::current_exe().expect("current_exe must be resolvable under cargo test");

    for point in [
        AbortPoint::AfterHeader,
        AbortPoint::MidAppend,
        AbortPoint::BeforeSync,
        AbortPoint::AfterSync,
    ] {
        let name = abort_point_name(point);
        let dir = unique_dir(name);

        let status = Command::new(&exe)
            .arg("child_worker")
            .arg("--exact")
            .arg("--nocapture")
            .env(ENV_DIR, &dir)
            .env(ENV_ABORT_POINT, name)
            .status()
            .expect("failed to spawn child test process");
        assert!(
            !status.success(),
            "child process for abort point {name} was expected to abort(), \
             but exited successfully"
        );

        let (wal, result) = FileWal::open_for_recovery(&dir, WalConfig::default())
            .unwrap_or_else(|e| panic!("recovery after abort point {name} failed: {e}"));
        assert!(
            result.corrupted_segments.is_empty(),
            "abort point {name} must never leave corruption behind, got {:?}",
            result.corrupted_segments
        );
        for (i, (seq, _)) in result.records.iter().enumerate() {
            assert_eq!(
                *seq,
                (i as u64) + 1,
                "gap or reordering in recovered records at abort point {name}"
            );
        }
        assert_eq!(
            wal.next_seq(),
            result.records.len() as u64 + 1,
            "next_seq must resume exactly after the last recovered record at abort point {name}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// The child-side worker. Registered as an ordinary `#[test]` so it can be
/// selected by name via `--exact` from the parent's `Command`; when run as
/// part of a normal, un-parented `cargo test` sweep (the environment
/// variables below are absent), it immediately returns and passes
/// trivially — it only does real work when deliberately invoked as a
/// child process by [`crash_consistency_across_abort_points`].
#[test]
fn child_worker() {
    let Ok(dir) = std::env::var(ENV_DIR) else {
        return;
    };
    let Ok(point_name) = std::env::var(ENV_ABORT_POINT) else {
        return;
    };
    let target = parse_abort_point(&point_name);

    // `FileWal::set_abort_hook` takes a plain `fn(AbortPoint)` (no closure
    // capture), so the target point is stashed in a process-wide
    // `OnceLock` the hook function reads from — fine here, since this
    // process exists for exactly one child run.
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

    let (mut wal, _) = FileWal::open_for_recovery(Path::new(&dir), WalConfig::default()).unwrap();
    for i in 0..20u32 {
        wal.append_sync(rubixdb::wal::WalOp::Put {
            key: format!("k{i}").as_bytes(),
            value: b"v",
        })
        .unwrap();
    }
    // Reaching here without aborting is only expected to be unreachable
    // for the four points above, all of which fire within the first
    // `append_sync` call (or, for `AfterHeader`, within
    // `open_for_recovery` itself) — but this is a test-only harness, not
    // a correctness assumption the WAL itself depends on, so there is no
    // assertion here forcing an abort to have happened.
}

//! M1.4: an `fsync` failure on the leader's batch under concurrent load
//! must propagate to every waiter as `Err`, never hang any of them, and
//! permanently poison the committer (a fresh record afterward also fails).
//!
//! Uses `GroupCommitter::install_fsync_fault_hook` rather than `wal::
//! testing::FaultInjectingIo`: the leader's `fsync` deliberately bypasses
//! the `SegmentIo`/`WalFile` layer that harness intercepts (Shape B — see
//! `group_commit.rs`'s module doc comment and `PROCESS.md` §1.3-§1.4), so
//! `FaultInjectingIo` cannot reach this call at all. `install_fsync_fault_
//! hook` is the purpose-built seam for exactly this call site, scoped to
//! one `GroupCommitter` instance (not a process-wide global — see that
//! method's doc comment for the concurrent-test-isolation bug an earlier,
//! global-`static` version of this seam actually hit during development,
//! recorded in `PROCESS.md`'s M0 milestone entry).

use std::io;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use rubixdb::wal::{FileWal, GroupCommitter, Wal, WalOp};
use rubixdb::EngineError;

use crate::support;

const WAITERS: usize = 50;
/// A generous bound that turns "no waiter hangs" into an actual
/// measurement rather than an assumption: every waiter's result must
/// arrive through the channel within this, or the test itself fails (a
/// genuinely hung waiter would simply never send).
const NO_HANG_BOUND: Duration = Duration::from_secs(15);

#[test]
fn leader_failure_propagation() {
    let dir = support::temp_dir("m1_4");
    let (wal, _) = FileWal::open_for_recovery(&dir, support::group_commit_config())
        .expect("opening a fresh WAL must succeed");
    // GroupCommitter::new performs its own real warm-up fsync before
    // returning — it must succeed before any fault is injected.
    let committer = Arc::new(
        GroupCommitter::new(wal).expect("construction must succeed before any fault is injected"),
    );

    committer
        .install_fsync_fault_hook(|| Err(io::Error::other("M1.4 injected leader fsync failure")));

    let (tx, rx) = mpsc::channel();
    let handles: Vec<_> = (0..WAITERS)
        .map(|i| {
            let committer = Arc::clone(&committer);
            let tx = tx.clone();
            thread::spawn(move || {
                let key = format!("k{i}");
                let position = committer
                    .append(WalOp::Put {
                        key: key.as_bytes(),
                        value: b"v",
                    })
                    .expect("append itself never fails in this scenario: only fsync is faulted");
                let result = committer.await_durable(position.seq);
                let _ = tx.send(result);
            })
        })
        .collect();
    drop(tx);

    let mut results = Vec::with_capacity(WAITERS);
    for _ in 0..WAITERS {
        let result = rx.recv_timeout(NO_HANG_BOUND).expect(
            "a waiter's result must arrive within the no-hang bound; a recv timeout here \
             means a waiter genuinely hung",
        );
        results.push(result);
    }
    for h in handles {
        h.join().expect("writer thread must not panic");
    }

    assert!(
        results.iter().all(|r| r.is_err()),
        "every waiter must receive Err once the leader's fsync has failed; \
         got at least one Ok: {results:?}"
    );
    assert!(
        results.iter().any(|r| matches!(r, Err(EngineError::Io(_)))),
        "at least one waiter must observe the concrete injected Io error \
         (others may legitimately observe Timeout under scheduling races \
         that resolve to the poisoned state slightly late — see PROCESS.md — \
         but at least one must see the real failure): {results:?}"
    );

    // Permanent poisoning: a fresh record, appended *after* the failure,
    // must also fail, with the same error class, immediately — no further
    // real fsync is ever attempted once poisoned.
    let fresh = committer
        .append(WalOp::Put {
            key: b"fresh-after-poison",
            value: b"v",
        })
        .expect("append itself never fails in this scenario: only fsync is faulted");
    let fresh_result = committer.await_durable(fresh.seq);
    assert!(
        matches!(fresh_result, Err(EngineError::Io(_))),
        "a fresh record appended after poisoning must fail immediately with the \
         same error class, got {fresh_result:?}"
    );

    committer.clear_fsync_fault_hook();
    let _ = std::fs::remove_dir_all(&dir);
}

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::*;
use crate::execution::batch_coordinator::BatchCoordinatorConfig;
use crate::wal::SyncMode;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("rubixdb_lsm_test_{tag}_{nanos}_{n}"));
    fs::create_dir_all(&path).unwrap();
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

fn open(dir: &Path, lsm_config: LsmConfig) -> LsmEngine {
    LsmEngine::open(dir, test_wal_config(), small_pool_config(), lsm_config).unwrap()
}

#[test]
fn put_then_get_round_trips() {
    let dir = temp_dir("put_get");
    let engine = open(&dir, LsmConfig::default());
    let seq = engine.put(b"k1", b"v1").unwrap();
    assert_eq!(seq, 1);
    assert_eq!(engine.get(b"k1").unwrap(), Some(b"v1".to_vec()));
    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn delete_then_get_returns_not_found() {
    let dir = temp_dir("delete_get");
    let engine = open(&dir, LsmConfig::default());
    engine.put(b"k1", b"v1").unwrap();
    engine.delete(b"k1").unwrap();
    assert_eq!(engine.get(b"k1").unwrap(), None);
    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn put_delete_put_resolves_to_the_newest_write() {
    let dir = temp_dir("put_delete_put");
    let engine = open(&dir, LsmConfig::default());
    let seq1 = engine.put(b"k1", b"v1").unwrap();
    let seq2 = engine.delete(b"k1").unwrap();
    let seq3 = engine.put(b"k1", b"v3").unwrap();

    assert_eq!(engine.get_as_of(b"k1", seq1).unwrap(), Some(b"v1".to_vec()));
    assert_eq!(engine.get_as_of(b"k1", seq2).unwrap(), None);
    assert_eq!(engine.get_as_of(b"k1", seq3).unwrap(), Some(b"v3".to_vec()));
    assert_eq!(engine.get(b"k1").unwrap(), Some(b"v3".to_vec()));
    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn snapshot_reads_remain_stable_across_later_writes() {
    let dir = temp_dir("snapshot_stability");
    let engine = open(&dir, LsmConfig::default());
    let seq1 = engine.put(b"k1", b"v1").unwrap();
    let snapshot = engine.snapshot_seq();
    assert!(snapshot >= seq1);

    // New Put and new Delete after the snapshot was taken.
    engine.put(b"k1", b"v2").unwrap();
    engine.delete(b"k2").unwrap();
    engine.put(b"k2", b"v-after-snapshot").unwrap();

    // The snapshot must still see exactly the state as of its own boundary.
    assert_eq!(
        engine.get_as_of(b"k1", snapshot).unwrap(),
        Some(b"v1".to_vec())
    );
    assert_eq!(engine.get_as_of(b"k2", snapshot).unwrap(), None);
    // "Now" (no snapshot) sees the latest.
    assert_eq!(engine.get(b"k1").unwrap(), Some(b"v2".to_vec()));
    assert_eq!(
        engine.get(b"k2").unwrap(),
        Some(b"v-after-snapshot".to_vec())
    );
    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn freeze_triggers_at_the_configured_threshold_and_data_remains_visible() {
    let dir = temp_dir("freeze_threshold");
    // Small threshold so a handful of Puts force a freeze.
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: 200,
        max_immutable_memtables: 8,
        ..LsmConfig::default()
    };
    let engine = open(&dir, lsm_config);

    for i in 0..20u32 {
        engine
            .put(format!("k{i:03}").as_bytes(), format!("v{i:03}").as_bytes())
            .unwrap();
    }
    assert!(
        engine.immutable_count() >= 1,
        "at least one freeze must have happened by now"
    );
    // Every key, old and new, must still be visible via get() regardless
    // of which memtable (active or immutable) it now lives in.
    for i in 0..20u32 {
        assert_eq!(
            engine.get(format!("k{i:03}").as_bytes()).unwrap(),
            Some(format!("v{i:03}").into_bytes())
        );
    }
    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn immutable_backpressure_rejects_further_freezes_past_the_limit() {
    let dir = temp_dir("immutable_backpressure");
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: 80, // freezes almost every write
        max_immutable_memtables: 2,
        ..LsmConfig::default()
    };
    let engine = open(&dir, lsm_config);
    // Phase 4B's background flush thread now actively drains `immutables`
    // (`PHASE4B_ADR.md` ADR-P4B-5) -- without slowing it down, this
    // tiny-payload workload's real disk I/O for each flush can easily
    // keep pace with (or outrun) this single thread's own WAL-durability-
    // bound write rate, so backpressure might never trigger at all. A
    // deliberate artificial delay makes the "flush cannot keep up" case
    // this test exists to cover reproducible instead of timing-dependent.
    engine.set_flush_delay_for_test(Duration::from_millis(200));

    // Accepted contract (`PHASE4A_FAILURE_MODEL.md` §2, `PHASE4A_ADR.md`
    // ADR-P4A-5): the triggering write is already durable+applied before
    // this check runs, but `put`/`delete` still surfaces
    // `Err(CapacityExceeded)` to the caller as the backpressure signal —
    // it is not silently swallowed. `capacity_pressure_events()` is an
    // additional observability counter alongside that `Err`, not a
    // replacement for it.
    let mut saw_capacity_error = false;
    for i in 0..40u32 {
        match engine.put(format!("k{i:03}").as_bytes(), b"v") {
            Ok(_) => {}
            Err(EngineError::CapacityExceeded { .. }) => {
                saw_capacity_error = true;
                break;
            }
            Err(e) => panic!("unexpected error: {e}"),
        }
    }
    assert!(
        saw_capacity_error,
        "sustained writes past max_immutable_memtables must eventually hit backpressure, \
         not silently accumulate unbounded immutable memtables"
    );
    assert!(
        engine.capacity_pressure_events() > 0,
        "the capacity_pressure_events observability counter must track the same event"
    );
    assert!(engine.immutable_count() <= 2);
    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn memory_accounting_remains_correct_across_freeze() {
    let dir = temp_dir("memory_after_freeze");
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: 150,
        max_immutable_memtables: 8,
        ..LsmConfig::default()
    };
    let engine = open(&dir, lsm_config);
    // Same reasoning as the backpressure test above: pause the background
    // flush thread so the immutable memtable this test creates is still
    // observable when the assertions below run, rather than racing real
    // disk I/O (`PHASE4B_ADR.md` ADR-P4B-5).
    engine.set_flush_delay_for_test(Duration::from_millis(200));

    for i in 0..10u32 {
        engine.put(format!("k{i}").as_bytes(), b"value").unwrap();
    }
    let active_bytes = engine.active_size_bytes();
    let immutable_bytes = engine.immutable_total_bytes();
    assert!(
        immutable_bytes > 0,
        "at least one freeze must have happened"
    );
    // Neither figure is ever negative/overflowed (usize can't be
    // negative, but a wraparound bug would show up as an absurdly large
    // value) — sanity bound relative to what was actually written.
    assert!(active_bytes < 10_000);
    assert!(immutable_bytes < 10_000);
    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn wal_durability_ordering_is_respected_not_just_memtable_visibility() {
    // A write that fails at the WAL layer must never have been applied to
    // the MemTable — verified by injecting a real fsync failure and
    // confirming the key is not visible afterward.
    let dir = temp_dir("durability_ordering");
    let (wal, _) = crate::wal::FileWal::open_for_recovery(&dir, test_wal_config()).unwrap();
    let committer = crate::wal::GroupCommitter::new(wal).unwrap();
    committer.install_fsync_fault_hook(|| Err(std::io::Error::other("injected fsync failure")));
    let pool = BatchCoordinatorPool::new(committer, small_pool_config()).unwrap();
    // No flush thread for this raw-constructed, WAL-fault-injection-only
    // engine: nothing in this test ever freezes a memtable, so a `Sender`
    // whose `Receiver` is immediately dropped (never joined, never sent
    // to) is harmless -- `shutdown`'s `flush_handle.take()` finds `None`
    // and skips the join.
    let (flush_sender, _unused_receiver) = mpsc::channel();
    // `FileWal::open_for_recovery` above already holds this directory's
    // exclusive lock in this same process, satisfying `Manifest::open_
    // after_exclusive_lock`'s own contract.
    let (manifest, _) = Manifest::open_after_exclusive_lock(&dir).unwrap();
    let engine = LsmEngine {
        pool: Arc::new(pool),
        active: RwLock::new(MemTable::new(LsmConfig::default().memtable_max_size_bytes)),
        immutables: Arc::new(RwLock::new(VecDeque::new())),
        sstables: Arc::new(RwLock::new(Vec::new())),
        next_sstable_id: Arc::new(AtomicU64::new(1)),
        sstables_dir: dir.join("sstables"),
        manifest: Arc::new(Mutex::new(manifest)),
        checkpoint_seq: Arc::new(AtomicU64::new(0)),
        recovery_stats: RecoveryStats::default(),
        flush_sender,
        flush_handle: Mutex::new(None),
        flush_stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        flush_delay_ms: Arc::new(AtomicU64::new(0)),
        capacity_pressure_events: AtomicU64::new(0),
        flush_fault_hook: Arc::new(Mutex::new(None)),
        config: LsmConfig::default(),
    };

    let result = engine.put(b"k1", b"v1");
    assert!(
        result.is_err(),
        "a WAL durability failure must propagate as an error"
    );
    assert_eq!(
        engine.get(b"k1").unwrap(),
        None,
        "a write that never became durable must never be visible in the MemTable"
    );
    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn recovery_reconstructs_the_memtable_from_the_wal_after_restart() {
    let dir = temp_dir("recovery_basic");
    {
        let engine = open(&dir, LsmConfig::default());
        engine.put(b"k1", b"v1").unwrap();
        engine.put(b"k2", b"v2").unwrap();
        engine.delete(b"k1").unwrap();
        engine.put(b"k3", b"v3").unwrap();
        engine.shutdown();
    }

    let engine2 = open(&dir, LsmConfig::default());
    assert_eq!(
        engine2.get(b"k1").unwrap(),
        None,
        "k1 was deleted before shutdown"
    );
    assert_eq!(engine2.get(b"k2").unwrap(), Some(b"v2".to_vec()));
    assert_eq!(engine2.get(b"k3").unwrap(), Some(b"v3".to_vec()));
    engine2.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn recovery_reconstructs_multiple_versions_correctly_across_a_restart() {
    let dir = temp_dir("recovery_multiversion");
    let (seq1, seq2, seq3);
    {
        let engine = open(&dir, LsmConfig::default());
        seq1 = engine.put(b"k1", b"v1").unwrap();
        seq2 = engine.put(b"k1", b"v2").unwrap();
        seq3 = engine.put(b"k1", b"v3").unwrap();
        engine.shutdown();
    }

    let engine2 = open(&dir, LsmConfig::default());
    assert_eq!(
        engine2.get_as_of(b"k1", seq1).unwrap(),
        Some(b"v1".to_vec())
    );
    assert_eq!(
        engine2.get_as_of(b"k1", seq2).unwrap(),
        Some(b"v2".to_vec())
    );
    assert_eq!(
        engine2.get_as_of(b"k1", seq3).unwrap(),
        Some(b"v3".to_vec())
    );
    engine2.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn recovery_across_multiple_wal_segments_and_rotation() {
    let dir = temp_dir("recovery_rotation");
    {
        let wal_config = WalConfig {
            max_segment_size: 512,
            ..test_wal_config()
        };
        let engine =
            LsmEngine::open(&dir, wal_config, small_pool_config(), LsmConfig::default()).unwrap();
        for i in 0..100u32 {
            engine
                .put(format!("k{i:04}").as_bytes(), format!("v{i:04}").as_bytes())
                .unwrap();
        }
        engine.shutdown();
    }

    let wal_config = WalConfig {
        max_segment_size: 512,
        ..test_wal_config()
    };
    let engine2 =
        LsmEngine::open(&dir, wal_config, small_pool_config(), LsmConfig::default()).unwrap();
    for i in 0..100u32 {
        assert_eq!(
            engine2.get(format!("k{i:04}").as_bytes()).unwrap(),
            Some(format!("v{i:04}").into_bytes())
        );
    }
    engine2.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

// --- Concurrency (operating brief §31) ---

fn concurrency_smoke(writer_count: usize, per_writer: usize) {
    let dir = temp_dir(&format!("concurrency_{writer_count}"));
    let engine = Arc::new(open(&dir, LsmConfig::default()));

    let handles: Vec<_> = (0..writer_count)
        .map(|t| {
            let engine = Arc::clone(&engine);
            thread::spawn(move || {
                for i in 0..per_writer {
                    let key = format!("t{t}-k{i}");
                    let value = format!("t{t}-v{i}");
                    engine.put(key.as_bytes(), value.as_bytes()).unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }

    let mut missing = 0usize;
    for t in 0..writer_count {
        for i in 0..per_writer {
            let key = format!("t{t}-k{i}");
            let expected = format!("t{t}-v{i}").into_bytes();
            if engine.get(key.as_bytes()).unwrap() != Some(expected) {
                missing += 1;
            }
        }
    }
    assert_eq!(missing, 0, "no record may be lost or hold the wrong value");
    assert_eq!(
        engine.active_entry_count()
            + engine
                .lock_immutables_read()
                .iter()
                .map(|m| m.entry_count())
                .sum::<usize>(),
        writer_count * per_writer,
        "no duplicate and no lost entries across active + immutable memtables"
    );
    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn concurrency_one_writer() {
    concurrency_smoke(1, 50);
}

#[test]
fn concurrency_ten_writers() {
    concurrency_smoke(10, 30);
}

#[test]
fn concurrency_hundred_writers() {
    concurrency_smoke(100, 10);
}

/// Operating brief §31: "The 1,000-writer workload must continue using
/// the production Dedicated Batch Coordinator rather than creating
/// unnecessary OS threads inside MemTable" — satisfied structurally:
/// `LsmEngine::put` submits through the exact same, unmodified
/// `BatchCoordinatorPool::submit` every other 1,000-writer test in this
/// project already uses (`tests/group_commit/thousand_writers_
/// throughput.rs`, `examples/batch_coordinator_load_test.rs`); no new
/// thread is spawned by this module for the write path itself — only
/// the 1,000 logical-writer test threads this test harness itself
/// spawns, matching every prior phase's own methodology.
#[test]
fn concurrency_thousand_logical_writers() {
    concurrency_smoke(1000, 3);
}

// --- Security / resource-bound edge cases (operating brief §34) ---

#[test]
fn large_key_and_value_are_handled_without_overflow_or_panic() {
    let dir = temp_dir("large_key_value");
    let engine = open(&dir, LsmConfig::default());

    let large_key = vec![b'k'; 64 * 1024]; // 64 KiB key
    let large_value = vec![b'v'; 1024 * 1024]; // 1 MiB value
    engine.put(&large_key, &large_value).unwrap();
    assert_eq!(engine.get(&large_key).unwrap(), Some(large_value));
    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

/// A single entry larger than `memtable_max_size_bytes` must still be
/// handled deterministically (accepted, then immediately eligible for
/// freeze) rather than looping forever trying to make room, and must
/// never let `size_bytes` silently wrap or bypass the configured limit.
#[test]
fn an_entry_larger_than_the_configured_limit_does_not_hang_or_overflow() {
    let dir = temp_dir("oversized_entry");
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: 100,
        max_immutable_memtables: 4,
        ..LsmConfig::default()
    };
    let engine = open(&dir, lsm_config);

    let big_value = vec![b'x'; 10_000]; // far larger than the 100-byte limit
    engine.put(b"k1", &big_value).unwrap();
    assert_eq!(engine.get(b"k1").unwrap(), Some(big_value));
    // The oversized entry must have triggered an immediate freeze rather
    // than leaving `is_full()` permanently true with nothing able to
    // ever "fit" — verified indirectly: a further write must still
    // succeed (a fresh active memtable was installed), not error or hang.
    engine.put(b"k2", b"v2").unwrap();
    assert_eq!(engine.get(b"k2").unwrap(), Some(b"v2".to_vec()));
    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn memtable_size_accounting_never_overflows_with_many_large_entries() {
    // usize::MAX-adjacent accounting bugs would show up as a wrapped
    // (tiny or zero) size_bytes despite substantial real data — assert
    // monotonic growth instead of an exact figure, real enough to catch
    // a wraparound.
    let mut m = MemTable::new(usize::MAX);
    let value = vec![0u8; 100_000];
    let mut previous_size = 0usize;
    for i in 0..50u64 {
        m.put(format!("k{i}").as_bytes(), i + 1, &value);
        assert!(
            m.size_bytes() > previous_size,
            "size_bytes must strictly grow with each new (key, seq) insert, never wrap"
        );
        previous_size = m.size_bytes();
    }
}

// --- Recovery equivalence against a reference model (operating brief §25) ---

/// The same sequence of Put/Delete operations, applied to a real
/// `LsmEngine` (WAL + MemTable) and to a naive in-memory reference
/// model, must produce identical final state after a real restart
/// (drop the engine, reopen — exercising `LsmEngine::open`'s own
/// recovery path via `wal::replay_streaming`, not a mock).
#[test]
fn recovery_matches_a_reference_model_after_restart() {
    let dir = temp_dir("recovery_reference_model");
    let mut reference: std::collections::HashMap<Vec<u8>, Option<Vec<u8>>> =
        std::collections::HashMap::new();

    {
        let engine = open(&dir, LsmConfig::default());
        for i in 0..200u32 {
            let key = format!("k{}", i % 20).into_bytes(); // 20 distinct keys, heavy overwrite
            if i % 5 == 0 {
                engine.delete(&key).unwrap();
                reference.insert(key, None);
            } else {
                let value = format!("v{i}").into_bytes();
                engine.put(&key, &value).unwrap();
                reference.insert(key, Some(value));
            }
        }
        engine.shutdown();
    }

    let engine2 = open(&dir, LsmConfig::default());
    for (key, expected) in &reference {
        assert_eq!(
            engine2.get(key).unwrap(),
            expected.clone(),
            "mismatch for key {key:?} after restart"
        );
    }
    engine2.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

// --- RUBIC SSTable flush integration (Phase 4B) ---

fn wait_until(mut condition: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if condition() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// A flush must eventually move a frozen memtable's data into a
/// published, independently-readable SSTable, and the key must remain
/// correctly readable via `LsmEngine::get` throughout (`PHASE4B_
/// ARCHITECTURE.md` §4-§5) — whether it's currently served from
/// `immutables` or from `sstables` is an implementation detail the
/// caller never needs to know.
#[test]
fn flush_moves_data_into_a_published_sstable_that_remains_readable() {
    let dir = temp_dir("flush_publishes_sstable");
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: 100,
        max_immutable_memtables: 8,
        ..LsmConfig::default()
    };
    let engine = open(&dir, lsm_config);

    for i in 0..30u32 {
        engine
            .put(format!("k{i:03}").as_bytes(), format!("v{i:03}").as_bytes())
            .unwrap();
    }

    assert!(
        wait_until(|| engine.sstable_count() >= 1, Duration::from_secs(5)),
        "at least one flush must publish an SSTable within a reasonable time"
    );
    assert!(
        engine.next_sstable_id() >= 2,
        "an id must have been consumed"
    );

    for i in 0..30u32 {
        assert_eq!(
            engine.get(format!("k{i:03}").as_bytes()).unwrap(),
            Some(format!("v{i:03}").into_bytes()),
            "key must remain correctly readable regardless of which tier now holds it"
        );
    }
    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

/// Closes the flush-thread-panic gap `PHASE4B_FAILURE_MODEL.md`/
/// `PHASE5_ADR.md` ADR-P5-5 named but never deterministically tested:
/// the existing SSTable/Manifest crash tests kill the whole process
/// externally, which never exercises `catch_unwind` (a killed process
/// doesn't unwind). This injects a real panic, exactly once, at each
/// `FlushFaultPoint` in turn — the historically buggiest one
/// (`AfterCheckpointMarker`, the exact step `PHASE5_ADR.md`'s own
/// idempotent-retry bug was found at, there under an external kill) plus
/// the earliest and latest points as boundary cases — and verifies: the
/// panic is caught (the flush thread survives and keeps processing later
/// messages), the immutable MemTable is retained until a later retry
/// truly succeeds, checkpoint/SSTable state is never partially advanced
/// by the panicking attempt, the retry succeeds shortly after, no caller
/// ever hangs (every `put` in this test returns promptly), and every
/// written value remains correctly readable throughout and after.
#[test]
fn flush_thread_panic_is_caught_and_retried_without_data_loss_or_duplication() {
    for point in [
        FlushFaultPoint::BeforeSstableWrite,
        FlushFaultPoint::AfterSstablePublish,
        FlushFaultPoint::AfterRotate,
        FlushFaultPoint::AfterCheckpointMarker,
        FlushFaultPoint::AfterSetCheckpoint,
    ] {
        let dir = temp_dir(&format!("flush_panic_{point:?}"));
        // Every key/value below is "kNNN"/"vNNN" (4 bytes each), so each
        // entry costs exactly 4+4+32=40 bytes (`memtable::entry_size`).
        // 1180 sits strictly between 29*40=1160 and 30*40=1200, so all 30
        // puts land in one MemTable and exactly one freeze (hence exactly
        // one flush, exactly one SSTable) happens, on the very last put —
        // required so this test's "exactly one SSTable, never duplicated"
        // assertion below is meaningful.
        let lsm_config = LsmConfig {
            memtable_max_size_bytes: 1180,
            max_immutable_memtables: 8,
            ..LsmConfig::default()
        };
        let engine = open(&dir, lsm_config);

        let already_panicked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let already_panicked = Arc::clone(&already_panicked);
            engine.install_flush_fault_hook(move |p| {
                if p == point
                    && already_panicked
                        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                {
                    panic!("injected flush panic at {point:?} (deterministic fault test)");
                }
            });
        }

        // Every put must return promptly regardless of what the flush
        // thread is doing — the write path never waits on flush.
        for i in 0..30u32 {
            let put_started = std::time::Instant::now();
            engine
                .put(format!("k{i:03}").as_bytes(), format!("v{i:03}").as_bytes())
                .unwrap();
            assert!(
                put_started.elapsed() < Duration::from_secs(2),
                "put must never hang waiting on the flush thread ({point:?})"
            );
        }

        // The freeze that queues this flush happens synchronously inside
        // the 30th `put` above, but the background flush thread reaching
        // this fault point is asynchronous — poll with a bound instead of
        // checking immediately.
        assert!(
            wait_until(
                || already_panicked.load(Ordering::SeqCst),
                Duration::from_secs(5)
            ),
            "the fault point {point:?} must actually have been reached and fired"
        );

        // The panicking attempt must never leave partial progress:
        // eventually exactly one flush succeeds (retried by the same
        // still-alive thread), publishing exactly one SSTable and
        // advancing the checkpoint exactly once.
        assert!(
            wait_until(
                || engine.checkpoint_seq() > 0 && engine.immutable_count() == 0,
                Duration::from_secs(10)
            ),
            "the flush must eventually succeed after the injected panic ({point:?})"
        );
        assert_eq!(
            engine.sstable_count(),
            1,
            "the panic must not cause a duplicate SSTable publication ({point:?})"
        );

        for i in 0..30u32 {
            assert_eq!(
                engine.get(format!("k{i:03}").as_bytes()).unwrap(),
                Some(format!("v{i:03}").into_bytes()),
                "no acknowledged durable write may be lost across the panic ({point:?})"
            );
        }

        engine.clear_flush_fault_hook();
        engine.shutdown();
        let _ = fs::remove_dir_all(&dir);
    }
}

/// Per `PHASE4B_ADR.md` ADR-P4B-1 (no Manifest, no WAL purge this
/// phase): after a restart, published SSTables from before the restart
/// must be rediscovered, AND the WAL must still independently reproduce
/// every record via the unchanged full-replay path — the two are
/// deliberately redundant, and either one alone must already answer
/// every read correctly.
#[test]
fn sstables_are_rediscovered_after_restart_and_reads_remain_correct() {
    let dir = temp_dir("sstable_restart");
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: 100,
        max_immutable_memtables: 8,
        ..LsmConfig::default()
    };
    let sstables_before;
    {
        let engine = open(&dir, lsm_config.clone());
        for i in 0..30u32 {
            engine
                .put(format!("k{i:03}").as_bytes(), format!("v{i:03}").as_bytes())
                .unwrap();
        }
        // Wait for every freeze this loop triggered to be fully flushed
        // (not just "at least one") before reading a stable count --
        // otherwise a flush still in flight at the moment of the read
        // would land between this capture and `shutdown()`'s own full
        // drain, making the two counts legitimately differ.
        assert!(wait_until(
            || engine.immutable_count() == 0,
            Duration::from_secs(5)
        ));
        sstables_before = engine.sstable_count();
        engine.shutdown();
    }
    assert!(sstables_before >= 1);

    let engine2 = open(&dir, lsm_config);
    assert_eq!(
        engine2.sstable_count(),
        sstables_before,
        "every previously-published SSTable must be rediscovered on restart"
    );
    for i in 0..30u32 {
        assert_eq!(
            engine2.get(format!("k{i:03}").as_bytes()).unwrap(),
            Some(format!("v{i:03}").into_bytes())
        );
    }
    engine2.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

/// `PHASE4B_ADR.md` ADR-P4B-2: a corrupt, previously-published SSTable
/// must make `LsmEngine::open` fail closed, not silently start with
/// reduced read coverage.
#[test]
fn open_fails_closed_when_a_published_sstable_is_corrupt() {
    let dir = temp_dir("sstable_open_fails_closed");
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: 100,
        max_immutable_memtables: 8,
        ..LsmConfig::default()
    };
    {
        let engine = open(&dir, lsm_config.clone());
        for i in 0..30u32 {
            engine.put(format!("k{i:03}").as_bytes(), b"v").unwrap();
        }
        assert!(wait_until(
            || engine.sstable_count() >= 1,
            Duration::from_secs(5)
        ));
        engine.shutdown();
    }

    let sstables_dir = dir.join("sstables");
    let mut corrupted_any = false;
    for entry in fs::read_dir(&sstables_dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) == Some("sst") {
            let mut bytes = fs::read(&path).unwrap();
            let last = bytes.len() - 1;
            bytes[last] ^= 0xFF; // footer_crc32c's last byte
            fs::write(&path, bytes).unwrap();
            corrupted_any = true;
        }
    }
    assert!(
        corrupted_any,
        "test setup must have produced at least one .sst file"
    );

    let result = LsmEngine::open(&dir, test_wal_config(), small_pool_config(), lsm_config);
    assert!(
        matches!(result, Err(EngineError::Corruption { .. })),
        "open() must fail closed on a corrupt published SSTable, not silently exclude it"
    );
    let _ = fs::remove_dir_all(&dir);
}

// --- Manifest / checkpoint / WAL purge integration (Phase 5) ---

fn count_wal_segment_files(dir: &Path) -> usize {
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_name()
                        .to_str()
                        .map(|n| n.starts_with("wal-") && n.ends_with(".log"))
                        .unwrap_or(false)
                })
                .count()
        })
        .unwrap_or(0)
}

/// A flush must durably advance the Manifest checkpoint and, only after
/// that, allow the WAL to purge fully-covered segments
/// (`PHASE5_MANIFEST_ARCHITECTURE.md` §5). With a small memtable and a
/// small `max_segment_size`, sustained writes must keep the live WAL
/// segment count bounded rather than growing without limit — direct
/// evidence that `purge_before` is actually being called, not merely
/// wired up unused.
#[test]
fn checkpoint_advances_and_wal_segments_stay_bounded() {
    let dir = temp_dir("checkpoint_purge");
    let wal_config = WalConfig {
        max_segment_size: 4096,
        ..test_wal_config()
    };
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: 4096,
        max_immutable_memtables: 16,
        ..LsmConfig::default()
    };
    let engine = LsmEngine::open(&dir, wal_config, small_pool_config(), lsm_config).unwrap();

    for i in 0..2000u32 {
        engine
            .put(
                format!("k{i:05}").as_bytes(),
                b"some-reasonably-sized-value",
            )
            .unwrap();
    }

    assert!(
        wait_until(|| engine.checkpoint_seq() > 0, Duration::from_secs(10)),
        "at least one checkpoint must have been durably recorded"
    );
    assert!(wait_until(
        || engine.immutable_count() == 0,
        Duration::from_secs(10)
    ));

    let segment_count = count_wal_segment_files(&dir);
    assert!(
        segment_count < 20,
        "WAL segment count must stay bounded once checkpointing/purging is active, got {segment_count}"
    );
    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

/// After a checkpoint has been durably recorded and the WAL purged
/// below it, a restart must replay only the records *after* the
/// checkpoint into the fresh active MemTable (LSM Engine Spec §7.1 step
/// 5) — not the full history a second time. Every key must still be
/// correctly readable afterward regardless of which tier now holds it.
#[test]
fn restart_replays_only_post_checkpoint_records_and_all_data_remains_correct() {
    let dir = temp_dir("bounded_replay");
    let wal_config = WalConfig {
        max_segment_size: 4096,
        ..test_wal_config()
    };
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: 4096,
        max_immutable_memtables: 16,
        ..LsmConfig::default()
    };
    let total_keys = 2000u32;
    {
        let engine = LsmEngine::open(
            &dir,
            wal_config.clone(),
            small_pool_config(),
            lsm_config.clone(),
        )
        .unwrap();
        for i in 0..total_keys {
            engine
                .put(
                    format!("k{i:05}").as_bytes(),
                    b"some-reasonably-sized-value",
                )
                .unwrap();
        }
        assert!(wait_until(
            || engine.checkpoint_seq() > 0,
            Duration::from_secs(10)
        ));
        assert!(wait_until(
            || engine.immutable_count() == 0,
            Duration::from_secs(10)
        ));
        engine.shutdown();
    }

    let engine2 = LsmEngine::open(&dir, wal_config, small_pool_config(), lsm_config).unwrap();
    assert!(
        engine2.active_entry_count() < total_keys as usize,
        "replay must be bounded by the checkpoint, not replay the full history again \
         (active_entry_count={}, total_keys={total_keys})",
        engine2.active_entry_count()
    );
    for i in 0..total_keys {
        assert_eq!(
            engine2.get(format!("k{i:05}").as_bytes()).unwrap(),
            Some(b"some-reasonably-sized-value".to_vec()),
            "key k{i:05} must remain correctly readable after bounded-replay restart"
        );
    }
    engine2.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

/// `PHASE5_MANIFEST_ARCHITECTURE.md` §6: if the Manifest says an
/// SSTable is live but its file is physically missing, `open()` must
/// fail closed, never silently omit it from the live set.
#[test]
fn missing_live_sstable_fails_closed_on_open() {
    let dir = temp_dir("missing_live_sstable");
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: 100,
        max_immutable_memtables: 8,
        ..LsmConfig::default()
    };
    {
        let engine = open(&dir, lsm_config.clone());
        for i in 0..30u32 {
            engine.put(format!("k{i:03}").as_bytes(), b"v").unwrap();
        }
        assert!(wait_until(
            || engine.sstable_count() >= 1,
            Duration::from_secs(5)
        ));
        engine.shutdown();
    }

    let sstables_dir = dir.join("sstables");
    let mut deleted_any = false;
    for entry in fs::read_dir(&sstables_dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) == Some("sst") {
            fs::remove_file(&path).unwrap();
            deleted_any = true;
            break; // just the first one -- enough to prove the point
        }
    }
    assert!(deleted_any);

    let result = LsmEngine::open(&dir, test_wal_config(), small_pool_config(), lsm_config);
    assert!(
        matches!(result, Err(EngineError::Corruption { .. })),
        "open() must fail closed when the Manifest's live set references a missing file"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// An invalid file sitting at a never-before-used SSTable id must never
/// silently become live (it isn't valid) and must never be silently
/// ignored either (operating brief: "extra orphan SSTables... must not
/// become live merely because they are present on disk") -- `open()`
/// fails closed either way it might otherwise be mishandled.
#[test]
fn garbage_orphan_sstable_file_fails_closed_not_silently_handled() {
    let dir = temp_dir("garbage_orphan");
    let sstables_dir = dir.join("sstables");
    fs::create_dir_all(&sstables_dir).unwrap();
    fs::write(
        sstables_dir.join("00000000000000000099.sst"),
        b"not a real sstable",
    )
    .unwrap();

    let result = LsmEngine::open(
        &dir,
        test_wal_config(),
        small_pool_config(),
        LsmConfig::default(),
    );
    assert!(
        matches!(result, Err(EngineError::Corruption { .. })),
        "a garbage file at a never-acknowledged sstable id must fail closed, not be silently \
         adopted or silently skipped"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// A non-tail-corrupted `MANIFEST` file must fail `LsmEngine::open`
/// closed (`RUBIC_MANIFEST_FORMAT_SPECIFICATION.md` §4.1), never guess
/// at recovery.
#[test]
fn manifest_corruption_fails_closed_on_open() {
    let dir = temp_dir("manifest_corruption");
    {
        let engine = open(&dir, LsmConfig::default());
        engine.put(b"k1", b"v1").unwrap();
        engine.shutdown();
    }
    // No flush happened (large default memtable), so MANIFEST may not
    // exist yet -- create a minimally corrupt one directly to exercise
    // the corruption path deterministically.
    let manifest_path = dir.join("MANIFEST");
    fs::write(&manifest_path, [0xFFu8; 40]).unwrap(); // garbage: bad length/crc, not a clean torn tail

    let result = LsmEngine::open(
        &dir,
        test_wal_config(),
        small_pool_config(),
        LsmConfig::default(),
    );
    assert!(
        matches!(result, Err(EngineError::Corruption { .. })),
        "a corrupted (non-tail) MANIFEST must fail open() closed"
    );
    let _ = fs::remove_dir_all(&dir);
}

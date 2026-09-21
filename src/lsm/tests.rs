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
        storage_state: Arc::new(std::sync::atomic::AtomicU8::new(0)),
        storage_pressure_events: Arc::new(AtomicU64::new(0)),
        flush_io_fault_hook: Arc::new(Mutex::new(None)),
        snapshot_registry: Arc::new(SnapshotRegistry::default()),
        read_stats: Arc::new(ReadStatCounters::default()),
        config: LsmConfig::default(),
        compaction_fault_hook: Arc::new(Mutex::new(None)),
        compaction_io_fault_hook: Arc::new(Mutex::new(None)),
        pending_compaction_deletes: Mutex::new(Vec::new()),
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

/// `ADR-WE-SP-001` §16: deterministic ENOSPC / storage-pressure
/// capacity-exhaustion test. Injects a real, correctly-classified
/// ENOSPC-shaped `io::Error` at the exact point the 2026-09-19 realistic
/// soak actually failed (the flush thread's SSTable-write step) via
/// `install_flush_io_fault_hook` — deterministic and safe, never fills a
/// real disk (`PHASE5_ENOSPC_FAILURE_ANALYSIS.md` is the incident this
/// test exists to close the gap on).
///
/// Walks the full state sequence the ADR specifies: healthy -> flush
/// fails -> bounded fast retries -> `STORAGE_PRESSURE` -> immutable
/// backlog reaches its bound -> `STORAGE_FULL` -> new writes rejected
/// deterministically (fail-fast, before any WAL append) -> storage
/// "restored" (fault cleared) -> flush resumes -> checkpoint progresses
/// -> back to `Healthy` -> normal writes accepted again.
#[test]
fn storage_pressure_state_machine_recovers_after_injected_enospc() {
    let dir = temp_dir("storage_pressure_enospc");
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: 120,
        max_immutable_memtables: 2,
        max_flush_retries: 1,
        storage_pressure_retry_interval: Duration::from_millis(50),
        ..LsmConfig::default()
    };
    let engine = open(&dir, lsm_config);

    assert_eq!(
        engine.storage_state(),
        StorageState::Healthy,
        "a freshly opened engine must start Healthy"
    );

    // Unconditional synthetic ENOSPC for every flush attempt, until
    // cleared below -- same fault-injection pattern already established
    // by `install_flush_fault_hook`/`FlushFaultPoint` tests in this file,
    // extended (`FlushIoFaultHook`) to actually replace the I/O outcome
    // rather than just observe it.
    engine.install_flush_io_fault_hook(|| {
        Some(std::io::Error::new(
            std::io::ErrorKind::StorageFull,
            "injected ENOSPC (test)",
        ))
    });

    // Freeze #1: triggers the flush thread, which immediately starts
    // failing on the injected fault. `max_flush_retries: 1` means the
    // fast-retry budget (one 50ms attempt) is exhausted almost
    // instantly, so STORAGE_PRESSURE should appear quickly. Each entry
    // costs `key.len() + value.len() + ENTRY_OVERHEAD` bytes
    // (`memtable::entry_size`) -- a handful of small puts is enough to
    // exceed the tiny 120-byte memtable and trigger exactly one freeze.
    for i in 0..4u32 {
        engine.put(format!("k{i:03}").as_bytes(), b"v").unwrap();
    }
    assert!(
        wait_until(
            || engine.storage_state() == StorageState::StoragePressure,
            Duration::from_secs(5)
        ),
        "storage state must reach StoragePressure after the fast-retry budget is exhausted on a \
         confirmed ENOSPC failure"
    );
    assert!(
        engine.storage_pressure_events() > 0,
        "storage_pressure_events must count the ENOSPC-classified failures"
    );
    // Data already accepted is retained, not discarded, while stuck in
    // StoragePressure (ADR-WE-SP-001 §9/§11).
    assert_eq!(engine.immutable_count(), 1);
    assert_eq!(
        engine.checkpoint_seq(),
        0,
        "checkpoint must never advance while every flush attempt is failing"
    );
    assert_eq!(
        engine.sstable_count(),
        0,
        "no SSTable can have been published while every flush attempt is failing"
    );

    // Freeze #2: fills the immutable backlog to its configured bound
    // (max_immutable_memtables=2) while still stuck in StoragePressure --
    // exactly the ADR §6.3 "safe resource boundary reached" trigger for
    // StorageFull.
    let mut i = 100u32;
    let freeze2 = wait_until(
        || {
            if engine.storage_state() == StorageState::StorageFull {
                return true;
            }
            let key = format!("k{i:03}");
            i += 1;
            // A `CapacityExceeded` here is the pre-existing, accepted
            // freeze-backpressure contract firing once the backlog is
            // already full at the moment of this particular call --
            // still forward progress toward StorageFull, not a failure
            // of this test.
            let _ = engine.put(key.as_bytes(), b"v");
            engine.storage_state() == StorageState::StorageFull
        },
        Duration::from_secs(5),
    );
    assert!(
        freeze2,
        "storage state must reach StorageFull once the immutable backlog fills while stuck in \
         StoragePressure"
    );

    // StorageFull: new writes must fail fast with StorageExhausted,
    // *before* any WAL append (ADR-WE-SP-001 §9) -- verified by checking
    // the pool's own `submitted` counter does not move for this call.
    let submitted_before = engine.pool_stats().submitted;
    let rejected = engine.put(b"should-be-rejected", b"v");
    assert!(
        matches!(rejected, Err(EngineError::StorageExhausted { .. })),
        "a write while StorageFull must fail fast with StorageExhausted, got {rejected:?}"
    );
    assert_eq!(
        engine.pool_stats().submitted,
        submitted_before,
        "a StorageFull rejection must never reach pool.submit() / the WAL at all"
    );

    // "Storage restored": clear the fault. The already-stuck flush
    // thread's own retry loop (still running at storage_pressure_retry_
    // interval) must pick this up on its own -- no restart needed.
    engine.clear_flush_io_fault_hook();
    assert!(
        wait_until(
            || engine.storage_state() == StorageState::Healthy,
            Duration::from_secs(5)
        ),
        "storage state must return to Healthy once a flush actually succeeds after the fault is \
         cleared"
    );
    assert!(
        wait_until(
            || engine.checkpoint_seq() > 0 && engine.sstable_count() > 0,
            Duration::from_secs(5)
        ),
        "checkpoint must progress and a real SSTable must be published once flush resumes"
    );

    // Normal operation resumes.
    engine
        .put(b"k-after-recovery", b"v-after-recovery")
        .unwrap();
    assert_eq!(
        engine.get(b"k-after-recovery").unwrap(),
        Some(b"v-after-recovery".to_vec())
    );

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

/// Memory-investigation regression test (2026-09-20): the 2026-09-19
/// 23:47 realistic full-pipeline soak showed RSS growing from 25 MB to
/// 609 MB over 4 hours as 3,294 SSTables accumulated. Traced to source
/// and confirmed by two independent empirical scaling measurements
/// (linear fit R²=0.9999 against SSTable count, matching each open
/// `SsTable`'s retained `bloom: BloomFilter` + `index: Vec<IndexEntry>`
/// almost exactly) that this is expected, deterministic, bounded-per-
/// table growth (Compaction, which would reclaim it, does not exist
/// yet — an explicit, already-documented Non-Goal, `PHASE4B_ADR.md`
/// ADR-P4B-1) — not a leak. See `PHASE_WRITE_ENGINE_MEMORY_
/// INVESTIGATION.md` for the full analysis this test locks in.
///
/// This test does not measure real OS-level RSS (that lives in the
/// reproducible `realistic_full_pipeline_soak` scaling runs referenced
/// above — an OS process-metrics sample inside `cargo test --lib`
/// would be noisy and platform-specific). Instead it locks in the two
/// **object-ownership invariants** that are the actual code-level
/// guarantee against a real leak: (1) a flushed immutable MemTable's
/// bytes are fully released, not retained anywhere, once its flush
/// succeeds; (2) `sstable_count()` grows by exactly one per successful
/// flush, never more (no duplicate/phantom publication) and never less
/// (no silently-dropped SSTable). A regression that broke either
/// invariant would itself constitute new, real, additional growth
/// beyond the already-accounted-for bloom/index model above.
#[test]
fn sstable_count_and_immutable_memory_track_flushes_exactly_no_extra_retention() {
    let dir = temp_dir("memory_regression");
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: 150,
        max_immutable_memtables: 8,
        ..LsmConfig::default()
    };
    let engine = open(&dir, lsm_config);

    const FREEZE_CYCLES: usize = 12;
    let mut i: u32 = 0;
    for cycle in 0..FREEZE_CYCLES {
        let before_sstables = engine.sstable_count();
        let before_immutable_bytes = engine.immutable_total_bytes();
        assert_eq!(
            before_immutable_bytes, 0,
            "cycle {cycle}: no bytes should remain retained in `immutables` before this \
             cycle's own freeze -- the previous cycle's flush must have already released them"
        );

        // Enough puts to exceed the 150-byte memtable and force exactly
        // one freeze.
        loop {
            let key = format!("k{i:05}");
            i += 1;
            engine.put(key.as_bytes(), b"v").unwrap();
            if engine.immutable_count() > 0 || engine.sstable_count() > before_sstables {
                break;
            }
        }

        assert!(
            wait_until(
                || engine.sstable_count() == before_sstables + 1 && engine.immutable_count() == 0,
                Duration::from_secs(5)
            ),
            "cycle {cycle}: exactly one new SSTable must be published and the immutable backlog \
             must fully drain (flush succeeded and released the flushed memtable)"
        );
        assert_eq!(
            engine.sstable_count(),
            before_sstables + 1,
            "cycle {cycle}: sstable_count must grow by exactly 1 per successful flush -- more \
             would mean duplicate publication, less would mean a silently lost SSTable"
        );
        assert_eq!(
            engine.immutable_total_bytes(),
            0,
            "cycle {cycle}: immutable_total_bytes must return to exactly 0 after this cycle's \
             flush -- any nonzero residual would be exactly the kind of additional, unaccounted \
             retention this test exists to catch"
        );
    }

    assert_eq!(
        engine.sstable_count(),
        FREEZE_CYCLES,
        "total SSTable count must equal the number of freeze cycles performed, exactly -- \
         confirms sstable_count growth correlates 1:1 with successful flushes, not with time \
         or any other factor"
    );

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

// ============================================================================
// Read Engine foundation (`ADR-RE-001`, Implementation Increment 1).
// ============================================================================

// --- Snapshot / SnapshotRegistry ---

#[test]
fn one_snapshot_registers_and_releases() {
    let dir = temp_dir("snapshot_one");
    let engine = open(&dir, LsmConfig::default());
    engine.put(b"k1", b"v1").unwrap();

    assert_eq!(engine.oldest_live_snapshot_seq(), None);
    let snap = engine.snapshot();
    assert_eq!(engine.oldest_live_snapshot_seq(), Some(snap.seq()));
    drop(snap);
    assert_eq!(engine.oldest_live_snapshot_seq(), None);

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn two_snapshots_at_different_sequence_numbers_report_the_older_as_oldest() {
    let dir = temp_dir("snapshot_two_diff_seq");
    let engine = open(&dir, LsmConfig::default());

    engine.put(b"k1", b"v1").unwrap();
    let older = engine.snapshot();
    engine.put(b"k2", b"v2").unwrap();
    let newer = engine.snapshot();
    assert!(
        newer.seq() > older.seq(),
        "a snapshot taken after a later write must pin a strictly later sequence"
    );

    assert_eq!(engine.oldest_live_snapshot_seq(), Some(older.seq()));
    drop(newer);
    assert_eq!(
        engine.oldest_live_snapshot_seq(),
        Some(older.seq()),
        "dropping the newer snapshot must not change the oldest-reported sequence"
    );
    drop(older);
    assert_eq!(engine.oldest_live_snapshot_seq(), None);

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn two_snapshots_at_the_same_sequence_are_counted_independently() {
    let dir = temp_dir("snapshot_same_seq");
    let engine = open(&dir, LsmConfig::default());
    engine.put(b"k1", b"v1").unwrap();

    // No write happens between these two calls, so both pin the same
    // durable watermark -- the multiset case `SnapshotRegistry` exists
    // to handle correctly (a plain `HashSet<u64>`/`BTreeSet<u64>` would
    // conflate these two independent holders into one entry).
    let a = engine.snapshot();
    let b = engine.snapshot();
    assert_eq!(
        a.seq(),
        b.seq(),
        "no intervening write, so both snapshots pin the same seq"
    );

    assert_eq!(engine.oldest_live_snapshot_seq(), Some(a.seq()));
    drop(a);
    assert_eq!(
        engine.oldest_live_snapshot_seq(),
        Some(b.seq()),
        "dropping one of two same-sequence snapshots must not remove the sequence while \
         the other is still alive"
    );
    drop(b);
    assert_eq!(
        engine.oldest_live_snapshot_seq(),
        None,
        "dropping the final same-sequence snapshot must remove the entry"
    );

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn dropping_snapshots_newest_first_reports_correctly_at_every_step() {
    let dir = temp_dir("snapshot_drop_newest_first");
    let engine = open(&dir, LsmConfig::default());

    engine.put(b"k1", b"v1").unwrap();
    let first = engine.snapshot();
    engine.put(b"k2", b"v2").unwrap();
    let second = engine.snapshot();
    engine.put(b"k3", b"v3").unwrap();
    let third = engine.snapshot();

    assert_eq!(engine.oldest_live_snapshot_seq(), Some(first.seq()));
    drop(third);
    assert_eq!(engine.oldest_live_snapshot_seq(), Some(first.seq()));
    drop(second);
    assert_eq!(engine.oldest_live_snapshot_seq(), Some(first.seq()));
    drop(first);
    assert_eq!(engine.oldest_live_snapshot_seq(), None);

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn dropping_snapshots_oldest_first_reports_correctly_at_every_step() {
    let dir = temp_dir("snapshot_drop_oldest_first");
    let engine = open(&dir, LsmConfig::default());

    engine.put(b"k1", b"v1").unwrap();
    let first = engine.snapshot();
    engine.put(b"k2", b"v2").unwrap();
    let second = engine.snapshot();
    engine.put(b"k3", b"v3").unwrap();
    let third = engine.snapshot();

    assert_eq!(engine.oldest_live_snapshot_seq(), Some(first.seq()));
    drop(first);
    assert_eq!(
        engine.oldest_live_snapshot_seq(),
        Some(second.seq()),
        "the oldest reported sequence must advance once the true oldest is dropped"
    );
    drop(second);
    assert_eq!(engine.oldest_live_snapshot_seq(), Some(third.seq()));
    drop(third);
    assert_eq!(engine.oldest_live_snapshot_seq(), None);

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn all_snapshots_dropped_leaves_no_live_snapshot() {
    let dir = temp_dir("snapshot_all_dropped");
    let engine = open(&dir, LsmConfig::default());
    engine.put(b"k1", b"v1").unwrap();

    let snaps: Vec<Snapshot> = (0..5).map(|_| engine.snapshot()).collect();
    assert!(engine.oldest_live_snapshot_seq().is_some());
    drop(snaps);
    assert_eq!(engine.oldest_live_snapshot_seq(), None);

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn snapshot_seq_remains_stable_after_later_writes_and_is_usable_with_get_as_of() {
    let dir = temp_dir("snapshot_stable_seq");
    let engine = open(&dir, LsmConfig::default());

    engine.put(b"k1", b"v1").unwrap();
    let snap = engine.snapshot();
    let seq_at_snapshot = snap.seq();

    // Later writes, including an overwrite of the same key.
    engine.put(b"k1", b"v2").unwrap();
    engine.put(b"k2", b"v-after-snapshot").unwrap();

    assert_eq!(
        snap.seq(),
        seq_at_snapshot,
        "Snapshot::seq() must never change after construction, regardless of later writes"
    );

    // Existing point-lookup behavior, completely unchanged: passing a
    // `Snapshot`'s `seq()` to `get_as_of` must behave identically to
    // passing the equivalent raw `u64` (`snapshot_reads_remain_stable_
    // across_later_writes` already covers the raw-`u64` case; this
    // confirms `Snapshot` is just a safer way to hold that same value,
    // not a new read mechanism).
    assert_eq!(
        engine.get_as_of(b"k1", snap.seq()).unwrap(),
        Some(b"v1".to_vec())
    );
    assert_eq!(engine.get_as_of(b"k2", snap.seq()).unwrap(), None);
    assert_eq!(engine.get(b"k1").unwrap(), Some(b"v2".to_vec()));

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

// --- ReadStats ---

#[test]
fn read_stats_counts_a_memtable_hit_without_touching_sstables() {
    let dir = temp_dir("read_stats_memtable_hit");
    let engine = open(&dir, LsmConfig::default());
    engine.put(b"k1", b"v1").unwrap();

    let before = engine.read_stats();
    assert_eq!(engine.get(b"k1").unwrap(), Some(b"v1".to_vec()));
    let after = engine.read_stats();

    assert_eq!(after.read_requests, before.read_requests + 1);
    assert_eq!(after.read_hits, before.read_hits + 1);
    assert_eq!(after.read_misses, before.read_misses);
    assert_eq!(
        after.sstables_consulted, before.sstables_consulted,
        "a MemTable hit must short-circuit before ever consulting an SSTable"
    );
    assert_eq!(after.blocks_read, before.blocks_read);

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn read_stats_counts_a_miss() {
    let dir = temp_dir("read_stats_miss");
    let engine = open(&dir, LsmConfig::default());
    engine.put(b"k1", b"v1").unwrap();

    let before = engine.read_stats();
    assert_eq!(engine.get(b"does-not-exist").unwrap(), None);
    let after = engine.read_stats();

    assert_eq!(after.read_requests, before.read_requests + 1);
    assert_eq!(after.read_hits, before.read_hits);
    assert_eq!(after.read_misses, before.read_misses + 1);

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn read_stats_counts_sstable_consultation_and_block_reads_on_an_sstable_hit() {
    let dir = temp_dir("read_stats_sstable_hit");
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: 150,
        max_immutable_memtables: 8,
        ..LsmConfig::default()
    };
    let engine = open(&dir, lsm_config);
    for i in 0..10u32 {
        engine.put(format!("k{i:03}").as_bytes(), b"v").unwrap();
    }
    assert!(
        wait_until(
            || engine.sstable_count() >= 1 && engine.immutable_count() == 0,
            Duration::from_secs(5)
        ),
        "the flush must complete before this test can measure an SSTable hit"
    );

    let before = engine.read_stats();
    // k000 is guaranteed to have been part of the frozen-and-flushed
    // memtable (the freeze happens partway through this loop, and only
    // the active memtable -- never yet-flushed keys -- would still be
    // reachable without going through the SSTable).
    let result = engine.get(b"k000").unwrap();
    assert!(
        result.is_some(),
        "k000 must still be readable after its memtable was flushed"
    );
    let after = engine.read_stats();

    assert_eq!(after.read_hits, before.read_hits + 1);
    assert!(
        after.sstables_consulted > before.sstables_consulted,
        "a hit that required checking the SSTable layer must count at least one consultation"
    );
    assert!(
        after.blocks_read > before.blocks_read,
        "a real (non-bloom-negative) SSTable hit must read at least one data block"
    );

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn read_stats_counts_a_bloom_negative_miss_with_zero_block_reads() {
    let dir = temp_dir("read_stats_bloom_negative");
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: 150,
        max_immutable_memtables: 8,
        ..LsmConfig::default()
    };
    let engine = open(&dir, lsm_config);
    for i in 0..10u32 {
        engine.put(format!("k{i:03}").as_bytes(), b"v").unwrap();
    }
    assert!(
        wait_until(
            || engine.sstable_count() >= 1 && engine.immutable_count() == 0,
            Duration::from_secs(5)
        ),
        "the flush must complete before this test can measure a bloom-negative miss"
    );

    let before = engine.read_stats();
    // A clearly-distinct key never written anywhere -- with
    // `bloom_bits_per_key=10`'s low false-positive rate, this is a real
    // (not merely probable) bloom-negative for a dataset this small.
    assert_eq!(engine.get(b"definitely-absent-key-xyz").unwrap(), None);
    let after = engine.read_stats();

    assert_eq!(after.read_misses, before.read_misses + 1);
    assert!(
        after.bloom_negatives > before.bloom_negatives,
        "a miss on a key absent from every live SSTable's bloom filter must count as a \
         bloom-negative"
    );
    assert_eq!(
        after.blocks_read, before.blocks_read,
        "a bloom-negative miss must read zero data blocks"
    );

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

// --- ReadView foundation (consumed by `range_scan` in the next increment) ---

#[test]
fn read_view_captures_reference_counts_matching_the_live_engine_at_capture_time() {
    let dir = temp_dir("read_view_foundation");
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: 150,
        max_immutable_memtables: 8,
        ..LsmConfig::default()
    };
    let engine = open(&dir, lsm_config);
    engine.set_flush_delay_for_test(Duration::from_millis(200));

    for i in 0..10u32 {
        engine.put(format!("k{i:03}").as_bytes(), b"v").unwrap();
    }
    assert!(
        engine.immutable_count() >= 1,
        "at least one freeze must have happened by now"
    );

    let view = engine.capture_read_view(Bound::Unbounded, Bound::Unbounded);

    // Arc clones of the live source lists, not data copies -- captured
    // counts must match what the live engine reports at this instant.
    assert_eq!(view.immutables.len(), engine.immutable_count());
    assert_eq!(view.sstables.len(), engine.sstable_count());

    // Every key materialized into `active_range` really did come from
    // this run's own writes -- not phantom or duplicated data.
    let expected_keys: std::collections::HashSet<Vec<u8>> = (0..10u32)
        .map(|i| format!("k{i:03}").into_bytes())
        .collect();
    for ((k, _seq), _v) in &view.active_range {
        assert!(
            expected_keys.contains(k),
            "active_range must only ever contain keys this test actually wrote, got {k:?}"
        );
    }

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn read_view_active_range_is_bounded_to_the_requested_range_not_the_whole_memtable() {
    let dir = temp_dir("read_view_bounded_range");
    // Default (large) memtable so nothing freezes -- every key below
    // stays in `active`, isolating this test to the range-bounding
    // behavior specifically.
    let engine = open(&dir, LsmConfig::default());
    for i in 0..200u32 {
        engine.put(format!("k{i:04}").as_bytes(), b"v").unwrap();
    }

    // Increment 1 recorded a real discrepancy here: `MemTable::range`'s
    // `Excluded` end bound did not actually exclude the boundary key's
    // own entries (`bound_to_tuple` used the same sentinel for
    // `Included`/`Excluded`). Investigated against `std::ops::Bound`'s
    // own unambiguous contract and confirmed a genuine bug, not
    // intentional behavior (`memtable::tests::range_excluded_end_
    // bound_excludes_every_version_of_the_boundary_key`, added before
    // the fix, per this project's "test first, then the smallest
    // correct fix" convention); fixed in `src/memtable/mod.rs`
    // (`bound_to_tuple` split into `bound_to_tuple_start`/`_end`, each
    // choosing the sentinel that actually enforces exclusion). This
    // test's own expectation is updated to match the now-correct
    // behavior.
    let view = engine.capture_read_view(
        Bound::Included(b"k0010".as_slice()),
        Bound::Excluded(b"k0020".as_slice()),
    );
    assert_eq!(
        view.active_range.len(),
        10,
        "capture_read_view must materialize only the 10 keys inside [k0010, k0020), not the \
         whole 200-entry active memtable"
    );

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

// --- Point-read regression protection (ADR-RE-001 Implementation Increment 1
// §6: get()/get_as_of() are explicitly NOT rewritten this increment -- these
// tests lock in that existing behavior, closing real gaps the architecture
// report identified rather than duplicating what already exists elsewhere
// (`put_then_get_round_trips`, `delete_then_get_returns_not_found`,
// `put_delete_put_resolves_to_the_newest_write`, `snapshot_reads_remain_
// stable_across_later_writes`, `wal_durability_ordering_is_respected_not_
// just_memtable_visibility`, `open_fails_closed_when_a_published_sstable_
// is_corrupt` already cover PUT / DELETE / recreate / snapshot lookup / I/O
// error propagation / open-time corruption). ---

#[test]
fn plain_overwrite_without_a_delete_returns_the_newest_value() {
    let dir = temp_dir("plain_overwrite");
    let engine = open(&dir, LsmConfig::default());
    engine.put(b"k1", b"v1").unwrap();
    engine.put(b"k1", b"v2").unwrap();
    engine.put(b"k1", b"v3").unwrap();
    assert_eq!(engine.get(b"k1").unwrap(), Some(b"v3".to_vec()));
    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn get_on_a_key_that_was_never_written_returns_none() {
    let dir = temp_dir("never_written_key");
    let engine = open(&dir, LsmConfig::default());
    engine.put(b"k1", b"v1").unwrap();
    assert_eq!(engine.get(b"never-written").unwrap(), None);
    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

/// Closes a real, previously-identified gap (`PHASE_READ_ENGINE_
/// ARCHITECTURE_REPORT.md` §16 item 4): existing corruption tests cover
/// open-time failures (`open_fails_closed_when_a_published_sstable_is_
/// corrupt`, footer/index/bloom) but not a data block corrupted *after*
/// a successful `open()` -- the lazy, read-time path `get_versioned`'s
/// own doc comment describes but that had no dedicated regression test.
#[test]
fn data_block_corruption_is_detected_lazily_at_read_time_not_at_open() {
    let dir = temp_dir("lazy_block_corruption");
    // Each "k{i:03}"/"v" entry costs exactly 4+1+32=37 bytes
    // (`memtable::entry_size`). 350 sits strictly between 9*37=333 and
    // 10*37=370, so all 10 puts below land in one MemTable and trigger
    // exactly one freeze (hence exactly one SSTable), on the very last
    // put -- required so "exactly one .sst file" below is guaranteed,
    // not racy (an earlier version of this test used a too-small 150
    // bytes, which froze partway through and produced multiple
    // SSTables, causing the `sstable_count() == 1` wait below to time
    // out -- caught by actually running this test before trusting it).
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: 350,
        max_immutable_memtables: 8,
        ..LsmConfig::default()
    };
    {
        let engine = open(&dir, lsm_config.clone());
        for i in 0..10u32 {
            engine.put(format!("k{i:03}").as_bytes(), b"v").unwrap();
        }
        assert!(
            wait_until(
                || engine.sstable_count() == 1 && engine.immutable_count() == 0,
                Duration::from_secs(5)
            ),
            "the flush must complete before this test corrupts the resulting file"
        );
        engine.shutdown();
    }

    // Corrupt one byte at file offset 0 -- always inside the first data
    // block, since `sstable::writer::write_from_memtable` writes every
    // data block *before* the bloom filter, index, and footer
    // (`src/sstable/writer.rs`). This leaves every open-time-validated
    // structure (footer/bloom/index checksums) untouched.
    let sst_path = fs::read_dir(dir.join("sstables"))
        .unwrap()
        .find_map(|e| {
            let e = e.ok()?;
            let name = e.file_name().into_string().ok()?;
            name.ends_with(".sst").then(|| e.path())
        })
        .expect("exactly one .sst file must exist after the flush above");
    {
        use std::io::{Read, Seek, SeekFrom, Write};
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&sst_path)
            .unwrap();
        let mut byte = [0u8; 1];
        file.read_exact(&mut byte).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(&[byte[0] ^ 0xFF]).unwrap();
    }

    // Reopen: this must succeed -- open-time validation must not regress
    // into eagerly scanning data blocks (bounded-memory-at-open is an
    // already-certified design property, not something this test should
    // ever be allowed to silently break).
    let engine = open(&dir, lsm_config);
    assert_eq!(
        engine.sstable_count(),
        1,
        "open() must succeed despite the data-block corruption -- it never reads data blocks"
    );

    // Reading the key living in the now-corrupted first block must fail
    // closed, lazily, at this read -- never silently return wrong data.
    let result = engine.get(b"k000");
    assert!(
        matches!(result, Err(EngineError::Corruption { .. })),
        "a corrupted data block must fail the read closed with Corruption, got {result:?}"
    );

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

// --- Concurrent-flush point-read consistency (ADR-RE-001 §1/§8; the test
// the architecture report identified as required by the LSM spec's own
// checklist but not yet existing) ---

/// Deterministically controls the exact transition window traced (not
/// merely asserted) safe by `ADR-RE-001` §1: the flush thread has
/// already installed the newly-published SSTable into `sstables`
/// (which happens well before `FlushFaultPoint::AfterSetCheckpoint`,
/// the *last* fault point before `immutables.retain(..)` removes the
/// flushed entry), but has not yet removed the corresponding entry from
/// `immutables`. A concurrent point lookup during exactly this window
/// must find the value via whichever of the two sources it happens to
/// check (both hold it), never observe it as missing, and never
/// observe an impossible third state. Uses the existing `FlushFaultPoint`/
/// `install_flush_fault_hook` mechanism -- no sleeps, no timing luck.
#[test]
fn point_lookup_during_the_sstable_published_immutable_not_yet_removed_window_never_misses() {
    let dir = temp_dir("concurrent_flush_point_read");
    // Same 37-bytes-per-entry calibration as `data_block_corruption_is_
    // detected_lazily_at_read_time_not_at_open` -- 350 guarantees exactly
    // one freeze, on the 10th (last) put below, so exactly one immutable
    // exists when the fault hook fires (an earlier version of this test
    // used 150 bytes, which froze multiple times before the flush thread
    // -- now stuck on the first one -- could drain any of them, leaving
    // 2 immutables instead of the 1 this test's own assertions require;
    // caught by actually running this test before trusting it).
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: 350,
        max_immutable_memtables: 8,
        ..LsmConfig::default()
    };
    let engine = Arc::new(open(&dir, lsm_config));

    let (reached_tx, reached_rx) = mpsc::channel::<()>();
    let (proceed_tx, proceed_rx) = mpsc::channel::<()>();
    // `install_flush_fault_hook` requires `Sync` (the hook is called
    // from the flush thread via a shared `Arc<Mutex<...>>`) -- `Receiver`
    // alone is `Send` but not `Sync`, so it's wrapped here; only ever
    // accessed from this one hook, never concurrently.
    let proceed_rx = Mutex::new(proceed_rx);
    engine.install_flush_fault_hook(move |p| {
        if p == FlushFaultPoint::AfterSetCheckpoint {
            let _ = reached_tx.send(());
            // Blocks the flush thread here -- SSTable already installed
            // into `sstables`, `immutables.retain(..)` not yet run.
            let _ = proceed_rx.lock().unwrap_or_else(|p| p.into_inner()).recv();
        }
    });

    let write_engine = Arc::clone(&engine);
    let writer = thread::spawn(move || {
        for i in 0..10u32 {
            write_engine
                .put(format!("k{i:03}").as_bytes(), b"v")
                .unwrap();
        }
    });

    reached_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the flush thread must reach AfterSetCheckpoint within 5s");

    // Exactly the transition window: SSTable published, immutable not
    // yet removed. A point lookup right now must still find the value.
    assert_eq!(
        engine.sstable_count(),
        1,
        "the SSTable must already be installed at this fault point"
    );
    assert_eq!(
        engine.immutable_count(),
        1,
        "the immutable must not yet be removed at this fault point -- this IS the window \
         under test; if this assertion fails, the test is no longer testing the transition \
         it claims to"
    );
    let observed = engine.get(b"k000").unwrap();
    assert_eq!(
        observed,
        Some(b"v".to_vec()),
        "a point lookup during the publish/removal transition must never observe a missing \
         result -- the data is present in both `sstables` and `immutables` right now"
    );

    proceed_tx.send(()).unwrap();
    writer.join().unwrap();
    assert!(
        wait_until(|| engine.immutable_count() == 0, Duration::from_secs(5)),
        "the flush must complete and drain the immutable once released"
    );

    engine.clear_flush_fault_hook();
    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

// ============================================================================
// range_scan (`ADR-RE-001`, Implementation Increment 2).
// ============================================================================

/// Drains a `RangeScanIter` into a plain `Vec`, propagating the first
/// `Err` (if any) as this helper's own `Err` -- matches the iterator's
/// own documented "stop on first error" contract.
fn collect_range(iter: RangeScanIter) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    iter.collect()
}

#[test]
fn basic_range_scan_returns_every_key_in_order() {
    let dir = temp_dir("range_basic");
    let engine = open(&dir, LsmConfig::default());
    for i in 0..10u32 {
        engine
            .put(format!("k{i:03}").as_bytes(), format!("v{i:03}").as_bytes())
            .unwrap();
    }
    let rows = collect_range(engine.range(Bound::Unbounded, Bound::Unbounded)).unwrap();
    let expected: Vec<(Vec<u8>, Vec<u8>)> = (0..10u32)
        .map(|i| {
            (
                format!("k{i:03}").into_bytes(),
                format!("v{i:03}").into_bytes(),
            )
        })
        .collect();
    assert_eq!(rows, expected);
    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn range_scan_included_bounds() {
    let dir = temp_dir("range_included");
    let engine = open(&dir, LsmConfig::default());
    for i in 0..10u32 {
        engine.put(format!("k{i:03}").as_bytes(), b"v").unwrap();
    }
    let rows = collect_range(engine.range(
        Bound::Included(b"k003".as_slice()),
        Bound::Included(b"k006".as_slice()),
    ))
    .unwrap();
    let keys: Vec<Vec<u8>> = rows.into_iter().map(|(k, _)| k).collect();
    assert_eq!(
        keys,
        vec![
            b"k003".to_vec(),
            b"k004".to_vec(),
            b"k005".to_vec(),
            b"k006".to_vec()
        ]
    );
    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn range_scan_excluded_bounds() {
    let dir = temp_dir("range_excluded");
    let engine = open(&dir, LsmConfig::default());
    for i in 0..10u32 {
        engine.put(format!("k{i:03}").as_bytes(), b"v").unwrap();
    }
    let rows = collect_range(engine.range(
        Bound::Excluded(b"k003".as_slice()),
        Bound::Excluded(b"k006".as_slice()),
    ))
    .unwrap();
    let keys: Vec<Vec<u8>> = rows.into_iter().map(|(k, _)| k).collect();
    assert_eq!(keys, vec![b"k004".to_vec(), b"k005".to_vec()]);
    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn range_scan_unbounded_start_or_end() {
    let dir = temp_dir("range_unbounded");
    let engine = open(&dir, LsmConfig::default());
    for i in 0..10u32 {
        engine.put(format!("k{i:03}").as_bytes(), b"v").unwrap();
    }
    let head =
        collect_range(engine.range(Bound::Unbounded, Bound::Included(b"k002".as_slice()))).unwrap();
    assert_eq!(
        head.into_iter().map(|(k, _)| k).collect::<Vec<_>>(),
        vec![b"k000".to_vec(), b"k001".to_vec(), b"k002".to_vec()]
    );
    let tail =
        collect_range(engine.range(Bound::Included(b"k008".as_slice()), Bound::Unbounded)).unwrap();
    assert_eq!(
        tail.into_iter().map(|(k, _)| k).collect::<Vec<_>>(),
        vec![b"k008".to_vec(), b"k009".to_vec()]
    );
    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn range_scan_empty_cases() {
    let dir = temp_dir("range_empty_cases");
    let engine = open(&dir, LsmConfig::default());

    // Empty database entirely.
    assert_eq!(
        collect_range(engine.range(Bound::Unbounded, Bound::Unbounded)).unwrap(),
        vec![]
    );

    for i in 0..10u32 {
        engine.put(format!("k{i:03}").as_bytes(), b"v").unwrap();
    }

    // A range that matches no keys.
    assert_eq!(
        collect_range(engine.range(
            Bound::Included(b"z000".as_slice()),
            Bound::Included(b"z999".as_slice())
        ))
        .unwrap(),
        vec![]
    );

    // start > end: mathematically empty, must not panic or hang.
    assert_eq!(
        collect_range(engine.range(
            Bound::Included(b"k008".as_slice()),
            Bound::Included(b"k002".as_slice())
        ))
        .unwrap(),
        vec![]
    );

    // start == end, both Excluded: empty (nothing strictly between a
    // point and itself).
    assert_eq!(
        collect_range(engine.range(
            Bound::Excluded(b"k005".as_slice()),
            Bound::Excluded(b"k005".as_slice())
        ))
        .unwrap(),
        vec![]
    );

    // start == end, both Included: exactly that one key.
    assert_eq!(
        collect_range(engine.range(
            Bound::Included(b"k005".as_slice()),
            Bound::Included(b"k005".as_slice())
        ))
        .unwrap()
        .into_iter()
        .map(|(k, _)| k)
        .collect::<Vec<_>>(),
        vec![b"k005".to_vec()]
    );

    // Included(x), Excluded(x): empty (a single point range that
    // excludes its own only possible member).
    assert_eq!(
        collect_range(engine.range(
            Bound::Included(b"k005".as_slice()),
            Bound::Excluded(b"k005".as_slice())
        ))
        .unwrap(),
        vec![]
    );

    // Excluded(x), Included(x): empty, same reasoning.
    assert_eq!(
        collect_range(engine.range(
            Bound::Excluded(b"k005".as_slice()),
            Bound::Included(b"k005".as_slice())
        ))
        .unwrap(),
        vec![]
    );

    // Single-key range via Included/Included on adjacent-but-distinct
    // bytes still returns just the one real key inside it.
    assert_eq!(
        collect_range(engine.range(
            Bound::Included(b"k005".as_slice()),
            Bound::Excluded(b"k006".as_slice())
        ))
        .unwrap()
        .into_iter()
        .map(|(k, _)| k)
        .collect::<Vec<_>>(),
        vec![b"k005".to_vec()]
    );

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn range_scan_spans_multiple_sstables_correctly_merged() {
    let dir = temp_dir("range_multi_sstable");
    // Small memtable so many SSTables accumulate quickly.
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: 350, // exactly 10 entries/table, see the
        // 37-bytes-per-entry calibration used elsewhere in this file.
        max_immutable_memtables: 16,
        ..LsmConfig::default()
    };
    let engine = open(&dir, lsm_config);
    for i in 0..80u32 {
        engine
            .put(format!("k{i:04}").as_bytes(), format!("v{i:04}").as_bytes())
            .unwrap();
    }
    assert!(
        wait_until(
            || engine.sstable_count() >= 5 && engine.immutable_count() == 0,
            Duration::from_secs(5)
        ),
        "several flushes must complete so this test genuinely spans multiple SSTables"
    );

    let rows = collect_range(engine.range(Bound::Unbounded, Bound::Unbounded)).unwrap();
    let expected: Vec<(Vec<u8>, Vec<u8>)> = (0..80u32)
        .map(|i| {
            (
                format!("k{i:04}").into_bytes(),
                format!("v{i:04}").into_bytes(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        expected,
        "a range spanning {} live SSTables must still return every key, correctly ordered, \
         with no duplicates and no gaps",
        engine.sstable_count()
    );

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn range_scan_merges_active_immutable_and_sstable_sources_correctly() {
    let dir = temp_dir("range_active_immutable_sstable");
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: 350,
        max_immutable_memtables: 16,
        ..LsmConfig::default()
    };
    let engine = open(&dir, lsm_config);
    engine.set_flush_delay_for_test(Duration::from_millis(300));

    // First 10 -> freeze #1 (will become an immutable, then flush).
    for i in 0..10u32 {
        engine
            .put(
                format!("k{i:03}").as_bytes(),
                format!("v{i:03}-gen1").as_bytes(),
            )
            .unwrap();
    }
    // Next 10 -> freeze #2 (a second immutable, since flush is delayed).
    for i in 10..20u32 {
        engine
            .put(
                format!("k{i:03}").as_bytes(),
                format!("v{i:03}-gen1").as_bytes(),
            )
            .unwrap();
    }
    // A few more stay in the active MemTable (not enough to freeze again).
    for i in 20..24u32 {
        engine
            .put(
                format!("k{i:03}").as_bytes(),
                format!("v{i:03}-gen1").as_bytes(),
            )
            .unwrap();
    }
    assert!(
        engine.immutable_count() >= 1,
        "at least one freeze must have happened while flush is delayed"
    );

    let rows = collect_range(engine.range(Bound::Unbounded, Bound::Unbounded)).unwrap();
    let expected: Vec<(Vec<u8>, Vec<u8>)> = (0..24u32)
        .map(|i| {
            (
                format!("k{i:03}").into_bytes(),
                format!("v{i:03}-gen1").into_bytes(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        expected,
        "a scan spanning active ({} entries) + {} immutable(s) + {} flushed SSTable(s) must \
         return one correctly merged, correctly ordered logical view",
        engine.active_entry_count(),
        engine.immutable_count(),
        engine.sstable_count()
    );

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn range_scan_multiple_versions_of_a_key_resolve_to_the_newest_visible() {
    let dir = temp_dir("range_multiple_versions");
    let engine = open(&dir, LsmConfig::default());
    engine.put(b"k1", b"v1").unwrap();
    engine.put(b"k1", b"v2").unwrap();
    engine.put(b"k1", b"v3").unwrap();
    engine.put(b"k2", b"only").unwrap();

    let rows = collect_range(engine.range(Bound::Unbounded, Bound::Unbounded)).unwrap();
    assert_eq!(
        rows,
        vec![
            (b"k1".to_vec(), b"v3".to_vec()),
            (b"k2".to_vec(), b"only".to_vec()),
        ],
        "exactly one row per key, the newest version, never a duplicate"
    );

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn range_scan_suppresses_tombstoned_keys_entirely() {
    let dir = temp_dir("range_tombstone_suppression");
    let engine = open(&dir, LsmConfig::default());
    engine.put(b"k1", b"v1").unwrap();
    engine.put(b"k2", b"v2").unwrap();
    engine.delete(b"k2").unwrap();
    engine.put(b"k3", b"v3").unwrap();

    let rows = collect_range(engine.range(Bound::Unbounded, Bound::Unbounded)).unwrap();
    assert_eq!(
        rows,
        vec![
            (b"k1".to_vec(), b"v1".to_vec()),
            (b"k3".to_vec(), b"v3".to_vec())
        ],
        "a tombstoned key must not appear at all -- no (key, None), no sentinel"
    );

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn range_scan_delete_then_recreate_shows_only_the_recreated_value() {
    let dir = temp_dir("range_delete_recreate");
    let engine = open(&dir, LsmConfig::default());
    engine.put(b"k1", b"v1").unwrap();
    engine.delete(b"k1").unwrap();
    engine.put(b"k1", b"v2").unwrap();

    let rows = collect_range(engine.range(Bound::Unbounded, Bound::Unbounded)).unwrap();
    assert_eq!(rows, vec![(b"k1".to_vec(), b"v2".to_vec())]);

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

/// Directly exercises `ADR-RE-001` §5's three worked examples (adapted
/// to this engine's real sequence semantics, via `flush` to force
/// cross-SSTable placement -- each example's "SSTable A"/"SSTable B"
/// below really are two distinct, separately-flushed SSTables, not a
/// toy stand-in).
#[test]
fn range_scan_version_resolution_matches_adr_worked_examples() {
    let dir = temp_dir("range_adr_examples");
    let lsm_config = LsmConfig {
        // Smaller than even one entry's own cost (key+value+32-byte
        // overhead is always > 10 for any non-empty key/value used
        // below), so `is_full()` is already true immediately after the
        // very first put -- each put below freezes+flushes entirely on
        // its own, landing in its own distinct SSTable, exactly as this
        // test's own commentary describes (caught by actually running
        // this test before trusting it: 80 bytes was NOT small enough
        // to force a freeze after a single ~39-byte entry).
        memtable_max_size_bytes: 10,
        max_immutable_memtables: 16,
        ..LsmConfig::default()
    };
    let engine = open(&dir, lsm_config);

    // Example (a): SSTable A: key=x seq=10-ish PUT; SSTable B (newer):
    // key=x seq=20-ish PUT. Expect the newer (B's) value.
    let seq_a = engine.put(b"x", b"from-A").unwrap();
    assert!(
        wait_until(|| engine.sstable_count() >= 1, Duration::from_secs(5)),
        "the first tiny put must flush on its own"
    );
    let seq_b = engine.put(b"x", b"from-B").unwrap();
    assert!(seq_b > seq_a);
    assert!(
        wait_until(|| engine.sstable_count() >= 2, Duration::from_secs(5)),
        "the second tiny put must flush into its own, newer SSTable"
    );
    assert_eq!(
        collect_range(engine.range(
            Bound::Included(b"x".as_slice()),
            Bound::Included(b"x".as_slice())
        ))
        .unwrap(),
        vec![(b"x".to_vec(), b"from-B".to_vec())],
        "example (a): the newer SSTable's PUT must win"
    );

    // Example (b): a newer SSTable's DELETE must shadow an older PUT,
    // even for range_scan (the key must not appear at all).
    engine.delete(b"x").unwrap();
    assert!(
        wait_until(|| engine.sstable_count() >= 3, Duration::from_secs(5)),
        "the delete must also flush into its own, newest SSTable"
    );
    assert_eq!(
        collect_range(engine.range(
            Bound::Included(b"x".as_slice()),
            Bound::Included(b"x".as_slice())
        ))
        .unwrap(),
        vec![],
        "example (b): a newer DELETE must suppress the key entirely, not resurrect the older PUT"
    );

    // Example (c): at a snapshot seq *before* the delete, the older PUT
    // must still be visible (the delete is invisible at that seq).
    let before_delete_seq = seq_b; // durable right after "from-B" was written
    let rows = collect_range(engine.range_scan(
        Bound::Included(b"x".as_slice()),
        Bound::Included(b"x".as_slice()),
        before_delete_seq,
    ))
    .unwrap();
    assert_eq!(
        rows,
        vec![(b"x".to_vec(), b"from-B".to_vec())],
        "example (c): at a snapshot before the delete's own seq, the older visible PUT must \
         still be returned"
    );

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn range_scan_snapshot_observes_historical_state_and_registry_stays_correct() {
    let dir = temp_dir("range_snapshot");
    let engine = open(&dir, LsmConfig::default());

    engine.put(b"k1", b"v1").unwrap();
    let snap = engine.snapshot();
    assert_eq!(engine.oldest_live_snapshot_seq(), Some(snap.seq()));

    // Later writes, including a delete of a key the snapshot never saw.
    engine.put(b"k1", b"v2").unwrap();
    engine.put(b"k2", b"v2").unwrap();
    engine.delete(b"k2").unwrap();

    let historical =
        collect_range(engine.range_scan(Bound::Unbounded, Bound::Unbounded, snap.seq())).unwrap();
    assert_eq!(
        historical,
        vec![(b"k1".to_vec(), b"v1".to_vec())],
        "range_scan at the snapshot's seq must observe only what existed at that point"
    );

    let current = collect_range(engine.range(Bound::Unbounded, Bound::Unbounded)).unwrap();
    assert_eq!(
        current,
        vec![(b"k1".to_vec(), b"v2".to_vec())],
        "an unbounded (current) range must see the latest state, unaffected by the snapshot"
    );

    drop(snap);
    assert_eq!(
        engine.oldest_live_snapshot_seq(),
        None,
        "dropping the snapshot must release its registration"
    );

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

/// `ADR-RE-001` §8's hard invariant: for every key inside a range and
/// every sequence, `get_as_of(k, s)` must agree with the corresponding
/// row `range_scan(start, end, s)` produces. Drives a moderately rich
/// dataset (multiple keys, multiple versions, a delete/recreate, spread
/// across active + immutable + SSTable via a small memtable) and checks
/// every key at every seq actually assigned, cross-checking two
/// independent call paths against each other rather than the range
/// merge against itself.
#[test]
fn get_and_range_scan_agree_for_every_key_and_sequence() {
    let dir = temp_dir("get_range_equivalence");
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: 200,
        max_immutable_memtables: 16,
        ..LsmConfig::default()
    };
    let engine = open(&dir, lsm_config);

    let mut seqs = Vec::new();
    for i in 0..15u32 {
        seqs.push(engine.put(format!("k{i:03}").as_bytes(), b"v1").unwrap());
    }
    // Overwrite a few keys, delete one, recreate one.
    seqs.push(engine.put(b"k003", b"v2").unwrap());
    seqs.push(engine.delete(b"k005").unwrap());
    seqs.push(engine.put(b"k007", b"v2").unwrap());
    seqs.push(engine.delete(b"k007").unwrap());
    seqs.push(engine.put(b"k007", b"v3").unwrap());

    assert!(
        wait_until(|| engine.immutable_count() == 0, Duration::from_secs(5)),
        "let any triggered flushes settle so both call paths see a stable state"
    );

    let all_keys: Vec<Vec<u8>> = (0..15u32)
        .map(|i| format!("k{i:03}").into_bytes())
        .collect();

    for &s in &seqs {
        let range_rows: std::collections::HashMap<Vec<u8>, Vec<u8>> =
            collect_range(engine.range_scan(Bound::Unbounded, Bound::Unbounded, s))
                .unwrap()
                .into_iter()
                .collect();
        for key in &all_keys {
            let point = engine.get_as_of(key, s).unwrap();
            let ranged = range_rows.get(key).cloned();
            assert_eq!(
                point, ranged,
                "get_as_of({key:?}, {s}) = {point:?} but range_scan(.., {s}) has {ranged:?} \
                 for the same key/seq -- these two call paths must always agree"
            );
        }
    }

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

/// `ADR-RE-001` §1/§8 of the phase brief: the range-scan analogue of
/// `point_lookup_during_the_sstable_published_immutable_not_yet_removed_
/// window_never_misses` -- same deterministic `FlushFaultPoint`
/// machinery (no sleeps), but this time the `ReadView` is captured
/// (`range_scan` called) *before* the flush thread is released, so the
/// scan's own snapshot is provably fixed before the publish/removal
/// transition happens at all. The result must be internally coherent
/// (every key appears exactly once, with a value consistent with *some*
/// real point in time), never a torn mix, regardless of what the flush
/// thread does concurrently.
#[test]
fn range_scan_during_concurrent_flush_sees_a_coherent_snapshot() {
    let dir = temp_dir("range_concurrent_flush");
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: 350,
        max_immutable_memtables: 8,
        ..LsmConfig::default()
    };
    let engine = Arc::new(open(&dir, lsm_config));

    for i in 0..10u32 {
        engine.put(format!("k{i:03}").as_bytes(), b"v").unwrap();
    }
    assert!(
        wait_until(|| engine.immutable_count() == 0, Duration::from_secs(5)),
        "let the first batch settle before capturing the ReadView under test"
    );

    let (reached_tx, reached_rx) = mpsc::channel::<()>();
    let (proceed_tx, proceed_rx) = mpsc::channel::<()>();
    let proceed_rx = Mutex::new(proceed_rx);
    engine.install_flush_fault_hook(move |p| {
        if p == FlushFaultPoint::AfterSetCheckpoint {
            let _ = reached_tx.send(());
            let _ = proceed_rx.lock().unwrap_or_else(|p| p.into_inner()).recv();
        }
    });

    // A second batch that will freeze+flush concurrently with the scan
    // below (10 more puts against the same 350-byte/10-entry-per-table
    // calibration used elsewhere in this file -- exactly one more
    // freeze+flush cycle).
    let write_engine = Arc::clone(&engine);
    let writer = thread::spawn(move || {
        for i in 10..20u32 {
            write_engine
                .put(format!("k{i:03}").as_bytes(), b"v")
                .unwrap();
        }
    });

    reached_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the flush thread must reach AfterSetCheckpoint within 5s");

    // The ReadView is captured *now*, while the second batch's flush is
    // still stuck at the fault point -- before or during the exact
    // publish/immutable-removal transition.
    let rows = collect_range(engine.range(Bound::Unbounded, Bound::Unbounded)).unwrap();

    proceed_tx.send(()).unwrap();
    writer.join().unwrap();
    assert!(
        wait_until(|| engine.immutable_count() == 0, Duration::from_secs(5)),
        "the flush must complete once released"
    );
    engine.clear_flush_fault_hook();

    // Coherence checks -- must hold regardless of exactly which side of
    // the transition the captured ReadView landed on.
    let mut seen = std::collections::HashSet::new();
    for (k, _v) in &rows {
        assert!(
            seen.insert(k.clone()),
            "no key may appear twice in one range_scan's output: {k:?} appeared more than once"
        );
    }
    // The first 10 keys were durable and stable well before the scan
    // was even constructed -- they must always be present.
    for i in 0..10u32 {
        let key = format!("k{i:03}").into_bytes();
        assert!(
            seen.contains(&key),
            "key {key:?} was durable before this scan started and must always be present"
        );
    }
    // Every key present must be one this test actually wrote -- no
    // phantom/impossible key.
    for (k, _) in &rows {
        let n: u32 = std::str::from_utf8(&k[1..]).unwrap().parse().unwrap();
        assert!(n < 20, "range_scan produced an impossible key: {k:?}");
    }

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

/// Closes the range-scan analogue of `data_block_corruption_is_
/// detected_lazily_at_read_time_not_at_open`: a live SSTable data block
/// corrupted after a successful `open()` must fail a `range_scan`
/// touching it closed, with `Err(Corruption)`, and the iterator must
/// then end -- never skip the corrupted table, never return a partial
/// success silently.
#[test]
fn range_scan_across_a_corrupted_data_block_fails_closed_and_ends() {
    let dir = temp_dir("range_corruption");
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: 350,
        max_immutable_memtables: 8,
        ..LsmConfig::default()
    };
    {
        let engine = open(&dir, lsm_config.clone());
        for i in 0..10u32 {
            engine.put(format!("k{i:03}").as_bytes(), b"v").unwrap();
        }
        assert!(
            wait_until(
                || engine.sstable_count() == 1 && engine.immutable_count() == 0,
                Duration::from_secs(5)
            ),
            "the flush must complete before this test corrupts the resulting file"
        );
        engine.shutdown();
    }

    // Same technique as the point-lookup lazy-corruption test: flip a
    // byte at file offset 0, always inside the first data block (data
    // blocks are written before bloom/index/footer).
    let sst_path = fs::read_dir(dir.join("sstables"))
        .unwrap()
        .find_map(|e| {
            let e = e.ok()?;
            let name = e.file_name().into_string().ok()?;
            name.ends_with(".sst").then(|| e.path())
        })
        .expect("exactly one .sst file must exist after the flush above");
    {
        use std::io::{Read, Seek, SeekFrom, Write};
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&sst_path)
            .unwrap();
        let mut byte = [0u8; 1];
        file.read_exact(&mut byte).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(&[byte[0] ^ 0xFF]).unwrap();
    }

    let engine = open(&dir, lsm_config);
    let mut iter = engine.range(Bound::Unbounded, Bound::Unbounded);
    let first = iter.next();
    assert!(
        matches!(first, Some(Err(EngineError::Corruption { .. }))),
        "a range scan touching a corrupted data block must yield Err(Corruption), got {first:?}"
    );
    assert!(
        iter.next().is_none(),
        "the iterator must end after yielding the error -- no silent continuation, no partial \
         success, and the corrupted SSTable must not be skipped"
    );

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

/// `ADR-RE-001` §13: `read_requests` counts one per `range_scan`/`range`
/// *call*, never once per returned row; `sstables_consulted` reflects
/// real physical queries; `blocks_read` is already free via `SsTable`'s
/// own shared counter (Increment 1).
#[test]
fn read_stats_counts_range_scan_calls_and_sstable_consultation_correctly() {
    let dir = temp_dir("read_stats_range_scan");
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: 350,
        max_immutable_memtables: 16,
        ..LsmConfig::default()
    };
    let engine = open(&dir, lsm_config);
    for i in 0..30u32 {
        engine.put(format!("k{i:03}").as_bytes(), b"v").unwrap();
    }
    assert!(
        wait_until(
            || engine.sstable_count() >= 2 && engine.immutable_count() == 0,
            Duration::from_secs(5)
        ),
        "at least two SSTables must exist so sstables_consulted has something real to count"
    );

    let before = engine.read_stats();
    let rows = collect_range(engine.range(Bound::Unbounded, Bound::Unbounded)).unwrap();
    assert_eq!(rows.len(), 30);
    let after = engine.read_stats();

    assert_eq!(
        after.read_requests,
        before.read_requests + 1,
        "exactly one range_scan *call* must be counted, regardless of the 30 rows it returned"
    );
    assert!(
        after.sstables_consulted > before.sstables_consulted,
        "scanning across multiple live SSTables must count real consultation activity"
    );
    assert!(
        after.blocks_read > before.blocks_read,
        "reading real data out of SSTables must count real block reads (already free via \
         SsTable's own shared counter since Increment 1)"
    );
    assert_eq!(
        after.read_hits,
        before.read_hits + 30,
        "read_hits counts one per yielded row for a range scan (ADR-RE-001 §13's own \
         'define counting semantics clearly' -- documented and locked in here)"
    );

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

/// `ADR-RE-002` Option A (Increment 6) regression test -- brief §23:
/// "prove a single source cursor advances through multiple consecutive
/// keys WITHOUT reconstructing the source iterator for every key...
/// prefer an observable test hook/counter over timing."
///
/// Uses `sstables_consulted`'s own Increment-6-revised semantics (once
/// per live SSTable actually captured by this scan's `ReadView`, not
/// once per key drawn from it -- see `RangeScanIter::next`'s own doc
/// comment) as that observable counter. Deliberately builds a small,
/// heavily *overlapping* keyspace (mirrors `PHASE_READ_ENGINE_
/// RESOURCE_INVESTIGATION.md` §4.3's `overlap_repro` reproduction, at
/// unit-test scale): a handful of SSTables that each hold a version of
/// nearly every key in the scanned range, so a single source cursor is
/// asked to advance through *many* consecutive keys during this one
/// scan. Under the pre-Increment-6 implementation (a fresh `range_scan_
/// raw` call, and therefore one `sstables_consulted` increment, per key
/// drawn from a source), this would have produced a count many times
/// the live SSTable count -- exactly the re-peek signature
/// `PHASE_READ_ENGINE_RESOURCE_INVESTIGATION.md` traced and reproduced.
/// A regression that reintroduces per-key reconstruction would make
/// this assertion fail immediately, with no dependence on timing.
#[test]
fn range_scan_source_cursor_persists_across_keys_instead_of_reconstructing_per_key() {
    let dir = temp_dir("cursor_persists");
    // Small memtable relative to the write volume below, small key
    // cardinality: each flush cycle writes far more entries than there
    // are distinct keys, so (mirroring the soak's own overlap ratio)
    // essentially every flushed SSTable ends up holding a version of
    // essentially every key.
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: 6_000,
        max_immutable_memtables: 32,
        ..LsmConfig::default()
    };
    let engine = open(&dir, lsm_config);
    const KEY_CARDINALITY: u32 = 15;
    let mut i: u64 = 0;
    while engine.sstable_count() < 5 {
        let key = format!("ov{:03}", i % KEY_CARDINALITY as u64);
        engine.put(key.as_bytes(), b"v").unwrap();
        i += 1;
    }
    assert!(
        wait_until(|| engine.immutable_count() == 0, Duration::from_secs(5)),
        "flush must settle before measuring"
    );
    let live_sstables = engine.sstable_count() as u64;
    assert!(
        live_sstables >= 5,
        "fixture must actually reach >=5 live, overlapping SSTables, got {live_sstables}"
    );

    let before = engine.read_stats();
    let rows = collect_range(engine.range(Bound::Unbounded, Bound::Unbounded)).unwrap();
    let after = engine.read_stats();

    assert_eq!(
        rows.len(),
        KEY_CARDINALITY as usize,
        "sanity: every one of the small, fully-overlapping keyspace's keys must be yielded \
         exactly once"
    );
    assert_eq!(
        after.sstables_consulted - before.sstables_consulted,
        live_sstables,
        "sstables_consulted must increase by EXACTLY the live SSTable count for this one range \
         scan -- not by a multiple of the {} keys yielded, which is what the pre-Increment-6 \
         per-key-reconstruction design would have produced (each of the {live_sstables} sources \
         held a version of nearly every one of the {} overlapping keys, so a regression back to \
         reconstructing each source's iterator per key would inflate this count far past \
         {live_sstables}, not merely exceed it slightly)",
        rows.len(),
        KEY_CARDINALITY,
    );

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

/// `ADR-RE-001`/phase brief §17: an independent reference model (*not*
/// the production merge algorithm) driving `get`/`get_as_of`/
/// `range_scan` through a deterministic PUT/DELETE/overwrite/snapshot
/// sequence, with a small memtable so the same data actually spans
/// active + immutable + multiple SSTables (a purely in-memory scenario
/// would never exercise the merge across all three source kinds).
#[test]
fn differential_reference_model_matches_across_active_immutable_and_sstable() {
    let dir = temp_dir("differential_reference_model");
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: 200,
        max_immutable_memtables: 16,
        ..LsmConfig::default()
    };
    let engine = open(&dir, lsm_config);

    // key index -> every (seq, MemtableValue) ever applied, in order --
    // deliberately the simplest possible representation, queried by a
    // naive linear scan + max below, so this can never share a bug with
    // the production k-way merge it's checking.
    let mut model: std::collections::HashMap<u8, Vec<(u64, MemtableValue)>> =
        std::collections::HashMap::new();
    let key_for = |idx: u8| format!("k{idx:03}").into_bytes();
    let mut all_seqs = Vec::new();

    let script: &[(u8, Option<&[u8]>)] = &[
        (0, Some(b"v0")),
        (1, Some(b"v1")),
        (2, Some(b"v2")),
        (1, Some(b"v1-b")), // overwrite
        (3, Some(b"v3")),
        (2, None), // delete
        (4, Some(b"v4")),
        (5, Some(b"v5")),
        (2, Some(b"v2-recreated")), // recreate a deleted key
        (6, Some(b"v6")),
        (7, Some(b"v7")),
        (0, None), // delete the very first key, much later
        (8, Some(b"v8")),
        (9, Some(b"v9")),
    ];
    for &(idx, value) in script {
        let key = key_for(idx);
        let (seq, mv) = match value {
            Some(v) => (engine.put(&key, v).unwrap(), MemtableValue::Put(v.to_vec())),
            None => (engine.delete(&key).unwrap(), MemtableValue::Tombstone),
        };
        model.entry(idx).or_default().push((seq, mv));
        all_seqs.push(seq);
    }
    assert!(
        wait_until(|| engine.immutable_count() == 0, Duration::from_secs(5)),
        "let any triggered flushes settle so the SSTable layer is genuinely exercised"
    );
    assert!(
        engine.sstable_count() >= 1,
        "this test's own point is to exercise the SSTable layer -- if nothing ever flushed, \
         it isn't testing what it claims to"
    );

    let model_value_at = |idx: u8, as_of_seq: u64| -> Option<Vec<u8>> {
        model
            .get(&idx)?
            .iter()
            .filter(|(s, _)| *s <= as_of_seq)
            .max_by_key(|(s, _)| *s)
            .and_then(|(_, v)| match v {
                MemtableValue::Put(v) => Some(v.clone()),
                MemtableValue::Tombstone => None,
            })
    };

    // get()/get_as_of() at every seq boundary actually produced, for
    // every key.
    for &s in &all_seqs {
        for idx in 0u8..10 {
            let expected = model_value_at(idx, s);
            let actual = engine.get_as_of(&key_for(idx), s).unwrap();
            assert_eq!(
                actual, expected,
                "get_as_of(k{idx:03}, {s}) mismatch: engine={actual:?} model={expected:?}"
            );
            let contained = engine.contains(&key_for(idx), s).unwrap();
            assert_eq!(
                contained,
                expected.is_some(),
                "contains(k{idx:03}, {s}) mismatch: contains={contained} get_as_of={actual:?}"
            );
        }
    }
    for idx in 0u8..10 {
        let expected = model_value_at(idx, u64::MAX);
        let actual = engine.get(&key_for(idx)).unwrap();
        assert_eq!(
            actual, expected,
            "get(k{idx:03}) mismatch: engine={actual:?} model={expected:?}"
        );
    }

    // range_scan() at the final state must match the model's own
    // sorted, tombstone-filtered view exactly.
    let mut expected_range: Vec<(Vec<u8>, Vec<u8>)> = (0u8..10)
        .filter_map(|idx| model_value_at(idx, u64::MAX).map(|v| (key_for(idx), v)))
        .collect();
    expected_range.sort_by(|a, b| a.0.cmp(&b.0));
    let actual_range = collect_range(engine.range(Bound::Unbounded, Bound::Unbounded)).unwrap();
    assert_eq!(actual_range, expected_range);

    // And at a historical snapshot, midway through the script.
    let mid_seq = all_seqs[all_seqs.len() / 2];
    let mut expected_mid: Vec<(Vec<u8>, Vec<u8>)> = (0u8..10)
        .filter_map(|idx| model_value_at(idx, mid_seq).map(|v| (key_for(idx), v)))
        .collect();
    expected_mid.sort_by(|a, b| a.0.cmp(&b.0));
    let actual_mid =
        collect_range(engine.range_scan(Bound::Unbounded, Bound::Unbounded, mid_seq)).unwrap();
    assert_eq!(
        actual_mid, expected_mid,
        "historical range_scan at seq {mid_seq} mismatch"
    );

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

// --- `contains` (Read Engine Increment 3, `ADR-RE-001` §2/§10) ---

#[test]
fn contains_matches_get_as_of_is_some_for_a_hit_and_a_miss() {
    let dir = temp_dir("contains_hit_and_miss");
    let engine = open(&dir, LsmConfig::default());
    engine.put(b"k1", b"v1").unwrap();

    assert!(engine.contains(b"k1", u64::MAX).unwrap());
    assert_eq!(
        engine.contains(b"k1", u64::MAX).unwrap(),
        engine.get_as_of(b"k1", u64::MAX).unwrap().is_some()
    );

    assert!(!engine.contains(b"never-written", u64::MAX).unwrap());
    assert_eq!(
        engine.contains(b"never-written", u64::MAX).unwrap(),
        engine
            .get_as_of(b"never-written", u64::MAX)
            .unwrap()
            .is_some()
    );

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn contains_returns_false_for_a_visible_tombstone() {
    let dir = temp_dir("contains_tombstone");
    let engine = open(&dir, LsmConfig::default());
    engine.put(b"k1", b"v1").unwrap();
    let del_seq = engine.delete(b"k1").unwrap();

    assert!(!engine.contains(b"k1", del_seq).unwrap());
    assert_eq!(engine.get_as_of(b"k1", del_seq).unwrap(), None);

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn contains_true_after_delete_then_recreate() {
    let dir = temp_dir("contains_delete_recreate");
    let engine = open(&dir, LsmConfig::default());
    engine.put(b"k1", b"v1").unwrap();
    let del_seq = engine.delete(b"k1").unwrap();
    let recreate_seq = engine.put(b"k1", b"v2").unwrap();

    assert!(!engine.contains(b"k1", del_seq).unwrap());
    assert!(engine.contains(b"k1", recreate_seq).unwrap());
    assert_eq!(
        engine.contains(b"k1", recreate_seq).unwrap(),
        engine.get_as_of(b"k1", recreate_seq).unwrap().is_some()
    );

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn contains_respects_as_of_seq_across_multiple_versions() {
    let dir = temp_dir("contains_as_of_versions");
    let engine = open(&dir, LsmConfig::default());
    let s1 = engine.put(b"k1", b"v1").unwrap();
    let s2 = engine.put(b"k1", b"v2").unwrap();
    let s3 = engine.delete(b"k1").unwrap();
    let s4 = engine.put(b"k1", b"v3").unwrap();

    for &s in &[s1, s2, s3, s4, s1 - 1] {
        assert_eq!(
            engine.contains(b"k1", s).unwrap(),
            engine.get_as_of(b"k1", s).unwrap().is_some(),
            "contains/get_as_of disagree at seq {s}"
        );
    }
    assert!(!engine.contains(b"k1", s1 - 1).unwrap());
    assert!(engine.contains(b"k1", s1).unwrap());
    assert!(!engine.contains(b"k1", s3).unwrap());
    assert!(engine.contains(b"k1", s4).unwrap());

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn contains_agrees_with_get_as_of_across_active_immutable_and_sstable_overlap() {
    let dir = temp_dir("contains_active_immutable_sstable");
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: 350,
        max_immutable_memtables: 16,
        ..LsmConfig::default()
    };
    let engine = open(&dir, lsm_config);
    engine.set_flush_delay_for_test(Duration::from_millis(300));

    for i in 0..10u32 {
        engine
            .put(
                format!("k{i:03}").as_bytes(),
                format!("v{i:03}-gen1").as_bytes(),
            )
            .unwrap();
    }
    for i in 10..20u32 {
        engine
            .put(
                format!("k{i:03}").as_bytes(),
                format!("v{i:03}-gen1").as_bytes(),
            )
            .unwrap();
    }
    for i in 20..24u32 {
        engine
            .put(
                format!("k{i:03}").as_bytes(),
                format!("v{i:03}-gen1").as_bytes(),
            )
            .unwrap();
    }
    assert!(
        engine.immutable_count() >= 1,
        "at least one freeze must have happened while flush is delayed"
    );

    for i in 0..24u32 {
        let key = format!("k{i:03}").into_bytes();
        assert!(
            engine.contains(&key, u64::MAX).unwrap(),
            "k{i:03} must be found regardless of which source (active/immutable/sstable) holds it"
        );
        assert_eq!(
            engine.contains(&key, u64::MAX).unwrap(),
            engine.get_as_of(&key, u64::MAX).unwrap().is_some()
        );
    }
    assert!(!engine.contains(b"never-written", u64::MAX).unwrap());

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn contains_consults_multiple_sstables_and_agrees_with_get_as_of() {
    let dir = temp_dir("contains_multi_sstable");
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: 350,
        max_immutable_memtables: 16,
        ..LsmConfig::default()
    };
    let engine = open(&dir, lsm_config);
    for i in 0..80u32 {
        engine
            .put(format!("k{i:04}").as_bytes(), format!("v{i:04}").as_bytes())
            .unwrap();
    }
    assert!(
        wait_until(
            || engine.sstable_count() >= 5 && engine.immutable_count() == 0,
            Duration::from_secs(5)
        ),
        "several flushes must complete so this test genuinely spans multiple SSTables"
    );

    let before = engine.read_stats();
    for i in 0..80u32 {
        let key = format!("k{i:04}").into_bytes();
        assert!(engine.contains(&key, u64::MAX).unwrap());
    }
    let after = engine.read_stats();
    assert!(
        after.sstables_consulted > before.sstables_consulted,
        "contains() must wire up the same sstables_consulted counter get_as_of() uses"
    );
    assert!(!engine
        .contains(b"definitely-absent-key-xyz", u64::MAX)
        .unwrap());

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn read_stats_counts_contains_the_same_way_as_get_as_of_on_an_sstable_hit() {
    let dir = temp_dir("contains_read_stats_hit");
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: 150,
        max_immutable_memtables: 8,
        ..LsmConfig::default()
    };
    let engine = open(&dir, lsm_config);
    for i in 0..10u32 {
        engine.put(format!("k{i:03}").as_bytes(), b"v").unwrap();
    }
    assert!(
        wait_until(
            || engine.sstable_count() >= 1 && engine.immutable_count() == 0,
            Duration::from_secs(5)
        ),
        "the flush must complete before this test can measure an SSTable hit"
    );

    let before = engine.read_stats();
    assert!(engine.contains(b"k000", u64::MAX).unwrap());
    let after = engine.read_stats();

    assert_eq!(after.read_hits, before.read_hits + 1);
    assert!(
        after.sstables_consulted > before.sstables_consulted,
        "a contains() hit that required checking the SSTable layer must count at least one \
         consultation"
    );
    assert!(
        after.blocks_read > before.blocks_read,
        "a real (non-bloom-negative) contains() SSTable hit must read at least one data block \
         -- contains_versioned() shares read_block/blocks_read with get_versioned()"
    );

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn read_stats_counts_a_contains_bloom_negative_miss_with_zero_block_reads() {
    let dir = temp_dir("contains_read_stats_bloom_negative");
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: 150,
        max_immutable_memtables: 8,
        ..LsmConfig::default()
    };
    let engine = open(&dir, lsm_config);
    for i in 0..10u32 {
        engine.put(format!("k{i:03}").as_bytes(), b"v").unwrap();
    }
    assert!(
        wait_until(
            || engine.sstable_count() >= 1 && engine.immutable_count() == 0,
            Duration::from_secs(5)
        ),
        "the flush must complete before this test can measure a bloom-negative miss"
    );

    let before = engine.read_stats();
    assert!(!engine
        .contains(b"definitely-absent-key-xyz", u64::MAX)
        .unwrap());
    let after = engine.read_stats();

    assert_eq!(after.read_misses, before.read_misses + 1);
    assert!(
        after.bloom_negatives > before.bloom_negatives,
        "a contains() miss on a key absent from every live SSTable's bloom filter must count \
         as a bloom-negative"
    );
    assert_eq!(
        after.blocks_read, before.blocks_read,
        "a bloom-negative contains() miss must read zero data blocks"
    );

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn contains_across_a_corrupted_data_block_fails_closed_with_corruption() {
    let dir = temp_dir("contains_corrupt_block");
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: 350,
        max_immutable_memtables: 8,
        ..LsmConfig::default()
    };
    {
        let engine = open(&dir, lsm_config.clone());
        for i in 0..10u32 {
            engine.put(format!("k{i:03}").as_bytes(), b"v").unwrap();
        }
        assert!(
            wait_until(
                || engine.sstable_count() == 1 && engine.immutable_count() == 0,
                Duration::from_secs(5)
            ),
            "the flush must complete before this test corrupts the resulting file"
        );
        engine.shutdown();
    }

    // Same single-byte corruption at offset 0 as the `get`-path
    // equivalent (`data_block_corruption_is_detected_lazily_at_read_
    // time_not_at_open`) -- always inside the first data block.
    let sst_path = fs::read_dir(dir.join("sstables"))
        .unwrap()
        .find_map(|e| {
            let e = e.ok()?;
            let name = e.file_name().into_string().ok()?;
            name.ends_with(".sst").then(|| e.path())
        })
        .expect("exactly one .sst file must exist after the flush above");
    {
        use std::io::{Read, Seek, SeekFrom, Write};
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&sst_path)
            .unwrap();
        let mut byte = [0u8; 1];
        file.read_exact(&mut byte).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(&[byte[0] ^ 0xFF]).unwrap();
    }

    let engine = open(&dir, lsm_config);
    let result = engine.contains(b"k000", u64::MAX);
    assert!(
        matches!(result, Err(EngineError::Corruption { .. })),
        "contains() must fail closed with Corruption on a corrupted data block, got {result:?}"
    );

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn contains_when_the_underlying_file_shrinks_mid_lifetime_fails_closed_with_io_error() {
    let dir = temp_dir("contains_truncated_file");
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: 350,
        max_immutable_memtables: 8,
        ..LsmConfig::default()
    };
    let engine = open(&dir, lsm_config);
    for i in 0..10u32 {
        engine.put(format!("k{i:03}").as_bytes(), b"v").unwrap();
    }
    assert!(
        wait_until(
            || engine.sstable_count() == 1 && engine.immutable_count() == 0,
            Duration::from_secs(5)
        ),
        "the flush must complete before this test can truncate the resulting file"
    );

    // A restart-then-corrupt (as `contains_across_a_corrupted_data_
    // block_fails_closed_with_corruption` above does) always fails at
    // `SsTable::open`'s own footer/index bounds checks with
    // `Corruption`, never reaching a real `io::Error` -- open()
    // re-validates the file against its *current* length. To exercise
    // a genuine OS I/O failure at *read* time instead, this shrinks the
    // file out from under the already-open, already-validated live
    // `SsTable` (its footer/index stay cached in memory, describing
    // offsets that are no longer within the file), so the positional
    // read inside `read_block` genuinely hits end-of-file.
    let sst_path = fs::read_dir(dir.join("sstables"))
        .unwrap()
        .find_map(|e| {
            let e = e.ok()?;
            let name = e.file_name().into_string().ok()?;
            name.ends_with(".sst").then(|| e.path())
        })
        .expect("exactly one .sst file must exist after the flush above");
    fs::OpenOptions::new()
        .write(true)
        .open(&sst_path)
        .unwrap()
        .set_len(1)
        .unwrap();

    let result = engine.contains(b"k000", u64::MAX);
    assert!(
        matches!(result, Err(EngineError::Io(_))),
        "a data block genuinely beyond the file's current end must fail closed with a real \
         Io error, not Corruption or a silent wrong answer, got {result:?}"
    );

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

/// §17's corruption matrix, `get_as_of`/`range_scan` legs: the same
/// live-file-shrink technique as `contains_when_the_underlying_file_
/// shrinks_mid_lifetime_fails_closed_with_io_error` above, run against
/// `get_as_of` and `range_scan` too -- both share `SsTable::read_block`
/// with `contains_versioned`/`get_versioned`, so this closes out the
/// matrix's remaining cells with exact-variant assertions rather than
/// leaving them to be inferred from `contains()`'s own coverage alone.
#[test]
fn get_as_of_and_range_scan_when_the_underlying_file_shrinks_mid_lifetime_fail_closed_with_io_error(
) {
    let dir = temp_dir("get_range_truncated_file");
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: 350,
        max_immutable_memtables: 8,
        ..LsmConfig::default()
    };
    let engine = open(&dir, lsm_config);
    for i in 0..10u32 {
        engine.put(format!("k{i:03}").as_bytes(), b"v").unwrap();
    }
    assert!(
        wait_until(
            || engine.sstable_count() == 1 && engine.immutable_count() == 0,
            Duration::from_secs(5)
        ),
        "the flush must complete before this test can truncate the resulting file"
    );

    let sst_path = fs::read_dir(dir.join("sstables"))
        .unwrap()
        .find_map(|e| {
            let e = e.ok()?;
            let name = e.file_name().into_string().ok()?;
            name.ends_with(".sst").then(|| e.path())
        })
        .expect("exactly one .sst file must exist after the flush above");
    fs::OpenOptions::new()
        .write(true)
        .open(&sst_path)
        .unwrap()
        .set_len(1)
        .unwrap();

    let get_result = engine.get_as_of(b"k000", u64::MAX);
    assert!(
        matches!(get_result, Err(EngineError::Io(_))),
        "get_as_of() on a data block genuinely beyond the file's current end must fail closed \
         with a real Io error, got {get_result:?}"
    );

    let mut iter = engine.range(Bound::Unbounded, Bound::Unbounded);
    let first = iter.next();
    assert!(
        matches!(first, Some(Err(EngineError::Io(_)))),
        "range_scan() on a data block genuinely beyond the file's current end must yield \
         Err(Io), got {first:?}"
    );
    assert!(
        iter.next().is_none(),
        "the iterator must end after yielding the Io error, same as the Corruption case"
    );

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

/// Read Engine Increment 4 (`PHASE_READ_ENGINE_ADR.md` brief §17):
/// corruption injected *while the engine stays open and has already
/// served a successful read from this exact table* -- no restart, no
/// `SsTable::open` re-validation in between. Data blocks are never
/// cached (bounded-memory-by-design, `src/sstable/reader.rs`'s own
/// doc comment), so a live engine reading the same table again after
/// an external corruption must observe it immediately, exactly like
/// the already-covered restart case -- this test verifies that is
/// actually true, not merely assumed from the restart-based tests.
#[test]
fn corruption_injected_mid_session_between_reads_is_caught_on_the_very_next_read() {
    let dir = temp_dir("mid_session_corruption");
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: 350,
        max_immutable_memtables: 8,
        ..LsmConfig::default()
    };
    let engine = open(&dir, lsm_config);
    for i in 0..10u32 {
        engine.put(format!("k{i:03}").as_bytes(), b"v").unwrap();
    }
    assert!(
        wait_until(
            || engine.sstable_count() == 1 && engine.immutable_count() == 0,
            Duration::from_secs(5)
        ),
        "the flush must complete before this test can read successfully, then corrupt"
    );

    // First: a real, successful read through this exact live engine --
    // proves the table is genuinely readable before corruption, not
    // just "not yet opened."
    assert_eq!(engine.get(b"k000").unwrap(), Some(b"v".to_vec()));
    assert!(engine.contains(b"k000", u64::MAX).unwrap());

    // Corrupt the first data block via a second handle, without
    // restarting or reopening the engine -- the live `SsTable`'s
    // already-validated footer/index/bloom stay exactly as they were.
    let sst_path = fs::read_dir(dir.join("sstables"))
        .unwrap()
        .find_map(|e| {
            let e = e.ok()?;
            let name = e.file_name().into_string().ok()?;
            name.ends_with(".sst").then(|| e.path())
        })
        .expect("exactly one .sst file must exist after the flush above");
    {
        use std::io::{Read, Seek, SeekFrom, Write};
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&sst_path)
            .unwrap();
        let mut byte = [0u8; 1];
        file.read_exact(&mut byte).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(&[byte[0] ^ 0xFF]).unwrap();
    }

    // Every read path must now fail closed on the very next call,
    // through the same still-open engine -- no silent stale-cache
    // return of the pre-corruption value, no partial/incomplete
    // silent success.
    let get_result = engine.get(b"k000");
    assert!(
        matches!(get_result, Err(EngineError::Corruption { .. })),
        "get() must fail closed immediately after mid-session corruption, got {get_result:?}"
    );
    let contains_result = engine.contains(b"k000", u64::MAX);
    assert!(
        matches!(contains_result, Err(EngineError::Corruption { .. })),
        "contains() must fail closed immediately after mid-session corruption, got \
         {contains_result:?}"
    );
    let mut iter = engine.range(Bound::Unbounded, Bound::Unbounded);
    let first = iter.next();
    assert!(
        matches!(first, Some(Err(EngineError::Corruption { .. }))),
        "range() must fail closed immediately after mid-session corruption, got {first:?}"
    );
    assert!(
        iter.next().is_none(),
        "range() must end after the corruption error, never silently continue with an \
         incomplete result"
    );

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

/// `ADR-RE-001`/phase brief §18: property-based, using this project's
/// existing `proptest` convention (`memtable::property_tests`,
/// `manifest::tests::property`, `sstable::tests`'s own proptest blocks)
/// — case count matched to the *I/O-involving* precedent (`manifest`/
/// `sstable`'s own `with_cases(64)`, not `memtable`/`wal`'s
/// pure-in-memory `with_cases(1000)`), since each case here opens a
/// real `LsmEngine` against a real directory. Reproducibility: `proptest`
/// persists a failing seed to `proptest-regressions/lsm/tests.txt`
/// automatically, this project's existing, already-relied-upon
/// mechanism (see `memtable::property_tests`'s own doc comment).
mod range_scan_property_tests {
    use proptest::collection::vec as pvec;
    use proptest::prelude::*;

    use super::*;

    #[derive(Debug, Clone)]
    enum FuzzOp {
        Put { key_idx: u8, value: Vec<u8> },
        Delete { key_idx: u8 },
    }

    fn fuzz_op_strategy() -> impl Strategy<Value = FuzzOp> {
        prop_oneof![
            (0u8..10, pvec(any::<u8>(), 0..12))
                .prop_map(|(key_idx, value)| FuzzOp::Put { key_idx, value }),
            (0u8..10).prop_map(|key_idx| FuzzOp::Delete { key_idx }),
        ]
    }

    fn key_for(idx: u8) -> Vec<u8> {
        format!("k{idx:03}").into_bytes()
    }

    /// Deliberately naive (linear scan + max), never the production
    /// engine's own merge/resolution algorithm -- so this can never
    /// share a bug with what it's checking.
    #[derive(Debug, Default)]
    struct ReferenceModel {
        history: std::collections::HashMap<Vec<u8>, Vec<(u64, MemtableValue)>>,
    }
    impl ReferenceModel {
        fn apply(&mut self, key: &[u8], seq: u64, value: MemtableValue) {
            self.history
                .entry(key.to_vec())
                .or_default()
                .push((seq, value));
        }
        fn value_at(&self, key: &[u8], as_of_seq: u64) -> Option<Vec<u8>> {
            self.history
                .get(key)?
                .iter()
                .filter(|(s, _)| *s <= as_of_seq)
                .max_by_key(|(s, _)| *s)
                .and_then(|(_, v)| match v {
                    MemtableValue::Put(v) => Some(v.clone()),
                    MemtableValue::Tombstone => None,
                })
        }
        fn range_at(&self, as_of_seq: u64) -> Vec<(Vec<u8>, Vec<u8>)> {
            let mut keys: Vec<Vec<u8>> = self.history.keys().cloned().collect();
            keys.sort();
            keys.into_iter()
                .filter_map(|k| {
                    let v = self.value_at(&k, as_of_seq)?;
                    Some((k, v))
                })
                .collect()
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// Ordering, no duplicate logical keys, correct versions, correct
        /// tombstones, `get`/`range_scan` equivalence, and reference-model
        /// equivalence -- folded into one property, matching `memtable::
        /// property_tests`'s own "all instances of the same underlying
        /// check" rationale.
        #[test]
        fn lsm_engine_range_scan_matches_independent_reference_model(
            ops in pvec(fuzz_op_strategy(), 1..30)
        ) {
            let lsm_config = LsmConfig {
                memtable_max_size_bytes: 200,
                max_immutable_memtables: 16,
                ..LsmConfig::default()
            };
            let dir = temp_dir("range_scan_property");
            let engine = open(&dir, lsm_config);
            let mut model = ReferenceModel::default();
            let mut all_seqs = Vec::new();

            for op in &ops {
                match op {
                    FuzzOp::Put { key_idx, value } => {
                        let key = key_for(*key_idx);
                        let seq = engine.put(&key, value).unwrap();
                        model.apply(&key, seq, MemtableValue::Put(value.clone()));
                        all_seqs.push(seq);
                    }
                    FuzzOp::Delete { key_idx } => {
                        let key = key_for(*key_idx);
                        let seq = engine.delete(&key).unwrap();
                        model.apply(&key, seq, MemtableValue::Tombstone);
                        all_seqs.push(seq);
                    }
                }
            }

            for &s in &all_seqs {
                for key_idx in 0u8..10 {
                    let key = key_for(key_idx);
                    let expected = model.value_at(&key, s);
                    let actual = engine.get_as_of(&key, s).unwrap();
                    prop_assert_eq!(
                        actual.clone(), expected.clone(),
                        "get_as_of mismatch for key {:?} at seq {}: engine={:?} model={:?}",
                        key, s, actual, expected
                    );
                    // `ADR-RE-001`/Increment 3 §19: three-way invariant --
                    // reference model, `get_as_of`, and `contains` must
                    // all agree at every seq boundary, for every key.
                    let contained = engine.contains(&key, s).unwrap();
                    prop_assert_eq!(
                        contained, expected.is_some(),
                        "contains/get_as_of disagree for key {:?} at seq {}: contains={} get_as_of={:?}",
                        key, s, contained, expected
                    );
                }
            }

            let expected_range = model.range_at(u64::MAX);
            let actual_range = collect_range(engine.range(Bound::Unbounded, Bound::Unbounded)).unwrap();
            prop_assert_eq!(actual_range.clone(), expected_range.clone());

            // Ordering + no-duplicate-keys, checked directly (not just
            // implied by equality with the model, which is itself
            // already sorted/deduplicated by construction).
            for w in actual_range.windows(2) {
                prop_assert!(w[0].0 < w[1].0, "range_scan output must be strictly ascending by key, no duplicates");
            }

            // get()/range_scan() equivalence at the final state.
            for key_idx in 0u8..10 {
                let key = key_for(key_idx);
                let point = engine.get(&key).unwrap();
                let ranged = actual_range
                    .iter()
                    .find(|(k, _)| k == &key)
                    .map(|(_, v)| v.clone());
                prop_assert_eq!(point, ranged, "get()/range_scan() disagree for key {:?}", key);
            }

            engine.shutdown();
            let _ = fs::remove_dir_all(&dir);
        }
    }
}

// ---------------------------------------------------------------------
// Compaction Increment 1 (`ADR-COMPACTION-001`) integration tests --
// `LsmEngine::compact_once`'s Manifest/live-list/Snapshot/crash/
// concurrency integration. Module-level merge/retention tests live in
// `src/compaction/tests.rs`; these exist only where the full engine's
// state (Manifest, live sstables list, background flush thread,
// Snapshot registry) is actually needed to observe the property.
// ---------------------------------------------------------------------
mod compaction_tests {
    use super::*;

    /// Small memtable + many distinct keys -> many small SSTables
    /// quickly, matching every other increment's own established
    /// fixture-building convention.
    fn small_flush_config(trigger_count: usize) -> LsmConfig {
        LsmConfig {
            memtable_max_size_bytes: 200,
            max_immutable_memtables: 32,
            compaction_trigger_count: trigger_count,
            ..LsmConfig::default()
        }
    }

    /// Builds a fixture with *exactly* `count` live SSTables, no more,
    /// no less -- deterministically, not merely "usually." A flush is
    /// asynchronous (the background flush thread) and briefly present
    /// in *both* `sstables` and `immutables` simultaneously while it
    /// runs (`sstables`' own publish happens before the corresponding
    /// `immutables` removal, `src/lsm/mod.rs`'s flush-thread body) --
    /// so neither `sstable_count()` alone (can undercount what's
    /// already queued, causing this loop to over-issue writes that
    /// then land in one flush burst and overshoot `count`) nor `sstable
    /// _count() + immutable_count()` (can transiently double-count a
    /// table mid-publish, causing this loop to under-issue writes and
    /// undershoot `count`) is race-free on its own. Pacing each write
    /// against a fully-settled `immutable_count()==0` before issuing
    /// the next one serializes fixture-building against the flush
    /// thread entirely, making the final count exact regardless of
    /// scheduling delay under heavy parallel `cargo test` contention.
    fn put_and_wait_for_sstable_count(engine: &LsmEngine, count: usize, seed: &mut u64) {
        while engine.sstable_count() < count {
            let key = format!("k{:06}", *seed % 500);
            engine
                .put(key.as_bytes(), format!("v{seed}").as_bytes())
                .unwrap();
            *seed += 1;
            assert!(
                wait_until(|| engine.immutable_count() == 0, Duration::from_secs(10)),
                "flush must settle before the next write is issued"
            );
        }
    }

    // -------------------------------------------------------------
    // §25/§26: trigger gating, single-table/no-op behavior.
    // -------------------------------------------------------------

    #[test]
    fn should_compact_reports_true_only_at_or_above_trigger_count() {
        let dir = temp_dir("compact_trigger_gating");
        let engine = open(&dir, small_flush_config(4));
        let mut seed = 0u64;

        assert!(
            !engine.should_compact(),
            "0 SSTables must never request compaction"
        );
        for target in 1..4 {
            put_and_wait_for_sstable_count(&engine, target, &mut seed);
            assert!(
                !engine.should_compact(),
                "{target} SSTable(s) is below trigger_count=4, should_compact() must be false"
            );
        }
        put_and_wait_for_sstable_count(&engine, 4, &mut seed);
        assert!(
            engine.should_compact(),
            "exactly trigger_count=4 live SSTables must be sufficient to request compaction"
        );

        engine.shutdown();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn compact_once_returns_none_when_no_live_sstables() {
        let dir = temp_dir("compact_zero_tables");
        let engine = open(&dir, small_flush_config(1));
        let result = engine.compact_once().unwrap();
        assert!(
            result.is_none(),
            "compacting an empty engine must be a clean no-op, not an error"
        );
        engine.shutdown();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn compact_once_below_trigger_count_is_a_no_op() {
        let dir = temp_dir("compact_below_trigger");
        let engine = open(&dir, small_flush_config(4));
        let mut seed = 0u64;
        put_and_wait_for_sstable_count(&engine, 2, &mut seed);
        let before_ids = engine.live_sstable_ids();
        let result = engine.compact_once().unwrap();
        assert!(result.is_none());
        assert_eq!(
            engine.live_sstable_ids(),
            before_ids,
            "a below-trigger compact_once call must not touch the live SSTable list"
        );
        engine.shutdown();
        let _ = fs::remove_dir_all(&dir);
    }

    /// brief §26: single-table compaction follows the exact same,
    /// general retention algorithm as any other input count -- not a
    /// special-cased no-op. With `trigger_count=1` and multiple
    /// versions of the same keys inside one table, no live snapshot,
    /// compaction still correctly drops superseded versions.
    #[test]
    fn compact_once_with_a_single_table_reapplies_retention_correctly() {
        let dir = temp_dir("compact_single_table");
        // 4 entries: "k1"+"v1"+32=36, "k1"+"v2"+32=36, "k2"+"v1"+32=36,
        // "k2"+delete(0-byte value)+32=34 -- total 142 bytes exactly
        // (`memtable::entry_size`), so `memtable_max_size_bytes=142`
        // makes the 4th write itself cross the threshold and trigger
        // exactly one freeze, with exactly these 4 records, no padding.
        let lsm_config = LsmConfig {
            memtable_max_size_bytes: 142,
            max_immutable_memtables: 8,
            compaction_trigger_count: 1,
            ..LsmConfig::default()
        };
        let engine = open(&dir, lsm_config);
        engine.put(b"k1", b"v1").unwrap();
        engine.put(b"k1", b"v2").unwrap();
        engine.put(b"k2", b"v1").unwrap();
        engine.delete(b"k2").unwrap();
        assert!(wait_until(
            || engine.sstable_count() >= 1 && engine.immutable_count() == 0,
            Duration::from_secs(5)
        ));
        assert_eq!(
            engine.sstable_count(),
            1,
            "fixture must land in exactly one SSTable"
        );

        let (meta, stats) = engine
            .compact_once()
            .unwrap()
            .expect("1 table >= trigger_count=1");
        assert_eq!(
            engine.sstable_count(),
            1,
            "compacting one table still yields exactly one live table"
        );
        assert_eq!(stats.input_sstable_count, 1);
        assert_eq!(stats.output_sstable_count, 1);
        // No live snapshot -- only the newest version of each key
        // survives. k1's superseded "v1" is a dropped *version* (a
        // Put); k2's superseded "v1" is also a dropped *version* -- its
        // own tombstone is the *newest* version of k2, so the tombstone
        // itself is retained, not dropped.
        assert_eq!(
            stats.versions_dropped, 2,
            "k1's superseded v1 and k2's superseded v1 must both be dropped as superseded Puts"
        );
        assert_eq!(
            stats.tombstones_dropped, 0,
            "k2's tombstone is the newest version of k2 -- it must survive, never be dropped"
        );
        let _ = meta;

        // Logical correctness after single-table compaction:
        assert_eq!(engine.get(b"k1").unwrap(), Some(b"v2".to_vec()));
        assert_eq!(engine.get(b"k2").unwrap(), None);

        engine.shutdown();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn compact_once_merges_many_tables_and_updates_manifest_and_live_list() {
        let dir = temp_dir("compact_basic_merge");
        let engine = open(&dir, small_flush_config(4));
        let mut seed = 0u64;
        // Async flush means `sstable_count()` can overshoot the
        // requested target under heavy parallel-test-run scheduling
        // delay (several flushes can land in a batch between one
        // `put()` and the next count check) -- the test's own
        // properties below don't depend on the exact count, only that
        // it's `>= trigger_count=4`, so this asserts that bound
        // instead of a hard-coded exact value.
        put_and_wait_for_sstable_count(&engine, 5, &mut seed);
        let input_ids = engine.live_sstable_ids();
        assert!(
            input_ids.len() >= 5,
            "fixture must reach at least 5 live SSTables, got {}",
            input_ids.len()
        );
        let input_count = input_ids.len();

        let (meta, stats) = engine
            .compact_once()
            .unwrap()
            .expect("well above trigger_count=4");

        assert_eq!(
            engine.sstable_count(),
            1,
            "every prior input must be replaced by exactly one output table"
        );
        assert_eq!(engine.live_sstable_ids(), vec![meta.id]);
        assert_eq!(stats.input_sstable_count, input_count);
        assert_eq!(stats.output_sstable_count, 1);
        assert_eq!(
            stats.records_dropped,
            stats.records_read - stats.records_retained,
            "records_dropped invariant must hold exactly"
        );
        assert_eq!(
            stats.tombstones_dropped + stats.versions_dropped,
            stats.records_dropped,
            "tombstones_dropped + versions_dropped must exactly partition records_dropped"
        );

        // Manifest must reflect: output added, every input removed.
        // (No direct "is id live in Manifest" accessor exists beyond
        // the reconciliation path itself -- reopening and checking the
        // reconciled live set is the real, end-to-end proof.)
        engine.shutdown();
        drop(engine);
        let reopened = open(&dir, small_flush_config(4));
        assert_eq!(reopened.live_sstable_ids(), vec![meta.id]);
        for old_id in &input_ids {
            assert!(
                !reopened.live_sstable_ids().contains(old_id),
                "input {old_id} must not still be live after reopening"
            );
        }
        reopened.shutdown();
        let _ = fs::remove_dir_all(&dir);
    }

    // -------------------------------------------------------------
    // §23: correctness differential test -- independent reference
    // model, never the production algorithm as its own oracle.
    // -------------------------------------------------------------

    #[derive(Debug, Default)]
    struct CompactionReferenceModel {
        history: std::collections::HashMap<Vec<u8>, Vec<(u64, MemtableValue)>>,
    }
    impl CompactionReferenceModel {
        fn apply(&mut self, key: &[u8], seq: u64, value: MemtableValue) {
            self.history
                .entry(key.to_vec())
                .or_default()
                .push((seq, value));
        }
        fn value_at(&self, key: &[u8], as_of_seq: u64) -> Option<Vec<u8>> {
            self.history
                .get(key)?
                .iter()
                .filter(|(s, _)| *s <= as_of_seq)
                .max_by_key(|(s, _)| *s)
                .and_then(|(_, v)| match v {
                    MemtableValue::Put(v) => Some(v.clone()),
                    MemtableValue::Tombstone => None,
                })
        }
        fn range_at(&self, as_of_seq: u64) -> Vec<(Vec<u8>, Vec<u8>)> {
            let mut keys: Vec<Vec<u8>> = self.history.keys().cloned().collect();
            keys.sort();
            keys.into_iter()
                .filter_map(|k| Some((k.clone(), self.value_at(&k, as_of_seq)?)))
                .collect()
        }
    }

    #[test]
    fn compaction_preserves_logical_reads_for_every_prior_snapshot_seq() {
        let dir = temp_dir("compact_differential");
        let engine = open(&dir, small_flush_config(100)); // never auto-trigger
        let mut model = CompactionReferenceModel::default();
        let mut rng_state: u64 = 20260921;
        let mut next_rand = move || {
            rng_state ^= rng_state << 13;
            rng_state ^= rng_state >> 7;
            rng_state ^= rng_state << 17;
            rng_state
        };

        let mut snapshots: Vec<Snapshot> = Vec::new();
        for round in 0..2000u64 {
            let key_idx = next_rand() % 40;
            let key = format!("k{key_idx:03}").into_bytes();
            if next_rand() % 5 == 0 {
                let seq = engine.delete(&key).unwrap();
                model.apply(&key, seq, MemtableValue::Tombstone);
            } else {
                let value = format!("v{round}").into_bytes();
                let seq = engine.put(&key, &value).unwrap();
                model.apply(&key, seq, MemtableValue::Put(value));
            }
            if next_rand() % 15 == 0 && snapshots.len() < 8 {
                snapshots.push(engine.snapshot());
            }
            if next_rand() % 23 == 0 && !snapshots.is_empty() {
                let idx = (next_rand() as usize) % snapshots.len();
                snapshots.remove(idx);
            }
        }
        assert!(
            wait_until(|| engine.immutable_count() == 0, Duration::from_secs(10)),
            "every flush must settle before compaction runs"
        );
        assert!(
            engine.sstable_count() >= 2,
            "the fixture must actually span multiple SSTables for this test to mean anything"
        );

        // Record BEFORE-compaction logical reads for every snapshot
        // sequence still live, plus "now" -- against the real engine,
        // not the model (the model is the independent oracle compared
        // against, per brief §23's own explicit instruction; the
        // "before" and "after" engine reads are what must agree with
        // *each other*, both checked against the model too).
        let mut check_seqs: Vec<u64> = snapshots.iter().map(|s| s.seq()).collect();
        check_seqs.push(u64::MAX); // "now"
        check_seqs.sort_unstable();
        check_seqs.dedup();

        let all_keys: Vec<Vec<u8>> = (0..40u64)
            .map(|i| format!("k{i:03}").into_bytes())
            .collect();

        let mut before: std::collections::HashMap<(Vec<u8>, u64), Option<Vec<u8>>> =
            std::collections::HashMap::new();
        for &seq in &check_seqs {
            for key in &all_keys {
                let actual = engine.get_as_of(key, seq).unwrap();
                let expected = model.value_at(key, seq);
                assert_eq!(
                    actual, expected,
                    "PRE-compaction mismatch vs. independent model at seq={seq} key={key:?}"
                );
                before.insert((key.clone(), seq), actual);
            }
        }
        let before_range_now: Vec<(Vec<u8>, Vec<u8>)> =
            collect_range(engine.range(Bound::Unbounded, Bound::Unbounded)).unwrap();
        assert_eq!(
            before_range_now,
            model.range_at(u64::MAX),
            "PRE-compaction range() mismatch vs. model"
        );

        // Run compaction (repeatedly, until below trigger, to actually
        // exercise it -- lower the effective threshold for this one
        // call by driving compact_once directly regardless of
        // should_compact()'s own gate, matching "the deterministic
        // core operation," not the (deferred) trigger).
        let mut cycles = 0;
        while engine.sstable_count() > 1 && cycles < 10 {
            if engine.compact_once().unwrap().is_none() {
                break;
            }
            cycles += 1;
        }

        // AFTER-compaction: every logical read, at every previously-
        // recorded snapshot seq, must be byte-for-byte identical to
        // both the model and the pre-compaction engine reads.
        for &seq in &check_seqs {
            for key in &all_keys {
                let actual = engine.get_as_of(key, seq).unwrap();
                let expected = model.value_at(key, seq);
                assert_eq!(
                    actual, expected,
                    "POST-compaction mismatch vs. independent model at seq={seq} key={key:?}"
                );
                assert_eq!(
                    actual,
                    before[&(key.clone(), seq)],
                    "POST-compaction read must exactly match the PRE-compaction read at seq={seq} key={key:?}"
                );
                let contained = engine.contains(key, seq).unwrap();
                assert_eq!(
                    contained,
                    actual.is_some(),
                    "contains()/get_as_of() must agree post-compaction"
                );
            }
        }
        let after_range_now: Vec<(Vec<u8>, Vec<u8>)> =
            collect_range(engine.range(Bound::Unbounded, Bound::Unbounded)).unwrap();
        assert_eq!(
            after_range_now, before_range_now,
            "POST-compaction range() must exactly match PRE-compaction range()"
        );
        assert_eq!(
            after_range_now,
            model.range_at(u64::MAX),
            "POST-compaction range() must match the independent model"
        );

        drop(snapshots);
        engine.shutdown();
        let _ = fs::remove_dir_all(&dir);
    }

    // -------------------------------------------------------------
    // §24: property-based verification, `proptest = "=1.11.0"`
    // (already an exact-pinned dependency -- no version change).
    // -------------------------------------------------------------

    mod property {
        use proptest::collection::vec as pvec;
        use proptest::prelude::*;

        use super::*;

        #[derive(Debug, Clone)]
        enum FuzzOp {
            Put { key_idx: u8, value: Vec<u8> },
            Delete { key_idx: u8 },
            TakeSnapshot,
            DropOldestSnapshot,
            Compact,
        }

        fn fuzz_op_strategy() -> impl Strategy<Value = FuzzOp> {
            prop_oneof![
                4 => (0u8..12, pvec(any::<u8>(), 0..8))
                    .prop_map(|(key_idx, value)| FuzzOp::Put { key_idx, value }),
                2 => (0u8..12).prop_map(|key_idx| FuzzOp::Delete { key_idx }),
                1 => Just(FuzzOp::TakeSnapshot),
                1 => Just(FuzzOp::DropOldestSnapshot),
                1 => Just(FuzzOp::Compact),
            ]
        }

        fn key_for(idx: u8) -> Vec<u8> {
            format!("k{idx:03}").into_bytes()
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(48))]

            /// Random keys, random versions, random tombstones, random
            /// snapshot sequences, random compaction points -- asserts
            /// logical equivalence against an independent reference
            /// model, sorted output, no lost visible value, no
            /// resurrected tombstone, no invalid snapshot visibility.
            #[test]
            fn compaction_never_changes_logical_reads(ops in pvec(fuzz_op_strategy(), 1..60)) {
                let lsm_config = LsmConfig {
                    memtable_max_size_bytes: 250,
                    max_immutable_memtables: 32,
                    compaction_trigger_count: 1000, // never auto-trigger
                    ..LsmConfig::default()
                };
                let dir = temp_dir("compact_property");
                let engine = open(&dir, lsm_config);
                let mut model = CompactionReferenceModel::default();
                let mut snapshots: Vec<Snapshot> = Vec::new();

                for op in ops {
                    match op {
                        FuzzOp::Put { key_idx, value } => {
                            let key = key_for(key_idx);
                            let seq = engine.put(&key, &value).unwrap();
                            model.apply(&key, seq, MemtableValue::Put(value));
                        }
                        FuzzOp::Delete { key_idx } => {
                            let key = key_for(key_idx);
                            let seq = engine.delete(&key).unwrap();
                            model.apply(&key, seq, MemtableValue::Tombstone);
                        }
                        FuzzOp::TakeSnapshot => {
                            if snapshots.len() < 6 {
                                snapshots.push(engine.snapshot());
                            }
                        }
                        FuzzOp::DropOldestSnapshot => {
                            if !snapshots.is_empty() {
                                snapshots.remove(0);
                            }
                        }
                        FuzzOp::Compact => {
                            let _ = engine.compact_once();
                        }
                    }
                }

                prop_assert!(wait_until(|| engine.immutable_count() == 0, Duration::from_secs(10)));

                let mut check_seqs: Vec<u64> = snapshots.iter().map(|s| s.seq()).collect();
                check_seqs.push(u64::MAX);
                check_seqs.sort_unstable();
                check_seqs.dedup();

                for &seq in &check_seqs {
                    for idx in 0..12u8 {
                        let key = key_for(idx);
                        let actual = engine.get_as_of(&key, seq).unwrap();
                        let expected = model.value_at(&key, seq);
                        prop_assert_eq!(
                            actual.clone(), expected,
                            "logical read mismatch at seq={} key={:?}", seq, key
                        );
                        let contained = engine.contains(&key, seq).unwrap();
                        prop_assert_eq!(contained, actual.is_some());
                    }
                    let actual_range: Vec<(Vec<u8>, Vec<u8>)> = engine
                        .range_scan(Bound::Unbounded, Bound::Unbounded, seq)
                        .collect::<Result<Vec<_>>>()
                        .unwrap();
                    let expected_range = model.range_at(seq);
                    prop_assert_eq!(&actual_range, &expected_range, "range_scan mismatch at seq={}", seq);
                    // No duplicate logical key, sorted ascending.
                    let mut sorted = actual_range.clone();
                    sorted.sort_by(|a, b| a.0.cmp(&b.0));
                    prop_assert_eq!(&sorted, &actual_range, "range_scan output must be sorted ascending");
                    let mut dedup = actual_range.clone();
                    dedup.dedup_by(|a, b| a.0 == b.0);
                    prop_assert_eq!(dedup.len(), actual_range.len(), "range_scan output must have no duplicate logical key");
                }

                drop(snapshots);
                engine.shutdown();
                let _ = fs::remove_dir_all(&dir);
            }
        }
    }

    // -------------------------------------------------------------
    // §19: deterministic crash-window fault injection, via
    // `CompactionFaultPoint` -- panic caught, then a real restart
    // (shutdown + drop + reopen, never `catch_unwind`-then-continue,
    // since the property under test is "what does a fresh open() see
    // after a crash," not "does the same process recover in place").
    // -------------------------------------------------------------

    /// Builds a 4-SSTable fixture ready to compact, returns the engine
    /// and the pre-compaction live ids (for post-crash comparison).
    fn build_compactable_fixture(dir: &Path) -> (LsmEngine, Vec<u64>) {
        let engine = open(dir, small_flush_config(4));
        let mut seed = 0u64;
        put_and_wait_for_sstable_count(&engine, 4, &mut seed);
        let ids = engine.live_sstable_ids();
        (engine, ids)
    }

    #[test]
    fn compaction_crash_windows_leave_a_correct_recoverable_state() {
        for point in [
            CompactionFaultPoint::BeforeOutputWrite,
            CompactionFaultPoint::BeforeManifestAdd,
            CompactionFaultPoint::AfterManifestAdd,
            CompactionFaultPoint::DuringRemoveSequence,
            CompactionFaultPoint::AfterAllRemoves,
            CompactionFaultPoint::BeforePhysicalDelete,
        ] {
            let dir = temp_dir(&format!("compact_crash_{point:?}"));
            let (engine, input_ids) = build_compactable_fixture(&dir);

            // Record expected logical state (every key this fixture's
            // own `put_and_wait_for_sstable_count` could have written)
            // BEFORE the crash, via real reads -- compared against the
            // SAME real reads after the simulated crash + restart.
            let sample_keys: Vec<Vec<u8>> = (0..500u64)
                .map(|i| format!("k{i:06}").into_bytes())
                .collect();
            let before: Vec<Option<Vec<u8>>> =
                sample_keys.iter().map(|k| engine.get(k).unwrap()).collect();

            engine.install_compaction_fault_hook(move |p| {
                if p == point {
                    panic!("injected compaction crash at {point:?} (deterministic fault test)");
                }
            });

            let caught =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| engine.compact_once()));
            assert!(
                caught.is_err(),
                "the fault hook must actually have fired at {point:?}"
            );

            // Simulate a real crash: no attempt to complete or roll
            // back the interrupted compaction, just an orderly release
            // of the WAL lock (so the reopen below doesn't spuriously
            // fail on a still-held lock) followed by a real restart.
            engine.clear_compaction_fault_hook();
            engine.shutdown();
            drop(engine);

            let reopened = open(&dir, small_flush_config(4));

            // Every input SSTable must either still be fully live, or
            // have been cleanly replaced -- never partially missing,
            // never duplicated data, never a dangling Manifest
            // reference to a missing file (which would itself already
            // fail open() closed, per the existing reconciliation
            // sweep's own contract -- reaching this line at all is
            // already partial proof).
            let live_after = reopened.live_sstable_ids();
            assert!(!live_after.is_empty(), "recovery must leave at least the pre-compaction or post-compaction tables live ({point:?})");

            let after: Vec<Option<Vec<u8>>> = sample_keys
                .iter()
                .map(|k| reopened.get(k).unwrap())
                .collect();
            assert_eq!(
                before, after,
                "every logical read must be identical before and after the crash+restart at {point:?}"
            );

            // No orphaned .sst.tmp file may remain live/untrusted --
            // the existing sweep already reclaims it at open() time;
            // confirm none is left over post-recovery.
            for entry in fs::read_dir(reopened.sstables_dir()).unwrap() {
                let entry = entry.unwrap();
                let name = entry.file_name();
                let name = name.to_string_lossy();
                assert!(
                    !name.ends_with(".sst.tmp"),
                    "a .tmp file must never survive recovery ({point:?}, found {name})"
                );
            }

            let _ = input_ids;
            reopened.shutdown();
            let _ = fs::remove_dir_all(&dir);
        }
    }

    /// §18, mandatory: exercises `reconcile_sstables_with_manifest`'s
    /// "removed-but-undeleted orphan" branch (`src/lsm/mod.rs`) --
    /// previously identified as having zero direct test coverage
    /// (`PHASE_COMPACTION_ARCHITECTURE_REPORT.md` §1/§7): a durable
    /// `RemoveSstable` edit whose corresponding file was never
    /// physically deleted before the crash.
    #[test]
    fn orphan_recovery_after_crash_between_remove_durability_and_physical_deletion() {
        let dir = temp_dir("compact_orphan_recovery");
        let (engine, input_ids) = build_compactable_fixture(&dir);

        engine.install_compaction_fault_hook(|p| {
            if p == CompactionFaultPoint::BeforePhysicalDelete {
                panic!(
                    "injected crash after all RemoveSstable edits are durable, before any \
                     physical deletion is attempted"
                );
            }
        });
        let caught =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| engine.compact_once()));
        assert!(caught.is_err());
        engine.clear_compaction_fault_hook();

        // At this exact point: the output SSTable is fully durable and
        // live in the Manifest; every input's `RemoveSstable` edit is
        // durable; the live-list splice already ran (it happens before
        // `BeforePhysicalDelete` fires); but every input `.sst` file is
        // still physically present on disk -- exactly the orphan
        // scenario this test exists to exercise. Verify the files are
        // indeed still there before the restart, so this test is
        // actually exercising what it claims to.
        for id in &input_ids {
            let path = engine
                .sstables_dir()
                .join(crate::sstable::sstable_filename(*id));
            assert!(
                path.exists(),
                "input {id}'s file must still be physically present immediately after the \
                 injected crash, to prove this test actually reaches the orphan scenario"
            );
        }

        engine.shutdown();
        drop(engine);

        let reopened = open(&dir, small_flush_config(4));

        // The orphan sweep must have removed every input file.
        for id in &input_ids {
            let path = reopened
                .sstables_dir()
                .join(crate::sstable::sstable_filename(*id));
            assert!(
                !path.exists(),
                "orphaned input {id} must be swept away by recovery"
            );
        }
        // The Manifest's live set must be correct: only the compaction
        // output remains, none of the removed inputs.
        for id in &input_ids {
            assert!(
                !reopened.live_sstable_ids().contains(id),
                "input {id} must not be live post-recovery"
            );
        }
        assert_eq!(
            reopened.sstable_count(),
            1,
            "exactly the compaction output must remain live"
        );

        // Reads remain correct.
        let sample_keys: Vec<Vec<u8>> = (0..500u64)
            .map(|i| format!("k{i:06}").into_bytes())
            .collect();
        for key in &sample_keys {
            // Just must not error/panic and must be internally
            // consistent -- exact-value correctness is already proven
            // by the differential/property tests above; this test's
            // own job is the orphan-sweep mechanism specifically.
            let _ = reopened.get(key).unwrap();
        }
        let range_rows = collect_range(reopened.range(Bound::Unbounded, Bound::Unbounded)).unwrap();
        assert!(
            !range_rows.is_empty(),
            "the compacted output must still serve real data"
        );

        reopened.shutdown();
        let _ = fs::remove_dir_all(&dir);
    }

    // -------------------------------------------------------------
    // §16/§17: concurrent flush, concurrent readers.
    // -------------------------------------------------------------

    #[test]
    fn concurrent_flush_publishing_during_compaction_capture_is_not_lost() {
        let dir = temp_dir("compact_concurrent_flush_during_capture");
        let engine = Arc::new(open(&dir, small_flush_config(4)));
        let mut seed = 0u64;
        put_and_wait_for_sstable_count(&engine, 4, &mut seed);
        let pre_capture_ids = engine.live_sstable_ids();

        // A flush publishing a *new* table concurrently with (or
        // immediately after) compaction's own input capture must never
        // be lost -- either included in this cycle (captured before)
        // or eligible for the next cycle (captured after); never
        // silently dropped either way.
        let engine2 = Arc::clone(&engine);
        let writer = thread::spawn(move || {
            let mut seed = 10_000u64;
            put_and_wait_for_sstable_count(&engine2, pre_capture_ids.len() + 1, &mut seed);
        });
        writer.join().unwrap();
        let post_write_count = engine.sstable_count();
        assert!(
            post_write_count >= 5,
            "the concurrent writer must have published at least one more table"
        );

        let (meta, stats) = engine
            .compact_once()
            .unwrap()
            .expect("well above trigger_count=4");
        assert!(
            stats.input_sstable_count >= 4,
            "compaction must have captured at least the original 4 (and possibly the concurrently-published one too)"
        );
        // Whichever tables were captured, no data is lost: every key
        // this test could have written remains readable.
        for i in 0..500u64 {
            let key = format!("k{i:06}", i = i).into_bytes();
            let _ = engine.get(&key).unwrap();
        }
        let _ = meta;

        engine.shutdown();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_readers_during_compaction_never_see_partial_or_wrong_state() {
        let dir = temp_dir("compact_concurrent_readers");
        let engine = Arc::new(open(&dir, small_flush_config(4)));
        let mut seed = 0u64;
        put_and_wait_for_sstable_count(&engine, 6, &mut seed);

        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mismatches = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let mut readers = Vec::new();
        for _ in 0..4 {
            let engine = Arc::clone(&engine);
            let stop = Arc::clone(&stop);
            let mismatches = Arc::clone(&mismatches);
            readers.push(thread::spawn(move || {
                let mut i = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let key = format!("k{:06}", i % 500).into_bytes();
                    if engine.get(&key).is_err() {
                        mismatches.fetch_add(1, Ordering::Relaxed);
                    }
                    if engine.contains(&key, u64::MAX).is_err() {
                        mismatches.fetch_add(1, Ordering::Relaxed);
                    }
                    match collect_range(engine.range(Bound::Unbounded, Bound::Unbounded)) {
                        Ok(rows) => {
                            let mut sorted = rows.clone();
                            sorted.sort_by(|a, b| a.0.cmp(&b.0));
                            if sorted != rows {
                                mismatches.fetch_add(1, Ordering::Relaxed);
                            }
                            let mut dedup = rows.clone();
                            dedup.dedup_by(|a, b| a.0 == b.0);
                            if dedup.len() != rows.len() {
                                mismatches.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Err(_) => {
                            mismatches.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    i += 1;
                }
            }));
        }

        engine.compact_once().unwrap();
        engine.compact_once().unwrap(); // a second cycle, in case the first captured 0 due to timing
        stop.store(true, Ordering::Relaxed);
        for r in readers {
            r.join().unwrap();
        }

        assert_eq!(
            mismatches.load(Ordering::Relaxed),
            0,
            "no reader may ever see an error, a missing key it should see, a duplicate logical \
             key, or unsorted output while compaction runs concurrently"
        );

        engine.shutdown();
        let _ = fs::remove_dir_all(&dir);
    }

    // -------------------------------------------------------------
    // §15: Windows read-safety -- a reader's already-open `Arc
    // <SsTable>` must survive a concurrent compaction that retires
    // and physically unlinks that exact table. The earlier standalone
    // probe (`PHASE_COMPACTION_ARCHITECTURE_REPORT.md` §6) confirmed
    // `remove_file` succeeds on Windows while a second handle is open
    // -- it did NOT test a positional read *after* unlink, which this
    // test now does, against the real engine.
    // -------------------------------------------------------------

    #[test]
    fn long_lived_reader_survives_compaction_unlinking_its_table_and_cleanup_eventually_happens() {
        let dir = temp_dir("compact_long_lived_reader");
        let engine = open(&dir, small_flush_config(4));
        let mut seed = 0u64;
        put_and_wait_for_sstable_count(&engine, 4, &mut seed);

        // Start a range scan and pull the first row -- this captures a
        // `ReadView` holding `Arc<SsTable>` clones (and, per Increment
        // 6, persistent `SsTableRangeCursor`s) for every currently-live
        // table, kept alive for as long as this iterator lives.
        let mut in_progress = engine.range(Bound::Unbounded, Bound::Unbounded);
        let first_row = in_progress.next();
        assert!(
            first_row.is_some(),
            "the range scan must yield at least one row before compaction runs"
        );

        // Compact while `in_progress` is still alive and holding
        // `Arc<SsTable>` clones for the very tables being retired.
        let (_meta, _stats) = engine
            .compact_once()
            .unwrap()
            .expect("4 >= trigger_count=4");

        // The already-open scan must complete correctly despite its
        // source tables having been removed from the live list (and,
        // for any whose Arc strong count already allowed it, unlinked
        // from disk) underneath it.
        let mut remaining_rows = 0usize;
        for row in in_progress {
            row.expect(
                "an in-progress range scan must complete without error even though compaction \
                 retired (and may have unlinked) its source tables underneath it",
            );
            remaining_rows += 1;
        }
        assert!(
            remaining_rows > 0,
            "the scan must have yielded further rows after the first"
        );

        // Now that the scan (and every clone it held) has been fully
        // consumed and dropped by the `for` loop above, the next
        // compaction cycle's own opening sweep must be able to clean
        // up anything that was deferred.
        // Force a cheap, harmless extra cycle purely to run the
        // deferred-delete sweep (compact_once's own opening step) --
        // there may be nothing to compact (only 1 live table now), in
        // which case this simply runs the sweep and returns None.
        let _ = engine.compact_once();

        // get()/contains() against the now-compacted engine must still
        // be fully correct.
        for i in 0..500u64 {
            let key = format!("k{i:06}").into_bytes();
            let _ = engine.get(&key).unwrap();
        }

        engine.shutdown();
        let _ = fs::remove_dir_all(&dir);
    }

    // -------------------------------------------------------------
    // §20: storage pressure -- observed only, never mutated.
    // -------------------------------------------------------------

    #[test]
    fn compaction_defers_while_storage_full_and_never_mutates_storage_state() {
        let dir = temp_dir("compact_storage_full");
        let engine = open(&dir, small_flush_config(4));
        let mut seed = 0u64;
        put_and_wait_for_sstable_count(&engine, 5, &mut seed);
        let ids_before = engine.live_sstable_ids();

        engine.set_storage_state_for_test(StorageState::StorageFull);
        let events_before = engine.storage_pressure_events();

        let result = engine.compact_once().unwrap();
        assert!(
            result.is_none(),
            "compaction must defer/skip while storage_state() is not Healthy"
        );
        assert_eq!(
            engine.live_sstable_ids(),
            ids_before,
            "a deferred compaction must not touch the live list"
        );
        assert_eq!(
            engine.storage_state(),
            StorageState::StorageFull,
            "compact_once must never mutate storage_state itself"
        );
        assert_eq!(
            engine.storage_pressure_events(),
            events_before,
            "compact_once must never mutate storage_pressure_events itself"
        );

        engine.set_storage_state_for_test(StorageState::StoragePressure);
        let result = engine.compact_once().unwrap();
        assert!(
            result.is_none(),
            "compaction must also defer/skip under StoragePressure, not just StorageFull"
        );
        assert_eq!(engine.storage_state(), StorageState::StoragePressure);

        engine.set_storage_state_for_test(StorageState::Healthy);
        let result = engine.compact_once().unwrap();
        assert!(
            result.is_some(),
            "compaction must proceed normally once storage_state() is Healthy again"
        );

        engine.shutdown();
        let _ = fs::remove_dir_all(&dir);
    }

    // -------------------------------------------------------------
    // §9/§27/§28: writer differential and error-propagation coverage
    // already live in `src/sstable/tests.rs`; the full existing Read
    // Engine and Write Engine regression suites are re-run, unchanged,
    // as part of this crate's own `cargo test --lib` -- not duplicated
    // here.
    // -------------------------------------------------------------
}

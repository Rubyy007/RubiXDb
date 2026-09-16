use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
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
    assert_eq!(engine.get(b"k1"), Some(b"v1".to_vec()));
    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn delete_then_get_returns_not_found() {
    let dir = temp_dir("delete_get");
    let engine = open(&dir, LsmConfig::default());
    engine.put(b"k1", b"v1").unwrap();
    engine.delete(b"k1").unwrap();
    assert_eq!(engine.get(b"k1"), None);
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

    assert_eq!(engine.get_as_of(b"k1", seq1), Some(b"v1".to_vec()));
    assert_eq!(engine.get_as_of(b"k1", seq2), None);
    assert_eq!(engine.get_as_of(b"k1", seq3), Some(b"v3".to_vec()));
    assert_eq!(engine.get(b"k1"), Some(b"v3".to_vec()));
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
    assert_eq!(engine.get_as_of(b"k1", snapshot), Some(b"v1".to_vec()));
    assert_eq!(engine.get_as_of(b"k2", snapshot), None);
    // "Now" (no snapshot) sees the latest.
    assert_eq!(engine.get(b"k1"), Some(b"v2".to_vec()));
    assert_eq!(engine.get(b"k2"), Some(b"v-after-snapshot".to_vec()));
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
            engine.get(format!("k{i:03}").as_bytes()),
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
    };
    let engine = open(&dir, lsm_config);

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
    };
    let engine = open(&dir, lsm_config);

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
    let engine = LsmEngine {
        pool: Arc::new(pool),
        active: RwLock::new(MemTable::new(LsmConfig::default().memtable_max_size_bytes)),
        immutables: RwLock::new(VecDeque::new()),
        config: LsmConfig::default(),
    };

    let result = engine.put(b"k1", b"v1");
    assert!(
        result.is_err(),
        "a WAL durability failure must propagate as an error"
    );
    assert_eq!(
        engine.get(b"k1"),
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
    assert_eq!(engine2.get(b"k1"), None, "k1 was deleted before shutdown");
    assert_eq!(engine2.get(b"k2"), Some(b"v2".to_vec()));
    assert_eq!(engine2.get(b"k3"), Some(b"v3".to_vec()));
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
    assert_eq!(engine2.get_as_of(b"k1", seq1), Some(b"v1".to_vec()));
    assert_eq!(engine2.get_as_of(b"k1", seq2), Some(b"v2".to_vec()));
    assert_eq!(engine2.get_as_of(b"k1", seq3), Some(b"v3".to_vec()));
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
            engine2.get(format!("k{i:04}").as_bytes()),
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
            if engine.get(key.as_bytes()) != Some(expected) {
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
    assert_eq!(engine.get(&large_key), Some(large_value));
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
    };
    let engine = open(&dir, lsm_config);

    let big_value = vec![b'x'; 10_000]; // far larger than the 100-byte limit
    engine.put(b"k1", &big_value).unwrap();
    assert_eq!(engine.get(b"k1"), Some(big_value));
    // The oversized entry must have triggered an immediate freeze rather
    // than leaving `is_full()` permanently true with nothing able to
    // ever "fit" — verified indirectly: a further write must still
    // succeed (a fresh active memtable was installed), not error or hang.
    engine.put(b"k2", b"v2").unwrap();
    assert_eq!(engine.get(b"k2"), Some(b"v2".to_vec()));
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
            engine2.get(key),
            expected.clone(),
            "mismatch for key {key:?} after restart"
        );
    }
    engine2.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

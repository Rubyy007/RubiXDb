//! D1b proof tests (docs/PERSISTENCE_MODEL.md §6): they gate every class-C claim. If any of P1-P4 fails, class-C evidence is
//! inadmissible. Requires `--features sim-fs`.
//!
//! P1 `unsynced_write_is_lost_on_crash`          M1
//! P2 `synced_write_survives_crash`              M2 (assumption A1 made executable - NOT verified against a device)
//! P3 `dir_entry_lost_without_dir_sync`          M6 (A4, A5)
//! P4 `torn_sector_is_rejected_or_repaired`      M3 (A2): the real WAL on the simulator, recovery on the survivors
//! plus: M7 (`fsync_lie_...`), M8 (`same_dump_and_seed_...`), and an engine run that never touches the real disk.
//!
//! Everything simulated here is simulated under docs/PERSISTENCE_MODEL.md. Assumptions A1-A5 are inherited and untested on
//! this platform. A simulator is process-global, so every test serialises on one lock and uninstalls its simulator before
//! it looks at the real disk.

#![cfg(feature = "sim-fs")]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use rubixdb::execution::batch_coordinator::BatchCoordinatorConfig;
use rubixdb::lsm::{LsmConfig, LsmEngine};
use rubixdb::persist::sim::{install, CrashCfg, DataPolicy, NsPolicy, SimConfig, SimFs, Survivors};
use rubixdb::persist::{self, OpenOptions};
use rubixdb::wal::{self, FileWal, SyncMode, Wal, WalConfig, WalOp};

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|p| p.into_inner())
}

const ROOT: &str = "/simroot";

fn new_sim(dir_sync_effective: bool) -> std::sync::Arc<SimFs> {
    SimFs::new(
        SimConfig {
            dir_sync_effective,
            ..SimConfig::default()
        },
        &[Path::new(ROOT)],
    )
}

fn content(s: &Survivors, p: &str) -> Option<Vec<u8>> {
    s.files.get(p).cloned()
}

fn real_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "rbx_simproof_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Create `/simroot/d`, make the directory entry durable, then create `/simroot/d/f` containing `data`.
fn file_with(sim: &std::sync::Arc<SimFs>, data: &[u8], sync_file: bool, sync_dir: bool) {
    let _g = install(std::sync::Arc::clone(sim));
    persist::create_dir_all("/simroot/d").unwrap();
    persist::sync_dir(ROOT).unwrap();
    let mut f = OpenOptions::new()
        .create(true)
        .write(true)
        .open("/simroot/d/f")
        .unwrap();
    f.write_all(data).unwrap();
    if sync_file {
        f.sync_all().unwrap();
    }
    if sync_dir {
        persist::sync_dir("/simroot/d").unwrap();
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// P1
// ---------------------------------------------------------------------------------------------------------------------

#[test]
fn unsynced_write_is_lost_on_crash() {
    let _s = serial();
    let sim = new_sim(true);
    file_with(&sim, b"hello", false, true);
    // M1, strict: the write was never covered by a sync_all, so it does not survive.
    let strict = sim.crash(&CrashCfg::strict(1));
    assert_eq!(
        content(&strict, "/simroot/d/f"),
        Some(Vec::new()),
        "an unsynced write must not survive a strict power loss"
    );
    // M1 under the random policy: it MAY be lost, and what survives is only ever (a prefix of / torn) its own bytes or zeros.
    let mut lost = 0;
    let mut kept = 0;
    for seed in 0..300 {
        let s = sim.crash(&CrashCfg::random(seed));
        let c = content(&s, "/simroot/d/f").expect("the entry was synced");
        if c == b"hello" {
            kept += 1;
        } else {
            lost += 1;
            assert!(
                c.len() <= 5 && c.iter().zip(b"hello").all(|(a, b)| *a == 0 || a == b),
                "invented bytes: {c:?}"
            );
        }
    }
    assert!(lost > 0 && kept > 0, "the random policy must be able to lose and to keep an unsynced write (lost={lost}, kept={kept})");
}

// ---------------------------------------------------------------------------------------------------------------------
// P2
// ---------------------------------------------------------------------------------------------------------------------

#[test]
fn synced_write_survives_crash() {
    let _s = serial();
    let sim = new_sim(true);
    {
        let _g = install(std::sync::Arc::clone(&sim));
        persist::create_dir_all("/simroot/d").unwrap();
        persist::sync_dir(ROOT).unwrap();
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .open("/simroot/d/f")
            .unwrap();
        f.write_all(b"hello").unwrap();
        f.sync_all().unwrap();
        persist::sync_dir("/simroot/d").unwrap();
        // later, unsynced activity on the same file must never damage the synced prefix
        f.write_all(b" world, this tail is not synced").unwrap();
    }
    for ns in [NsPolicy::KeepAll, NsPolicy::DropUnsynced] {
        for seed in 0..300 {
            for data in [
                DataPolicy::DiscardUnsynced,
                DataPolicy::Random {
                    reorder: false,
                    tear: true,
                    length_before_data: true,
                },
                DataPolicy::Random {
                    reorder: true,
                    tear: true,
                    length_before_data: true,
                },
            ] {
                let s = sim.crash(&CrashCfg {
                    seed,
                    data,
                    ns,
                    sector: 0,
                });
                let c = content(&s, "/simroot/d/f")
                    .expect("a synced entry in a synced directory survives every policy");
                assert!(
                    c.starts_with(b"hello"),
                    "synced bytes damaged (seed {seed}, {data:?}, {ns:?}): {c:?}"
                );
            }
        }
    }
    // a file whose last write was synced is exactly its synced content under the strict policy
    let sim2 = new_sim(true);
    file_with(&sim2, b"exact", true, true);
    assert_eq!(
        content(&sim2.crash(&CrashCfg::strict(0)), "/simroot/d/f"),
        Some(b"exact".to_vec())
    );
}

// ---------------------------------------------------------------------------------------------------------------------
// P3
// ---------------------------------------------------------------------------------------------------------------------

#[test]
fn dir_entry_lost_without_dir_sync() {
    let _s = serial();
    let pessimistic = CrashCfg {
        seed: 7,
        data: DataPolicy::DiscardUnsynced,
        ns: NsPolicy::DropUnsynced,
        sector: 0,
    };
    // data synced, directory entry NOT synced: the entry is lost
    let sim = new_sim(true);
    file_with(&sim, b"data", true, false);
    assert_eq!(
        content(&sim.crash(&pessimistic), "/simroot/d/f"),
        None,
        "a synced file whose directory was not synced must be able to vanish"
    );
    // ...but not when the platform keeps ordered metadata (W1) - the optimistic policy
    assert_eq!(
        content(
            &sim.crash(&CrashCfg {
                ns: NsPolicy::KeepAll,
                ..pessimistic
            }),
            "/simroot/d/f"
        ),
        Some(b"data".to_vec())
    );
    // after sync_dir (effective) it survives the pessimistic policy
    let sim = new_sim(true);
    file_with(&sim, b"data", true, true);
    assert_eq!(
        content(&sim.crash(&pessimistic), "/simroot/d/f"),
        Some(b"data".to_vec())
    );
    // the Windows model: sync_dir is a no-op (A5), so calling it changes nothing and the entry can still vanish
    let sim = new_sim(false);
    file_with(&sim, b"data", true, true);
    assert_eq!(
        content(&sim.crash(&pessimistic), "/simroot/d/f"),
        None,
        "with a no-op sync_dir (the Windows RealFs) the entry is never durable by itself"
    );
    // a random policy loses it for some seeds and keeps it for others
    let sim = new_sim(true);
    file_with(&sim, b"data", true, false);
    let outcomes: Vec<bool> = (0..200)
        .map(|seed| {
            content(
                &sim.crash(&CrashCfg {
                    seed,
                    data: DataPolicy::DiscardUnsynced,
                    ns: NsPolicy::Random(0.5),
                    sector: 0,
                }),
                "/simroot/d/f",
            )
            .is_some()
        })
        .collect();
    assert!(outcomes.iter().any(|b| *b) && outcomes.iter().any(|b| !*b));
}

// ---------------------------------------------------------------------------------------------------------------------
// P4 - the real WAL on the simulator, recovery of the survivors by the real recovery code
// ---------------------------------------------------------------------------------------------------------------------

#[test]
fn torn_sector_is_rejected_or_repaired() {
    let _s = serial();
    let sim = new_sim(true);
    let big = vec![b'Z'; 1500]; // spans three 512-byte sectors
    {
        let _g = install(std::sync::Arc::clone(&sim));
        let (mut w, _) =
            FileWal::open_for_recovery(Path::new("/simroot/p4"), WalConfig::default()).unwrap();
        for i in 0..3u32 {
            w.append_sync(WalOp::Put {
                key: format!("k{i}").as_bytes(),
                value: b"synced-value",
            })
            .unwrap();
        }
        // the 4th record is written but NOT synced: it is the one a power loss can tear
        w.append(WalOp::Put {
            key: b"k3",
            value: &big,
        })
        .unwrap();
        std::mem::forget(w); // no destructor activity: the machine "loses power" here
    }
    let dump = sim.dump();
    let crashed = SimFs::from_dump(&dump).unwrap();
    // The synced image: under the strict policy exactly the bytes covered by sync_all survive.
    let strict = crashed.crash(&CrashCfg::strict(0));
    let seg = "/simroot/p4/wal/wal-00000000000000000001.log";
    let synced_image = strict
        .files
        .get(seg)
        .expect("the segment exists in the synced image")
        .clone();
    let (mut three, mut four, mut torn_seen, mut zero_cases, mut rejected) = (0, 0, 0, 0, 0);
    for seed in 0..400u64 {
        let surv = crashed.crash(&CrashCfg::random(seed));
        // Whatever the crash did, the synced bytes are untouched (A1/A2): the damage is confined to the unsynced write.
        assert!(
            surv.files
                .get(seg)
                .expect("segment")
                .starts_with(&synced_image),
            "seed {seed}: the synced prefix of the segment was damaged"
        );
        let dir = real_dir("p4");
        surv.write_under(Path::new("/simroot/p4"), &dir).unwrap();
        let r = wal::inspect(&dir, &WalConfig::default()).unwrap();
        if !r.corrupted_segments.is_empty() {
            // REJECTED (fail closed): recovery refuses the segment as corrupt. Allowed by this proof - a torn sector is never
            // ACCEPTED - but it is an availability finding, F-D1b-1 (docs/PERSISTENCE_MODEL.md §7): the shape is a lost
            // first sector with later sectors persisted, which recovery classifies as corruption, not a torn tail.
            assert_eq!(r.corrupted_segments, vec![1], "seed {seed}");
            rejected += 1;
            let _ = std::fs::remove_dir_all(&dir);
            continue;
        }
        assert!(
            r.records.len() == 3 || r.records.len() == 4,
            "seed {seed}: {} records",
            r.records.len()
        );
        // the three synced records are intact and exact
        for (i, (seq, op)) in r.records.iter().take(3).enumerate() {
            assert_eq!(*seq, i as u64 + 1);
            match op {
                rubixdb::wal::WalOpOwned::Put { key, value } => {
                    assert_eq!(key, format!("k{i}").as_bytes());
                    assert_eq!(value, b"synced-value");
                }
                other => panic!("seed {seed}: unexpected record {other:?}"),
            }
        }
        if r.records.len() == 4 {
            // a torn record is never accepted as valid: if the 4th is present it is byte-exact
            match &r.records[3].1 {
                rubixdb::wal::WalOpOwned::Put { key, value } => {
                    assert_eq!(key, b"k3");
                    assert_eq!(
                        value, &big,
                        "seed {seed}: a torn record was accepted with altered content"
                    );
                }
                other => panic!("seed {seed}: unexpected 4th record {other:?}"),
            }
            four += 1;
        } else {
            three += 1;
            if r.truncated {
                torn_seen += 1;
            } else {
                zero_cases += 1; // the whole unsynced write was lost without leaving a stub
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
    assert!(
        three > 0 && four > 0,
        "non-vacuous: both outcomes must occur (three={three}, four={four})"
    );
    assert!(torn_seen > 0, "non-vacuous: at least one seed must leave a torn tail that recovery detected and truncated (zero_cases={zero_cases})");
    assert!(
        rejected < 400,
        "every seed was rejected: the proof would be vacuous"
    );
    eprintln!("P4 outcomes over 400 seeds: 4 exact records={four}, 3 records (lost, no stub)={zero_cases}, 3 records (torn tail truncated)={torn_seen}, rejected as corrupt={rejected}");
}

// ---------------------------------------------------------------------------------------------------------------------
// M7, M8
// ---------------------------------------------------------------------------------------------------------------------

/// M7: a device that lies about flushing violates A1 and loses SYNCED data. This test exists to show the model can express
/// that and to label it; it is never a claim about any real device.
#[test]
fn fsync_lie_loses_synced_data_and_is_a_violation_of_a1() {
    let _s = serial();
    let sim = SimFs::new(
        SimConfig {
            dir_sync_effective: true,
            fsync_lie: Some((9, 1_000_000)),
            ..SimConfig::default()
        },
        &[Path::new(ROOT)],
    );
    file_with(&sim, b"believed-durable", true, true);
    let s = sim.crash(&CrashCfg::strict(3));
    assert_eq!(
        content(&s, "/simroot/d/f"),
        Some(Vec::new()),
        "M7: the device acknowledged sync_all but kept nothing (A1 violated)"
    );
}

/// M8: a crash is a pure function of (dump, configuration, seed).
#[test]
fn same_dump_and_seed_give_the_same_survivors() {
    let _s = serial();
    let sim = new_sim(true);
    {
        let _g = install(std::sync::Arc::clone(&sim));
        persist::create_dir_all("/simroot/d").unwrap();
        for i in 0..5 {
            let mut f = OpenOptions::new()
                .create(true)
                .write(true)
                .open(format!("/simroot/d/f{i}"))
                .unwrap();
            for j in 0..6 {
                f.write_all(&vec![(i * 16 + j) as u8; 700 + j * 37])
                    .unwrap();
            }
        }
    }
    let d1 = SimFs::from_dump(&sim.dump()).unwrap();
    let d2 = SimFs::from_dump(&sim.dump()).unwrap();
    let mut distinct = std::collections::HashSet::new();
    for seed in 0..30 {
        let a = d1.crash(&CrashCfg::random(seed));
        let b = d2.crash(&CrashCfg::random(seed));
        assert_eq!(
            a, b,
            "seed {seed}: the same dump and seed must give identical survivors"
        );
        let c = sim.crash(&CrashCfg::random(seed));
        assert_eq!(
            a, c,
            "seed {seed}: a dump round trip must not change the outcome"
        );
        distinct.insert(format!(
            "{:?}",
            a.files.values().map(|v| v.len()).collect::<Vec<_>>()
        ));
    }
    assert!(
        distinct.len() > 3,
        "different seeds must give different survivors (distinct={})",
        distinct.len()
    );
}

// ---------------------------------------------------------------------------------------------------------------------
// the whole engine on the simulator
// ---------------------------------------------------------------------------------------------------------------------

fn pool() -> BatchCoordinatorConfig {
    BatchCoordinatorConfig {
        queue_capacity: 512,
        max_queued_bytes: 16 * 1024 * 1024,
        submission_timeout: Duration::from_secs(5),
        shutdown_drain_bound: Duration::from_secs(10),
        await_retry_budget: Duration::from_secs(5),
        max_drain_per_batch: 4096,
    }
}

fn wal_cfg() -> WalConfig {
    WalConfig {
        sync_mode: SyncMode::GroupCommit {
            max_wait: Duration::from_millis(5),
            max_batch_bytes: 256 * 1024,
        },
        max_segment_size: 16 * 1024,
        ..WalConfig::default()
    }
}

fn lsm_cfg() -> LsmConfig {
    LsmConfig {
        memtable_max_size_bytes: 8 * 1024,
        compaction_auto_trigger: false,
        ..LsmConfig::default()
    }
}

/// The engine (WAL, MANIFEST, SSTables, flush, rotation, purge, orphan sweep) runs ENTIRELY on the simulator: nothing it
/// does reaches the real disk (the virtual path does not exist there), it works, and a clean-stop crash image recovers every
/// acknowledged key on the real filesystem. This is the dynamic evidence that no persistence path escapes `crate::persist`.
#[test]
fn the_engine_runs_entirely_on_the_simulated_filesystem() {
    let _s = serial();
    let sim = new_sim(true);
    let dump;
    {
        let _g = install(std::sync::Arc::clone(&sim));
        let e =
            LsmEngine::open(Path::new("/simroot/engine"), wal_cfg(), pool(), lsm_cfg()).unwrap();
        for i in 0..250u32 {
            e.put(format!("key-{i:05}").as_bytes(), &[b'v'; 90])
                .unwrap();
        }
        while e.immutable_count() > 0 {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            e.sstable_count() >= 1,
            "the workload must have flushed at least once"
        );
        let _ = e.shutdown();
        drop(e);
        // same process, same simulator: reopen and read back
        let e =
            LsmEngine::open(Path::new("/simroot/engine"), wal_cfg(), pool(), lsm_cfg()).unwrap();
        assert_eq!(e.get(b"key-00249").unwrap().map(|v| v.len()), Some(90));
        let _ = e.shutdown();
        drop(e);
        let names = sim.list_paths();
        for needle in [
            "/simroot/engine/MANIFEST",
            "/simroot/engine/wal",
            "/simroot/engine/sstables",
        ] {
            assert!(
                names.iter().any(|n| n == needle),
                "{needle} missing from the simulated tree: {names:?}"
            );
        }
        assert!(
            names.iter().any(|n| n.ends_with(".sst")),
            "no SSTable in the simulated tree"
        );
        dump = sim.dump();
    }
    // the real disk saw nothing
    assert!(
        !Path::new("/simroot/engine").exists(),
        "the engine leaked a directory onto the real disk"
    );
    // strict power loss at the end of a clean run: every acknowledged key is on the survivors
    let crashed = SimFs::from_dump(&dump).unwrap();
    let surv = crashed.crash(&CrashCfg::strict(5));
    let dir = real_dir("engine").join("engine");
    surv.write_under(Path::new("/simroot/engine"), &dir)
        .unwrap();
    let e = LsmEngine::open(&dir, wal_cfg(), pool(), lsm_cfg()).unwrap();
    for i in 0..250u32 {
        assert!(
            e.get(format!("key-{i:05}").as_bytes()).unwrap().is_some(),
            "key-{i:05} lost across a strict simulated power loss"
        );
    }
    let _ = e.shutdown();
}

/// Reading through the simulator behaves like reading a file.
#[test]
fn the_simulated_handle_reads_seeks_and_truncates_like_a_file() {
    let _s = serial();
    let sim = new_sim(true);
    let _g = install(std::sync::Arc::clone(&sim));
    persist::create_dir_all("/simroot/rw").unwrap();
    let mut f = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open("/simroot/rw/a")
        .unwrap();
    f.write_all(b"0123456789").unwrap();
    f.set_len(6).unwrap();
    let mut s = String::new();
    std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(0)).unwrap();
    f.read_to_string(&mut s).unwrap();
    assert_eq!(s, "012345");
    assert_eq!(persist::metadata("/simroot/rw/a").unwrap().len(), 6);
    persist::rename("/simroot/rw/a", "/simroot/rw/b").unwrap();
    assert!(persist::exists("/simroot/rw/b") && !persist::exists("/simroot/rw/a"));
    let names: Vec<String> = persist::read_dir("/simroot/rw")
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(names, vec!["b".to_string()]);
    assert!(persist::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open("/simroot/rw/b")
        .is_err());
}

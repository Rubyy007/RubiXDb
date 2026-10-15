//! F-D1b-2 (docs/adr/ADR-DATA-DIR-DURABILITY-01.md): the engine must make the directory entries it creates durable - the data
//! directory (and any ancestor it created), `wal/`, `sstables/`, `MANIFEST` and the format marker. Requires `--features sim-fs`.
//!
//! EVIDENCE CLASS, read this before quoting a result:
//!   * The `posix_model_*` tests run the real engine on the simulator configured with the POSIX directory-sync semantics
//!     (`dir_sync_effective = true`, assumption A5). They prove that the engine CALLS `sync_dir` in the right places. They are
//!     SIMULATED; they were NOT run against a real Unix kernel and are NOT power-loss evidence for any real filesystem.
//!   * The `windows_model_*` test runs the same open with `dir_sync_effective = false` (this platform's behaviour) and asserts
//!     that the directory entries ARE NOT protected. That is the documented Windows limitation (C-SYNC-2, A5, W1), executed.
//!   * `real_posix_kernel_directory_durability` is the test a real Unix kernel needs. It cannot run on Windows and is reported
//!     IGNORED (= SKIPPED) there, never PASS. On Windows `windows_real_sync_dir_is_a_noop` pins that the no-op is preserved.
//!
//! "On Windows, sync_dir is a no-op. Protection of directory entries rests on assumption W1 (NTFS metadata-log ordering), which
//!  is NOT established by any test in this repository. Power-loss durability of newly created wal/, sstables/ and MANIFEST
//!  entries on Windows is therefore NOT established. This is a Windows-specific exposure, not a documented guarantee."

#![cfg(feature = "sim-fs")]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use rubixdb::execution::batch_coordinator::BatchCoordinatorConfig;
use rubixdb::lsm::{LsmConfig, LsmEngine};
use rubixdb::persist::sim::{install, CrashCfg, DataPolicy, NsPolicy, SimConfig, SimFs, SimGuard};
use rubixdb::wal::{SyncMode, WalConfig};

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|p| p.into_inner())
}

const ROOT: &str = "/simroot";
const DATA: &str = "/simroot/a/b/engine";

fn new_sim(dir_sync_effective: bool) -> Arc<SimFs> {
    SimFs::new(
        SimConfig {
            dir_sync_effective,
            ..SimConfig::default()
        },
        &[Path::new(ROOT)],
    )
}

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
        ..WalConfig::default()
    }
}

fn lsm_cfg() -> LsmConfig {
    LsmConfig {
        compaction_auto_trigger: false,
        ..LsmConfig::default()
    }
}

fn real_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "rbx_dirdur_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// The directory entries the engine creates on a fresh start, as virtual paths.
fn entries() -> Vec<String> {
    vec![
        "/simroot/a".to_string(),
        "/simroot/a/b".to_string(),
        DATA.to_string(),
        format!("{DATA}/wal"),
        format!("{DATA}/sstables"),
        format!("{DATA}/MANIFEST"),
    ]
}

/// Open a fresh engine on the simulator and acknowledge one write. The engine keeps running; `park` stops it.
fn fresh_engine_with_one_acked_write(
    dir_sync_effective: bool,
) -> (Arc<SimFs>, LsmEngine, SimGuard) {
    let sim = new_sim(dir_sync_effective);
    let g = install(Arc::clone(&sim));
    let e = LsmEngine::open(Path::new(DATA), wal_cfg(), pool(), lsm_cfg()).unwrap();
    e.put(b"the-key", b"the-value").unwrap();
    (sim, e, g)
}

/// Freezes the simulated machine (every later simulated operation parks) and abandons the engine, as a power cut would.
fn park(sim: &SimFs, e: LsmEngine, g: SimGuard) {
    let _ = sim.dump_and_kill();
    std::mem::forget(e);
    drop(g);
}

#[test]
fn posix_model_every_directory_entry_the_engine_creates_is_synced() {
    let _s = serial();
    let (sim, e, g) = fresh_engine_with_one_acked_write(true);
    let pending = sim.non_durable_ns_ops();
    for want in entries() {
        assert!(
            !pending.iter().any(|p| p.contains(&format!("\"{want}\""))),
            "SIMULATED POSIX dir-sync model: the creation of {want} is still awaiting a sync_dir after open + one acknowledged \
             write; pending = {pending:#?}"
        );
    }
    park(&sim, e, g);
}

#[test]
fn posix_model_power_loss_with_the_pessimistic_namespace_policy_keeps_the_directories_and_the_acked_write(
) {
    let _s = serial();
    let (sim, e, g) = fresh_engine_with_one_acked_write(true);
    let survivors = sim.crash(&CrashCfg {
        seed: 7,
        data: DataPolicy::DiscardUnsynced,
        ns: NsPolicy::DropUnsynced,
        sector: 512,
    });
    park(&sim, e, g);
    for d in ["/simroot/a", "/simroot/a/b", DATA] {
        assert!(
            survivors.dirs.contains(d),
            "SIMULATED POSIX model: directory {d} vanished; dirs = {:?}",
            survivors.dirs
        );
    }
    for d in [format!("{DATA}/wal"), format!("{DATA}/sstables")] {
        assert!(survivors.dirs.contains(&d), "{d} vanished");
    }
    assert!(
        survivors.files.contains_key(&format!("{DATA}/MANIFEST")),
        "MANIFEST vanished; files = {:?}",
        survivors.files.keys().collect::<Vec<_>>()
    );
    // and the engine reopens on the survivors with the acknowledged write
    let dir = real_dir("posix").join("engine");
    survivors.write_under(Path::new(DATA), &dir).unwrap();
    let e = LsmEngine::open(&dir, wal_cfg(), pool(), lsm_cfg()).unwrap();
    assert_eq!(
        e.get(b"the-key").unwrap().as_deref(),
        Some(&b"the-value"[..]),
        "SIMULATED POSIX model: the acknowledged write was lost"
    );
    let _ = e.shutdown();
}

/// The product start path (`ops::open`) also stamps the format marker; its directory entry must be durable too.
#[test]
fn posix_model_the_product_start_path_leaves_nothing_awaiting_a_directory_sync() {
    let _s = serial();
    let sim = new_sim(true);
    let g = install(Arc::clone(&sim));
    let e = rubixdb::ops::open::open_engine_for_ops(Path::new(DATA)).unwrap();
    e.put(b"the-key", b"the-value").unwrap();
    let pending = sim.non_durable_ns_ops();
    let survivors = sim.crash(&CrashCfg {
        seed: 3,
        data: DataPolicy::DiscardUnsynced,
        ns: NsPolicy::DropUnsynced,
        sector: 512,
    });
    park(&sim, e, g);
    for want in entries().into_iter().chain([format!("{DATA}/DATA_FORMAT")]) {
        assert!(
            !pending.iter().any(|p| p.contains(&format!("\"{want}\""))),
            "SIMULATED POSIX dir-sync model: {want} is still awaiting a sync_dir; pending = {pending:#?}"
        );
    }
    assert!(
        survivors.files.contains_key(&format!("{DATA}/DATA_FORMAT")),
        "the format marker vanished under the pessimistic namespace policy; files = {:?}",
        survivors.files.keys().collect::<Vec<_>>()
    );
}

/// The Windows limitation, executed: the same open, with this platform's `sync_dir` no-op, leaves the entries unprotected. This
/// test PASSES by observing the exposure; it is not a claim that Windows is safe.
#[test]
fn windows_model_the_same_open_leaves_directory_entries_unprotected() {
    let _s = serial();
    let (sim, e, g) = fresh_engine_with_one_acked_write(false);
    let pending = sim.non_durable_ns_ops();
    assert!(
        pending.iter().any(|p| p.contains(&format!("\"{DATA}/wal\""))
            || p.contains(&format!("\"{DATA}/MANIFEST\""))
            || p.contains(&format!("\"{DATA}/sstables\""))),
        "the Windows model must show unprotected entries (F-D1b-2, G6-Windows); pending = {pending:#?}"
    );
    let survivors = sim.crash(&CrashCfg {
        seed: 7,
        data: DataPolicy::DiscardUnsynced,
        ns: NsPolicy::DropUnsynced,
        sector: 512,
    });
    park(&sim, e, g);
    assert!(
        !survivors.dirs.contains(DATA),
        "SIMULATED Windows model: expected the data directory to be lost under the pessimistic namespace policy; dirs = {:?}",
        survivors.dirs
    );
}

/// What a real Unix kernel run would assert. NOT RUNNABLE here: reported IGNORED (SKIPPED), never PASS.
#[cfg(windows)]
#[test]
#[ignore = "SKIPPED: needs a real POSIX kernel (and a power-cut or dm-flakey rig); this host is Windows/NTFS where sync_dir is a no-op"]
fn real_posix_kernel_directory_durability() {}

/// The no-op is preserved on Windows: syncing a directory that does not exist succeeds (a real fsync would fail).
#[cfg(windows)]
#[test]
fn windows_real_sync_dir_is_a_noop() {
    let _s = serial();
    let d = real_dir("noop");
    rubixdb::persist::sync_dir(&d).unwrap();
    rubixdb::persist::sync_dir(d.join("does-not-exist")).unwrap();
}

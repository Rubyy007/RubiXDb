//! D1a kill-9 rig child (ADR-FAULTPOINTS-01 section 6.1). Spawned by `tests/fault_points/`; requires the `fault-points`
//! feature. It contains NO recovery logic and NO judgement: it runs a deterministic workload on a disposable directory,
//! writes a ledger of what it submitted / what the engine acknowledged, and then exits - or is stopped by the fault
//! registry (`RUBIXDB_FAULT_SPEC`) or killed by its parent.
//!
//! Usage: `fault_child <mode> <dir> <ledger_dir> [key=value ...]`
//!
//! * `engine`  - run the workload on an engine at `<dir>` (options `seed writers ops keys end`; `end` = `clean` | `abort`
//!   | `exit`). Writes `DONE` into `<ledger_dir>` after the workload and a background quiescence wait.
//! * `recover` - the product start sequence (format decision, WAL preflight + tail policy, fresh stamp, engine open) on an
//!   existing directory, then a clean stop. Writes `RECOVERED`.
//! * `rel-setup` - create the relational fixture (catalog, table `t`, index `t_name_idx`) on an empty directory, clean stop.
//! * `rel`     - the relational workload (rows + secondary-index entries through `TableStore`) on a database prepared by
//!   `rel-setup`; same ledger protocol as `engine`. Options `seed ops keys end`.
//! * `sim=1 dump=<file> [dirsync=0|1] [precreate=1]` (any mode, needs `--features sim-fs`; D1b): run the engine on a SIMULATED filesystem
//!   rooted at `/simroot` (pass `/simroot/data` as `<dir>`); the ledger stays on the real disk. A fault rule with the
//!   `simcrash` action (or `end=simcrash`) dumps the simulated filesystem to `<file>` and exits - a *simulated power loss*
//!   request, applied by the parent (crash class C). The real disk is never touched by the engine in this mode.
//! * `wal`     - bare Immediate-mode `FileWal` workload (the `FileWal::sync` path the product leader bypasses).
//! * `ops`     - backup then restore of an existing database (`<dir>` = source; `bk1.rbxbackup` and `restored/` under
//!   `out=<dir>`). Writes `OPS_DONE` (the source's ledger directory already holds the preparation run's `DONE`).

#[path = "../tests/support/rel_workload.rs"]
mod rel_workload;
#[path = "../tests/support/workload.rs"]
mod workload;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rubixdb::lsm::{LsmEngine, WriteOp};
use rubixdb::wal::{FileWal, SyncMode, Wal, WalConfig, WalOp};
use workload::{cfg, gen_op, Item, LedgerWriter, Params};

fn opt(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .find_map(|a| a.strip_prefix(&format!("{name}=")).map(str::to_string))
}

fn opt_u64(args: &[String], name: &str, default: u64) -> u64 {
    opt(args, name)
        .map(|v| v.parse().expect("numeric option"))
        .unwrap_or(default)
}

fn params(args: &[String]) -> Params {
    Params {
        seed: opt_u64(args, "seed", 1),
        writers: opt_u64(args, "writers", 1) as usize,
        ops: opt_u64(args, "ops", 300) as usize,
        keys: opt_u64(args, "keys", 24) as usize,
    }
}

fn mark(ledger_dir: &Path, name: &str) {
    std::fs::write(ledger_dir.join(name), b"1").expect("marker");
}

fn apply_to_engine(e: &LsmEngine, op: &workload::Op) -> Result<u64, rubixdb::error::EngineError> {
    if op.len() == 1 {
        return match &op[0] {
            Item::Put(k, v) => e.put(k, v),
            Item::Del(k) => e.delete(k),
        };
    }
    let ops: Vec<WriteOp> = op
        .iter()
        .map(|it| match it {
            Item::Put(k, v) => WriteOp::Put {
                key: k.clone(),
                value: v.clone(),
            },
            Item::Del(k) => WriteOp::Delete { key: k.clone() },
        })
        .collect();
    e.write_batch(&ops)
}

/// Waits (bounded) until background flush and compaction have nothing left to do, so a point that only fires in the
/// background after the last write still fires before the process ends.
fn wait_quiescent(e: &LsmEngine) {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut quiet = 0;
    while Instant::now() < deadline && quiet < 3 {
        if e.immutable_count() == 0 && !e.compaction_running() {
            quiet += 1;
        } else {
            quiet = 0;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn run_engine(dir: &Path, ledger_dir: &Path, args: &[String]) {
    let p = params(args);
    let mut lsm = cfg::lsm();
    if opt(args, "compaction").as_deref() == Some("off") {
        lsm.compaction_auto_trigger = false;
    }
    let engine = match LsmEngine::open(dir, cfg::wal(), cfg::pool(), lsm) {
        Ok(e) => Arc::new(e),
        Err(e) => {
            eprintln!("engine open failed: {e}");
            mark(ledger_dir, "OPEN_FAILED");
            std::process::exit(4);
        }
    };
    // MUTANT (rig self-test only): acknowledge in the ledger BEFORE the engine call, i.e. claim durability that does not exist
    // yet. The oracle must reject the recovered state of a run stopped before the append (tests/fault_points/mutants.rs).
    let early_ack = opt(args, "mutant").as_deref() == Some("early_ack");
    let mut handles = Vec::new();
    for w in 0..p.writers {
        let e = Arc::clone(&engine);
        let ld = ledger_dir.to_path_buf();
        handles.push(
            std::thread::Builder::new()
                .name(format!("writer-{w}"))
                .spawn(move || {
                    let mut ledger = LedgerWriter::create(&ld, w);
                    for i in 0..p.ops {
                        let op = gen_op(&p, w, i);
                        ledger.submit(i);
                        if early_ack {
                            ledger.ack(i, 0);
                        }
                        match apply_to_engine(&e, &op) {
                            Ok(_) if early_ack => {}
                            Ok(seq) => ledger.ack(i, seq),
                            Err(err) => {
                                ledger.err(i, &err.to_string());
                                return false;
                            }
                        }
                    }
                    true
                })
                .expect("spawn writer"),
        );
    }
    // Join every writer (no short-circuit): each must finish before the quiescence wait starts.
    let outcomes: Vec<bool> = handles
        .into_iter()
        .map(|h| h.join().unwrap_or(false))
        .collect();
    let all_ok = outcomes.iter().all(|ok| *ok);
    if !all_ok {
        mark(ledger_dir, "WRITER_ERROR");
        let _ = std::fs::write(
            ledger_dir.join("OUTCOME"),
            format!(
                "poisoned={}\nstorage_state={:?}\n",
                engine.committer_poisoned(),
                engine.storage_state()
            ),
        );
    }
    wait_quiescent(&engine);
    mark(ledger_dir, "DONE");
    match opt(args, "end").as_deref().unwrap_or("clean") {
        "abort" => std::process::abort(),
        "exit" => std::process::exit(0),
        #[cfg(feature = "sim-fs")]
        "simcrash" => sim_power_loss(),
        _ => {
            // A watchdog: after an injected error the engine may be unable to stop; that must not hang the rig.
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_secs(25));
                eprintln!("SHUTDOWN HUNG");
                std::process::exit(3);
            });
            let _ = engine.shutdown();
            // The WAL lock is released only when the engine is dropped; the attestation needs the stopped directory.
            drop(engine);
            if opt(args, "attest").as_deref() == Some("1") {
                // The product host writes the clean-stop attestation after a graceful stop; the bare engine does not.
                let _ = rubixdb::ops::wal_tail::write_attestation(dir);
            }
            std::process::exit(if all_ok { 0 } else { 5 });
        }
    }
}

fn run_rel_setup(dir: &Path, ledger_dir: &Path) {
    let engine = Arc::new(LsmEngine::open(dir, cfg::wal(), cfg::pool(), cfg::lsm()).expect("open"));
    if let Err(e) = rel_workload::setup(Arc::clone(&engine)) {
        eprintln!("relational setup failed: {e}");
        std::process::exit(8);
    }
    let _ = engine.shutdown();
    drop(engine);
    mark(ledger_dir, "DONE");
}

fn run_rel(dir: &Path, ledger_dir: &Path, args: &[String]) {
    let p = params(args);
    let mut lsm = cfg::lsm();
    if opt(args, "compaction").as_deref() == Some("off") {
        lsm.compaction_auto_trigger = false;
    }
    let engine = match LsmEngine::open(dir, cfg::wal(), cfg::pool(), lsm) {
        Ok(e) => Arc::new(e),
        Err(e) => {
            eprintln!("engine open failed: {e}");
            mark(ledger_dir, "OPEN_FAILED");
            std::process::exit(4);
        }
    };
    let stack = match rel_workload::attach(Arc::clone(&engine)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("relational attach failed: {e}");
            mark(ledger_dir, "OPEN_FAILED");
            std::process::exit(9);
        }
    };
    let mut ledger = LedgerWriter::create(ledger_dir, 0);
    let mut ok = true;
    for i in 0..p.ops {
        let op = rel_workload::gen_rel_op(&p, i);
        ledger.submit(i);
        match rel_workload::apply_op(&stack, &op) {
            Ok(seq) => ledger.ack(i, seq),
            Err(e) => {
                ledger.err(i, &e);
                ok = false;
                break;
            }
        }
    }
    if !ok {
        mark(ledger_dir, "WRITER_ERROR");
    }
    wait_quiescent(&engine);
    mark(ledger_dir, "DONE");
    match opt(args, "end").as_deref().unwrap_or("clean") {
        "abort" => std::process::abort(),
        "exit" => std::process::exit(0),
        _ => {
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_secs(25));
                eprintln!("SHUTDOWN HUNG");
                std::process::exit(3);
            });
            drop(stack);
            let _ = engine.shutdown();
            std::process::exit(if ok { 0 } else { 5 });
        }
    }
}

fn run_recover(dir: &Path, ledger_dir: &Path) {
    let guard = match rubixdb::ops::format::startup_guard_with_tail_policy(dir, false) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("REFUSED: {e}");
            mark(ledger_dir, "REFUSED");
            std::fs::write(ledger_dir.join("REFUSED_WHY"), e.to_string()).ok();
            std::process::exit(10);
        }
    };
    if let Err(e) = rubixdb::ops::format::stamp_if_fresh(dir, guard.format_state) {
        eprintln!("stamp failed: {e}");
        std::process::exit(11);
    }
    let lsm = rubixdb::lsm::LsmConfig {
        compaction_auto_trigger: false,
        ..rubixdb::lsm::LsmConfig::default()
    };
    let engine = match LsmEngine::open(
        dir,
        rubixdb::ops::open::product_wal_config(),
        rubixdb::ops::open::product_pool_config(),
        lsm,
    ) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("engine open failed: {e}");
            mark(ledger_dir, "OPEN_FAILED");
            std::process::exit(4);
        }
    };
    mark(ledger_dir, "RECOVERED");
    let _ = engine.shutdown();
}

fn run_wal(dir: &Path, ledger_dir: &Path, args: &[String]) {
    let n = opt_u64(args, "ops", 120) as usize;
    let cfg = WalConfig {
        sync_mode: SyncMode::Immediate,
        max_segment_size: 4096,
        ..WalConfig::default()
    };
    let (mut wal, _) = FileWal::open_for_recovery(dir, cfg).expect("wal open");
    let mut ledger = LedgerWriter::create(ledger_dir, 0);
    for i in 0..n {
        let k = format!("k{i:05}");
        let v = format!("value-{i:05}-{}", "x".repeat(40));
        ledger.submit(i);
        match wal.append_sync(WalOp::Put {
            key: k.as_bytes(),
            value: v.as_bytes(),
        }) {
            Ok(pos) => ledger.ack(i, pos.seq),
            Err(e) => {
                ledger.err(i, &e.to_string());
                break;
            }
        }
    }
    mark(ledger_dir, "DONE");
}

fn run_ops(dir: &Path, ledger_dir: &Path, args: &[String]) {
    let out = PathBuf::from(opt(args, "out").expect("out="));
    std::fs::create_dir_all(&out).expect("out dir");
    let engine = rubixdb::ops::open::open_engine_for_ops(dir).expect("open source");
    let backup = out.join("bk1.rbxbackup");
    let opts = rubixdb::ops::backup::BackupOptions::default();
    let rep = match rubixdb::ops::backup::create_backup(&engine, &backup, &opts) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("backup failed: {e}");
            std::fs::write(ledger_dir.join("BACKUP_ERROR"), e.to_string()).ok();
            let _ = engine.shutdown();
            std::process::exit(6);
        }
    };
    eprintln!("backup ok: {} entries", rep.entries);
    let _ = engine.shutdown();
    drop(engine);
    let restored = out.join("restored");
    match rubixdb::ops::restore::restore_backup(
        &backup,
        &restored,
        &rubixdb::ops::restore::RestoreOptions::default(),
    ) {
        Ok(r) => eprintln!("restore ok: {} entries", r.entries),
        Err(e) => {
            eprintln!("restore failed: {e}");
            std::fs::write(ledger_dir.join("RESTORE_ERROR"), e.to_string()).ok();
            std::process::exit(7);
        }
    }
    mark(ledger_dir, "OPS_DONE");
}

#[cfg(feature = "sim-fs")]
static SIM: std::sync::OnceLock<Arc<rubixdb::persist::sim::SimFs>> = std::sync::OnceLock::new();
#[cfg(feature = "sim-fs")]
static DUMP_PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// `simcrash`: atomically dump the simulated filesystem and mark it dead (so no operation completes - and is acknowledged -
/// after this instant), write the dump to the real disk and exit without any other write.
#[cfg(feature = "sim-fs")]
fn sim_power_loss() {
    if let (Some(sim), Some(path)) = (SIM.get(), DUMP_PATH.get()) {
        let bytes = sim.dump_and_kill();
        std::fs::write(path, bytes).expect("write the simulator dump");
    }
    std::process::exit(0);
}

#[cfg(feature = "sim-fs")]
fn install_sim(args: &[String]) {
    use rubixdb::persist::sim::{install, SimConfig, SimFs};
    let dump = opt(args, "dump").expect("sim=1 needs dump=<file>");
    let cfg = SimConfig {
        dir_sync_effective: opt(args, "dirsync").as_deref() == Some("1"),
        ..SimConfig::default()
    };
    // `precreate=1`: `/simroot/data` exists and is durable from the start (the instance layer, outside the engine, creates the
    // database directory); without it the engine's own creation of that directory is part of what a power loss can undo.
    let mut existing: Vec<&Path> = vec![Path::new("/simroot")];
    if opt(args, "precreate").as_deref() == Some("1") {
        existing.push(Path::new("/simroot/data"));
    }
    let sim = SimFs::new(cfg, &existing);
    let _ = SIM.set(Arc::clone(&sim));
    let _ = DUMP_PATH.set(PathBuf::from(dump));
    rubixdb::faults::set_simcrash_hook(sim_power_loss);
    // The guard is leaked on purpose: the simulator stays installed for the life of the process.
    std::mem::forget(install(sim));
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if opt(&args, "sim").as_deref() == Some("1") {
        #[cfg(feature = "sim-fs")]
        install_sim(&args);
        #[cfg(not(feature = "sim-fs"))]
        {
            eprintln!("sim=1 needs --features sim-fs");
            std::process::exit(2);
        }
    }
    if args.len() < 4 {
        eprintln!("usage: fault_child <mode> <dir> <ledger_dir> [key=value ...]");
        std::process::exit(2);
    }
    let dir = PathBuf::from(&args[2]);
    let ledger_dir = PathBuf::from(&args[3]);
    std::fs::create_dir_all(&ledger_dir).expect("ledger dir");
    let rest = &args[4..];
    match args[1].as_str() {
        "engine" => run_engine(&dir, &ledger_dir, rest),
        "recover" => run_recover(&dir, &ledger_dir),
        "rel-setup" => run_rel_setup(&dir, &ledger_dir),
        "rel" => run_rel(&dir, &ledger_dir, rest),
        "wal" => run_wal(&dir, &ledger_dir, rest),
        "ops" => run_ops(&dir, &ledger_dir, rest),
        other => {
            eprintln!("unknown mode {other}");
            std::process::exit(2);
        }
    }
}

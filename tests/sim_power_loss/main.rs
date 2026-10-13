//! D1b crash-injection cycle (docs/PERSISTENCE_MODEL.md §4) - a SMOKE set, NOT the D1c class-C matrix. Requires
//! `--features fault-points,sim-fs` and the `fault_child` example built with the same features
//! (`cargo build --features fault-points,sim-fs --example fault_child`).
//!
//! One cycle:
//!  1. the parent spawns `fault_child` on a SIMULATED filesystem (`sim=1`), with a fault point + trigger whose action is
//!     `simcrash` (or `end=simcrash`) and a deterministic workload seed;
//!  2. the child runs the workload (the real disk holds only the ledger, which is test machinery);
//!  3. at the point the child dumps the simulated filesystem and exits - no real-disk flush of the data directory ever happens;
//!  4. the parent rebuilds the simulator from the dump and applies the power-loss MODEL (`CrashCfg`: discard unsynced,
//!     tear, reorder, drop un-dir-synced entries, leave zero tails);
//!  5. the parent serialises the survivors into a real temp directory;
//!  6. a fresh engine on the real filesystem recovers it (the product start sequence);
//!  7. the oracle compares the recovered keyspace against K in [acked, submitted] (tests/support/oracle.rs).
//!
//! The kill-9 variant of the cycle skips step 4: the parent writes `SimFs::process_kill_image` (every returned write kept).
//!
//! **Every row is "crash class C - SIMULATED power loss under docs/PERSISTENCE_MODEL.md assumptions A1-A5/W1" (or class B for
//! the kill-9 variant). It is not a real power loss. Real power loss: NOT TESTED.**
//! A FAIL row here is a finding for D1c (the matrix), not something this harness hides or fixes; the integrity test below only
//! asserts that the harness itself did what it says (the child really crashed at the point, the model really changed the
//! state). Set `RUBIXDB_SIM_STRICT=1` to additionally fail the run when any row is FAIL.

#![allow(dead_code)]

#[path = "../support/oracle.rs"]
mod oracle;
#[path = "../fault_points/rig.rs"]
mod rig;
#[path = "../support/workload.rs"]
mod workload;

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use rig::{Outcome, Stop};
use rubixdb::persist::sim::{CrashCfg, DataPolicy, NsPolicy, SimFs, Survivors};
use workload::Params;

const LABEL_C: &str = "crash class C - SIMULATED power loss (docs/PERSISTENCE_MODEL.md; A1-A5/W1; real power loss NOT TESTED)";
const LABEL_B: &str = "crash class B - process kill (kill-9 variant; not power-loss evidence)";

#[derive(Clone, Copy)]
enum Image {
    Model(&'static str, CrashCfg),
    ProcessKill,
}

impl Image {
    fn name(&self) -> String {
        match self {
            Image::Model(n, c) => format!("{n}/seed{}", c.seed),
            Image::ProcessKill => "process-kill".into(),
        }
    }
    fn class(&self) -> &'static str {
        match self {
            Image::Model(..) => "C",
            Image::ProcessKill => "B",
        }
    }
}

#[derive(Clone)]
struct Cycle {
    /// `engine` or `wal`.
    mode: &'static str,
    /// `Some("wal.fsync=nth=20")` (trigger without the action) or `None` = crash at the end of the workload.
    fault: Option<&'static str>,
    dirsync: bool,
    /// `/simroot/data` is durable before the child starts (see `fault_child`'s `precreate`).
    precreate: bool,
    params: Params,
}

struct Row {
    class: &'static str,
    cycle: String,
    image: String,
    verdict: &'static str,
    /// Attribution BY SHAPE of a FAIL row (not proven per row): `F-D1b-1` (the WAL is rejected as corrupt after a torn/lost
    /// sector of the last unsynced write), `F-D1b-2` (every directory entry the engine created is gone: it never syncs the
    /// database directory), or `UNCLASSIFIED` (a new finding that needs a look). Empty for PASS.
    shape: &'static str,
    detail: String,
}

fn params() -> Params {
    Params {
        seed: 3,
        writers: 1,
        ops: 250,
        keys: 20,
    }
}

fn images() -> Vec<Image> {
    let mut v = vec![
        Image::ProcessKill,
        Image::Model("strict-discard", CrashCfg::strict(0)),
    ];
    for seed in 1..=4 {
        v.push(Image::Model("random-keepns", CrashCfg::random(seed)));
    }
    for seed in 1..=2 {
        v.push(Image::Model(
            "random-dropns",
            CrashCfg {
                seed,
                data: DataPolicy::Random {
                    reorder: true,
                    tear: true,
                    length_before_data: true,
                },
                ns: NsPolicy::DropUnsynced,
                sector: 0,
            },
        ));
    }
    v.push(Image::Model(
        "strict-dropns",
        CrashCfg {
            seed: 0,
            data: DataPolicy::DiscardUnsynced,
            ns: NsPolicy::DropUnsynced,
            sector: 0,
        },
    ));
    v
}

fn cycles() -> Vec<Cycle> {
    let mut v = Vec::new();
    let p = params();
    for dirsync in [true, false] {
        // mid-operation points of the commit and checkpoint paths, plus the end of the workload
        for fault in [
            Some("wal.fsync=nth=30"),
            Some("wal.fsync_done=nth=60"),
            Some("wal.record_written=nth=100"),
            Some("flush.after_sstable_publish=once"),
            Some("manifest.synced=once"),
            Some("flush.after_set_checkpoint=once"),
            None,
        ] {
            v.push(Cycle {
                mode: "engine",
                fault,
                dirsync,
                precreate: true,
                params: p,
            });
        }
        v.push(Cycle {
            mode: "wal",
            fault: Some("wal.sync=nth=50"),
            dirsync,
            precreate: true,
            params: Params { ops: 120, ..p },
        });
    }
    // F-D1b-2 probe: the same crash with the database directory NOT pre-created (first open), dir-sync effective.
    v.push(Cycle {
        mode: "engine",
        fault: Some("wal.fsync=nth=30"),
        dirsync: true,
        precreate: false,
        params: p,
    });
    v
}

fn s(x: &Path) -> String {
    x.to_string_lossy().to_string()
}

struct Ran {
    work: PathBuf,
    ledger: PathBuf,
    dump: PathBuf,
    note: String,
    crashed_ok: bool,
}

fn run_cycle(c: &Cycle, idx: usize) -> Ran {
    let label = format!(
        "simcycle-{idx}-{}-{}-ds{}",
        c.mode,
        c.fault.unwrap_or("end").replace(['=', '.'], "_"),
        c.dirsync as u8
    );
    let work = rig::fresh_case(&label);
    let ledger = work.join("ledger");
    let dump = work.join("sim.dump");
    let mut args: Vec<String> = vec![c.mode.into(), "/simroot/data".into(), s(&ledger)];
    if c.mode == "engine" {
        args.push(format!("seed={}", c.params.seed));
        args.push(format!("writers={}", c.params.writers));
        args.push(format!("ops={}", c.params.ops));
        args.push(format!("keys={}", c.params.keys));
        args.push("compaction=off".into());
        if c.fault.is_none() {
            args.push("end=simcrash".into());
        }
    } else {
        args.push(format!("ops={}", c.params.ops));
    }
    args.push("sim=1".into());
    args.push(format!("dump={}", s(&dump)));
    args.push(format!("dirsync={}", c.dirsync as u8));
    if c.precreate {
        args.push("precreate=1".into());
    }
    let (spec, want) = match c.fault {
        Some(f) => (
            Some(format!("{f}:simcrash")),
            Some(f.split('=').next().unwrap().to_string()),
        ),
        None => (None, None),
    };
    let run = rig::run_child(
        &work,
        &args,
        spec.as_deref(),
        Stop::RunToEnd,
        want.as_deref(),
        false,
    );
    let mut note = String::new();
    let mut ok = true;
    match &run.outcome {
        Outcome::Fired => {}
        other => {
            ok = false;
            let _ = write!(note, "{other:?}");
        }
    }
    if !dump.exists() {
        ok = false;
        let _ = write!(note, " no simulator dump written; {}", run.exit_text());
    }
    if c.mode == "engine" && c.fault.is_some() && ledger.join("DONE").exists() {
        ok = false;
        let _ = write!(
            note,
            " workload completed before the point fired (DONE marker present)"
        );
    }
    Ran {
        work,
        ledger,
        dump,
        note,
        crashed_ok: ok,
    }
}

fn judge(
    c: &Cycle,
    ran: &Ran,
    sim: &SimFs,
    img: Image,
    survivors: &Survivors,
    rowno: usize,
) -> Row {
    let dir = ran.work.join(format!("survivor-{rowno}"));
    let _ = fs::remove_dir_all(&dir);
    survivors
        .write_under(Path::new("/simroot/data"), &dir)
        .expect("write survivors");
    let _ = sim;
    let detail_prefix = format!("files={} ", survivors.files.len());
    let (verdict, detail) = if c.mode == "wal" {
        judge_wal(&dir, &ran.ledger)
    } else {
        let ledgers: Vec<workload::WriterLedger> = (0..c.params.writers)
            .map(|w| {
                workload::read_ledger(&ran.ledger, w).unwrap_or_else(|e| panic!("ledger {w}: {e}"))
            })
            .collect();
        let rep = oracle::recover_and_check(&dir, &c.params, &ledgers);
        let mut d = rep.summary();
        d.push_str(&format!(
            " || pre-recovery: wal_corrupted_segments={} wal_torn_tail={} physical_errors={}",
            rep.pre_wal_corrupted_segments, rep.pre_wal_truncated, rep.pre_physical_errors
        ));
        (if rep.accepted() { "PASS" } else { "FAIL" }, d)
    };
    if verdict == "PASS" {
        let _ = fs::remove_dir_all(&dir);
    }
    let shape = if verdict == "PASS" {
        ""
    } else if detail.contains("WAL_CORRUPT") || detail.contains("as corrupt") {
        "F-D1b-1"
    } else if survivors.files.is_empty() && img.name().contains("dropns") {
        "F-D1b-2"
    } else {
        "UNCLASSIFIED"
    };
    Row {
        shape,
        class: img.class(),
        cycle: format!(
            "{} {} dirsync={} precreate={}",
            c.mode,
            c.fault.unwrap_or("end-of-workload"),
            c.dirsync as u8,
            c.precreate as u8
        ),
        image: img.name(),
        verdict,
        detail: format!("{detail_prefix}{detail}"),
    }
}

/// Bare-WAL judgement: every acknowledged record is present, in order, and no record is invented or altered.
fn judge_wal(dir: &Path, ledger: &Path) -> (&'static str, String) {
    let acked = workload::read_ledger(ledger, 0)
        .map(|l| l.ack_seqs.len())
        .unwrap_or(0);
    let res = match rubixdb::wal::inspect(dir, &rubixdb::wal::WalConfig::default()) {
        Ok(r) => r,
        Err(e) => return ("FAIL", format!("recovery refused: {e}")),
    };
    if !res.corrupted_segments.is_empty() {
        return (
            "FAIL",
            format!("recovery rejects the WAL as corrupt (segments {:?}); {} record(s) acknowledged -> F-D1b-1 shape", res.corrupted_segments, acked),
        );
    }
    let n = res.records.len();
    if n < acked {
        return (
            "FAIL",
            format!("LOST ACKNOWLEDGED: recovered {n} records, {acked} acknowledged"),
        );
    }
    for (i, (seq, op)) in res.records.iter().enumerate() {
        let want_key = format!("k{i:05}");
        let ok = *seq == i as u64 + 1
            && matches!(op, rubixdb::wal::WalOpOwned::Put { key, .. } if key.as_slice() == want_key.as_bytes());
        if !ok {
            return (
                "FAIL",
                format!("record {i} is not the record that was written (seq {seq})"),
            );
        }
    }
    (
        "PASS",
        format!(
            "recovered {n} records, acknowledged {acked}, torn_tail={}",
            res.truncated
        ),
    )
}

fn run_smoke() -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    let mut integrity: Vec<String> = Vec::new();
    for (i, c) in cycles().iter().enumerate() {
        let ran = run_cycle(c, i);
        if !ran.crashed_ok {
            integrity.push(format!(
                "cycle {i} ({} {:?} dirsync={}): {}",
                c.mode, c.fault, c.dirsync, ran.note
            ));
            continue;
        }
        let bytes = fs::read(&ran.dump).expect("dump");
        let sim = SimFs::from_dump(&bytes).expect("decode dump");
        let kill_image = sim.process_kill_image();
        let mut distinct = std::collections::HashSet::new();
        for (n, img) in images().into_iter().enumerate() {
            let surv = match img {
                Image::ProcessKill => kill_image.clone(),
                Image::Model(_, cfg) => sim.crash(&cfg),
            };
            distinct.insert(format!(
                "{:?}",
                surv.files
                    .iter()
                    .map(|(k, v)| (k.clone(), v.len()))
                    .collect::<Vec<_>>()
            ));
            rows.push(judge(c, &ran, &sim, img, &surv, n));
        }
        // the model must actually do something: the images of one dump are not all the same
        if distinct.len() < 2 {
            integrity.push(format!(
                "cycle {i}: all {} images are identical - the model changed nothing",
                images().len()
            ));
        }
        if rows
            .iter()
            .rev()
            .take(images().len())
            .all(|r| r.verdict == "PASS")
        {
            let _ = fs::remove_dir_all(&ran.work);
        }
    }
    write_rows(&rows);
    assert!(
        integrity.is_empty(),
        "harness integrity failures: {integrity:#?}"
    );
    rows
}

fn out_dir() -> PathBuf {
    let d = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("sim-power-loss-results");
    fs::create_dir_all(&d).expect("results dir");
    d
}

fn write_rows(rows: &[Row]) {
    let clean = |t: &str| t.replace(['\t', '\n', '\r'], " ");
    let mut out = String::from("class\tlabel\tcycle\timage\tverdict\tshape\tdetail\n");
    for r in rows {
        let label = if r.class == "C" { LABEL_C } else { LABEL_B };
        let _ = writeln!(
            out,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            r.class,
            label,
            clean(&r.cycle),
            clean(&r.image),
            r.verdict,
            r.shape,
            clean(&r.detail)
        );
    }
    fs::write(out_dir().join("smoke.tsv"), out).expect("write smoke.tsv");
}

#[test]
fn d1b_smoke_crash_injection_cycles() {
    let rows = run_smoke();
    let by = |class: &str, v: &str| {
        rows.iter()
            .filter(|r| r.class == class && r.verdict == v)
            .count()
    };
    eprintln!("D1b SMOKE (not the D1c matrix). {LABEL_C}");
    eprintln!(
        "rows={} | class C: PASS={} FAIL={} | class B (kill-9 variant): PASS={} FAIL={}",
        rows.len(),
        by("C", "PASS"),
        by("C", "FAIL"),
        by("B", "PASS"),
        by("B", "FAIL")
    );
    for shape in ["F-D1b-1", "F-D1b-2", "UNCLASSIFIED"] {
        eprintln!(
            "  class C FAIL rows with shape {shape}: {}",
            rows.iter()
                .filter(|r| r.class == "C" && r.shape == shape)
                .count()
        );
    }
    for r in rows
        .iter()
        .filter(|r| r.verdict == "FAIL" && r.shape == "UNCLASSIFIED")
    {
        eprintln!(
            "  UNCLASSIFIED FAIL [{}] {} / {} :: {}",
            r.class, r.cycle, r.image, r.detail
        );
    }
    // The kill-9 variant never applies the model: D1a certifies class B, so a failure here is a rig fault, not a power-loss artefact.
    assert_eq!(by("B", "FAIL"), 0, "the kill-9 variant of the cycle failed - the sim harness disagrees with D1a's class-B result");
    assert!(
        rows.iter().any(|r| r.class == "B") && rows.iter().any(|r| r.class == "C"),
        "both cycles must have produced rows"
    );
    if std::env::var("RUBIXDB_SIM_STRICT").as_deref() == Ok("1") {
        assert_eq!(
            rows.iter().filter(|r| r.verdict == "FAIL").count(),
            0,
            "RUBIXDB_SIM_STRICT=1: FAIL rows exist (see target/sim-power-loss-results/smoke.tsv)"
        );
    }
}

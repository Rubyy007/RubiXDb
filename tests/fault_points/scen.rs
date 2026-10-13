//! Scenarios: which workload reaches which point, how a case is prepared, and how the state a stop left behind is judged.
//! Everything judged here goes through `tests/support/oracle.rs`; nothing about the expected state comes from the engine.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::oracle::{self, RecoveryReport};
use crate::rig::{self, Outcome, Run, Stop};
use crate::workload::{self, Params, WriterLedger};
use rubixdb::faults::Point;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Fixture {
    /// The workload ended with `abort` (WAL holds unflushed records, no clean stop).
    AbortEnd,
    /// `AbortEnd` plus a synthetic partial frame appended to the last WAL segment. SYNTHETIC: not an observed crash state.
    TornWalTail,
    /// `AbortEnd` plus a synthetic partial frame appended to MANIFEST. SYNTHETIC.
    TornManifestTail,
    /// `AbortEnd` plus a junk `*.sst.tmp`.
    StaleSstTmp,
    /// The workload was killed at `sstable.dir_sync` (a valid table published but unknown to the MANIFEST).
    OrphanSst,
    /// The workload was killed at `compaction.before_physical_delete` (inputs removed in the MANIFEST, files still there).
    RemovedNotDeleted,
    /// Clean stop followed by the clean-stop attestation the product host writes.
    Attested,
    /// `AbortEnd` plus a stale `wal-quarantine/*.tail.tmp`.
    StaleQuarantineTmp,
    /// Clean stop, complete workload (source database for backup/restore).
    Full,
    /// The relational fixture: catalog, table `t`, secondary index `t_name_idx`, clean stop (no rows).
    RelSetup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scen {
    Engine,
    Wal,
    Recover(Fixture),
    EmptyStart,
    Ops,
    /// The relational workload (rows + index entries) on a copy of the `RelSetup` fixture.
    Rel,
}

impl Scen {
    pub fn label(self) -> String {
        match self {
            Scen::Engine => "engine".into(),
            Scen::Wal => "bare-wal".into(),
            Scen::Recover(f) => format!("recover/{f:?}"),
            Scen::EmptyStart => "empty-start".into(),
            Scen::Ops => "ops".into(),
            Scen::Rel => "relational".into(),
        }
    }
}

pub fn scenario_of(p: Point) -> Option<Scen> {
    use Point::*;
    Some(match p {
        WalRecordWrite
        | WalRecordWritten
        | WalFsync
        | WalFsyncDone
        | WalSegmentTmpWrite
        | WalSegmentTmpSync
        | WalSegmentRename
        | WalSegmentDirSync
        | WalSegmentPublished
        | WalPurgeUnlink
        | WalPurgeDirSync
        | SstableTmpWrite
        | SstableTmpSync
        | SstableRename
        | SstableDirSync
        | ManifestWrite
        | ManifestSync
        | ManifestSynced
        | FlushBeforeSstableWrite
        | FlushAfterSstablePublish
        | FlushAfterRotate
        | FlushAfterCheckpointMarker
        | LsmBeforeApply
        | FlushAfterSetCheckpoint
        | CompactionBeforeOutputWrite
        | CompactionBeforeManifestAdd
        | CompactionAfterManifestAdd
        | CompactionDuringRemoveSequence
        | CompactionAfterAllRemoves
        | CompactionBeforePhysicalDelete
        | CompactionObsoleteUnlink => Scen::Engine,
        WalSync => Scen::Wal,
        StartupEngineWalReplayRecord
        | StartupEngineWalReplayed
        | StartupEngineWalOpened
        | StartupManifestOpened
        | StartupEngineOpened
        | StartupGuardWalPreflightRecord => Scen::Recover(Fixture::AbortEnd),
        WalTailTruncate | StartupQuarantineTempWritten | StartupQuarantineRenamed => {
            Scen::Recover(Fixture::TornWalTail)
        }
        ManifestTailTruncate => Scen::Recover(Fixture::TornManifestTail),
        LsmSweepTmpUnlink | StartupReconcileTmpSwept => Scen::Recover(Fixture::StaleSstTmp),
        StartupReconcileBeforeAdopt | StartupReconcileAfterAdopt => {
            Scen::Recover(Fixture::OrphanSst)
        }
        LsmSweepOrphanUnlink => Scen::Recover(Fixture::RemovedNotDeleted),
        StartupAttestationConsumed => Scen::Recover(Fixture::Attested),
        StartupQuarantineTmpSwept => Scen::Recover(Fixture::StaleQuarantineTmp),
        FormatStampRename | StartupFormatMarkerTempWritten => Scen::EmptyStart,
        BackupPartialSynced
        | BackupPublish
        | BackupPublished
        | RestorePromote
        | RestoreVerified
        | RestoreStagingCreated
        | RestoreEngineOpened
        | RestoreBatchApplied
        | RestoreLoaded
        | RestoreDigestVerified
        | RestoreCheckPassed
        | RestoreEngineClosed
        | RestorePromoted
        | RestoreMarkerRemoved => Scen::Ops,
        ManifestReplace | StartupHostReadySent => return None,
    })
}

pub const DEFAULT: Params = Params {
    seed: 1,
    writers: 1,
    ops: 300,
    keys: 24,
};

pub const CONCURRENT: Params = Params {
    seed: 7,
    writers: 4,
    ops: 100,
    keys: 16,
};

/// Relational workload parameters (single writer; `keys` = distinct primary keys).
pub const REL: Params = Params {
    seed: 11,
    writers: 1,
    ops: 150,
    keys: 12,
};

pub fn params_args(p: &Params) -> Vec<String> {
    vec![
        format!("seed={}", p.seed),
        format!("writers={}", p.writers),
        format!("ops={}", p.ops),
        format!("keys={}", p.keys),
    ]
}

pub fn read_ledgers(dir: &Path, p: &Params) -> Vec<WriterLedger> {
    (0..p.writers)
        .map(|w| workload::read_ledger(dir, w).unwrap_or_else(|e| panic!("ledger {w}: {e}")))
        .collect()
}

fn s(x: &Path) -> String {
    x.to_string_lossy().to_string()
}

// ---------------------------------------------------------------------------------------------------------------------
// fixture templates (built once, copied per case)
// ---------------------------------------------------------------------------------------------------------------------

pub struct Template {
    pub root: PathBuf,
    pub params: Params,
}

static TEMPLATES: OnceLock<Mutex<HashMap<Fixture, &'static Template>>> = OnceLock::new();

fn engine_args(data: &Path, ledger: &Path, p: &Params, extra: &[&str]) -> Vec<String> {
    let mut a = vec!["engine".to_string(), s(data), s(ledger)];
    a.extend(params_args(p));
    a.extend(extra.iter().map(|x| x.to_string()));
    a
}

pub fn template(fx: Fixture) -> &'static Template {
    let map = TEMPLATES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut g = map.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(t) = g.get(&fx) {
        return t;
    }
    let root = rig::fresh_case(&format!("template-{fx:?}"));
    let data = root.join("data");
    let ledger = root.join("ledger");
    let p = DEFAULT;
    let base_fx = match fx {
        Fixture::TornWalTail
        | Fixture::TornManifestTail
        | Fixture::StaleSstTmp
        | Fixture::StaleQuarantineTmp => Fixture::AbortEnd,
        other => other,
    };
    match base_fx {
        Fixture::AbortEnd => {
            let r = rig::run_child(
                &root,
                &engine_args(&data, &ledger, &p, &["end=abort"]),
                None,
                Stop::RunToEnd,
                None,
                false,
            );
            assert!(
                r.status.is_some_and(|st| !st.success()),
                "template {fx:?}: abort end expected, got {}",
                r.exit_text()
            );
            assert!(
                ledger.join("DONE").exists(),
                "template {fx:?}: workload did not complete"
            );
        }
        Fixture::Full => {
            let r = rig::run_child(
                &root,
                &engine_args(&data, &ledger, &p, &["end=clean"]),
                None,
                Stop::RunToEnd,
                None,
                false,
            );
            assert!(
                r.status.is_some_and(|st| st.success()),
                "template Full: {} {}",
                r.exit_text(),
                r.stderr_tail
            );
        }
        Fixture::RelSetup => {
            let args = vec!["rel-setup".to_string(), s(&data), s(&ledger)];
            let r = rig::run_child(&root, &args, None, Stop::RunToEnd, None, false);
            assert!(
                r.status.is_some_and(|st| st.success()),
                "template RelSetup: {} {}",
                r.exit_text(),
                r.stderr_tail
            );
        }
        Fixture::Attested => {
            let r = rig::run_child(
                &root,
                &engine_args(&data, &ledger, &p, &["end=clean", "attest=1"]),
                None,
                Stop::RunToEnd,
                None,
                false,
            );
            assert!(
                r.status.is_some_and(|st| st.success()),
                "template Attested: {} {}",
                r.exit_text(),
                r.stderr_tail
            );
            assert!(
                data.join("WAL_CLEAN_STOP").exists(),
                "template Attested: no attestation file was written"
            );
        }
        Fixture::OrphanSst => {
            let r = rig::run_child(
                &root,
                &engine_args(&data, &ledger, &p, &["end=exit"]),
                Some("sstable.dir_sync=nth=2:abort"),
                Stop::SelfAbort,
                Some("sstable.dir_sync"),
                false,
            );
            assert_eq!(
                r.outcome,
                Outcome::Fired,
                "template OrphanSst: {:?}",
                r.outcome
            );
        }
        Fixture::RemovedNotDeleted => {
            let r = rig::run_child(
                &root,
                &engine_args(&data, &ledger, &p, &["end=exit"]),
                Some("compaction.before_physical_delete=once:abort"),
                Stop::SelfAbort,
                Some("compaction.before_physical_delete"),
                false,
            );
            assert_eq!(
                r.outcome,
                Outcome::Fired,
                "template RemovedNotDeleted: {:?}",
                r.outcome
            );
        }
        _ => unreachable!(),
    }
    // Synthetic damage on top of the AbortEnd state.
    match fx {
        Fixture::TornWalTail => {
            let wal = data.join("wal");
            let mut segs: Vec<PathBuf> = fs::read_dir(&wal)
                .unwrap()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "log"))
                .collect();
            segs.sort();
            let last = segs.last().expect("a wal segment");
            use std::io::Write;
            let mut f = fs::OpenOptions::new().append(true).open(last).unwrap();
            // len = 1000, junk crc, 4 body bytes: the claimed extent runs past EOF => a torn tail, not corruption.
            f.write_all(&[0xE8, 0x03, 0, 0, 1, 2, 3, 4, 9, 9, 9, 9])
                .unwrap();
        }
        Fixture::TornManifestTail => {
            use std::io::Write;
            let mut f = fs::OpenOptions::new()
                .append(true)
                .open(data.join("MANIFEST"))
                .unwrap();
            // body_len 33 (an ADD_SSTABLE), junk crc, 3 body bytes: partial frame.
            f.write_all(&[33, 0, 0, 0, 1, 2, 3, 4, 1, 0, 0]).unwrap();
        }
        Fixture::StaleSstTmp => {
            fs::create_dir_all(data.join("sstables")).unwrap();
            fs::write(
                data.join("sstables").join("00000000000000000999.sst.tmp"),
                vec![7u8; 100],
            )
            .unwrap();
        }
        Fixture::StaleQuarantineTmp => {
            fs::create_dir_all(data.join("wal-quarantine")).unwrap();
            fs::write(
                data.join("wal-quarantine")
                    .join("wal-1.0.1234567890123.tail.tmp"),
                vec![7u8; 50],
            )
            .unwrap();
        }
        _ => {}
    }
    let t: &'static Template = Box::leak(Box::new(Template { root, params: p }));
    g.insert(fx, t);
    t
}

// ---------------------------------------------------------------------------------------------------------------------
// cases
// ---------------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Case {
    pub point: Point,
    pub scen: Scen,
    /// `once`, `nth=N` or `prob=P,seed=S`.
    pub trigger: String,
    pub stop: Stop,
    pub params: Params,
    /// `abort`, `pause`, or `error=other` (class A).
    pub action: String,
}

pub struct Prepared {
    pub work: PathBuf,
    pub data: PathBuf,
    pub ledger: PathBuf,
    pub out: PathBuf,
    pub args: Vec<String>,
    pub params: Params,
}

pub fn prepare(c: &Case, label: &str) -> Prepared {
    let work = rig::fresh_case(label);
    let data = work.join("data");
    let ledger = work.join("ledger");
    let out = work.join("out");
    let mut params = c.params;
    let args = match c.scen {
        Scen::Engine => engine_args(&data, &ledger, &params, &["end=exit"]),
        Scen::Wal => vec!["wal".into(), s(&data), s(&ledger), "ops=120".into()],
        Scen::EmptyStart => {
            params = Params {
                writers: 1,
                ops: 0,
                ..params
            };
            vec!["recover".into(), s(&data), s(&ledger)]
        }
        Scen::Recover(fx) => {
            let t = template(fx);
            rig::copy_dir(&t.root.join("data"), &data);
            rig::copy_dir(&t.root.join("ledger"), &ledger);
            params = t.params;
            vec!["recover".into(), s(&data), s(&ledger)]
        }
        Scen::Rel => {
            let t = template(Fixture::RelSetup);
            rig::copy_dir(&t.root.join("data"), &data);
            params = REL;
            let mut a = vec!["rel".to_string(), s(&data), s(&ledger)];
            a.extend(params_args(&params));
            a.push("end=exit".into());
            a
        }
        Scen::Ops => {
            let t = template(Fixture::Full);
            rig::copy_dir(&t.root.join("data"), &data);
            rig::copy_dir(&t.root.join("ledger"), &ledger);
            params = t.params;
            vec![
                "ops".into(),
                s(&data),
                s(&ledger),
                format!("out={}", s(&out)),
            ]
        }
    };
    Prepared {
        work,
        data,
        ledger,
        out,
        args,
        params,
    }
}

pub fn spec_of(c: &Case) -> String {
    format!("{}={}:{}", c.point.name(), c.trigger, c.action)
}

/// "Did the process stop mid-operation": no completion marker, and the process did not end normally.
pub fn distinguishable_from_clean_stop(scen: Scen, prep: &Prepared, run: &Run) -> (bool, String) {
    let marker = match scen {
        Scen::Recover(_) | Scen::EmptyStart => "RECOVERED",
        Scen::Ops => "OPS_DONE",
        _ => "DONE",
    };
    let marker_present = prep.ledger.join(marker).exists();
    let abnormal = run.killed_by_parent || run.status.is_some_and(|st| !st.success());
    (
        !marker_present && abnormal,
        format!(
            "{marker} marker {}; {}",
            if marker_present { "PRESENT" } else { "absent" },
            run.exit_text()
        ),
    )
}

// ---------------------------------------------------------------------------------------------------------------------
// judging
// ---------------------------------------------------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct Judged {
    pub accepted: bool,
    pub detail: String,
    pub report: Option<RecoveryReport>,
    pub notes: Vec<String>,
}

fn judge_wal(data: &Path, ledger: &Path) -> Judged {
    use rubixdb::wal::{replay_streaming, WalConfig, WalOp};
    let l = workload::read_ledger(ledger, 0).unwrap_or_else(|e| panic!("ledger: {e}"));
    let mut seen: Vec<(u64, Vec<u8>, Vec<u8>)> = Vec::new();
    let cfg = WalConfig::default();
    let summary = replay_streaming(data, &cfg, |seq, op| {
        if let WalOp::Put { key, value } = op {
            seen.push((seq, key.to_vec(), value.to_vec()));
        }
        Ok(())
    });
    let mut j = Judged::default();
    let summary = match summary {
        Ok(s) => s,
        Err(e) => {
            j.detail = format!("R9: replay failed: {e}");
            return j;
        }
    };
    let mut problems = Vec::new();
    if summary.corrupted_segments_count > 0 {
        problems.push(format!(
            "R9: {} corrupted segment(s) reported",
            summary.corrupted_segments_count
        ));
    }
    for (n, (seq, _, _)) in seen.iter().enumerate() {
        if *seq != n as u64 + 1 {
            problems.push(format!(
                "R3: recovered seqs not gap-free from 1 at index {n} (seq {seq})"
            ));
            break;
        }
    }
    if seen.len() < l.acked {
        problems.push(format!(
            "R1: {} acknowledged record(s) but only {} recovered",
            l.acked,
            seen.len()
        ));
    }
    if seen.len() > l.submitted {
        problems.push(format!(
            "R4: {} records recovered but only {} were ever submitted",
            seen.len(),
            l.submitted
        ));
    }
    for (i, (_, k, v)) in seen.iter().enumerate() {
        let wk = format!("k{i:05}");
        let wv = format!("value-{i:05}-{}", "x".repeat(40));
        if k != wk.as_bytes() || v != wv.as_bytes() {
            problems.push(format!("R4: record {i} differs from the submitted payload"));
            break;
        }
    }
    for (i, a) in l.ack_seqs.iter().enumerate() {
        if seen.get(i).map(|x| x.0) != Some(*a) {
            problems.push(format!(
                "R1: acknowledged seq {a} of op {i} is not what was recovered"
            ));
            break;
        }
    }
    j.accepted = problems.is_empty();
    j.detail = if problems.is_empty() {
        format!(
            "accepted: {} records recovered (acked {}, submitted {}), gap-free from 1, truncated={}",
            seen.len(),
            l.acked,
            l.submitted,
            summary.truncated
        )
    } else {
        problems.join(" | ")
    };
    j
}

fn full_ledgers(p: &Params) -> Vec<WriterLedger> {
    (0..p.writers)
        .map(|_| WriterLedger {
            submitted: p.ops,
            acked: p.ops,
            ack_seqs: Vec::new(),
            errored: 0,
        })
        .collect()
}

/// Opens `dir` like the product and returns the dump (no recovery oracle; for restored/derived databases).
fn open_dump(dir: &Path) -> Result<oracle::Dump, String> {
    let e = oracle::product_open(dir).map_err(|(_, m)| m)?;
    let d = oracle::dump(&e);
    let _ = e.shutdown();
    d
}

fn judge_ops(prep: &Prepared) -> Judged {
    let p = prep.params;
    let ledgers = read_ledgers(&prep.ledger, &p);
    let mut j = Judged::default();
    let mut problems: Vec<String> = Vec::new();

    // 1. the source database is untouched by a backup/restore that was cut short. The oracle's R6 probe writes a marker key,
    //    so it runs on a COPY; the retries below must see the source exactly as the interrupted operation left it.
    let src_copy = prep.work.join("source_copy");
    rig::copy_dir(&prep.data, &src_copy);
    let rep = oracle::recover_and_check(&src_copy, &p, &ledgers);
    if !rep.accepted() {
        problems.push(format!("source database: {}", rep.summary()));
    }
    j.notes.push(format!("source recovery: {}", rep.summary()));

    // 2. what is visible under the backup directory.
    let mut published: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = fs::read_dir(&prep.out) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.ends_with(".rbxbackup") {
                published.push(e.path());
            } else if name.ends_with(".partial") {
                j.notes.push(format!(
                    "leftover {name} ({} B)",
                    e.metadata().map(|m| m.len()).unwrap_or(0)
                ));
            } else if name != "restored" && !name.starts_with('.') {
                problems.push(format!("unexpected entry {name:?} next to the backup"));
            }
        }
    }
    let exp_ledgers = full_ledgers(&p);
    for b in &published {
        match rubixdb::ops::backup::verify_backup(b) {
            Err(e) => problems.push(format!(
                "R8: a file under a published backup name fails verification: {e}"
            )),
            Ok(v) => {
                j.notes
                    .push(format!("published backup verifies ({} entries)", v.entries));
                let tmp = prep.work.join("verify_restore");
                let _ = fs::remove_dir_all(&tmp);
                match rubixdb::ops::restore::restore_backup(
                    b,
                    &tmp,
                    &rubixdb::ops::restore::RestoreOptions::default(),
                ) {
                    Ok(_) => match open_dump(&tmp) {
                        Ok(d) => {
                            let v = oracle::check_state(&p, &exp_ledgers, &d);
                            if !v.ok() {
                                problems.push(format!(
                                    "published backup restores to the WRONG content: {}",
                                    v.summary()
                                ));
                            }
                        }
                        Err(e) => problems.push(format!(
                            "restored copy of a published backup cannot be read: {e}"
                        )),
                    },
                    Err(e) => {
                        problems.push(format!("a verified published backup does not restore: {e}"))
                    }
                }
            }
        }
    }
    // 3. the restore destination is either absent or complete and correct.
    let restored = prep.out.join("restored");
    if restored.exists() {
        match open_dump(&restored) {
            Ok(d) => {
                let v = oracle::check_state(&p, &exp_ledgers, &d);
                if v.ok() {
                    j.notes
                        .push("restored destination present and equals the source keyspace".into());
                } else {
                    problems.push(format!(
                        "R13: a partial/incorrect restore is visible at the destination: {}",
                        v.summary()
                    ));
                }
            }
            Err(e) => problems.push(format!(
                "R13: the restore destination exists but cannot be opened: {e}"
            )),
        }
    }
    // 4. a retry of what was interrupted must succeed (a failed operation never poisons the next).
    let bk = prep.out.join("bk1.rbxbackup");
    if !bk.exists() {
        let engine = oracle::product_open(&prep.data).map_err(|(_, m)| m);
        match engine {
            Ok(eng) => {
                let mut attempts = Vec::new();
                let mut ok = false;
                for _ in 0..2 {
                    match rubixdb::ops::backup::create_backup(&eng, &bk, &Default::default()) {
                        Ok(_) => {
                            ok = true;
                            break;
                        }
                        Err(e) => attempts.push(e.to_string()),
                    }
                }
                let _ = eng.shutdown();
                if ok {
                    j.notes.push(format!(
                        "backup retry succeeded after {} failed attempt(s) {:?}",
                        attempts.len(),
                        attempts
                    ));
                } else {
                    problems.push(format!(
                        "backup could not be re-created after the interruption: {attempts:?}"
                    ));
                }
            }
            Err(e) => problems.push(format!("source cannot be reopened for a retry: {e}")),
        }
    }
    if bk.exists() && !restored.exists() {
        match rubixdb::ops::restore::restore_backup(
            &bk,
            &restored,
            &rubixdb::ops::restore::RestoreOptions::default(),
        ) {
            Ok(_) => match open_dump(&restored) {
                Ok(d) => {
                    let v = oracle::check_state(&p, &exp_ledgers, &d);
                    if !v.ok() {
                        problems.push(format!(
                            "restore retry produced the wrong content: {}",
                            v.summary()
                        ));
                    } else {
                        j.notes
                            .push("restore retry succeeded and equals the source keyspace".into());
                    }
                }
                Err(e) => problems.push(format!("restore retry result unreadable: {e}")),
            },
            Err(e) => problems.push(format!("restore retry failed after the interruption: {e}")),
        }
    }
    j.accepted = problems.is_empty();
    j.detail = if problems.is_empty() {
        j.notes.join("; ")
    } else {
        problems.join(" | ")
    };
    j.report = Some(rep);
    j
}

pub fn judge(scen: Scen, prep: &Prepared) -> Judged {
    match scen {
        Scen::Wal => judge_wal(&prep.data, &prep.ledger),
        Scen::Ops => judge_ops(prep),
        Scen::Rel => {
            let ledger =
                workload::read_ledger(&prep.ledger, 0).unwrap_or_else(|e| panic!("ledger: {e}"));
            let template_data = template(Fixture::RelSetup).root.join("data");
            let pre = prep.work.join("pre_recovery");
            rig::copy_dir(&prep.data, &pre);
            let rep = crate::rel_oracle::recover_and_check_relational(
                &prep.data,
                &template_data,
                &prep.work,
                &prep.params,
                &ledger,
            );
            Judged {
                accepted: rep.accepted(),
                detail: rep.summary(),
                notes: rep.verdict.notes.clone(),
                report: Some(rep),
            }
        }
        _ => {
            let ledgers = read_ledgers(&prep.ledger, &prep.params);
            let pre = prep.work.join("pre_recovery");
            rig::copy_dir(&prep.data, &pre);
            let rep = oracle::recover_and_check(&prep.data, &prep.params, &ledgers);
            let mut detail = rep.summary();
            if !rep.accepted() {
                if let Some(rec) = &rep.recovered {
                    detail.push_str(&format!(
                        " || ATTRIBUTION: {}",
                        oracle::diagnose_lost_acks(&pre, &prep.params, &ledgers, rec)
                    ));
                }
            }
            Judged {
                accepted: rep.accepted(),
                detail,
                notes: vec![],
                report: Some(rep),
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// trace (hit counts per scenario) - used to choose "the Nth hit" honestly and by the coverage test
// ---------------------------------------------------------------------------------------------------------------------

pub type Counts = HashMap<(String, String), u64>;

static TRACES: OnceLock<Mutex<HashMap<String, &'static Counts>>> = OnceLock::new();

/// Runs the scenario once with tracing and no rule; returns `(point, scope) -> hits`. Cached per label.
pub fn trace(scen: Scen, params: Params) -> &'static Counts {
    let key = format!("{}/w{}o{}", scen.label(), params.writers, params.ops);
    let map = TRACES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut g = map.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(c) = g.get(&key) {
        return c;
    }
    let case = Case {
        point: Point::WalFsync,
        scen,
        trigger: "once".into(),
        stop: Stop::RunToEnd,
        params,
        action: "log".into(),
    };
    let prep = prepare(&case, &format!("trace-{key}"));
    let r = rig::run_child(&prep.work, &prep.args, None, Stop::RunToEnd, None, true);
    assert!(
        r.status.is_some_and(|st| st.success()),
        "trace run of {key} did not finish cleanly: {} {}",
        r.exit_text(),
        r.stderr_tail
    );
    let counts: &'static Counts = Box::leak(Box::new(r.hits.clone()));
    g.insert(key, counts);
    counts
}

pub fn total(counts: &Counts, point: &str) -> u64 {
    counts
        .iter()
        .filter(|((p, _), _)| p == point)
        .map(|(_, n)| *n)
        .sum()
}

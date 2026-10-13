//! Diagnostic, not a test of the engine: `RBX_INSPECT_CASE=<case dir> RBX_INSPECT_PARAMS=seed,writers,ops,keys
//! cargo test --features fault-points --test fault_points inspect_case -- --ignored --nocapture`
//! prints, per writer, the ledger, the recovered keyspace of a COPY of the case directory and how it differs from
//! `model(K)` for the K values around the acknowledged count.

use std::path::PathBuf;

use crate::oracle;
use crate::rig;
use crate::workload::{self, apply, gen_op, model_prefix, Params};

#[test]
#[ignore]
fn inspect_case() {
    let Ok(dir) = std::env::var("RBX_INSPECT_CASE") else {
        return;
    };
    let dir = PathBuf::from(dir);
    let pv: Vec<usize> = std::env::var("RBX_INSPECT_PARAMS")
        .unwrap_or_else(|_| "7,4,100,16".into())
        .split(',')
        .map(|x| x.parse().unwrap())
        .collect();
    let p = Params {
        seed: pv[0] as u64,
        writers: pv[1],
        ops: pv[2],
        keys: pv[3],
    };
    let copy = dir.parent().unwrap().join("inspect_copy");
    let _ = std::fs::remove_dir_all(&copy);
    rig::copy_dir(&dir.join("data"), &copy);
    let e = oracle::product_open(&copy)
        .map_err(|x| x.1)
        .expect("open copy");
    println!(
        "recovery_stats: {:?}  checkpoint_seq={}",
        e.recovery_stats(),
        e.checkpoint_seq()
    );
    let got = oracle::dump(&e).expect("dump");
    let _ = e.shutdown();
    for w in 0..p.writers {
        let l = workload::read_ledger(&dir.join("ledger"), w).unwrap();
        println!(
            "--- writer {w}: submitted={} acked={} last_ack_seqs={:?}",
            l.submitted,
            l.acked,
            &l.ack_seqs[l.ack_seqs.len().saturating_sub(3)..]
        );
        let pre = workload::writer_prefix(w);
        let g: oracle::Dump = got
            .iter()
            .filter(|(k, _)| k.starts_with(&pre))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for k in l.acked.saturating_sub(2)..=l.submitted {
            let m = model_prefix(&p, w, k);
            let mut diffs = Vec::new();
            for (key, v) in &m {
                match g.get(key) {
                    None => diffs.push(format!("MISSING {}", String::from_utf8_lossy(key))),
                    Some(x) if x != v => {
                        diffs.push(format!("DIFF {}", String::from_utf8_lossy(key)))
                    }
                    _ => {}
                }
            }
            for key in g.keys() {
                if !m.contains_key(key) {
                    diffs.push(format!("EXTRA {}", String::from_utf8_lossy(key)));
                }
            }
            println!("  vs model({k}): {} diffs {:?}", diffs.len(), diffs);
        }
        // For every key that differs from model(acked): the last operation that touched it and its acknowledged seq.
        let m = model_prefix(&p, w, l.acked);
        let mut keys: Vec<Vec<u8>> = m.keys().chain(g.keys()).cloned().collect();
        keys.sort();
        keys.dedup();
        for key in keys {
            if m.get(&key) != g.get(&key) {
                let mut last = None;
                for i in 0..l.acked {
                    if gen_op(&p, w, i).iter().any(|it| match it {
                        workload::Item::Put(k, _) | workload::Item::Del(k) => *k == key,
                    }) {
                        last = Some(i);
                    }
                }
                println!(
                    "  DIFFERING KEY {}: expected {} got {}; last op touching it = {:?}, its acknowledged seq = {:?}",
                    String::from_utf8_lossy(&key),
                    m.get(&key).map(|v| format!("{}B", v.len())).unwrap_or("absent".into()),
                    g.get(&key).map(|v| format!("{}B", v.len())).unwrap_or("absent".into()),
                    last,
                    last.map(|i| l.ack_seqs[i])
                );
            }
        }
        for i in l.acked.saturating_sub(3)..l.submitted {
            let op = gen_op(&p, w, i);
            let desc: Vec<String> = op
                .iter()
                .map(|it| match it {
                    workload::Item::Put(k, v) => {
                        format!("PUT {} ({}B)", String::from_utf8_lossy(k), v.len())
                    }
                    workload::Item::Del(k) => format!("DEL {}", String::from_utf8_lossy(k)),
                })
                .collect();
            let mut m = model_prefix(&p, w, i);
            let before = m.clone();
            apply(&mut m, &op);
            println!("  op {i}: {desc:?} (changes state: {})", before != m);
        }
    }
}

/// Prints every record of one SSTable for one key prefix (diagnostic).
#[test]
#[ignore]
fn inspect_sstable_key() {
    let Ok(path) = std::env::var("RBX_INSPECT_SST") else {
        return;
    };
    let key = std::env::var("RBX_INSPECT_KEY").unwrap_or_default();
    let id: u64 = std::env::var("RBX_INSPECT_SST_ID")
        .ok()
        .and_then(|x| x.parse().ok())
        .unwrap_or(0);
    let t = rubixdb::sstable::SsTable::open(std::path::Path::new(&path), id).expect("open sst");
    println!(
        "sst {id}: min_seq={} max_seq={} records={} blocks={}",
        t.min_seq(),
        t.max_seq(),
        t.record_count(),
        t.block_count()
    );
    println!(
        "get_versioned({key}) = {:?}",
        t.get_versioned(key.as_bytes(), u64::MAX)
            .map(|o| o.map(|(s, v)| (s, format!("{v:?}").len())))
    );
}

/// Diagnostic: repeat the concurrent workload with NO fault rule, end it with `abort` (after background work settles) and
/// recover. Isolates whether an acknowledged write can be lost without any injected fault, and whether compaction matters.
/// `RBX_HUNT_RUNS` (default 12), `RBX_HUNT_COMPACTION` = on|off (default on), `RBX_HUNT_WRITERS` (default 4).
#[test]
#[ignore]
fn hunt_lost_ack_without_faults() {
    let runs: usize = std::env::var("RBX_HUNT_RUNS")
        .ok()
        .and_then(|x| x.parse().ok())
        .unwrap_or(12);
    let comp = std::env::var("RBX_HUNT_COMPACTION").unwrap_or_else(|_| "on".into());
    let writers: usize = std::env::var("RBX_HUNT_WRITERS")
        .ok()
        .and_then(|x| x.parse().ok())
        .unwrap_or(4);
    let mut bad = 0;
    for r in 0..runs {
        let p = Params {
            seed: 100 + r as u64,
            writers,
            ops: 100,
            keys: 16,
        };
        let work = rig::fresh_case(&format!("hunt-{comp}-w{writers}-{r}"));
        let data = work.join("data");
        let ledger = work.join("ledger");
        let mut args = vec![
            "engine".to_string(),
            data.to_string_lossy().to_string(),
            ledger.to_string_lossy().to_string(),
        ];
        args.extend(crate::scen::params_args(&p));
        args.push(format!(
            "end={}",
            std::env::var("RBX_HUNT_END").unwrap_or_else(|_| "abort".into())
        ));
        args.push(format!("compaction={comp}"));
        let run = rig::run_child(&work, &args, None, rig::Stop::RunToEnd, None, false);
        assert!(
            ledger.join("DONE").exists(),
            "workload did not finish: {} {}",
            run.exit_text(),
            run.stderr_tail
        );
        let ledgers = crate::scen::read_ledgers(&ledger, &p);
        let rep = oracle::recover_and_check(&data, &p, &ledgers);
        if rep.accepted() {
            let _ = std::fs::remove_dir_all(&work);
        } else {
            bad += 1;
            println!(
                "run {r}: REJECTED {} (dir kept: {})",
                rep.summary(),
                work.display()
            );
        }
    }
    println!("hunt: compaction={comp} writers={writers}: {bad} of {runs} runs lost acknowledged data or were otherwise rejected");
}

/// Diagnostic: explain WHY the last acknowledged operation on one key is not in the recovered state. For the operation:
/// its acknowledged seq, the manifest checkpoint, whether the WAL still holds a record with that seq touching the key, and
/// which live SSTables hold a version of the key at exactly that seq. (`RBX_INSPECT_CASE`, `RBX_INSPECT_PARAMS`,
/// `RBX_INSPECT_WRITER`, `RBX_INSPECT_KEYNO`.)
#[test]
#[ignore]
fn explain_lost_ack() {
    let Ok(dir) = std::env::var("RBX_INSPECT_CASE") else {
        return;
    };
    let dir = PathBuf::from(dir);
    let pv: Vec<usize> = std::env::var("RBX_INSPECT_PARAMS")
        .unwrap_or_else(|_| "7,4,100,16".into())
        .split(',')
        .map(|x| x.parse().unwrap())
        .collect();
    let p = Params {
        seed: pv[0] as u64,
        writers: pv[1],
        ops: pv[2],
        keys: pv[3],
    };
    let w: usize = std::env::var("RBX_INSPECT_WRITER")
        .unwrap()
        .parse()
        .unwrap();
    let kn: usize = std::env::var("RBX_INSPECT_KEYNO").unwrap().parse().unwrap();
    let key = workload::key(w, kn);
    let l = workload::read_ledger(&dir.join("ledger"), w).unwrap();
    let mut last = None;
    for i in 0..l.acked {
        if gen_op(&p, w, i).iter().any(|it| match it {
            workload::Item::Put(k, _) | workload::Item::Del(k) => *k == key,
        }) {
            last = Some(i);
        }
    }
    let i = last.expect("no operation touches the key");
    let seq = l.ack_seqs[i];
    println!(
        "key {} : last acknowledged op = {i} (seq {seq}): {:?}",
        String::from_utf8_lossy(&key),
        gen_op(&p, w, i)
            .iter()
            .map(|it| match it {
                workload::Item::Put(k, v) =>
                    format!("PUT {} {}B", String::from_utf8_lossy(k), v.len()),
                workload::Item::Del(k) => format!("DEL {}", String::from_utf8_lossy(k)),
            })
            .collect::<Vec<_>>()
    );
    let copy = dir.parent().unwrap().join("explain_copy");
    let _ = std::fs::remove_dir_all(&copy);
    rig::copy_dir(&dir.join("data"), &copy);
    let m = rubixdb::manifest::replay_readonly(&copy).unwrap();
    let cp = m
        .state
        .checkpoint
        .map(|c| c.flushed_through_seq)
        .unwrap_or(0);
    println!(
        "manifest checkpoint = {cp}; live sstables = {:?}",
        m.state
            .live_sstables
            .iter()
            .map(|(id, e)| (*id, e.min_seq, e.max_seq))
            .collect::<Vec<_>>()
    );
    let mut in_wal = false;
    let mut wal_seqs = 0u64;
    let cfg = rubixdb::wal::WalConfig::default();
    let _ = rubixdb::wal::replay_streaming(&copy, &cfg, |s, op| {
        wal_seqs += 1;
        let touches = match &op {
            rubixdb::wal::WalOp::Put { key: k, .. } | rubixdb::wal::WalOp::Delete { key: k } => {
                *k == key.as_slice()
            }
            rubixdb::wal::WalOp::Group { members } => members.iter().any(|m| match m {
                rubixdb::wal::GroupMember::Put { key: k, .. }
                | rubixdb::wal::GroupMember::Delete { key: k } => *k == key.as_slice(),
            }),
            _ => false,
        };
        if s == seq && touches {
            in_wal = true;
        }
        Ok(())
    });
    println!("WAL still holds a record at seq {seq} touching the key: {in_wal} (WAL records visible: {wal_seqs})");
    for id in m.state.live_sstables.keys() {
        let path = copy.join("sstables").join(format!("{id:020}.sst"));
        let t = rubixdb::sstable::SsTable::open(&path, *id).unwrap();
        let at = t.get_versioned(&key, seq).unwrap().map(|(s, _)| s);
        let newest = t.get_versioned(&key, u64::MAX).unwrap().map(|(s, _)| s);
        println!("  sstable {id}: newest version of the key = {newest:?}; newest at-or-below seq {seq} = {at:?}  -> holds exactly seq {seq}: {}", at == Some(seq));
    }
    println!("=> if checkpoint ({cp}) >= seq ({seq}), the WAL record is skipped at recovery; if no sstable holds seq {seq}, the acknowledged write exists nowhere else.");
}

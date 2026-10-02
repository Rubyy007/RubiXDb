//! **Independent WAL durability oracle (analysis/certification tool, not part
//! of RubiXDB).** Verifies the one property that matters for any group-commit
//! change, against a reference that does NOT use the WAL as its own oracle:
//!
//! > every record whose acknowledgement was observed OUTSIDE the process
//! > before it was killed is present, byte-exact, after recovery.
//!
//! Design:
//! * The child (re-exec of this binary with `child`) runs W writer threads
//!   against `GroupCommitter::append_durable`. A writer prints an ack line to
//!   its stdout pipe ONLY AFTER `append_durable` returned `Ok`, so a line the
//!   parent has *received* proves the WAL had acknowledged that record.
//! * The parent kills the child abruptly at a random (seeded) moment
//!   (`Child::kill()`, `TerminateProcess`: no cooperation, may land
//!   mid-syscall), then reopens the WAL and checks, using payloads it derives
//!   itself from `(writer, n)` -- never read back from the WAL under test:
//!     1. no corrupted segments; recovered seqs are gap-free from 1;
//!     2. every received ack is recovered, at its acked seq, with the exact
//!        expected key/value (and, for group writes, all 3 members, in order);
//!     3. per writer, recovered operations form a contiguous prefix 0..m, and
//!        m <= (last received ack) + slack (only in-flight ops may exceed it);
//!     4. group writes are atomic: never a partial group.
//! * Cycles accumulate in the same directory; writer ids are unique per cycle.
//!
//! Usage: `wal_ack_oracle <cycles> <writers> [seed=1] [max_wait_us=5000]`

use std::collections::{BTreeMap, HashMap};
use std::env;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use rubixdb::wal::{
    FileWal, GroupCommitter, GroupMemberOwned, SyncMode, Wal, WalConfig, WalOpOwned,
};

fn config(max_wait_us: u64) -> WalConfig {
    WalConfig {
        sync_mode: SyncMode::GroupCommit {
            max_wait: Duration::from_micros(max_wait_us),
            max_batch_bytes: 256 * 1024,
        },
        ..WalConfig::default()
    }
}

/// Expected payload for writer `w`, op `n` (independent of the WAL).
fn expected_op(w: u64, n: u64) -> WalOpOwned {
    if w % 4 == 3 {
        WalOpOwned::Group(
            (0..3)
                .map(|m| GroupMemberOwned::Put {
                    key: format!("g{w}-{n}-{m}").into_bytes(),
                    value: format!("gv:{w}:{n}:{m}").into_bytes(),
                })
                .collect(),
        )
    } else {
        WalOpOwned::Put {
            key: format!("k{w}-{n}").into_bytes(),
            value: format!("v:{w}:{n}").into_bytes(),
        }
    }
}

fn child(dir: PathBuf, writers: usize, cycle: u64, max_wait_us: u64) {
    let (wal, _) = FileWal::open_for_recovery(&dir, config(max_wait_us)).unwrap();
    let committer = Arc::new(GroupCommitter::new(wal).unwrap());
    let early = env::var_os("ACK_EARLY").is_some();
    for t in 0..writers {
        let c = Arc::clone(&committer);
        let w = cycle * 10_000 + t as u64;
        thread::spawn(move || {
            let mut n = 0u64;
            loop {
                let op = expected_op(w, n);
                if early {
                    // MUTANT (oracle self-test only): acknowledge BEFORE the record is
                    // even written to the OS, i.e. while it lives only in process memory.
                    println!("A {w} {n} 0");
                    let _ = c.append_durable(op.as_wal_op());
                } else if let Ok(pos) = c.append_durable(op.as_wal_op()) {
                    // Printed strictly AFTER the WAL acknowledged the record.
                    println!("A {w} {n} {}", pos.seq);
                }
                n += 1;
            }
        });
    }
    thread::sleep(Duration::from_secs(3600));
}

fn main() {
    let a: Vec<String> = env::args().collect();
    if a.get(1).map(String::as_str) == Some("child") {
        child(
            PathBuf::from(&a[2]),
            a[3].parse().unwrap(),
            a[4].parse().unwrap(),
            a[5].parse().unwrap(),
        );
        return;
    }
    let cycles: u64 = a[1].parse().unwrap();
    let writers: usize = a[2].parse().unwrap();
    let mut seed: u64 = a.get(3).map(|s| s.parse().unwrap()).unwrap_or(1);
    let max_wait_us: u64 = a.get(4).map(|s| s.parse().unwrap()).unwrap_or(5000);
    let base = env::var("RUBIXDB_SOAK_BASE_DIR")
        .unwrap_or_else(|_| env::temp_dir().to_string_lossy().into());
    let dir = PathBuf::from(base).join(format!("wal_ack_oracle_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let exe = env::current_exe().unwrap();
    let mut rng = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut failures = 0u32;
    let mut total_acks = 0u64;
    let mut total_recovered = 0u64;

    for cycle in 1..=cycles {
        let mut ch = Command::new(&exe)
            .args([
                "child",
                dir.to_str().unwrap(),
                &writers.to_string(),
                &cycle.to_string(),
                &max_wait_us.to_string(),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = ch.stdout.take().unwrap();
        let acks: Arc<Mutex<Vec<(u64, u64, u64)>>> = Arc::new(Mutex::new(Vec::new()));
        let acks2 = Arc::clone(&acks);
        let reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let p: Vec<&str> = line.split(' ').collect();
                if p.len() == 4 && p[0] == "A" {
                    if let (Ok(w), Ok(n), Ok(s)) = (p[1].parse(), p[2].parse(), p[3].parse()) {
                        acks2.lock().unwrap().push((w, n, s));
                    }
                }
            }
        });
        // Random kill moment: 150..2500 ms, so kills land in every phase.
        thread::sleep(Duration::from_millis(150 + rng() % 2350));
        let _ = ch.kill();
        let _ = ch.wait();
        let _ = reader.join();
        let acks = acks.lock().unwrap().clone();

        let (wal, replay) = FileWal::open_for_recovery(&dir, config(max_wait_us)).unwrap();
        drop(wal);
        let mut problems: Vec<String> = Vec::new();
        if !replay.corrupted_segments.is_empty() {
            problems.push(format!(
                "corrupted_segments={:?}",
                replay.corrupted_segments
            ));
        }
        for (i, (seq, _)) in replay.records.iter().enumerate() {
            if *seq != i as u64 + 1 {
                problems.push(format!("gap: position {i} has seq {seq}"));
                break;
            }
        }
        // (2) every received ack is recovered with the exact payload (matched by
        // (writer, n), derived independently of the WAL); seq must match when known.
        let mut by_wn: HashMap<(u64, u64), (u64, &WalOpOwned)> = HashMap::new();
        for (s, op) in &replay.records {
            let key = match op {
                WalOpOwned::Put { key, .. } => Some(key.clone()),
                WalOpOwned::Group(m) => match &m[0] {
                    GroupMemberOwned::Put { key, .. } => Some(key.clone()),
                    _ => None,
                },
                _ => None,
            };
            if let Some(k) = key {
                let k = String::from_utf8_lossy(&k).to_string();
                let r: Vec<&str> = k[1..].split('-').collect();
                if let (Ok(w), Ok(n)) = (r[0].parse::<u64>(), r[1].parse::<u64>()) {
                    by_wn.insert((w, n), (*s, op));
                }
            }
        }
        for &(w, n, s) in &acks {
            match by_wn.get(&(w, n)) {
                None => problems.push(format!(
                    "ACKED RECORD LOST: writer {w} op {n} (acked seq {s})"
                )),
                Some((rs, op)) => {
                    if **op != expected_op(w, n) {
                        problems.push(format!("ACKED RECORD CORRUPT: writer {w} op {n}"));
                    }
                    if s != 0 && *rs != s {
                        problems.push(format!(
                            "ACKED SEQ MISMATCH: writer {w} op {n} acked {s} recovered {rs}"
                        ));
                    }
                }
            }
        }
        // (3)+(4) per-writer contiguous prefix; recovered ops are whole and match.
        let mut recovered_ops: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
        let mut cycle_recovered = 0u64;
        for (_, op) in &replay.records {
            let (w, n) = match op {
                WalOpOwned::Put { key, .. } => {
                    let k = String::from_utf8_lossy(key);
                    let r: Vec<&str> = k[1..].split('-').collect();
                    (
                        r[0].parse::<u64>().unwrap_or(u64::MAX),
                        r.get(1).and_then(|x| x.parse().ok()).unwrap_or(u64::MAX),
                    )
                }
                WalOpOwned::Group(m) => {
                    let GroupMemberOwned::Put { key, .. } = &m[0] else {
                        continue;
                    };
                    let k = String::from_utf8_lossy(key);
                    let r: Vec<&str> = k[1..].split('-').collect();
                    if m.len() != 3 {
                        problems.push(format!("PARTIAL GROUP ({} members) {k}", m.len()));
                    }
                    (
                        r[0].parse::<u64>().unwrap_or(u64::MAX),
                        r.get(1).and_then(|x| x.parse().ok()).unwrap_or(u64::MAX),
                    )
                }
                _ => continue,
            };
            if w / 10_000 != cycle && !(w / 10_000 >= 1 && w / 10_000 < cycle) {
                problems.push(format!("unexpected writer id {w}"));
            }
            recovered_ops.entry(w).or_default().push(n);
            if w / 10_000 == cycle {
                cycle_recovered += 1;
            }
        }
        let mut last_ack: HashMap<u64, u64> = HashMap::new();
        for &(w, n, _) in &acks {
            let e = last_ack.entry(w).or_insert(0);
            *e = (*e).max(n);
        }
        for (w, ns) in &recovered_ops {
            let mut sorted = ns.clone();
            sorted.sort_unstable();
            for (i, n) in sorted.iter().enumerate() {
                if *n != i as u64 {
                    problems.push(format!(
                        "writer {w}: recovered ops not a contiguous prefix at {i} (found {n})"
                    ));
                    break;
                }
            }
            if let Some(la) = last_ack.get(w) {
                // Recovered may exceed the last RECEIVED ack by in-flight/pipe-lag ops only.
                if (sorted.len() as u64) > la + 1 + 8 + 1 {
                    // generous bound: a writer has at most one op in flight; lag is pipe buffering
                    problems.push(format!(
                        "writer {w}: recovered {} ops but last received ack was op {la}",
                        sorted.len()
                    ));
                }
            }
        }
        total_acks += acks.len() as u64;
        total_recovered += replay.records.len() as u64;
        if problems.is_empty() {
            println!(
                "oracle cycle {cycle:3} OK  acks_received={:6} recovered_this_cycle={cycle_recovered:6} total_recovered={}",
                acks.len(),
                replay.records.len()
            );
        } else {
            failures += 1;
            println!("oracle cycle {cycle:3} FAIL");
            for p in problems.iter().take(8) {
                println!("    {p}");
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    println!("SUMMARY cycles={cycles} writers={writers} failures={failures} total_acks_verified={total_acks} final_recovered={total_recovered}");
    std::process::exit(if failures == 0 { 0 } else { 1 });
}

//! Independent recovery oracle (D1a, P11). "Independent" means: the expected state is regenerated from the seed
//! (`workload.rs`) and from the child's *ledger* of what it submitted and what the engine acknowledged; nothing about the
//! expected state is read from the engine, the WAL, the MANIFEST or any SSTable under test. The recovered keyspace is read
//! through the engine's public scan and compared key by key, value by value - never by row count.
//!
//! What it can and cannot see (stated so nobody reads more into a pass):
//! * R1-R4, R12: yes - the recovered state must equal `model(K)` for a per-writer prefix K with `acked <= K <= submitted`.
//! * R5 (no double application): NOT independently observable here (puts/deletes are idempotent); only the engine's own
//!   `RecoveryStats` identity is checked.
//! * R6: the sequence number the first post-recovery write gets must exceed every acknowledged seq and the checkpoint.
//! * R7/R8: the existing read-only physical checker on the pre- and post-recovery directory, plus "open succeeded".
//! * R10: a second recovery of the recovered directory yields the identical keyspace.
//! * R11: leftover `*.tmp` files after open.
//! * R9/R13 are exercised by the fixtures in `tests/fault_points/`, not by this module.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::Path;

use crate::workload::{apply, gen_op, key, writer_prefix, Item, Params, WriterLedger};
use rubixdb::lsm::{LsmConfig, LsmEngine};
use rubixdb::ops::open::{product_pool_config, product_wal_config};

pub type Dump = BTreeMap<Vec<u8>, Vec<u8>>;

#[derive(Debug, Clone)]
pub struct Violation {
    pub invariant: &'static str,
    pub detail: String,
}

#[derive(Debug, Default, Clone)]
pub struct Verdict {
    pub violations: Vec<Violation>,
    /// The per-writer prefix length K the recovered state matched (when it matched).
    pub chosen_k: Vec<Option<usize>>,
    pub notes: Vec<String>,
}

impl Verdict {
    pub fn ok(&self) -> bool {
        self.violations.is_empty()
    }

    fn push(&mut self, invariant: &'static str, detail: String) {
        self.violations.push(Violation { invariant, detail });
    }

    pub fn summary(&self) -> String {
        if self.ok() {
            format!("accepted (K per writer = {:?})", self.chosen_k)
        } else {
            self.violations
                .iter()
                .map(|v| format!("{}: {}", v.invariant, v.detail))
                .collect::<Vec<_>>()
                .join(" | ")
        }
    }
}

fn first_diff(expected: &Dump, got: &Dump) -> String {
    for (k, v) in expected {
        match got.get(k) {
            None => return format!("missing key {:?}", String::from_utf8_lossy(k)),
            Some(g) if g != v => {
                return format!("key {:?} has a different value", String::from_utf8_lossy(k))
            }
            _ => {}
        }
    }
    for k in got.keys() {
        if !expected.contains_key(k) {
            return format!("unexpected key {:?}", String::from_utf8_lossy(k));
        }
    }
    "no difference".into()
}

fn diff_count(a: &Dump, b: &Dump) -> usize {
    let mut n = 0;
    for (k, v) in a {
        if b.get(k) != Some(v) {
            n += 1;
        }
    }
    for k in b.keys() {
        if !a.contains_key(k) {
            n += 1;
        }
    }
    n
}

/// Checks a recovered keyspace against the ledgers. Every writer owns the keys `w<NN>-*`.
pub fn check_state(p: &Params, ledgers: &[WriterLedger], recovered: &Dump) -> Verdict {
    let mut v = Verdict::default();
    // Stray keys outside every writer's space (the oracle's own marker key is never in a dump).
    let prefixes: Vec<Vec<u8>> = (0..p.writers).map(writer_prefix).collect();
    for k in recovered.keys() {
        if !prefixes.iter().any(|pre| k.starts_with(pre)) {
            v.push(
                "R4",
                format!(
                    "phantom key {:?} belongs to no writer",
                    String::from_utf8_lossy(k)
                ),
            );
        }
    }
    for (w, prefix) in prefixes.iter().enumerate() {
        let l = ledgers.get(w).cloned().unwrap_or_default();
        if l.submitted > p.ops || l.acked > l.submitted {
            v.push("LEDGER", format!("writer {w}: ledger inconsistent ({l:?})"));
            v.chosen_k.push(None);
            continue;
        }
        let got: Dump = recovered
            .iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .map(|(k, x)| (k.clone(), x.clone()))
            .collect();

        // model(K) for K = 0..=submitted, incrementally.
        let mut model = Dump::new();
        let mut in_range: Option<usize> = None;
        let mut below: Option<usize> = None;
        let mut best: (usize, usize) = (usize::MAX, 0); // (diff, K)
        let mut all_values: BTreeMap<Vec<u8>, Vec<Vec<u8>>> = BTreeMap::new();
        for k in 0..=l.submitted {
            let d = diff_count(&model, &got);
            if d == 0 {
                if k >= l.acked {
                    in_range.get_or_insert(k);
                } else {
                    below = Some(k);
                }
            }
            if d < best.0 {
                best = (d, k);
            }
            if k < l.submitted {
                let op = gen_op(p, w, k);
                for it in &op {
                    if let Item::Put(key, val) = it {
                        all_values.entry(key.clone()).or_default().push(val.clone());
                    }
                }
                apply(&mut model, &op);
            }
        }
        // An exact match below `acked` means acknowledged commits were lost; none at all means the state is not a prefix.
        match in_range.map(Ok).or(below.map(Err)) {
            Some(Ok(k)) => v.chosen_k.push(Some(k)),
            Some(Err(k)) => {
                v.chosen_k.push(Some(k));
                v.push(
                    "R1",
                    format!(
                        "writer {w}: recovered state equals the state after {k} commits but {} were acknowledged \
                         ({} acknowledged commit(s) lost)",
                        l.acked,
                        l.acked - k
                    ),
                );
            }
            None => {
                v.chosen_k.push(None);
                let want = crate::workload::model_prefix(p, w, best.1);
                let mut detail = format!(
                    "writer {w}: recovered state matches no prefix (closest K={}, {} differing keys; acked={}, submitted={}): {}",
                    best.1,
                    best.0,
                    l.acked,
                    l.submitted,
                    first_diff(&want, &got)
                );
                // Classify. Order matters: a value or key no submitted operation ever produced is R4 whatever else is true.
                let mut inv: Option<&'static str> = None;
                for (k, g) in &got {
                    match all_values.get(k) {
                        None => {
                            inv = Some("R4");
                            detail.push_str(" [phantom key: no submitted operation ever wrote it]");
                            break;
                        }
                        Some(vals) if !vals.contains(g) => {
                            inv = Some("R4");
                            detail.push_str(
                                " [malformed value: not any value ever submitted for this key]",
                            );
                            break;
                        }
                        _ => {}
                    }
                }
                // A partial commit unit: the state is `model(K)` with a strict, non-empty subset of the next unit applied.
                if inv.is_none() {
                    let mut base = Dump::new();
                    'units: for k in 0..l.submitted {
                        let next = gen_op(p, w, k);
                        let n = next.len();
                        if n > 1 {
                            for mask in 1..(1u32 << n) - 1 {
                                let mut m = base.clone();
                                let part: Vec<Item> = next
                                    .iter()
                                    .enumerate()
                                    .filter(|(j, _)| mask & (1 << j) != 0)
                                    .map(|(_, it)| it.clone())
                                    .collect();
                                apply(&mut m, &part);
                                if m == got {
                                    inv = Some("R2");
                                    detail.push_str(&format!(" [commit unit {k} applied partially (items mask {mask:b} of {n})]"));
                                    break 'units;
                                }
                            }
                        }
                        apply(&mut base, &next);
                    }
                }
                if inv.is_none() {
                    // Missing or stale/resurrected data relative to the closest prefix: an acknowledged effect is not there.
                    let missing_or_stale = want.iter().any(|(k, x)| got.get(k) != Some(x))
                        || got.keys().any(|k| !want.contains_key(k));
                    inv = Some(if best.1 < l.acked || missing_or_stale {
                        "R1"
                    } else {
                        "R2/R3"
                    });
                }
                v.push(inv.unwrap_or("R2/R3"), detail);
            }
        }
    }
    v
}

/// R6: the first write after recovery must be numbered above every acknowledged seq and above the checkpoint.
pub fn check_seq(post_seq: u64, max_acked_seq: u64, checkpoint_seq: u64) -> Option<Violation> {
    if post_seq <= max_acked_seq || post_seq <= checkpoint_seq {
        Some(Violation {
            invariant: "R6",
            detail: format!(
                "first post-recovery write was numbered {post_seq}, not above max acknowledged seq {max_acked_seq} \
                 and checkpoint {checkpoint_seq}"
            ),
        })
    } else {
        None
    }
}

#[derive(Debug, Default, Clone)]
pub struct RecoveryReport {
    /// The product start sequence refused to start (fail-closed); the message.
    pub refused: Option<String>,
    /// The engine could not be opened after the guard accepted.
    pub open_error: Option<String>,
    pub verdict: Verdict,
    pub pre_wal_corrupted_segments: usize,
    pub pre_wal_truncated: bool,
    pub pre_physical_errors: u64,
    pub pre_physical_warnings: u64,
    pub post_physical_errors: u64,
    pub recovery_stats: String,
    pub checkpoint_seq: u64,
    pub post_seq: Option<u64>,
    pub idempotent: Option<bool>,
    pub tmp_files_after_open: usize,
    pub extra: Vec<Violation>,
    /// The recovered keyspace (first recovery), kept so a rejection can be attributed.
    pub recovered: Option<Dump>,
}

impl RecoveryReport {
    /// Accepted = recovered, no oracle violation, no extra violation.
    pub fn accepted(&self) -> bool {
        self.refused.is_none()
            && self.open_error.is_none()
            && self.verdict.ok()
            && self.extra.is_empty()
    }

    pub fn summary(&self) -> String {
        if let Some(r) = &self.refused {
            return format!("REFUSED (fail-closed): {r}");
        }
        if let Some(e) = &self.open_error {
            return format!("OPEN FAILED: {e}");
        }
        let mut s = self.verdict.summary();
        for x in &self.extra {
            s.push_str(&format!(" | {}: {}", x.invariant, x.detail));
        }
        s
    }
}

pub fn recovery_lsm_config() -> LsmConfig {
    LsmConfig {
        compaction_auto_trigger: false,
        ..LsmConfig::default()
    }
}

pub fn dump(engine: &LsmEngine) -> Result<Dump, String> {
    let mut out = Dump::new();
    for item in engine.range(Bound::Unbounded, Bound::Unbounded) {
        let (k, v) = item.map_err(|e| format!("scan error: {e}"))?;
        out.insert(k, v);
    }
    Ok(out)
}

fn count_tmp_files(dir: &Path) -> usize {
    let mut n = 0;
    for sub in ["sstables"] {
        if let Ok(rd) = std::fs::read_dir(dir.join(sub)) {
            n += rd
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
                .count();
        }
    }
    n
}

/// Opens like the product host does: format decision + WAL preflight + tail policy (the guard), the fresh-directory
/// stamp, then `LsmEngine::open`.
pub fn product_open(dir: &Path) -> Result<LsmEngine, (bool, String)> {
    let guard = rubixdb::ops::format::startup_guard_with_tail_policy(dir, false)
        .map_err(|e| (true, e.to_string()))?;
    rubixdb::ops::format::stamp_if_fresh(dir, guard.format_state)
        .map_err(|e| (true, e.to_string()))?;
    LsmEngine::open(
        dir,
        product_wal_config(),
        product_pool_config(),
        recovery_lsm_config(),
    )
    .map_err(|e| (false, e.to_string()))
}

/// Recovery + every check the oracle owns. `dir` must be a disposable directory.
pub fn recover_and_check(dir: &Path, p: &Params, ledgers: &[WriterLedger]) -> RecoveryReport {
    let mut r = RecoveryReport::default();

    // ---- observation before anything is touched (read-only) ----
    if dir.join("wal").is_dir() {
        if let Ok(res) = rubixdb::wal::inspect(dir, &rubixdb::wal::WalConfig::default()) {
            r.pre_wal_corrupted_segments = res.corrupted_segments.len();
            r.pre_wal_truncated = res.truncated;
        }
    }
    let phys = rubixdb::ops::check::check_physical(dir);
    r.pre_physical_errors = phys.errors;
    r.pre_physical_warnings = phys.warnings;

    // ---- recovery ----
    let engine = match product_open(dir) {
        Ok(e) => e,
        Err((true, msg)) => {
            r.refused = Some(msg);
            return r;
        }
        Err((false, msg)) => {
            r.open_error = Some(msg);
            return r;
        }
    };
    r.tmp_files_after_open = count_tmp_files(dir);
    let st = engine.recovery_stats();
    r.recovery_stats = format!("{st:?}");
    r.checkpoint_seq = engine.checkpoint_seq();
    let first = match dump(&engine) {
        Ok(d) => d,
        Err(e) => {
            r.extra.push(Violation {
                invariant: "R9",
                detail: e,
            });
            engine.shutdown();
            return r;
        }
    };
    r.verdict = check_state(p, ledgers, &first);
    r.recovered = Some(first.clone());
    if st.wal_records_applied
        + st.wal_records_skipped_by_checkpoint
        + st.checkpoint_markers_replayed
        != st.wal_records_visited
    {
        r.extra.push(Violation {
            invariant: "R5",
            detail: format!("RecoveryStats identity broken: {st:?}"),
        });
    }
    engine.shutdown();
    drop(engine);

    // ---- R10: recover the recovered directory again; the keyspace must not change ----
    match product_open(dir) {
        Ok(e2) => {
            match dump(&e2) {
                Ok(second) => {
                    let same = second == first;
                    r.idempotent = Some(same);
                    if !same {
                        r.extra.push(Violation {
                            invariant: "R10",
                            detail: format!(
                                "second recovery changed the keyspace: {}",
                                first_diff(&first, &second)
                            ),
                        });
                    }
                }
                Err(e) => r.extra.push(Violation {
                    invariant: "R10",
                    detail: e,
                }),
            }
            // ---- R6 ----
            let cp = e2.checkpoint_seq();
            let max_ack = ledgers
                .iter()
                .flat_map(|l| l.ack_seqs.iter().copied())
                .max()
                .unwrap_or(0);
            match e2.put(b"~post-recovery", b"x") {
                Ok(seq) => {
                    r.post_seq = Some(seq);
                    if let Some(vio) = check_seq(seq, max_ack, cp) {
                        r.extra.push(vio);
                    }
                }
                Err(e) => r.extra.push(Violation {
                    invariant: "R6",
                    detail: format!("first post-recovery write failed: {e}"),
                }),
            }
            e2.shutdown();
        }
        Err((_, msg)) => r.extra.push(Violation {
            invariant: "R10",
            detail: format!("second recovery of a recovered directory failed: {msg}"),
        }),
    }

    // ---- R7/R8 after a clean stop ----
    let post = rubixdb::ops::check::check_physical(dir);
    r.post_physical_errors = post.errors;
    if post.errors > 0 {
        let first_err = post
            .findings
            .iter()
            .find(|f| format!("{:?}", f.severity).contains("Error"))
            .map(|f| format!("{f:?}"))
            .unwrap_or_default();
        r.extra.push(Violation {
            invariant: "R7/R8",
            detail: format!("physical check of the recovered, cleanly stopped directory reports {} error(s): {first_err}", post.errors),
        });
    }
    if r.tmp_files_after_open > 0 {
        r.extra.push(Violation {
            invariant: "R11",
            detail: format!(
                "{} temporary file(s) still in sstables/ after open",
                r.tmp_files_after_open
            ),
        });
    }
    // Keep `key` referenced so a future change to the key format breaks loudly here.
    let _ = key(0, 0);
    r
}

/// Attribution for a rejected state (diagnostic; it never turns a rejection into an acceptance). For every key whose
/// recovered value differs from `model(acked)`, finds the last acknowledged operation on it and tests the **F-D1a-1
/// signature** on the PRE-recovery directory copy `pre`: the operation's acknowledged `seq` is at or below the manifest
/// checkpoint (so recovery skips the WAL record as "already flushed") yet no live SSTable holds a version of the key at
/// exactly that `seq` (so the write exists nowhere else). Reads only; `pre` must be a disposable copy.
pub fn diagnose_lost_acks(
    pre: &Path,
    p: &Params,
    ledgers: &[WriterLedger],
    recovered: &Dump,
) -> String {
    use crate::workload::model_prefix;
    let m = match rubixdb::manifest::replay_readonly(pre) {
        Ok(m) => m,
        Err(e) => {
            return format!("pre-recovery manifest unreadable ({e}); no attribution possible")
        }
    };
    let cp = m
        .state
        .checkpoint
        .map(|c| c.flushed_through_seq)
        .unwrap_or(0);
    let tables: Vec<rubixdb::sstable::SsTable> = m
        .state
        .live_sstables
        .keys()
        .filter_map(|id| {
            rubixdb::sstable::SsTable::open(
                &pre.join("sstables").join(format!("{id:020}.sst")),
                *id,
            )
            .ok()
        })
        .collect();
    let (mut matched, mut other) = (Vec::new(), Vec::new());
    for w in 0..p.writers {
        let Some(l) = ledgers.get(w) else { continue };
        let pre_w = writer_prefix(w);
        let got: Dump = recovered
            .iter()
            .filter(|(k, _)| k.starts_with(&pre_w))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        // The legal states are model(K) for acked <= K <= submitted (the unacknowledged in-flight unit may be present). Take
        // the closest one so that differences it explains are not blamed on anything.
        let (mut best_k, mut best_d) = (l.acked, usize::MAX);
        for k in l.acked..=l.submitted {
            let d = diff_count(&model_prefix(p, w, k), &got);
            if d < best_d {
                (best_k, best_d) = (k, d);
            }
        }
        let want = model_prefix(p, w, best_k);
        let mut keys: Vec<&Vec<u8>> = want.keys().chain(got.keys()).collect();
        keys.sort();
        keys.dedup();
        for k in keys {
            if want.get(k) == got.get(k) {
                continue;
            }
            let mut last = None;
            for i in 0..best_k {
                if gen_op(p, w, i).iter().any(|it| match it {
                    Item::Put(x, _) | Item::Del(x) => x == k,
                }) {
                    last = Some(i);
                }
            }
            let name = String::from_utf8_lossy(k).to_string();
            if last.is_some_and(|i| i >= l.acked) {
                other.push(format!(
                    "{name}: last touched by the unacknowledged in-flight unit {} (seq unknown)",
                    last.unwrap()
                ));
                continue;
            }
            let Some(i) = last else {
                other.push(format!("{name}: no acknowledged operation touches it"));
                continue;
            };
            let seq = l.ack_seqs.get(i).copied().unwrap_or(0);
            let in_sst = tables
                .iter()
                .any(|t| t.get_versioned(k, seq).ok().flatten().map(|(s, _)| s) == Some(seq));
            if seq <= cp && !in_sst {
                matched.push(format!("{name} (writer {w} op {i}, acked seq {seq} <= checkpoint {cp}, in no live SSTable)"));
            } else {
                other.push(format!("{name} (writer {w} op {i}, acked seq {seq}, checkpoint {cp}, in an SSTable: {in_sst})"));
            }
        }
    }
    if matched.is_empty() && other.is_empty() {
        "no differing key found".into()
    } else if other.is_empty() {
        format!(
            "F-D1a-1 SIGNATURE on all {} differing key(s): {}",
            matched.len(),
            matched.join("; ")
        )
    } else {
        format!(
            "{} differing key(s) carry the F-D1a-1 signature, {} do NOT (unexplained): {}",
            matched.len(),
            other.len(),
            other.join("; ")
        )
    }
}

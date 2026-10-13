//! Relational variant of the recovery oracle (ADR-FAULTPOINTS-01 section 12.6, A5 route (a)).
//!
//! The workload runs through the real relational write path (`TableStore::put_row` / `delete_row`: one `write_batch` per
//! commit unit carrying the row AND its secondary-index entries), below SQL text. Expected state is established twice, by
//! two independent means, and the recovered database must satisfy both:
//!
//! 1. **Decoded rows**: `TableStore::scan_table` of the recovered database must equal the model regenerated from the seed
//!    (`(id, name, grp)` per primary key) for some `K` with `acked <= K <= submitted`.
//! 2. **Raw differential**: the recovered database's complete raw keyspace (catalog rows, row keys, index entries) must equal
//!    the raw keyspace of a REFERENCE database that was never crashed - the setup template plus exactly the first `K` operations
//!    applied through the same API in a fresh process state. This is what catches "a row without its index entry (or the
//!    reverse)", a dangling index entry, a stale index entry after an update, or any catalog damage, none of which a
//!    decoded-row comparison can see because index lookups fetch rows and skip dangling entries.
//!
//! Limits, stated: the reference uses the same code as the engine under test (it is a differential oracle, not a spec
//! oracle); SQL parsing/planning is above this layer and is not exercised; single writer only.

#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;

use crate::oracle::{self, Dump, Verdict, Violation};
use crate::rel_workload::{apply_op, attach, decoded_rows, gen_rel_op, rel_model, RelStack};
use crate::workload::{Params, WriterLedger};

fn raw_diff(expected: &Dump, got: &Dump) -> Option<String> {
    for (k, v) in expected {
        match got.get(k) {
            None => {
                return Some(format!(
                    "raw key {} is missing from the recovered database",
                    hex(k)
                ))
            }
            Some(g) if g != v => return Some(format!("raw key {} has a different value", hex(k))),
            _ => {}
        }
    }
    got.keys().find(|k| !expected.contains_key(*k)).map(|k| {
        format!(
            "raw key {} is present in the recovered database but not in the reference",
            hex(k)
        )
    })
}

fn hex(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02x}"))
        .collect::<Vec<_>>()
        .join("")
}

/// Judges a recovered relational database. `template` is the setup state (catalog + empty table + index) the workload
/// started from; `work` is a scratch directory for the reference engine.
pub fn check_relational(
    template_data: &Path,
    work: &Path,
    p: &Params,
    ledger: &WriterLedger,
    recovered: &RelStack,
    recovered_raw: &Dump,
) -> Verdict {
    let mut v = Verdict::default();
    let push = |v: &mut Verdict, inv: &'static str, d: String| {
        v.violations.push(Violation {
            invariant: inv,
            detail: d,
        })
    };

    // 1. decoded rows
    let rows = match decoded_rows(recovered) {
        Ok(r) => r,
        Err(e) => {
            push(
                &mut v,
                "R9",
                format!("the recovered table cannot be read: {e}"),
            );
            return v;
        }
    };
    let mut row_k: Option<usize> = None;
    for k in ledger.acked..=ledger.submitted {
        if rel_model(p, k) == rows {
            row_k = Some(k);
            break;
        }
    }
    if row_k.is_none() {
        let below = (0..ledger.acked).rev().find(|k| rel_model(p, *k) == rows);
        push(
            &mut v,
            if below.is_some() { "R1" } else { "R2/R3/R4" },
            format!(
                "decoded rows match no legal state (acked={}, submitted={}, closest lower exact match: {below:?}); {} rows recovered",
                ledger.acked,
                ledger.submitted,
                rows.len()
            ),
        );
    }

    // 2. raw differential against a reference that never crashed
    let ref_dir = work.join("reference");
    let _ = std::fs::remove_dir_all(&ref_dir);
    crate::rig::copy_dir(template_data, &ref_dir);
    let engine = match oracle::product_open(&ref_dir) {
        Ok(e) => Arc::new(e),
        Err((_, m)) => {
            push(
                &mut v,
                "ORACLE",
                format!("reference database could not be opened: {m}"),
            );
            return v;
        }
    };
    let stack = match attach(Arc::clone(&engine)) {
        Ok(s) => s,
        Err(e) => {
            push(&mut v, "ORACLE", format!("reference stack: {e}"));
            return v;
        }
    };
    let mut raw_k: Option<usize> = None;
    let mut last_diff = String::new();
    for k in 0..=ledger.submitted {
        if k > 0 {
            if let Err(e) = apply_op(&stack, &gen_rel_op(p, k - 1)) {
                push(
                    &mut v,
                    "ORACLE",
                    format!("reference op {} failed: {e}", k - 1),
                );
                break;
            }
        }
        if k < ledger.acked {
            continue;
        }
        match oracle::dump(&engine) {
            Ok(d) => match raw_diff(&d, recovered_raw) {
                None => {
                    raw_k = Some(k);
                    break;
                }
                Some(diff) => last_diff = format!("K={k}: {diff}"),
            },
            Err(e) => {
                push(&mut v, "ORACLE", e);
                break;
            }
        }
    }
    let _ = engine.shutdown();
    if raw_k.is_none() && v.violations.iter().all(|x| x.invariant != "ORACLE") {
        push(
            &mut v,
            "R2/R7",
            format!(
                "the recovered raw keyspace (rows + index entries + catalog) equals the reference for no K in [{}, {}]; last: {last_diff}",
                ledger.acked, ledger.submitted
            ),
        );
    }
    if let (Some(a), Some(b)) = (row_k, raw_k) {
        v.notes.push(format!(
            "decoded rows match K={a}; raw keyspace matches the reference at K={b}"
        ));
        v.chosen_k.push(Some(b));
    }
    v
}

/// Recovery + the relational checks.
pub fn recover_and_check_relational(
    data: &Path,
    template_data: &Path,
    work: &Path,
    p: &Params,
    ledger: &WriterLedger,
) -> oracle::RecoveryReport {
    let mut r = oracle::RecoveryReport::default();
    let engine = match oracle::product_open(data) {
        Ok(e) => Arc::new(e),
        Err((true, m)) => {
            r.refused = Some(m);
            return r;
        }
        Err((false, m)) => {
            r.open_error = Some(m);
            return r;
        }
    };
    let stats = engine.recovery_stats();
    r.recovery_stats = format!("{stats:?}");
    let stack = match attach(Arc::clone(&engine)) {
        Ok(s) => s,
        Err(e) => {
            r.verdict.violations.push(Violation {
                invariant: "R7",
                detail: format!("catalog/table/index missing after recovery: {e}"),
            });
            return r;
        }
    };
    let raw = match oracle::dump(&engine) {
        Ok(d) => d,
        Err(e) => {
            r.extra.push(Violation {
                invariant: "R9",
                detail: e,
            });
            return r;
        }
    };
    r.verdict = check_relational(template_data, work, p, ledger, &stack, &raw);
    r.recovered = Some(raw.clone());
    let _ = engine.shutdown();
    drop(stack);
    drop(engine); // the WAL lock is released only when the last handle is dropped
                  // R10: a second recovery must not change the raw keyspace.
    match oracle::product_open(data)
        .map_err(|(_, m)| m)
        .and_then(|e| {
            let d = oracle::dump(&e);
            let _ = e.shutdown();
            d
        }) {
        Ok(second) => {
            r.idempotent = Some(second == raw);
            if second != raw {
                r.extra.push(Violation {
                    invariant: "R10",
                    detail: "second recovery changed the raw keyspace".into(),
                });
            }
        }
        Err(e) => r.extra.push(Violation {
            invariant: "R10",
            detail: e,
        }),
    }
    r
}

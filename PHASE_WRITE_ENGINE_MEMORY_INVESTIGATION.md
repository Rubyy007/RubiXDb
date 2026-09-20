# Write Engine — RSS Growth Investigation (2026-09-20)

**Trigger:** the 2026-09-19 23:47→03:47 realistic full-pipeline soak (200 writers, `LsmConfig::default()`, `temp/long_soak_logs/realistic_soak_200w_20260919_234720.log`) showed RSS growing from 25,012 KB to 608,824 KB (+583,812 KB, +2,334.1% by the harness's own start-vs-end computation) over its 14,400s duration. This document investigates whether that growth is expected/bounded or a defect, using measurement and source tracing — not intuition — per this project's own rigor convention.

**Conclusion, stated up front: (A) EXPECTED BOUNDED METADATA GROWTH.** Full reasoning and evidence below. No code fix required. No re-run of the 4-hour soak required — the already-completed run stands as valid endurance evidence.

---

## 1. Harness result file — verified, not inferred

`temp/long_soak_logs/realistic_soak_result_20260919_234720.txt`: the harness's own original run reported `REALISTIC FULL-PIPELINE SOAK RESULT: FAIL` — caused by an unrelated PowerShell `-notmatch` bug in the harness itself (documented in `PHASE_WRITE_ENGINE_CERTIFICATION.md` §3), not by anything memory-related. A corrected-logic replay, appended to the same file without altering the original record, yields PASS on every workload-health check. This document is exclusively about the RSS question that PASS verdict did not address.

## 2. Full RSS series from the real soak — not just start-vs-end

All 121 samples were extracted (not just first/last). Key facts:

- RSS is **monotonically non-decreasing across every single 120s sample** — it never drops, not even briefly, anywhere in the 4-hour run.
- The **last two samples are identical**: at t=14,399.6s, `sstable_count=3294, rss_kb=608824`; at t=14,400.9s (duration elapsed, no new SSTable created), `sstable_count=3294, rss_kb=608824` — RSS growth stops in exact lockstep with SSTable-count growth stopping, not merely correlates with it.
- A linear regression of RSS against `sstable_count` across all samples (excluding the first 5 as startup transient) gives **slope = 175.75 KB/SSTable, intercept = 28,735 KB**.

This is the first strong signal: RSS tracks SSTable count, not elapsed time, memory-allocation churn, or any other time-based process.

## 3. Independent empirical scaling test (bounded, not another 4-hour soak)

Per this investigation's own requirement not to rely solely on one dataset, a second, independent, faster measurement was run: `examples/realistic_full_pipeline_soak.rs`, same production config (`LsmConfig::default()`), 1,000 writers (higher throughput → reaches large SSTable counts faster than 200 writers would), 3,000s bounded duration, 10s sampling, on `E:` (`temp/mem_scaling_test_output.log`, run and monitored 2026-09-20; the process genuinely ran for the full 3,000s — an earlier claim that it had already exited was checked directly via `Get-Process` and found to be premature/incorrect before being acted on). It reached 2,557 SSTables in the allotted time, `completed_err=0` throughout, clean recovery.

**Checkpoint measurements** (first sample at or past each target):

| Target | t_secs | SSTables | RSS (KB) | Manifest (bytes) | Immutable | Queue depth | WAL (bytes) |
|---|---|---|---|---|---|---|---|
| 100 | 113.4 | 101 | 60,444 | 7,441 | 1 | 36 | 6,960,955 |
| 500 | 569.0 | 509 | 133,408 | 37,666 | 0 | 0 | 4,416,853 |
| 1,000 | 1,138.4 | 1,005 | 224,404 | 74,370 | 0 | 158 | 4,043,592 |
| 2,000 | 2,297.5 | 2,006 | 400,200 | 148,444 | 0 | 0 | 5,129,850 |

Baseline (first sample, t=10.0s): 9 SSTables, RSS 31,356 KB. Final sample (t=2,999.9s): 2,557 SSTables, RSS 497,340 KB.

**Linear fit on this independent dataset: `rss_kb = 43,869.9 + 177.558 × sstable_count`, R² = 0.9999.** Essentially a perfect line — two independent runs, different writer counts (200 vs. 1,000), different total durations (14,400s vs. 3,000s), converge on the same ~176-178 KB/SSTable slope.

**Cross-validation against the real soak:** extrapolating *this* scaling run's fit to 3,294 SSTables predicts 628,745 KB. The real soak observed 608,824 KB at 3,294 SSTables. **Residual: −19,921 KB, i.e. the model over-predicts by 3.3%** — the real run's actual growth is fully accounted for by this model, with no unexplained excess.

## 4. Ownership tracing — what actually holds this memory, by source

Traced the complete write path, not asserted:

- **`SsTable` struct** (`src/sstable/reader.rs:58-65`): `{ id, path, file, footer, bloom: BloomFilter, index: Vec<IndexEntry> }`. `SsTable::open()`'s own doc comment: *"Data blocks are not read here — bounded memory at `open()` regardless of table size."* Confirmed: only the footer, Bloom filter, and sparse block index are loaded and retained; large data blocks are read fresh from disk on every query, never cached. Every one of these structs, for every SSTable ever flushed, lives in `LsmEngine.sstables: Arc<RwLock<Vec<Arc<SsTable>>>>` for the engine's entire lifetime — nothing ever removes an entry (no Compaction exists yet).
- **`ManifestState`** (`src/manifest/state.rs:29-36`, holds `live_sstables: BTreeMap`, `ever_added: HashSet`): confirmed via `grep` that this type is used **only** as a local variable inside `LsmEngine::open()` (passed by `&mut` into `reconcile_sstables_with_manifest`) and is **never** stored on the `LsmEngine` struct — it is dropped when `open()` returns. The live `Manifest` struct kept for the engine's lifetime (`src/manifest/mod.rs:63-69`) holds only `{ file, record_count: u64, last_edit: Option<ManifestEdit> }` — no per-record history. This rules out Manifest in-memory state as a growth source (matches the on-disk Manifest file staying a small, append-only 243,756 bytes at 6,588 records).
- **Immutable MemTable release**: `spawn_flush_thread`'s success path (`src/lsm/mod.rs`) does `immutables.write()....retain(|m| !Arc::ptr_eq(m, &frozen))` immediately after a successful flush, and the local `frozen: Arc<MemTable>` goes out of scope at the end of that `FlushMsg::Flush` iteration — no cross-iteration retention. Empirically confirmed by both soaks: `max_immutable_count_observed` stayed at 1 (200w run) / rarely above 1 (1000w run) throughout, never growing.
- **Flush "jobs"/batches**: each `FlushMsg::Flush(Arc<MemTable>)` is consumed and dropped per outer-loop iteration; each Batch Coordinator `batch: Vec<QueueEntry>` is a local `process_batch` parameter, dropped on return. `queue_depth` stayed bounded (max 194/800 in the 200w soak) — no accumulation.
- **No caching layer exists**: `grep -rn "cache\|Cache" src/sstable/` returns nothing — not even a bounded LRU. Nothing here could be "unbounded" because nothing here retains data blocks at all.
- **WAL/GroupCommitter**: no growing collections found in production code (only test-harness `Vec<JoinHandle>`s, joined and dropped). WAL bytes on disk stayed bounded (3.59–6.94 MB) across the full 4-hour run, consistent with no in-memory WAL-side growth either.

**Nothing was found growing with time, write count, or any factor other than live SSTable count.**

## 5. On-disk vs. in-memory metadata — measured, not assumed

A real SSTable footer from the running 1,000-writer scaling test was parsed directly (`RUBIC_SSTABLE_FORMAT_SPECIFICATION.md`'s 72-byte footer layout, `src/sstable/format.rs`):

```
record_count = 102,721
bloom_length (on-disk) = 128,415 bytes (125.41 KB)  — exactly bloom_bits_per_key=10: 102,721×10/8 ≈ 128,401, matching to rounding
index_length (on-disk) = 15,494 bytes (15.13 KB)
```

The **on-disk** encoding (140.54 KB/table combined) is not the same number as the **in-memory** representation: the Bloom filter's `bits: Vec<u8>` is essentially identical in memory to its on-disk form (~125 KB, no per-key overhead), but the in-memory `index: Vec<IndexEntry>` — where each `IndexEntry` owns its own separately-heap-allocated `last_key: Vec<u8>` — carries real allocator overhead per entry (Vec header + heap allocation rounding) that the compact on-disk encoding does not. This plausibly explains the gap between the measured ~140.5 KB/table on-disk figure and the empirically observed ~176-178 KB/table RSS slope: ordinary Rust/Windows heap allocator overhead for roughly 500+ small, separately-owned key allocations per table, not any additional unaccounted-for structure. No further retained object was found in the ownership trace above that could explain the remainder.

## 6. Magnitude classification

| | |
|---|---|
| Expected from bloom+index model (208,735 KB at 3,294 SSTables using an intercept+177.56×3294 fit) | ~615,000-629,000 KB range across the two independent fits |
| Observed at 3,294 SSTables (real soak) | 608,824 KB |
| Residual | within 3.3%, model **over-**predicts |

**Classification: (A) EXPECTED BOUNDED METADATA GROWTH — bounded *per SSTable* (a fixed, deterministic ~176-178 KB/table, confirmed by two independent measurements and matched by direct on-disk measurement + code-level ownership tracing), not a leak.**

**Important, explicit caveat, not glossed over:** "bounded per table" is not the same claim as "bounded for all time." Total process RSS is a linear function of live SSTable count, and live SSTable count itself has no upper bound in the current architecture, because Compaction — the component whose job is to merge/reduce old SSTables — does not exist yet. This is not new information: it is the direct, already-anticipated, already-documented consequence of Compaction being an explicit Non-Goal of this phase (`PHASE4B_ADR.md` ADR-P4B-1). An operator running this engine under sustained write load for a much longer duration than 4 hours, without Compaction, will see RSS continue to grow linearly with SSTable count — this is a real operational characteristic to plan around, not a defect in the Write Engine to fix here.

**Secondary observation, also flagged, not hidden:** in the 1,000-writer scaling run, once SSTable count passed roughly 2,000, occasional throughput dips and latency spikes appear (e.g. `ops_per_sec` dropping to the 9,000-40,000 range in isolated samples, `max_ms` briefly exceeding 600ms, queue depth occasionally saturating at capacity 4,000/4,000) alongside the growing RSS. `completed_err` stayed 0 throughout and the run finished cleanly — this is bounded, correct backpressure behavior (the Batch Coordinator's queue doing exactly what it is designed to do under strain), not a failure. It is noted here as a forward-looking signal for when Compaction work begins, not a Write Engine certification blocker.

## 7. `sync_failures=1` — traced to source

`GroupCommitStats::sync_failures()` (`src/wal/group_commit.rs:156-160`) computes `sync_attempts.saturating_sub(sync_successes)` from two **independently-read, independently-incremented** atomics (`fetch_add` on `stat_sync_attempts` before the fsync call, `fetch_add` on `stat_sync_successes` only after it returns `Ok`, lines 908/928 and 943 respectively; both loaded with `Ordering::Relaxed` when a stats snapshot is built, lines 795-796).

**This is a genuine TOCTOU counter race, not a real failure**: if a stats snapshot is taken at the exact instant a sync has been counted as "attempted" but its matching "success" increment hasn't landed yet (the fsync syscall is still in flight), `sync_failures()` reads as 1 (or more, under heavier concurrency) even though no actual failure occurred — it resolves back to 0 the moment the in-flight sync completes and its success increment lands.

**Verified against the data, not just theorized**: across all 121 samples in the 200-writer soak, `sync_failures` took **only** the values 0 or 1 (never higher, never a growing/cumulative count) — 76 samples read 1, 45 read 0, oscillating, exactly consistent with "usually 0 or 1 sync briefly in flight at sampling time" and inconsistent with any real, unretried, accumulating failure. No caller-visible write ever failed (`completed_err=0` for the entire run), and final recovery found 0 corruption — if a real fsync failure had gone unretried, either `completed_err` would have moved or a durability gap would have surfaced at recovery. Neither happened.

**Conclusion: sampling/counter-race artifact, not a real sync failure.** No source change made — per this investigation's own instruction, sync behavior is not changed without evidence of a defect, and none was found. The metric itself is not suppressed; this finding is documented so a future observability pass can note that `sync_failures()` should ideally be read from a combined/atomic snapshot rather than two independently-read counters if a non-racy instantaneous reading is ever needed.

## 8. Harness improvement

`examples/realistic_full_pipeline_soak.rs` now also prints `min_rss_kb_observed`/`max_rss_kb_observed` (computed across every sample, not just first/last) in its final analysis output, alongside the existing `max_queue_depth_observed`/`max_wal_bytes_observed`/`max_immutable_count_observed` line. Harness-only change (no production engine code touched), verified working via a real smoke run (`min_rss_kb_observed=6500 max_rss_kb_observed=11940` printed correctly).

## 9. Regression test added

`lsm::tests::sstable_count_and_immutable_memory_track_flushes_exactly_no_extra_retention` (`src/lsm/tests.rs`) — does not measure real OS RSS (noisy and platform-specific inside `cargo test --lib`; real RSS evidence lives in the reproducible scaling runs above, re-runnable any time via `cargo run --release --example realistic_full_pipeline_soak -- <writers> <duration> <interval>`). Instead locks in the two **object-ownership invariants** that are the actual code-level guarantee against a real leak: across 12 freeze/flush cycles, (1) `immutable_total_bytes()` returns to exactly 0 after every successful flush (no residual retention), and (2) `sstable_count()` grows by exactly 1 per cycle, never more (duplicate publication) or less (silently lost SSTable). Verified stable across 5 consecutive runs.

## 10. Decision

**(A) EXPECTED BOUNDED METADATA GROWTH.** No code fix required. No re-run of the 4-hour soak required (per this investigation's own instruction: only re-run if a code fix changed the write path, which did not happen here) — the completed 2026-09-19 23:47 `E:` soak stands as valid, sufficient endurance evidence, now with its memory behavior fully explained rather than merely asserted.

See `PHASE_WRITE_ENGINE_CERTIFICATION.md` for how this closes into the final certification decision.

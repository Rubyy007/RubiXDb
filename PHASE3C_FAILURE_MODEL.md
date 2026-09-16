# RubiXDB Phase 3C — Failure Model

Companion to `PHASE1_FAILURE_MODEL.md`/`PHASE2B_FAILURE_MODEL.md`/
`PHASE3_FAILURE_MODEL.md`/`PHASE3B_FAILURE_MODEL.md` — every failure
mode those files document still applies unchanged; Phase 3C changes no
existing failure-handling code (`LeaderFailureGuard`,
`CoordinatorFaultPoint`, the poison policy). This file covers what
Phase 3C adds: the external-process-kill failure model and the
pathological-recovery classification evidence.

## 1. External-process-kill failure model (operating brief §5-§6)

Distinct from every prior fault-injection mechanism in this project
(`AbortPoint` — in-process, deterministic, code-chosen point;
`CoordinatorFaultPoint`/`install_fsync_fault_hook` — in-process,
panic-based): `examples/crash_cycle_test.rs` kills a **separate OS
process** externally, at a point neither the killed process nor this
project's own code chooses or is aware of in advance.

| Property | `AbortPoint` (existing) | `CoordinatorFaultPoint` (existing) | External kill (Phase 3C, new) |
|---|---|---|---|
| Granularity | One of 11 named code boundaries | One of 7 named coordinator-loop points | Anywhere — including mid-syscall |
| Who decides the point | This project's own code, at a hook call site | This project's own code, at a hook call site | The OS scheduler, asynchronously |
| Cooperation from the killed code | Full — it calls `process::abort()` itself | Full — it panics itself | None — it has no chance to react |
| What it proves | "This specific transition is crash-safe" | "This specific transition fails safely" | "The system survives being interrupted at a truly arbitrary point" |

**Expected outcome, every cycle**: the killed child process's exclusive
OS-level advisory lock on the WAL directory (`ARCHITECTURE.md`'s
"Cross-process file locking" section) is released automatically by the
OS the instant it dies — this is that mechanism's own explicit design
goal, not new behavior introduced this phase. The parent's next
`FileWal::open_for_recovery` call must therefore succeed (not hang,
not fail with a stale-lock error) and must recover exactly the durable
prefix that existed at the moment of the kill: no corruption, gap-free
sequences, monotonically non-decreasing highest sequence across cycles
(never a duplicate, never a rollback).

**Verified**: 40/40 cycles (`PHASE3C_TEST_RESULTS.md` §5) — every
property above held on every cycle.

## 2. Pathological recovery classification table (operating brief §7)

Every fixture in `tests/pathological_recovery_matrix.rs` maps to
exactly one of the three outcomes WAL Spec §6.3 defines, and every
fixture below was verified to land on the *correct* one — no fixture
was classified into the wrong bucket:

| Outcome | Meaning | Fixtures landing here |
|---|---|---|
| **Clean / valid** | Every record recovered exactly, `corrupted=false`, `truncated=false` | `valid_durable_tail`, `many_segments_all_valid`, `many_batches` |
| **Torn tail (expected, not alarming)** | `truncated=true`, `corrupted=false`; recovery returns the longest valid prefix and physically truncates | `partial_final_header`, `partial_final_body` |
| **Corruption (escalated, fail-closed)** | `corrupted=true`; scan stops at/before the corruption point per the Group 3.1 amendment (`ARCHITECTURE.md`'s "WAL hardening pass" section); nothing at or after it is trusted | `crc_corruption_non_tail`, `invalid_length_field_non_tail`, `malformed_frame_unrecognized_op_tag`, `mixed_valid_and_corrupt_segments` |

**No fixture produced a fourth, undocumented outcome** — the
three-way classification this project committed to in Phase 0/1
remains exhaustive under every corruption shape this phase tested,
including the two genuinely new ones (an out-of-range length field
specifically at the recovery/classification boundary rather than
write-time enforcement, and an unrecognized op tag with an otherwise-
valid CRC, isolating the op-decode failure path from the CRC-mismatch
path).

## 3. Recovery-memory limitation — formal characterization (operating brief §8-§9)

Supersedes `PHASE3B_ADR.md` ADR-P3B-5's anecdotal (single-run) account
with a swept measurement (`examples/recovery_memory_scaling.rs`,
`PHASE3C_TEST_RESULTS.md` §8):

- **Scaling law**: RSS at recovery completion ≈ `134 bytes × record
  count` (measured linear from 1M to 15M records — no superlinear
  growth observed in the tested range, i.e. no evidence of allocator
  fragmentation becoming dominant at this scale).
- **On-disk-to-RSS ratio**: ~1.83× (73 bytes/record on disk → ~134
  bytes/record in RSS) — consistent with two separate heap allocations
  per `Put` record (`key: Vec<u8>`, `value: Vec<u8>`) plus `Vec<(u64,
  WalOpOwned)>` growth/enum-discriminant overhead.
- **Recovery throughput**: flat at ~184-186K records/sec regardless of
  size — the *time* cost of recovery does not degrade at scale; only
  the *memory* cost does.
- **Attribution**: confirmed to be the public WAL recovery API's own
  design (`WalReplayResult`'s whole-file materialization,
  `src/wal/recovery.rs`'s `WalkOutcome.records: Vec<...>`), not any
  test harness — the harness itself holds no accumulating state beyond
  what it explicitly measures.
- **Practical ceiling on this project's own 16 GiB development host**:
  proven safe through 15M records (~2.0 GiB peak RSS); the 85M-record
  run that failed in Phase 3B extrapolates to ~11.4 GiB, consistent
  with what was actually observed (host RAM exhausted alongside
  whatever else was running in that session).

**This is not treated as a durability or correctness defect** —
`corrupted_segments` was `0` and every record count matched exactly at
every tested size; recovery is correct at every scale tested, merely
memory-expensive at very large scale. See `PHASE3C_ADR.md` ADR-P3C-1
for the design-decision analysis (streaming/callback/bounded-batch
directions), deliberately not implemented this phase.

## 4. What Phase 3C does NOT change

- `LeaderFailureGuard`/`PoisonReason` (Phase 3A) — unchanged.
- `CoordinatorFaultPoint`'s completion-guard fix (Phase 3B) — unchanged.
- The WAL binary format, recovery contract, or rotation semantics.
- `open_for_recovery`'s materialization behavior (ADR-P3C-1 — analyzed,
  not implemented).

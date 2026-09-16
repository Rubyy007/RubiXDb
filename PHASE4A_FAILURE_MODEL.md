# RubiXDB Phase 4A — Failure Model

Companion to `PHASE1_FAILURE_MODEL.md` through `PHASE3C_FAILURE_MODEL.md`
— every existing WAL/coordinator failure mode still applies unchanged
(operating brief §2). This file covers what Phase 4A adds: the WAL/
MemTable boundary's own failure semantics.

## 1. Failure table (operating brief §23)

| Failure | Detection | Response | Caller-visible outcome |
|---|---|---|---|
| WAL append failure | `BatchCoordinatorPool::submit`/`Completion::wait()` returns `Err` before durability was ever claimed | `LsmEngine::put`/`delete` returns that `Err` immediately, **never calls `MemTable::insert`** | No inconsistent state possible — the `?` after `completion.wait()` in `apply_after_durable`'s caller returns before the MemTable is ever touched |
| WAL sync (durability) failure | Same path — `Completion::wait()`'s `Err` covers both append and sync failures identically (unchanged `execution::batch_coordinator` semantics, `PHASE3B_FAILURE_MODEL.md` §4-§5) | Same — `put`/`delete` returns `Err`, MemTable untouched | Caller sees the exact same error class the WAL/coordinator layer already defines (`EngineError::WalUnavailable`/`Timeout`/etc., unchanged) |
| MemTable allocation failure | Not caught specially — Rust's global allocator aborts the process on OOM by default (`std`'s own behavior, not something this project can or should intercept without `#[global_allocator]` machinery far outside Phase 4A's scope) | N/A — matches every other component in this codebase (`Vec`/`BTreeMap` growth throughout the WAL layer has the identical property) | A true allocation failure is a process-fatal event project-wide, not a `MemTable`-specific new risk |
| MemTable capacity exceeded (immutable backpressure) | `LsmEngine::freeze_locked` checks `immutables.len() >= max_immutable_memtables` **before** freezing | `EngineError::CapacityExceeded` returned to the caller whose write triggered the freeze attempt | **The triggering write itself is NOT rolled back** — it is already durable (WAL) and already applied to what remains the active MemTable; only the *freeze* is refused. The caller's `put`/`delete` call itself still returns this error, so a caller retrying naïvely could believe the write failed when the data write did succeed — see §2 below for why this is the correct, honestly-recorded trade-off, not an oversight |
| Shutdown | `LsmEngine::shutdown` delegates to the unmodified `BatchCoordinatorPool::shutdown` | Identical to the existing, certified shutdown contract (`PHASE3B_FAILURE_MODEL.md` §2) | No new shutdown semantics introduced; in-flight `put`/`delete` calls already past `completion.wait()` complete their MemTable apply normally (that step has no I/O, cannot itself be interrupted by a WAL-level shutdown) |

## 2. The one genuinely awkward failure mode, named explicitly, not hidden

**`CapacityExceeded` on freeze reports failure for a write that actually
succeeded.** When a `put`/`delete` call's own insert makes the active
MemTable full, and freezing it would exceed `max_immutable_memtables`,
`LsmEngine::put`/`delete` returns `Err(EngineError::CapacityExceeded)`
— but the record is **already durable in the WAL and already applied to
the (now-full, not-yet-frozen) active MemTable**. A caller that
interprets this `Err` as "my write did not happen" and retries with the
same key/value would durably write the same logical value a second
time, under a **new** `seq` (not a duplicate `seq` — WAL sequence
assignment is unaffected by this failure, so no `(key, seq)` collision
or silent overwrite occurs) — merely a harmless, if slightly wasteful,
re-write, not a correctness violation.

**Why this was accepted rather than redesigned this phase**: rolling
back an already-durable, already-applied write to make the error
message "accurate" would require either (a) a compensating tombstone
(itself a new write, with its own durability requirements — the
MemTable is explicitly not a second durability system, §2 above), or
(b) refusing the underlying `insert` *before* it happens, which is not
possible without either violating the WAL-durability-before-MemTable-
apply ordering (`PHASE4A_ARCHITECTURE.md` §5 — the actual write must
already be durable and applied before this phase's code can even know
whether *this specific* write is the one that tips the MemTable over
its threshold) or speculatively refusing writes before the MemTable is
actually full (a correctness-for-simplicity trade this phase does not
need to make). This is recorded here as a known, explicit
characteristic — not silently accepted, not treated as a bug requiring
an urgent fix — and is exactly the kind of interaction Phase 4B's own
flush-to-SSTable work should keep in mind when it starts actually
draining the immutable list (at which point backpressure becomes
transient rather than a hard wall, changing this trade-off's practical
severity).

## 3. Crash boundaries (operating brief §26)

| Crash point | What survives | Why |
|---|---|---|
| Before WAL append | Nothing of this write | It never reached the WAL at all — identical to any pre-existing WAL crash-safety guarantee |
| After WAL append, before durability | Nothing of this write is *guaranteed* — the WAL's own existing torn-tail/corruption classification governs (unchanged, `PHASE3C_FAILURE_MODEL.md` §2) | `Completion::wait()` never returned `Ok`, so `LsmEngine::put`/`delete` never called `MemTable::insert` — no MemTable-side risk exists at this boundary at all |
| After WAL durability, before MemTable apply | The write **is** durable — `wal::replay_streaming` during the next `LsmEngine::open` will find and apply it | The calling thread simply never got to run `active.insert(...)` — recovery does not depend on that having happened; it rebuilds the MemTable fresh from the WAL every time |
| During MemTable apply | Same as above — `MemTable::insert` itself is a pure in-memory operation with no partial-completion state a crash could observe (a `BTreeMap::insert` either completes or the whole process is gone; there is no "half-inserted" entry) | Recovery is unaffected either way, for the same reason as the row above |
| After MemTable apply | The write is both durable and was applied before the crash — but this MemTable state is **discarded** on restart regardless (the MemTable is never itself persisted) and correctly rebuilt from the WAL from scratch | Recovery never trusts pre-crash MemTable state, by design (§1 above's "MemTable is not a durability system") |
| During freeze | The frozen memtable's own data is still exactly what the WAL has for those sequences — recovery rebuilds one fresh active MemTable from the WAL, with no concept of "immutable" at all (Phase 4A has no flush-to-SSTable, so there is nothing an immutable memtable durably represents that the WAL doesn't already durably represent) | Freeze is a purely in-memory state transition; losing it to a crash loses no durable information |
| After freeze | Same as above | Same as above |

**The single invariant every row above reduces to**: *after restart, the
recovered MemTable reflects exactly the durable WAL state — nothing
less, nothing more* (operating brief §26's own stated requirement).
Verified directly, not just reasoned about, by `examples/lsm_crash_
cycle_test.rs`'s 25/25 real external-process-kill cycles, each showing
`active_entries == highest_sequence == durable_through` exactly
(`PHASE4A_TEST_RESULTS.md` §5).

## 4. What Phase 4A does NOT change

- Every existing WAL/`GroupCommitter`/`BatchCoordinatorPool` failure
  mode (`LeaderFailureGuard`, `CoordinatorFaultPoint`, the fail-closed
  poison policy) — unchanged, re-verified passing under this phase's
  own new code (170/170 lib tests, including the full pre-existing
  suite).
- The WAL binary format, recovery contract, or rotation semantics.
- `open_for_recovery`'s own behavior — `wal::replay_streaming` is
  purely additive (`PHASE4A_MEMTABLE_ARCHITECTURE.md` §10).

# RubiXDB Phase 2 — Write Worker Pool Architecture

Design only — no benchmark numbers or pass/fail data here (see
`PHASE2_TEST_RESULTS.md`); no rationale for *why* a design choice was
made over an alternative (see `PHASE2_ADR.md`). Describes what
`src/execution/write_pool.rs` actually implements, kept in sync with the
code (`write_pool.rs`'s own module doc comment is the more detailed,
line-referenced version of everything below).

## 1. Goal

Separate logical client concurrency (10s/100s/1,000+ callers) from
physical storage execution concurrency (a bounded number of worker
threads), per the operating brief's architecture diagram:

```text
Logical Clients (10s/100s/1,000+)
         |
         v
Request Submission / Bounded Work Queue
         |
         v
Worker Pool (N controlled execution workers)
         |
         v
Group Commit / Durable WAL
         |
         v
Persistent WAL
```

**This is an execution-layer optimization, not a durability redesign.**
`GroupCommitter` (Phase 1, unchanged) remains the sole owner of sequence
assignment, the durability watermark, batching, rotation, and poisoning.
The worker pool only changes *which threads* call into it.

## 2. Components

| Type | Role |
|---|---|
| `WriteWorkerPool` | Owns the queue, the worker threads, and (via `Arc`) the wrapped `GroupCommitter`. Public API: `new`, `submit`, `shutdown`, `stats`, `state`, `into_inner`. |
| `WriteWorkerPoolConfig` | `worker_count`, `queue_capacity`, `max_queued_bytes`, `submission_timeout`, `shutdown_drain_bound`, `await_retry_budget` — every bound genuinely bounded, no unbounded configuration exists. |
| `Completion` | The caller-facing handle `submit()` returns — `wait()`/`wait_timeout()`, consuming (single-read by construction, not `Clone`). |
| `RequestId` | Opaque, monotonically-increasing, observability-only identifier — **not** used for ordering (see §4). |
| `PoolState` | `Running` → `Draining` → `Stopped` (clean path) or `Failed` (every worker died without a requested shutdown). |
| `WorkerPoolStats` | Pool-level counters plus the underlying `GroupCommitStats` (not duplicated, delegated). |

## 3. Ownership model (operating brief §6)

| Phase | Owner |
|---|---|
| Before `submit()` | Caller owns its `key`/`value` bytes. |
| `submit()`'s one copy | Copied into an owned `WalOpOwned` — the only copy this module makes; unavoidable, since a queued request must outlive the submitting caller's stack frame. |
| While queued | The `QueueEntry` (in the shared `VecDeque`) is the sole owner. |
| During processing | The dequeuing worker owns it; `WalOpOwned::as_wal_op` re-borrows (zero-copy) for the `GroupCommitter` call. |
| After completion | `WalOpOwned` dropped once `append` returns (already encoded into the WAL's own frame buffer by then — a second, WAL-internal copy this module has no visibility into and does not duplicate). `Result<WalPosition>` moved into the `Completion`, read at most once (the type system enforces this, not a runtime check). |

## 4. Ordering contract

**Queue insertion order is not sequence order — by design, not by
accident.** `GroupCommitter::append` is the sole assigner of `seq`,
under its own internal lock, exactly as in Phase 1's direct-thread
model — a worker's `append()` call is indistinguishable from a Phase 1
direct caller's. Two requests enqueued `A, B` may be assigned sequence
numbers in either order if two different workers happen to process them
concurrently. What the WAL format and `GroupCommitter` actually
guarantee — a gap-free, duplicate-free durable prefix — is entirely
`GroupCommitter`'s own responsibility and is untouched by this module.
**No sequence-allocation logic exists in `write_pool.rs` at all** — this
is a direct consequence of the ordering contract above, not an
oversight.

## 5. Queue

A single shared `Mutex<QueueState>` (`entries: VecDeque<QueueEntry>`,
`queued_bytes`, `pool_state`, `workers_alive` — one mutex, not several,
because these must be observed consistently together) plus two
`Condvar`s (`not_empty` for workers, `not_full` for submitters). No new
dependency; the same `Mutex`+`Condvar` idiom `GroupCommitter` already
uses throughout. Per operating brief §14, evaluated (via `PHASE2_
PERFORMANCE.md`'s benchmark) before considering any sharded/SPSC/
segmented alternative — the sweep found the *queue* was never the
measured bottleneck (§7 below), so no such alternative was built.

## 6. Completion mechanism

`CompletionSlot` (`Mutex<Option<Result<WalPosition>>>` + `Condvar`,
reached only through `Arc`) — a minimal, `std`-only "oneshot." `Completion`
consumes `self` on `wait`/`wait_timeout`, so the type system guarantees
single delivery. `CompletionGuard` (RAII, held only inside `process_entry`)
fires a fallback `Err` completion on `Drop` if the worker panics before
calling the real `complete` — guaranteeing no waiter is ever left
blocked forever by a panic it cannot see.

## 7. Backpressure

`submit()` blocks up to `submission_timeout` for queue capacity (both
`queue_capacity` and `max_queued_bytes` are checked), returning
`EngineError::Timeout` if exceeded — never drops a write, never blocks
unboundedly. This is the "bounded wait then error" choice among the
operating brief's three allowed options; see `PHASE2_ADR.md` for why.

## 8. Shutdown (three ordered steps)

1. `PoolState::Draining` — new submissions rejected immediately; every
   worker and blocked submitter woken.
2. Bounded wait (`shutdown_drain_bound`) for every worker to finish
   draining the queue and exit. Workers only exit once the queue is
   confirmed empty **and** a shutdown was requested — by that
   invariant, the queue is guaranteed empty the moment the last worker
   exits for an expected reason.
3. **Only once every worker has exited**: the underlying `GroupCommitter`
   is told to shut down too. Ordering this any earlier would make every
   still-queued (legitimately pre-shutdown) request's own `append()`
   fail immediately, per `GroupCommitter::append`'s own documented
   contract.

Idempotent; a `Drop` impl calls it as a safety net if a caller forgets.

## 9. State machine

```text
RUNNING --[shutdown() called]--> DRAINING --[queue empties, every worker exits]--> STOPPED
RUNNING/DRAINING --[every worker terminates WITHOUT a requested shutdown]--> FAILED
```

`FAILED`'s last-exiting worker drains and fails (with
`EngineError::WalUnavailable`) whatever is left in the queue, rather
than abandoning it. Both `STOPPED` and `FAILED` are terminal; `submit()`
rejects immediately from either. There is no state distinct from
`DRAINING` for "stopping" — draining is the only teardown work workers
have, so a fifth, never-actually-observed state was not added (matching
`src/wal/mod.rs`'s own standing rule against aspirational states with no
real code path).

## 10. Failure semantics

See `PHASE2_FAILURE_MODEL.md` for the full table.

## 11. What this module deliberately does not do

- **No sequence allocation of its own** (§4).
- **No cancellation of an in-flight/queued write** — once `submit()`
  returns `Ok`, the write will be attempted; a caller may only stop
  *waiting* (`wait_timeout`), never stop the write, for the same reason
  a silent retry-that-could-duplicate is forbidden.
- **No per-worker `GroupCommitter`/segment/durability domain** — the
  operating brief's own instruction (§10): the initial pool preserves
  one ordered WAL ownership boundary, not one per worker.
- **No retry of `append` under any circumstance** — only `await_durable`
  is ever retried (§13 of `PHASE2_TEST_RESULTS.md`), because retrying an
  `append` could duplicate a logical record; retrying a pure wait cannot.

# RubiXDB Phase 2 — Write Worker Pool Failure Model

Companion to `PHASE1_FAILURE_MODEL.md` (unchanged — every WAL/
`GroupCommitter` failure mode it documents still applies underneath this
module exactly as before). This file covers only failure modes
introduced by `execution::WriteWorkerPool` itself.

## 1. Failure table

| Failure | Detection | Response | Caller-visible outcome |
|---|---|---|---|
| Worker panics mid-request | The panic unwinds past `CompletionGuard`, whose `Drop` fires | That one request's `Completion` resolves to `Err(EngineError::Aborted)`; the panicking thread's own `WorkerAliveGuard::drop` decrements `workers_alive` | Caller's `wait()`/`wait_timeout()` returns `Err`, never hangs |
| Every worker has panicked | `workers_alive` reaches 0 without `PoolState::Draining` having been set | Pool transitions to `PoolState::Failed`; the last worker to exit drains and fails every request still in the queue with `EngineError::WalUnavailable` | No caller is left waiting forever; `submit()` rejects all further work immediately with a clear `WalUnavailable` error |
| `GroupCommitter::append` fails (I/O, capacity, shutdown-in-progress) | Returned directly from `append_then_await_durable_retrying` — no retry (an append failure is not a wait timing out) | Propagated verbatim to the request's `Completion` | Caller sees the real underlying error, unmodified |
| `GroupCommitter::await_durable` times out | `EngineError::Timeout` from `await_durable` | Retried, **only the wait, never the append**, up to `await_retry_budget`; the last `Timeout` is delivered if the budget is exhausted | Caller almost never sees a spurious timeout under ordinary load (verified: `PHASE2_TEST_RESULTS.md` §5's concurrency test, 50 threads × 20 ops, 0 errors); a genuine, sustained failure still surfaces honestly as `Timeout`, never silently dropped |
| `GroupCommitter` poisoned (a leader's `fsync` failed) | Every subsequent `append`/`await_durable` call returns the poisoned error | Propagated to every request still being processed or newly dequeued; the pool itself does not attempt recovery (matches Phase 1's own documented recovery model: construct a fresh `GroupCommitter`) | Every in-flight and future request fails cleanly with the poisoned error until the pool (and its underlying `GroupCommitter`) is reconstructed |
| Queue full | `submit()`'s own bounded-wait loop | Blocks up to `submission_timeout`, then `EngineError::Timeout` | Caller can retry `submit()` itself — safe, since nothing was appended (§2 below) |
| `shutdown()` called with requests still queued | `PoolState::Draining` | New submissions rejected; already-queued requests still processed normally against the still-live `GroupCommitter` | No queued request is lost; late submitters get a clear `Aborted` |
| `shutdown_drain_bound` exceeded | `shutdown()`'s own bounded wait | Returns anyway (`fully_drained: false`); surviving workers keep running and keep draining in the background | `shutdown()` itself never hangs; work already queued is not abandoned, just not yet confirmed finished by the time `shutdown()` returned |
| Worker thread fails to spawn (OS resource exhaustion) during `WriteWorkerPool::new` | `thread::Builder::spawn`'s own `io::Result` | Already-spawned workers are shut down and joined; `new` returns `Err(EngineError::Io(_))` | No thread leaked on this path; the pool never exists half-constructed |

## 2. Why retries in this module can never duplicate a record

Two, and only two, retry loops exist anywhere in `write_pool.rs`:

1. `submit()`'s own bounded wait for queue capacity — retried
   internally as a wait (not a resubmission); a caller-level retry (as
   in the benchmark harness/tests, `submit_retrying`) after a `Timeout`
   is also safe, because a `submit()` that returned `Err` never enqueued
   anything, let alone appended it.
2. `append_then_await_durable_retrying`'s retry of `await_durable` on
   `Timeout` — `GroupCommitter::append` is called **exactly once** per
   request, before the retry loop begins; only the *wait* for its
   already-assigned `seq` to become durable is retried. `await_durable`
   is a pure read of the durability watermark — calling it any number of
   times can never append a second frame.

No other code path in this module retries anything. This directly
satisfies the operating brief's absolute rule: "Do not silently retry
an operation in a way that can duplicate logical records."

## 3. Interaction with Phase 1's crash-consistency model

Every `AbortPoint` Phase 1 already instruments
(`src/wal/mod.rs::AbortPoint`) remains reachable and behaves identically
whether the calling thread is a Phase 1 direct caller or a Phase 2
worker — `GroupCommitter` cannot distinguish the two. `PHASE1_TEST_
RESULTS.md`'s 11-abort-point crash suite was re-run against the
unmodified `GroupCommitter`/`FileWal` after Phase 2's addition
(`PHASE2_TEST_RESULTS.md` §4) and remains green. No new `AbortPoint` was
added for the worker pool's own queue/completion machinery — per the
operating brief's own final decision (§16 of `PHASE2_TEST_RESULTS.md`),
this module's rejection made further fault-injection investment (beyond
the two targeted tests in §5 of that file: injected `fsync` failure,
injected worker panic) not the highest-value use of further effort.

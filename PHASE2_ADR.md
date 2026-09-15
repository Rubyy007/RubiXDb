# RubiXDB Phase 2 — Architecture Decision Records

Same format and standing rules as `PHASE1_ADR.md`: a decision recorded
here is never silently revised — a changed mind gets a new ADR entry
that supersedes, references, and keeps the old one.

## ADR-P2-1: The worker pool is an execution optimization, not a durability redesign

**Context**: the operating brief's architecture diagram places a
"Worker Pool" between logical clients and "Group Commit / Durable WAL."
It would be possible to read this as license to give the worker pool
its own sequence-allocation, batching, or durability logic.

**Decision**: `execution::WriteWorkerPool` owns no durability logic at
all. Every worker calls the *existing*, unmodified `GroupCommitter::
append`/`await_durable` — exactly the calls a Phase 1 direct caller
already made. Sequence assignment, the durability watermark, batching,
rotation, and poisoning remain entirely `GroupCommitter`'s
responsibility.

**Rationale**: `GroupCommitter` already tolerates concurrent callers
safely (that is Phase 1's entire design) and already owns every
durability guarantee this project has spent two phases establishing and
testing. Reimplementing any part of that inside the worker pool would
both violate the operating brief's explicit "do not destroy the existing
Group Commit correctness guarantees" rule and duplicate already-tested,
already-trusted logic for no benefit. The worker pool's only job is to
change *which threads* make those calls.

**Consequence**: no sequence-allocation code exists in `write_pool.rs`;
queue insertion order is explicitly not sequence order (`PHASE2_
WORKER_POOL_ARCHITECTURE.md` §4).

## ADR-P2-2: Bounded queue, "bounded wait then error" backpressure

**Context**: the operating brief allows three shapes for queue-full
behavior: block the caller, return an immediate backpressure error, or
bound the wait then error.

**Decision**: `submit()` blocks up to `config.submission_timeout`, then
returns `EngineError::Timeout`.

**Rationale**: a pure immediate-reject would turn an ordinary momentary
burst (1,000 callers submitting within the same microsecond — exactly
Phase 1's own benchmark shape) into routine failures a caller would have
to retry manually on every burst. Pure unbounded blocking would violate
the operating brief's own bounded-resources requirement, letting a
caller wait forever if every worker were wedged. A bounded wait absorbs
a real burst up to the configured timeout while still guaranteeing
`submit()` itself can never hang — the same trade-off `GroupCommitter::
await_durable`'s own follower timeout already makes, one layer down.

**Alternatives considered**: immediate rejection (rejected — see above);
unbounded blocking (rejected — violates §8's bounded-resources rule
outright).

## ADR-P2-3: `std`-only `Mutex`+`Condvar` completion, no new dependency

**Context**: a "oneshot" completion channel is a common pattern with
mature crates (`tokio::sync::oneshot`, `futures::channel::oneshot`).

**Decision**: implemented as a minimal, self-contained `CompletionSlot`
(`Mutex<Option<Result<WalPosition>>>` + `Condvar`, reached only through
`Arc`).

**Rationale, per the operating brief's own required justification for
any new dependency**: the standard-library primitive already used
throughout `GroupCommitter` itself (`batch`/`condvar`) is sufficient —
this is a single-value, single-reader, single-writer handoff, not a
general-purpose async channel; pulling in an async-runtime-oriented
crate for it would add a dependency, a maintenance surface, and (for the
`tokio`/`futures` shape specifically) an implicit expectation of an
async runtime this project does not otherwise use, none of which this
module needs. Rust's ownership model (an `Arc`-reached slot, `Completion`
consuming `self`) already gives the safety properties (no use-after-
free, no double-completion via the type system, no leak) a purpose-built
oneshot crate would also provide, at zero additional dependency cost.

**Security/maintenance**: zero new supply-chain surface; the primitive
is ~40 lines, directly auditable, and matches a pattern every maintainer
of this codebase already has to understand for `GroupCommitter` itself.

## ADR-P2-4: Retry `await_durable`, never `append`

**Context**: `GroupCommitter::append_durable` (append then a single,
non-retried `await_durable`) is the obvious first implementation for
`process_entry`, and was in fact the first one written.

**Decision, revised during this cycle**: `process_entry` calls `append`
exactly once, then retries only `await_durable` on `EngineError::Timeout`,
bounded by `config.await_retry_budget`.

**Finding that motivated the change**: the single-attempt version, under
this module's own `many_concurrent_submitters_all_land_a_gap_free_
recoverable_prefix` correctness test (50 real OS threads through a
4-worker pool), produced genuine `EngineError::Timeout` results that
propagated all the way to callers — not because any write failed or was
lost, but because a follower legitimately waited out its bounded timeout
under real, if modest, contention. `PHASE2_TEST_RESULTS.md` §13 has the
full account.

**Rationale**: `tests/group_commit/support.rs`'s own `await_durable_
retrying_on_timeout` already established, in Phase 1's own test harness,
that this is the correct way to consume a bounded-timeout wait API — a
`Timeout` means the wait itself ran out of time, not that the write was
lost (`PHASE1_FAILURE_MODEL.md` §3). Retrying the wait is provably safe
(ADR-P2-1's own boundary: `append` runs exactly once, before the retry
loop begins) and turns a transient, non-data-loss condition into a
correctly-resolved success in the common case, while still delivering a
genuine, final `Timeout` faithfully if the retry budget is exhausted.

**Alternatives considered**: leaving the single-attempt behavior and
documenting the caller's own responsibility to retry (rejected — this
project's own Phase 1 test harness already demonstrates the *library*
is the right place for this retry, not every individual caller);
retrying with no bound (rejected — violates the operating brief's own
"no permanent blocking" rule).

## ADR-P2-5: Reject the shared-queue/N-workers architecture as measured

**Context**: the critical experiment (`PHASE2_TEST_RESULTS.md` §7–§10)
measured `avg_batch_records` tracking `worker_count` almost exactly at
every worker count tested, at both 100 and 1,000 logical writers, and
found the pool's best-case configuration (`worker_count = writer_count`)
still underperforms Phase 1's direct-thread model by 28% at 1,000
writers.

**Decision**: **REJECT** this architecture as a production default.
`execution::WriteWorkerPool` remains in the tree — implemented, tested
(including two fault-injection tests, §5 of `PHASE2_TEST_RESULTS.md`),
fully documented — but is not called from anywhere in the default build,
test suite, or any recommended configuration.

**Rationale**: the operating brief's own governing rule (§26/§37): "Do
not assume the Worker Pool will be faster... If the existing
architecture is faster, retain the existing architecture." It is, at
every meaningfully-bounded worker count, and even at parity. The root
cause (`PHASE2_TEST_RESULTS.md` §7) is architectural and deterministic,
not noise or a tuning problem: a `GroupCommitter` batch can only ever
contain requests that have already reached `append()`, and a bounded
worker pool, by definition, limits how many requests can be "in"
`append()` at once to `worker_count` — which is smaller than the
logical writer count by the pool's own design premise. There is no
window-size, queue-implementation, or worker-count tuning that resolves
this without abandoning the "bounded, small worker count" premise
itself.

**What would change this decision**: a genuinely different worker
design — for example, one where the *current batch leader* actively
drains additional already-queued requests into its own batch before
`fsync` (rather than one worker processing one request per cycle, the
shape measured here) — could plausibly avoid the batch-size ceiling this
ADR's evidence identifies, since it would let a small number of workers
still expose the *queue's* full depth to a single batch, not just their
own count. This is a different hypothesis, not a parameter of the one
tested, and was not implemented or measured this cycle. Named explicitly
here as the most promising next avenue, not silently assumed to fail —
matching this project's own standing practice (`PHASE1_ADR.md` ADR-14's
own "what would change this decision" section for the pipelining
experiment).

**Verification**: `PHASE2_TEST_RESULTS.md` §4 (full regression gate,
zero Phase 1 regressions), §5 (9/9 correctness/fault-injection unit
tests), §7–§10 (the measurements this decision rests on).

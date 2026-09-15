# RubiXDB Phase 2B — Architecture Decision Records

Same format and standing rules as `PHASE1_ADR.md`/`PHASE2_ADR.md`: a
decision recorded here is never silently revised — a changed mind gets
a new ADR entry that supersedes, references, and keeps the old one.

## ADR-P2B-1: Test three genuinely different architectures, not three parameter variations of one

**Context**: the operating brief names three approaches (Leader Queue
Drain, Dedicated Batch Coordinator, Sharded Ingress) and explicitly
forbids treating them as a single design space to be tuned
("Do Not Mix Approaches... Each architecture must first be understood
independently").

**Decision**: implemented three structurally distinct modules
(`leader_drain`, `batch_coordinator`, `sharded_ingress`), sharing only
the completion-primitive types (`src/execution/common.rs` — `Completion`/
`CompletionSlot`/`CompletionGuard`, identical across all three because
the *completion* contract genuinely doesn't depend on how a request got
appended) and not sharing queue/worker-loop logic, even though Approach
B's single-coordinator design and Approach A's `worker_count=1`
configuration converge to similar *behavior*.

**Rationale**: `common.rs`'s types are a pure implementation-sharing
convenience with zero architectural content (a "oneshot," `std`-only, no
new dependency) — sharing them doesn't blur the boundary between "N
interchangeable workers coordinated at runtime" (A) and "exactly one
coordinator decided at construction time" (B), which is the actual
structural distinction under test. Deliberately **not** shared: `write_
pool.rs`'s own (already-shipped, Phase 2) copy of the same primitives —
touching already-accepted code for a pure internal refactor was judged
unnecessary risk for no behavior change.

## ADR-P2B-2: Retry only `await_durable`, never `append`, in every architecture

**Context**: identical to `PHASE2_ADR.md` ADR-P2-4's own finding,
rediscovered independently while building Approach A (`PHASE2B_FINAL_
TEST_RESULTS.md` §13's own "found and fixed" account references a
different bug in the same area — the `CompletionGuard`-ordering issue,
not this one; this decision was carried forward from Phase 2 by design,
not rediscovered from a fresh regression).

**Decision**: every architecture's `process_batch` calls `GroupCommitter::
append` exactly once per entry, then retries only the one shared `await_
durable` call for the whole batch, bounded by `await_retry_budget`.

**Rationale**: unchanged from ADR-P2-4 — retrying a pure wait can never
duplicate a record, since every entry's `append` has already completed
(with a real, assigned `seq`) before the retry loop begins.

## ADR-P2B-3: `CompletionGuard`s constructed before, not after, the shared `await_durable` call

**Context**: Approach A's first implementation built each entry's
`CompletionGuard` *after* the batch-wide `await_durable` call returned
— inside the loop that delivers results to each entry. This left every
entry in a batch with no panic-safety guard at all for the duration of
that call. Found by `one_worker_panicking_fails_only_its_own_request_
and_does_not_lose_others` hanging past a 60-second timeout.

**Decision**: every architecture (A, B, and C, the latter two written
after this fix) constructs the full `Vec<CompletionGuard>` for a
batch's appended entries **before** calling `await_durable`, and only
consumes each guard's `complete()` afterward.

**Rationale**: `CompletionGuard`'s entire purpose is to fire a fallback
completion if the code between its construction and its explicit
`complete()` call panics — constructing it *after* the one call most
likely to observe an injected or genuine failure (the `fsync` a leader/
coordinator performs, reached through `await_durable`) defeats that
purpose entirely for exactly the scenario it exists to protect against.

**Verification**: `PHASE2B_FINAL_TEST_RESULTS.md` §13 — the fixed
version resolves the same scenario in under 1ms, down from an
unresolved 60+ second hang.

## ADR-P2B-4: Approach C evaluated once (Attempt C1 only), per the operating brief's own conditional framing

**Context**: the operating brief's Approach C section opens *"If A and
B fail to reach the required performance, evaluate a third
architecture..."* Both A and B met both throughput targets comfortably
on their first (A) or only (B) attempt.

**Decision**: implemented and measured Approach C once, not the full
three-attempt cycle A and B each received.

**Rationale**: the brief's own conditional language ties Approach C's
existence to A/B failing; since neither failed, C's role shifts from
"the architecture that might close a remaining gap" to "a confirmatory
check of one specific hypothesis (queue contention) that A/B's own
success already made unlikely to matter." One clean measurement is
sufficient to confirm or refute that specific hypothesis; a further
two attempts optimizing an architecture with no identified problem to
fix would not be evidence-driven (operating brief §13: "do not perform
an unlimited sequence of random changes").

**Alternatives considered**: skip Approach C entirely (rejected — the
operating brief requires `PHASE2B_ARCHITECTURE_C.md` and a three-way
comparison table regardless of A/B's outcome; a real, if minimal,
implementation was judged more honest than a document describing an
architecture that was never actually built and measured).

## ADR-P2B-5: Winner — Approach B (Dedicated Batch Coordinator)

**Context**: `PHASE2B_FINAL_TEST_RESULTS.md` §6–§10's full comparison.
Correctness/durability/safety-under-failure tied across A, B, and C.
Throughput: B wins outright at 100 writers (median 17,512, best of any
architecture/configuration measured), statistically tied with A's best
and C at 1,000 writers. Complexity: B is simplest — no worker-election
machinery (unlike A) and no per-shard bookkeeping/cross-shard
coordination (unlike C).

**Decision**: **Approach B is the recommended default architecture.**
Approach A at `worker_count=2` is recorded as the explicit, documented
alternative for deployments requiring hot-standby redundancy against a
coordinator-thread panic, at the honestly-recorded cost of a small
100-writer throughput margin (median 14,652 vs. target 15,000 — within
this machine's own noise band, not a confirmed structural failure).
Approach C is rejected outright (ADR-P2B-4's own finding: no measured
benefit over B, more complexity).

**Rationale**: the operating brief's own priority order — correctness,
durability, stability, throughput, latency, complexity, in that order —
places complexity last, but only *as a tiebreaker*. With correctness/
durability/stability genuinely tied and throughput not decisively
favoring any one architecture at the level that matters most
structurally (redundancy vs. no redundancy is an orthogonal, deployment-
level choice, not a strict throughput ranking), the tiebreaker is
reached, and B wins it clearly.

**What would change this decision**: if a future cycle's evidence shows
the `leader_active`-stuck-forever limitation (`PHASE2B_FAILURE_MODEL.md`
§3) is a materially likely failure mode in real production conditions
(not just a fault-injection artifact), the calculus shifts meaningfully
toward requiring Approach A's redundancy as the default, not merely an
optional alternative — this has not been measured (real-world panic
rates are not something this cycle's fault-injection tests can estimate)
and is named here as an open question, not resolved.

## ADR-P2B-6: Neither Phase 1 nor Phase 2's `WriteWorkerPool` architecture is modified or removed

**Context**: `write_pool.rs` (Phase 2, rejected as a production default)
and Phase 1's direct-thread `GroupCommitter` calls remain in the tree,
unmodified, alongside the three new Phase 2B modules.

**Decision**: no cleanup, removal, or consolidation of prior phases'
code was performed as part of this cycle.

**Rationale**: matches this project's own standing practice
(`PHASE1_ADR.md` ADR-14, `PHASE2_ADR.md` ADR-P2-5) of keeping measured,
correct, but not-adopted-as-default implementations in the tree as
documented negative results rather than deleting them — each remains
available, tested, and instructive for any future cycle that revisits
these design questions.

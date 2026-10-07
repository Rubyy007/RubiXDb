# ADR-OBS-01 — Should `/readyz` include committer-poisoned / coordinator-failed?

**Status:** ACCEPTED 2026-10-07, applied in part: step 3 is applied to `instance.coordinator_state` and `instance.healthy` only; **`/readyz` is explicitly exempt and stays frozen at `ready: true`** (it was PROPOSED / OPEN until then; the Proposal section below is the original text and is kept as history). **Date:** 2026-10-06 (proposed), 2026-10-07 (accepted). **Decided by:** maintainer.
**Context mission:** Full Observability closure, Decision D5 (option R2).

## Context

* `GET /readyz` has always returned `ready: true` while the engine handle is alive and answering, regardless of `storage_state` (`api/src/routes/health.rs`). It is a certified, public contract.
* Two terminal conditions exist in the engine in which no write can complete and nothing in-process repairs it: the WAL coordinator thread has died (`PoolState::Failed`, `src/execution/batch_coordinator.rs`, `CoordinatorAliveGuard`) and a `GroupCommitter` poisoned by an fsync error or leader panic (`src/wal/group_commit.rs`, `PoisonReason`). In both, `/readyz` still says `ready: true`.
* An attempt to derive readiness from `GroupCommitStats::sync_failures()` made `/readyz` and `instance.readiness` flap under write load, because that value is `sync_attempts - sync_successes` read from two independent atomics and is transiently 1 while an fsync is in flight (`PHASE_WRITE_ENGINE_MEMORY_INVESTIGATION.md` section 7). It was withdrawn (D5 option R2).
* The observability layer now reports the coordinator's terminal state separately as `instance.coordinator_state` (`alive` | `poisoned` | `not_started`) and counts `poisoned` as `instance.healthy = failed`. It cannot see a committer poisoned by an fsync error, because `LsmEngine` has no accessor for it.

## Proposal (not applied)

1. Define readiness as: the engine accepts work **and** the coordinator is alive **and** the committer is not poisoned.
2. To make (1) observable without a racy counter, add a read-only accessor that exposes `GroupCommitter::is_poisoned()` through `BatchCoordinatorStats` / `LsmEngine` (an engine change: `src/execution/batch_coordinator.rs`, `src/lsm/mod.rs`; it needs its own mission authorization).
3. Change `observability::sampler::ready()` to that definition; `/readyz` and `instance.readiness` follow because both call it.
4. Re-run the lifecycle certification (a load balancer or supervisor that polls `/readyz` will start receiving `ready: false`), and document the change in the API contract.

## Consequences

* Positive: `/readyz` says "not ready" when writes can no longer succeed; `instance.healthy` and `/readyz` stay consistent.
* Negative: a change to a certified endpoint's meaning; a small engine accessor; every consumer that treats `/readyz` as constant must be reviewed (the embedded host, the CLI client's attach path, the frontend).
* Until decided: `/readyz` stays `ready: true`; the terminal coordinator state is visible in `instance.coordinator_state`; a committer poisoned by an fsync error is visible only as failed writes (and in the certified `/v1/admin/status` fields).

## Not part of this ADR

`errors.wal_sync_failures` and `/v1/admin/status` `sync_failures` / `poisoned` still read the racy counter; they are recorded in `OPEN_ITEMS.md`.

## Update 2026-10-07 (follow-up mission; the sections above are unchanged)

Step 2 of the proposal (the read-only accessor) now exists: `LsmEngine::committer_poisoned()` -> `BatchCoordinatorPool::committer_poisoned()` -> `GroupCommitter::is_poisoned()` (`PHASE_RUBIXDB_FULL_OBSERVABILITY_ADR_02.md`; nothing under `src/wal/`). It is used for `/v1/admin/status` `wal.poisoned` only. Steps 1, 3 and 4 are **not** applied and this ADR stays **PROPOSED / OPEN**: `/readyz` is unchanged, and `instance.healthy` / `instance.coordinator_state` do not reflect a poisoned committer (observed: `ready: true`, `healthy`, `alive` with a really poisoned committer). The statement above that the layer "cannot see a committer poisoned by an fsync error, because `LsmEngine` has no accessor" is therefore out of date; the policy question is what remains.

## Decision (2026-10-07, maintainer; follow-up mission 3) - applied

* `instance.coordinator_state` is `poisoned` when `LsmEngine::committer_poisoned()` (that is `GroupCommitter::is_poisoned()`, the single authoritative bit; ADR-OBS-02) is true **or** the pool state is `Failed`; `alive` while the coordinator thread is up; `not_started` otherwise (`sampler::coordinator_state_from(coordinator_alive, pool_state, committer_poisoned)`, evaluated once per sampler tick).
* `instance.healthy` is `failed` when `coordinator_state == poisoned` (Decision D1; `classify_health` already said so, but the rule could never fire while the coordinator state could never be `poisoned` from a poisoned committer).
* **`/readyz` exemption, stated explicitly:** `GET /readyz` keeps returning `ready: true` constant, as certified; `instance.readiness` stays the same value (both call `sampler::ready()`, unchanged). A poisoned committer therefore makes `wal.poisoned`, `coordinator_state` and `healthy` flip together while `/readyz` and `instance.readiness` do not. Steps 1 and 4 of the Proposal (a readiness definition that includes the coordinator and the committer; re-running lifecycle certification) are **not** applied and are not planned by this decision.
* Test: `wal_poisoned_stays_false_under_write_load_and_turns_true_only_when_the_committer_is_poisoned` (`api/tests/observability.rs`) installs the fsync-fault hook and requires the three signals to agree (`wal.poisoned` true, `coordinator_state` `poisoned`, `healthy` `failed`) and to stay so, with `/readyz` `ready: true` and `instance.readiness` `ready`; with the poison bit hidden from the sampler (mutation) the same test fails (`coordinator_state` stays `alive`). Unit: `policy_tests::coordinator_state_comes_from_pool_state_and_the_committer_poison_bit`.
* The earlier "Consequences / Until decided" bullet (`a committer poisoned by an fsync error is visible only as failed writes`) is superseded by this decision.

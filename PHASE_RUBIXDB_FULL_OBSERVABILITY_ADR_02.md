# ADR-OBS-02 — `/v1/admin/status` `wal.poisoned` is read from the committer's poison bit

**Status:** ACCEPTED and applied (maintainer instruction, follow-up mission 2026-10-07): a bug fix, not a semantic change. **Date:** 2026-10-07. **Decided by:** maintainer.
**Related:** `PHASE_RUBIXDB_FULL_OBSERVABILITY_ADR_03.md` (the racy counter), `PHASE_RUBIXDB_FULL_OBSERVABILITY_ADR_01.md` (readiness; stays PROPOSED / OPEN).

## The field and its history

* `GET /v1/admin/status` has carried `wal.poisoned` since the operations layer was added (commit `995a385`, 2026-10-03, "ops: backup/restore, integrity check, maintenance, observability, startup guard"). It was written as a *derived* value, with this comment in `api/src/routes/admin.rs`: the certified committer is poisoned permanently by any failed fsync (`a_failed_leader_fsync_poisons_the_committer_permanently`), so `sync_failures > 0` <=> poisoned; a dead coordinator (`PoolState::Failed`) also stops all writes. The expression was `g.sync_failures() > 0 || pool_state == Failed`.
* The field **always meant "terminal"**: the committer accepts no further write and nothing in-process repairs it. Its consumers (`rubixdb` CLI inspection output, operators) read it that way.
* The derivation was wrong because of its operand. `GroupCommitStats::sync_failures()` is `sync_attempts - sync_successes`, two independent atomics, the first incremented before the `fsync` call and the second after it succeeds (`PHASE_WRITE_ENGINE_MEMORY_INVESTIGATION.md` section 7 had already traced a "`sync_failures=1`" reading in the 200-writer soak to this race and concluded it is "a sampling/counter-race artifact, not a real sync failure"). So `wal.poisoned` read **true while an fsync was merely in flight**.

## Phantom evidence (before)

Binary `rubixdb_final2.exe` (source of `a3540ab`), 4 writers, `GET /v1/admin/status` and `GET /v1/metrics/system` polled in turn (`E:\rubixdb_closure\final\wsf_version.txt`, script `wsf_version.py`):

* **108 polls: `wal.poisoned` was `true` in 100 of 108** (and `errors.wal_sync_failures` non-zero in 97 of 108, values seen `{0, 1}`), with no write failing.
* The same racy value, used in a first draft of readiness, made `/readyz` return `ready: false` in 374 of 426 polls and `instance.healthy` read `failed` in 76 of 85 (`readyz_flap_before_after.txt`); that is why readiness was fixed to a constant (D5, option R2) in the closure.

## Decision

`wal.poisoned` = `engine.committer_poisoned() || pool_state == Failed`, where `committer_poisoned()` is a read-only pass-through of **`GroupCommitter::is_poisoned()`**, the single bit (`self.batch.poisoned.is_some()`) the committer itself consults before accepting an append. The `|| pool_state == Failed` term is kept: it is the part of the old expression that was always correct (a dead coordinator is also terminal), so the field's meaning does not change.

Implementation (no file under `src/wal/` is touched):

* `BatchCoordinatorPool::committer_poisoned()` (`src/execution/batch_coordinator.rs`) calls `self.committer.is_poisoned()`; `LsmEngine::committer_poisoned()` (`src/lsm/mod.rs`) passes it through. Both are observational and are never consulted by an engine decision. `is_poisoned()` takes the committer's short `batch` mutex; that mutex is dropped before the leader runs (`await_durable` drops the guard before `run_as_leader`, and followers wait on the condvar, which releases it), so a poll never waits for an `fsync`.
* A test-only seam (`#[cfg(any(test, feature = "test-util"))]`): `BatchCoordinatorPool::install_fsync_fault_hook` / `LsmEngine::install_wal_fsync_fault_hook` forward to the existing `GroupCommitter::install_fsync_fault_hook`, so an API-level test can poison the committer for real.

This is step 2 of ADR-OBS-01's proposal (the accessor). Steps 1, 3 and 4 (changing what `/readyz` and `instance.readiness` mean) are **not** applied: `/readyz` stays `ready: true`, `instance.coordinator_state` is unchanged (`alive` / `poisoned` / `not_started`, from public pool state), and `instance.healthy` is unchanged. ADR-OBS-01 remains PROPOSED / OPEN.

## Verification (after)

See `PHASE_RUBIXDB_FULL_OBSERVABILITY_CERTIFICATION.md` section 25.5 for the real-process polls (4 writers, at least 100 polls of `wal.poisoned` and of `errors.wal_sync_failures`) and the real fsync-failure test. The test `wal_poisoned_stays_false_under_write_load_and_turns_true_only_when_the_committer_is_poisoned` (`api/tests/observability.rs`) polls 120 or more times under 4 concurrent writers (it asserts that real fsyncs happened during the polls), then installs a failing fsync, requires the next write not to be acknowledged, and requires `wal.poisoned` to become and stay `true`.

## Consequences

* `wal.poisoned` can no longer be `true` unless the committer really is poisoned (or the coordinator is dead). It is a certified endpoint's field whose *value* was wrong; its name, type and meaning are unchanged.
* The sibling field `wal.sync_failures` on the same endpoint (and the CLI text that prints it) still shows the racy difference; see ADR-OBS-03 and `OPEN_ITEMS.md`.
* `instance.healthy` and `instance.coordinator_state` do **not** reflect a poisoned committer. With the accessor available that is now a policy choice rather than a technical limit; it belongs to ADR-OBS-01 and is recorded in `OPEN_ITEMS.md`.

## Update 2026-10-07 (mission 3; the text above is unchanged)

The last Consequences bullet is superseded: ADR-OBS-01 is ACCEPTED (step 3 applied to `instance.coordinator_state` and `instance.healthy`, `/readyz` exempt), so `coordinator_state` and `healthy` now do reflect a poisoned committer, using the accessor this ADR added. The sibling `wal.sync_failures` mentioned in the second bullet is now `null` (ADR-OBS-03, scope extended).

# ADR-OBS-03 — `errors.wal_sync_failures` and `wal.sync_failures` are reported as `null` in v1; a correct value needs an engine change

**Status:** ACCEPTED for v1 as a documented limitation (maintainer instruction, follow-up mission 2026-10-07); scope extended the same day to the sibling field `GET /v1/admin/status` `wal.sync_failures` (see "Scope"). The engine change below is **NOT REQUIRED FOR V1** and is **not applied**.
**Date:** 2026-10-07. **Decided by:** maintainer. **Related:** `PHASE_RUBIXDB_FULL_OBSERVABILITY_ADR_02.md` (the poison bit), `PHASE_RUBIXDB_FULL_OBSERVABILITY_ADR_01.md` (coordinator state and health, ACCEPTED; `/readyz` exempt).

## Context

* `GET /v1/metrics/system` carried `errors.wal_sync_failures`, taken from `GroupCommitStats::sync_failures()` (`src/wal/group_commit.rs`), which is `sync_attempts.saturating_sub(sync_successes)`.
* The two operands are separate atomics: the leader increments `stat_sync_attempts` **before** the `fsync` call and `stat_sync_successes` only **after** it returns `Ok`, and a stats snapshot loads them with `Ordering::Relaxed` one after the other. Whenever a snapshot is taken while an `fsync` is in flight, the difference is 1 although nothing failed. `PHASE_WRITE_ENGINE_MEMORY_INVESTIGATION.md` section 7 recorded this in the 200-writer soak (76 of 121 samples read 1, 45 read 0, `completed_err` 0 throughout, no corruption at recovery).
* It was measured again on the observability closure binary under 4 writers: `errors.wal_sync_failures` was non-zero in **97 of 108** polls, values seen `{0, 1}`, with no write failing (`E:\rubixdb_closure\final\wsf_version.txt`; see also `PHASE_RUBIXDB_FULL_OBSERVABILITY_CERTIFICATION.md` section 24.11, row A81).
* A field that reads 1 for most of the time under load, while nothing is wrong, is worse than no field: it trains operators to ignore it, or to alarm on a healthy instance.

## Decision

1. `errors.wal_sync_failures` is **always `null`** in v1. The key stays present, so the response schema (key set and the "number or null" typing the other `errors.*` fields use) does not change; only the value does. `Snapshot` no longer carries the number.
2. The field's doc comment (`api/src/routes/metrics_system.rs`) says so and points here.
3. Nothing is inferred in its place. In particular the field is **not** derived from `wal.poisoned`, `completed_err`, or `coordinator_state`: each of those answers a different question.
4. The terminal condition that matters operationally, "the committer is poisoned", is reported by `/v1/admin/status` `wal.poisoned`, which now reads the single authoritative bit (`GroupCommitter::is_poisoned()`; ADR-OBS-02).

## What a correct value would need (NOT REQUIRED FOR V1)

A non-racy count of failed `fsync` calls means counting **failures**, not `attempts - successes`: a `stat_sync_failures` atomic incremented at the one place the leader sees `fsync_result` is `Err` (`run_as_leader`, `src/wal/group_commit.rs`), exposed through `GroupCommitStats`. That is a change inside `src/wal/`, a protected path (`CLAUDE.md`): it needs an ADR and an explicit mission authorization, neither of which exists, and it is not needed for v1 because the poison bit already reports the only outcome that follows from a failed `fsync` (the committer is poisoned permanently, so there is at most one failure to count).

## Scope (extended 2026-10-07, same decision, no new ADR)

The same justification covers the sibling the certified `GET /v1/admin/status` carries: **`wal.sync_failures` is also `null`** (key present), exactly like `errors.wal_sync_failures`. It was the same racy `sync_attempts - sync_successes` (non-zero in 129 of 142 real-process polls under 4 writers, values `{0, 1}`, no write failing). This is a bug fix to the value of a certified endpoint's field, the same class as ADR-OBS-02, not a semantic change: the field's name, position and type-or-null are unchanged. The field's comment in `api/src/routes/admin.rs` points here. Consumers: the CLI inspection output (`rubixdb` `cli/src/ops_cmd.rs`) keeps its `sync_failures=` text and now prints **`-`** (its helper already renders JSON `null` as `-`, so no code was needed beyond a comment; the number is not removed from the line, it just never shows the racy value); the GUI Operations page (`frontend/src/pages/OperationsPage.tsx`, type `number | null`) shows `-`. `sync_attempts` is unchanged and is a true count. The terminal state is `wal.poisoned` (ADR-OBS-02).

## Consequences

* Positive: no field in `/v1/metrics/system` states something false. The read of `sync_failures()` in the sampler is gone.
* Negative: consumers lose a field they could not have trusted anyway; none in this repository read it (searched: `frontend/src`, `cli/`, `api/`).
* Tracking: `OPEN_ITEMS.md` (dated 2026-10-07); matrix row A81 is **NOT REQUIRED** (not PASS: there is no value to certify).

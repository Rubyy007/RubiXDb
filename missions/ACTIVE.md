# ACTIVE MISSION

No active mission.

Last closed mission: Full Observability final closure (2026-10-07) -- see `PHASE_RUBIXDB_FULL_OBSERVABILITY_CERTIFICATION.md`
section 27 (after sections 24-26) and `PROGRESS.md`. The next mission starts only on an explicit instruction; this file is replaced at that point.

Next mission (named by the maintainer in the "F-11 Final Closure" mission prompt, 2026-10-11): **F-08**. Not started; no mission prompt has been written yet. F-11 state: `PHASE_ITEM_F11_FINAL_CLOSURE.md`.

F-08 verification mission (2026-10-11) stopped at its stop condition: PARTIALLY VERIFIED, NAMED GAPS REMAIN, see `PHASE_ITEM_F08_VERIFICATION.md`. No next mission is named; the maintainer decides GAP-A..F (section 10 of that document).

F-08 second pass (2026-10-11) stopped at its stop condition: `PHASE_ITEM_F08_VERIFICATION.md` section 12. Decisions owed by the maintainer: GAP-A..F plus the check-path mutation remedy; no next mission named.

---

## SQL Mission 1 of 4 -- Subqueries, CTEs, Set Operations (opened 2026-10-11)

Prompt: "RubixDB SQL Mission 1 of 4" (scalar / IN / EXISTS / correlated subqueries, derived tables,
non-recursive and recursive CTEs, UNION / INTERSECT / EXCEPT with ALL variants). Feature matrix and
per-slice reports: `sql/MISSION1_FEATURE_MATRIX.md`. Evidence: `sql/mission1_evidence/`.

| Slice | State |
|---|---|
| A -- scalar / IN / EXISTS, uncorrelated (+ UPDATE/DELETE WHERE) | delivered 2026-10-11, awaiting maintainer review |
| B -- correlated subqueries | not started |
| C -- derived tables, non-recursive CTEs (+ LATERAL decision) | not started |
| D -- recursive CTEs with depth / row bounds | not started |
| E -- set operations | not started |

Stop condition: each slice is a distinct delivery; the mission closes with the closeout report
specified in the prompt. Nothing here is committed. (The F-08 notes above this section are the
maintainer's uncommitted text and are untouched.)

- 2026-10-11 update: Slice B (correlated subqueries, B1..B6 incl. `UPDATE ... SET (subquery)`) delivered, awaiting maintainer review; report in `sql/MISSION1_FEATURE_MATRIX.md` section 6. Slices C, D, E not started.

- 2026-10-11 update: Slice C (derived tables, `[LEFT] JOIN LATERAL`, non-recursive CTEs, C1..C6) delivered, awaiting maintainer review; report in `sql/MISSION1_FEATURE_MATRIX.md` section 6. Slices D, E not started (recommended order: E before D).

- 2026-10-12 update: Slice E (set operations, E1..E7) delivered, awaiting maintainer review; report in `sql/MISSION1_FEATURE_MATRIX.md` section 6. Slice D (WITH RECURSIVE) not started; it can now build on `UNION [ALL]` and the CTE machinery.

- 2026-10-12 update: Slice D (WITH RECURSIVE, D1..D7) delivered, awaiting maintainer review; report in `sql/MISSION1_FEATURE_MATRIX.md` section 6. **Mission 1 (SQL: subqueries, CTEs, set operations) is complete: Slices A-E delivered, none committed by this mission.** Next: Mission 2, on the maintainer's instruction.


---

## SQL Mission 2 of 4 -- Window Functions and Advanced Aggregation (opened and delivered 2026-10-13)

Prompt: "RubixDB SQL Mission 2 of 4" (window functions -- `OVER`, frames, ranking, navigation, aggregates, `QUALIFY` -- and advanced
aggregation -- `GROUPING SETS`/`ROLLUP`/`CUBE`/`GROUPING()`, aggregate `FILTER`, `DISTINCT` aggregates). Feature matrix, decisions, limits,
evidence and per-sub-slice reports: `sql/MISSION2_FEATURE_MATRIX.md`; evidence `sql/mission2_evidence/`.

| Sub-slice | State |
|---|---|
| 2A -- `OVER`, `PARTITION BY`, `ORDER BY`, default frames, ranking, aggregates (gate: the silent-drop mutation M08 caught) | delivered 2026-10-13, awaiting maintainer review |
| 2B -- `ROWS` / `RANGE` frames | delivered 2026-10-13, awaiting maintainer review |
| 2C -- `LAG`/`LEAD`/`FIRST_VALUE`/`LAST_VALUE`/`NTH_VALUE`/`NTILE`/`PERCENT_RANK`/`CUME_DIST` | delivered 2026-10-13, awaiting maintainer review |
| 2D -- `QUALIFY` | delivered 2026-10-13, awaiting maintainer review |
| 2E -- `GROUPING SETS` / `ROLLUP` / `CUBE` / `GROUPING()` | delivered 2026-10-13, awaiting maintainer review |
| 2F -- aggregate `FILTER`, `DISTINCT` aggregates | delivered 2026-10-13, awaiting maintainer review |

**Mission 2 is complete: nothing is committed (instructed).** Six older tests that pin refusals the mission implements fail by design and were left
untouched (`sql/MISSION2_FEATURE_MATRIX.md` 6.2). Next: Mission 3, on the maintainer's instruction.


---

## Durability / PITR / encrypted incremental backups / manifest versioning -- persistence-correctness mission (opened 2026-10-13)

Slices run strictly one at a time, each reported and stopped; no commits or pushes without separate authorisation. Priority: correctness > durability > recoverability > security > resource safety > latency > throughput.

| Slice | State |
|---|---|
| D0 -- reconnaissance (docs only) | done 2026-10-13, verdict PASS WITH OPEN ITEMS, awaiting maintainer direction; `docs/PERSISTENCE_MAP.md`, `docs/DURABILITY_CONTRACT.md`, `docs/PREREQUISITES.md`, `docs/D0_BASELINE.md` |
| D1a fault injection + kill-9 | not started |
| D1b power-loss persistence model | not started |
| D1c power-loss recovery invariants | not started |
| D2 manifest versioning | not started (requires D1c) |
| D3 full backup / D4 incremental / D5 encryption | not started |
| D6 PITR | BLOCKED at D0: no WAL retention or replay-to-seq primitive |
| D7 integrated certification | not started |

- 2026-10-13 D1a step 1: `docs/adr/ADR-FAULTPOINTS-01.md` written (PROPOSED, awaiting maintainer approval AD-1..AD-7). No source, test or Cargo file edited; no call site added. D1a implementation has not started.

- 2026-10-13 D1a done, verdict PASS WITH OPEN ITEMS, awaiting maintainer direction: registry + kill-9 rig + oracle delivered and executed (`docs/CRASH_CLASS_MATRIX.md`, `docs/adr/ADR-FAULTPOINTS-01.md`, D1a appendix of `docs/D0_BASELINE.md`). Class A 34/34, class B single-writer 161/161, relational 30/30, concurrent 28/30 (2 FAIL: F-D1a-1, a pre-existing acknowledged-write loss under concurrent writers). Class C / D empty. Stop gate: maintainer decisions F-D1a-1, F-D0-1/2/3/4/5 (`docs/D0_BASELINE.md` D1a appendix D). D1b is gated on D1a + those decisions; not started.

- 2026-10-13 D1a decision gate: STEP 1 documented, awaiting the maintainer's decisions on F-D1a-1 and F-D0-1/2/3/4/5 (`docs/D1a_DECISION_GATE.md`); STEP 2 (D1b) NOT started. Fixes are PROPOSED only (prototype outside the repository). Two existing F-07 tests pin the 64-byte zero WAL tail and need explicit authorisation to change if F-D0-1 is fixed.

- 2026-10-13 D1a decision gate RESOLVED: the maintainer approved all recommended fixes ("Approve all recommended fixes, then proceed to D1b"); F-D1a-1, F-D0-1/2/3/5 fixed in the repository, F-D0-4 accepted (fix in D2), F-D1a-2 still open; ADRs ACCEPTED/IMPLEMENTED; contract section 12 in force (`docs/D1a_DECISION_GATE.md` section 0.0 and 10).

- 2026-10-13 D1b done, verdict PASS WITH OPEN ITEMS (two new class C findings), awaiting maintainer direction: `crate::persist` + `SimFs` (feature `sim-fs`), `docs/PERSISTENCE_MODEL.md`, crash-injection cycle, proof tests P1-P4 all pass; workspace 2,181 / 2 / 32; D1a suite green. Findings F-D1b-1 (WAL "corrupt" instead of "torn" after a lost first sector; G14) and F-D1b-2 (data directory never dir-synced; G6) need a maintainer decision before D1c fixes expected states. Real power loss NOT TESTED. D1c NOT started.

- 2026-10-14 D1c STEP 1 (decisions) landed, awaiting the maintainer: F-D1b-1 FIXED (ADR-TORN-TAIL-LOST-SECTOR-01, G14 closed), F-D1b-2 SPLIT (ADR-DATA-DIR-DURABILITY-01: POSIX fixed in simulation, Windows accepted with the verbatim limitation). NEW finding F-D1b-3 / G15 (model M3 half-sector tear vs A2) needs a decision before D1c. STEP 2 (D1c) NOT started; it starts only on an explicit "proceed to D1c".

- 2026-10-14 FREEZE (maintainer): durability work paused for release stabilisation; D1c deferred and not started; F-D1b-3 decided Option 1 (conform M3 to A2) but not applied. Active work: release stabilisation (build, essential tests, packaging, honest limitations). See PROGRESS.md.

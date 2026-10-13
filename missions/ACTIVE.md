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

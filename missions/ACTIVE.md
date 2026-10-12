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

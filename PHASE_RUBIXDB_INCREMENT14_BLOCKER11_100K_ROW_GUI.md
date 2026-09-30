# Increment 14, Blocker 11 — The True 100,000-Row GUI Case

Real production frontend build, real `rubixdb gui` product path.
`frontend/e2e-gui/hundred_k_rows.spec.ts`.

## 1. Inspected the actual production limit first (never bypassed)

`sql/src/exec/mod.rs::execute_query` (§`ExecLimits::default()`,
`max_result_rows: 100_000`): the check is

```rust
if rows.len() >= limits.max_result_rows {
    return Err(SqlError::ResourceLimit { .. });
}
rows.push(tuple.projected);
```

evaluated **before** each push — so a result of exactly 100,000 rows
completes successfully (the check only trips when a 100,001st tuple
would be pushed). **100,000 rows are legitimately requestable**, per
the mission's own conditional instruction. No production limit was
weakened, raised, or bypassed anywhere in this work.

## 2. Real boundary proof (both sides)

- Seeded exactly 100,000 rows (batched multi-row `INSERT`, 24.5s real
  seed time).
- Temporarily inserted a 100,001st row and confirmed the real,
  unweakened rejection: `HTTP 413`, `code: RESOURCE_LIMIT`, `detail:
  "query result exceeds max_result_rows (100000)"`.
- Removed the extra row, restoring the table to exactly the real
  100,000-row case the GUI then queries.

## 3. Real GUI execution at the true limit

```
execute+render=1323ms renderedDomRows=200 jsHeapBytes=20500000
scroll-to-bottom=36ms
pagination click-to-settle=130ms
```

- **Execute + render**: 1,323ms for the actual 100,000-row `SELECT`,
  real HTTP transfer + real JSON parsing + real React render, measured
  end to end via the real UI (`Execute` click → `Result (100000
  rows)` text visible).
- **Bounded-DOM contract holds at the true limit**: exactly 200
  `<tr>` elements rendered, same as at 100/1,000/10,000 rows in
  Increment 13's own evidence — confirms pagination's O(page-size)
  rendering cost is genuinely independent of result size all the way
  up to the real production ceiling, not just up to 10,000.
- **Scroll and pagination stay responsive**: 36ms to scroll the
  `.app-content` region to bottom, 130ms for a real `Next` page click
  to settle — both trivial, proving the 100,000-row result set itself
  (held in React state, not re-rendered per row) does not degrade UI
  responsiveness.

## 4. Verdict

**100,000-ROW GUI = PASS** — the real production limit was inspected
first (not guessed, not bypassed), the exact boundary was proven on
both sides with real requests, and the true 100,000-row case was
executed through the real GUI product path with bounded rendering and
responsive scroll/pagination. This closes Increment 13's own named
gap (judged "not worth ~25-30 minutes of session time" previously —
actual real cost here was under 30 seconds total, including seeding).

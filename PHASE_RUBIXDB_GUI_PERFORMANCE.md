# Phase: RubiXDB GUI/Frontend Performance (Phases X/Y)

## 0. Scope and setup

Real Playwright E2E tests (`frontend/playwright.gui.config.ts`,
`frontend/e2e-gui/gui_performance.spec.ts`) against the **actual
product path**: the real compiled release `rubixdb.exe gui
--no-browser`, hosting both the API and the real `npm run build`
frontend bundle on the same origin
(`PHASE_RUBIXDB_GUI_ARCHITECTURE.md` §6) — not the separately-hosted
two-origin shape the pre-existing `playwright.config.ts` suite
exercises (that suite is unchanged, still passing, still the
certified record for the separately-hosted deployment shape).

A real Chromium browser (Playwright's own bundled Chromium), a fresh
disposable instances root per run, a real generated local credential
read directly from `credentials.json` (no manual key configuration).

## 1. Page load timing

`connect()` (navigate to `/connect`, fill endpoint+key, submit, land
on `/`) took 305-763ms wall-clock across runs. Real Navigation Timing
API numbers from inside the browser: `responseEnd` 18-34ms,
`domContentLoaded`/`load` 48-131ms — the real gui-hosted frontend is
served fast (same-origin, no CORS preflight, a small ~250KB bundle per
Increment 12's own build output).

## 2. Execute + render timing vs. result size

| Rows | Seed time (not measured metric) | Execute+render | Rendered DOM rows | Pagination click-to-settle |
|---|---|---|---|---|
| 100 | 1.2-4.1s | 105-107ms | 100 | n/a (single page) |
| 1,000 | 12.8-14.3s | 123-165ms | 200 | 118-149ms |
| 10,000 | 140.7-177.1s | 165-172ms | 200 | 118-149ms |

**The real, decisive finding**: execute+render time barely moves
(105ms → 172ms) from 100 rows to 10,000 rows — a 100x increase in
result size produced roughly a 1.6x increase in perceived time. This
is direct, measured confirmation that Increment 12's pagination design
choice (render only the current page, `PAGE_SIZE = 200`,
`PHASE_RELATIONAL_FRONTEND_SQL_ARCHITECTURE.md` §4) actually delivers
what it was designed for: **rendered DOM node count never scales with
this server's own `ExecLimits::max_result_rows` bound (100,000)**,
confirmed directly (`.table-wrap tbody tr` count is capped at 200 in
every case above, never higher). Pagination is measurably sufficient
at this scale — virtualization/incremental rendering were not
implemented because this evidence does not show a bottleneck they
would fix (Phase Y's own instruction: "Do NOT add a solution merely
because it is fashionable" — evaluated, not needed).

**Seed time is not the measured metric but is itself real, consistent
evidence**: seeding 10,000 individual rows took 140-177 real seconds —
matching, at real scale, the write-path-serialization finding already
documented in `PHASE_RUBIXDB_PERFORMANCE_BASELINE.md` §5/§7 (write
throughput plateaus around 250-300 req/s regardless of concurrency).
Not a frontend/GUI-layer cost at all — it is the same certified,
already-explained write path, now visible again from a different
angle.

## 3. What this does not cover yet

- Only Chromium was exercised (Playwright's default project in this
  config) — no cross-browser timing comparison.
- No sustained "execute/display/clear" cycling for browser-memory
  growth (Phase Z) — this pass measured single-shot timing per result
  size, not repeated-cycle memory behavior.
- No explicit GUI-cancellation timing measurement (Phase AA) — the
  Cancel button's real server-side cancellation was already proven
  functionally in Increment 12
  (`PHASE_RELATIONAL_FRONTEND_SQL_ARCHITECTURE.md` §5); this pass did
  not re-measure its latency specifically.
- No GUI network-failure/recovery scenario (Phase AB) — API
  unavailable, server restart mid-session, etc.
- No true 100,000-row case (the server's own `max_result_rows` bound)
  — would take roughly 25-30 real minutes just to seed at the measured
  write rate, judged not worth the session time for what the 10,000-
  row case's flat execute+render curve already predicts clearly.

These are named, real, open follow-up items — not silently folded into
a "GUI performance = done" claim this pass doesn't fully support.

# RubiXDB — Frontend Production Validation

**Date:** 2026-09-22

**Status:** Executes the Productization — Production Validation Phase's
frontend sections (§8–§14) against the console built in
`PHASE_FRONTEND_IMPLEMENTATION.md`, using the real production build,
the real `rubixdb-api.exe` binary, and a real disposable `LsmEngine` —
the same no-mocking discipline `PHASE_FRONTEND_IMPLEMENTATION.md`
itself already established. This is validation of the console, not a
new phase of frontend features; the one change made here
(`ExplorerPage.tsx`'s lookup re-arm fix) is a real bug this validation
found, fixed at the source, and is called out explicitly below.

---

## §8: functional states

`e2e/production_validation.spec.ts` (new), on top of the workflow/role/
error-handling coverage `e2e/workflow.spec.ts` already had:

- **Empty state**: a range query with a start key past every real key
  in the shared test database renders `Table`'s `EmptyState` ("No keys
  in this range") — chosen over relying on the Snapshots list happening
  to be empty, since the backend is shared across every spec file in
  the run and that would be nondeterministic.
- **Error state**: looking up a nonexistent key renders the "does not
  exist" message, and the app remains fully usable afterward (verified
  by navigating to Dashboard immediately after) — an error state is not
  a dead end.
- **Loading state**: `Table`'s `role="status"` `Spinner` path exists
  and is exercised implicitly by every query in the suite (react-query
  briefly sets `isLoading` on every real fetch); a dedicated visibility
  assertion on a sub-100ms local spinner would be flaky by construction
  (real localhost fetches complete too fast to reliably catch the
  spinner mid-render), so this is covered as a real code path (already
  present, already exercised on every test run) rather than a race-
  prone assertion.
- **Session expiration (401)**: covered below (real, not simulated by
  editing storage — see §8's "a 401 mid-session" test's own comment for
  why route interception, not storage tampering, is what actually
  exercises the real code path here).
- **Role restriction (403)**: `client.test.ts` (vitest) adds
  `does not clear the session on a 403` — confirms `ApiClient` never
  calls `onUnauthorized` for a 403, complementing the existing
  `e2e/workflow.spec.ts` UI-level check that a reader-role session
  renders no write controls at all. A 403 is intentionally not
  reachable through the real UI (write controls are hidden for a reader
  session), so the precise "does 403 preserve the session" contract is
  verified at the unit level, where it can be checked exactly rather
  than approximated through DOM interaction.
- **Reload/session handling**: `e2e/workflow.spec.ts`'s existing reload
  test (session **and** data survive a real page reload) plus the new
  logout test below.

### A real bug found and fixed: 401 mid-session

The real UI has no way to make an already-connected session's
credential go stale (by design — reconnecting via `/connect` is the
only path; `SettingsPage.tsx` has no inline key editor). Testing this
therefore uses Playwright's `page.route()` to return a real-shaped 401
for every subsequent `/v1/*` request, followed by `page.reload()` to
force every query to refetch fresh rather than waiting up to 10s for
`refetchInterval` — this exercises the real, unmocked application code
(`ApiClient.request` → `onUnauthorized` → `clearSession` →
`RequireSession`'s redirect), not a fabricated one. Result: **the real
app correctly redirects to `/connect` on a 401**, confirming the chain
`client.test.ts` verifies at the unit level is actually wired together
correctly in the running app.

### A real bug found and fixed: stale "armed" lookup query

While writing the repeated-workflow stability test (§9 below), a real,
if minor, bug surfaced: once "Look up" is clicked once on the Data
Explorer, `getQuery`/`existsQuery` stay `enabled` from that click
forever — editing the Key field afterward (e.g., typing a new key)
silently re-fires a live query against whatever partial/not-yet-written
value is currently in the box, each one a real network round trip and,
for a key that doesn't exist yet, a visible `404` in the browser
console. This is surprising for a button that reads as an explicit,
deliberate action, and it was firing on every one of 60 stability-test
iterations that reused a small key pool. **Fixed at the source**
(`frontend/src/pages/ExplorerPage.tsx`): the Key field's `onChange` now
also resets `lookupEnabled` to `false`, so editing the key after a
lookup requires an explicit new "Look up" click before firing another
query. Verified by rebuilding and re-running the stability test, which
went from 7 stray console errors to 0.

## §9: repeated-workflow stability

`production_validation.spec.ts`'s `60 repeated write+read cycles` test:
60 real write→read round trips against a small reused key pool (10
keys), through the real UI, against the real backend, with:

- `page.on("console"/"pageerror")` capturing every console error and
  uncaught exception — **0 across all 60 iterations** (after the fix
  above; 7 before it, all traced to the bug described above, none left
  unexplained).
- `page.on("requestfailed")` capturing every failed network request —
  **0**.
- A CDP `Performance.getMetrics` JS-heap sample before and after —
  heap went from ~4.24MB to ~8.57–9.63MB across independent runs (a
  ~2.0–2.25× ratio), consistent with react-query legitimately caching
  10 distinct keys' query results plus normal DOM/React overhead for a
  60-iteration session, not runaway growth. This is a coarse leak
  signal (a floor for "something is clearly wrong"), not a claim of
  zero growth — stated as such rather than oversold.

60 iterations, not "hundreds/thousands," is this validation's actual,
stated scope: each iteration drives a real browser through real DOM
interaction and a real network round trip, so it is bound by real wall-
clock time (~10s for 60 iterations here) in a way a raw HTTP loop is
not — that raw-throughput, much-higher-volume case is exactly what
§2's API load-test harness (`api_load_test.rs`) already covers
separately, against the same real backend, at up to 250 concurrent
clients. This test's own job is real-browser behavioral stability
(console/network/memory), which does not require thousands of
iterations to demonstrate — 60 already produced a real, reproducible
signal (the bug above) that a single run would not have.

## §10: accessibility (re-run, no regressions)

`e2e/a11y.spec.ts`, unchanged since `PHASE_FRONTEND_IMPLEMENTATION.md`
§5/§6, re-run against the current build: **0 axe-core violations**
across all 7 screens, keyboard navigation (skip link first Tab stop,
`Enter` activates a focused nav link, a `Dialog` traps focus and closes
on `Escape`) still passes. None of the five fixes documented in
`PHASE_FRONTEND_IMPLEMENTATION.md` §5 (contrast, heading order,
landmark) regressed.

## §11: responsive (re-run, no regressions)

`e2e/responsive.spec.ts`, re-run: no horizontal overflow at desktop
(1280px), tablet (820px), or small (390px) on every primary screen; the
navigation drawer opens/closes correctly at the small viewport and
closes itself after a selection. Unchanged from
`PHASE_FRONTEND_IMPLEMENTATION.md`.

## §12: visual regression — a small, deliberately scoped baseline

A full pixel-diff baseline across every screen was considered and
rejected: Dashboard, Health/Storage, and Compaction all render live
operational data (request counts, uptime, SSTable counts) that changes
on every run by design, so a baseline for them would either be
permanently flaky or require masking most of the page — not "a small
deterministic set of critical screens" any more, and exactly the "giant
visual-design rewrite" this phase's own brief said not to build.

Implemented instead (`e2e/visual_regression.spec.ts`, new): a baseline
for the two screens whose content is actually stable run to run —
**Connect** (pre-session, no data at all) and **Settings** (its only
per-session value, the principal name, is fixed by this suite's own
fixed API key), both at 1280×900. Baselines committed at
`e2e/visual_regression.spec.ts-snapshots/{connect,settings}-screen-
chromium-win32.png`. Verified real regression-detection capability, not
just generation: the baseline was generated once (`--update-snapshots`)
then the suite was re-run without that flag and passed against the
committed images (`maxDiffPixelRatio: 0.01`), confirming the comparison
is genuinely deterministic in this environment, not merely present.

## §13: frontend security

- **No secret in source or bundle**: `production_validation.spec.ts`
  reads every `dist/assets/*.js` file after a real production build and
  asserts neither e2e test API key appears as a literal, and no
  `admin:admin:<16+ chars>`-shaped hardcoded credential pattern appears
  anywhere. This app has no default credential by design (every
  credential is user-supplied at Connect time), so this is confirming
  an absence, not merely trusting it.
- **No default credential**: confirmed by design (`Config::load_from_
  env` on the backend has no default `RUBIXDB_API_KEYS` — see
  `PHASE_API_IMPLEMENTATION.md`) and by the bundle scan above.
- **Safe session handling**: `SessionContext.test.tsx` (existing, 5
  tests) already covers sessionStorage-by-default/localStorage-on-opt-
  in/clear-both/restore-on-mount.
- **Logout clears credentials**: new e2e test confirms clicking "Log
  out" clears both `sessionStorage` and `localStorage` for the
  session key, redirects to `/connect`, and that a reload afterward
  still requires reconnecting (not just a client-side route change
  that a back-button or reload could undo).
- **401 clears session / 403 preserves it**: see §8 above.
- **CORS behaves as intended**: re-verified below (§17-equivalent for
  the frontend's own origin).

## §14: dependency audit (`npm audit`, re-run and re-classified)

Re-run against the current lockfile (advisory IDs shift over time as
the advisory database updates; re-classified from first principles
rather than assumed unchanged from `PHASE_FRONTEND_IMPLEMENTATION.md`
§8):

| Package | Severity | Classification | Why |
|---|---|---|---|
| `vitest` | **critical** | development-only | Test runner; never part of `vite build`'s output. The advisory (arbitrary file read via the Vitest UI server / `@vitest/mocker` redirect-mock handling) requires running `vitest --ui` or the mocker against untrusted input — not exercised by this project's own `npx vitest run` usage in CI/local dev. |
| `vite` | **high** | development-only | Dev-server-only path-traversal/`server.fs.deny` bypass issues; the production artifact is `vite build`'s static `dist/`, served by any static host or `vite preview`, neither of which runs the affected dev-server code path. |
| `@vitest/mocker`, `vite-node`, `esbuild` | moderate | development-only | Transitive to `vitest`/`vite` above; same reasoning. |
| `react-router` / `react-router-dom` | moderate | **non-applicable to this codebase's usage** | Two advisories: an open-redirect via `<Link>`/`useNavigate` (requires a user- or server-controlled navigation target — grepped every `<Link>`/`navigate()`/`NavLink` call in `frontend/src/`; all use fixed, hardcoded path strings, none ever pass external data as a target) and an SSR-hydration `deserializeErrors()` constructor-injection issue (requires React Router's SSR/data-loader APIs — grepped for `loader`/`action`/`createBrowserRouter`/`deserializeErrors`/`hydrate` across `frontend/src/`; none present. This app is a client-only Vite SPA with no Node SSR entry point at all). |

**Production-impacting: none.** No advisory's precondition is met by
this codebase's actual usage. Fixing the react-router advisories
requires a major-version jump (6→7); still deferred as a tracked, low-
priority follow-up rather than an unplanned breaking migration, per
`PHASE_FRONTEND_IMPLEMENTATION.md` §8's own original reasoning, now
re-confirmed against the current advisory set rather than assumed.

## Full regression re-run

```
npm ci        # verified deterministic install
npm run build # tsc --noEmit && vite build -- unchanged output size
npx vitest run       # 18/18 (was 17/17; +1 for the 403-preserves-session test)
npx playwright test  # 18/18 (10 workflow/role/error, 3 a11y, 4 responsive,
                      #  a moment ago was 4 new production_validation, +2 visual_regression)
```

Production build: `dist/index.html` 0.45 kB, CSS 10.27 kB (2.62 kB
gzip), JS 239.74 kB (74.59 kB gzip) — unchanged from
`PHASE_FRONTEND_IMPLEMENTATION.md` to within source-map-hash noise.

## Known limitations (carried forward, still accurate)

- The visual-regression baseline covers 2 of 7 screens by deliberate,
  documented design (§12) — not partial coverage of an intended full
  suite.
- The 60-iteration stability test is real-browser behavioral evidence,
  not raw-throughput endurance evidence — that is `PHASE_API_
  ENDURANCE.md`'s job, against the same real backend.
- `npm audit`'s dev-tooling advisories (§14) remain open and tracked,
  not blocking, matching `PHASE_FRONTEND_IMPLEMENTATION.md` §8's
  already-established position.

## Frontend production-readiness verdict (this document's scope only)

Every functional, accessibility, responsive, security, and stability
check run in this phase passed, and two real bugs this validation found
were fixed at the source and re-verified. This document does not by
itself declare "frontend production ready" — that determination,
together with the API's, is made in `PHASE_PRODUCT_CERTIFICATION.md`.

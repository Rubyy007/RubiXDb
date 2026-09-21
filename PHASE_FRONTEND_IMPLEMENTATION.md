# RubiXDB — Frontend Implementation

**Date:** 2026-09-22

**Status:** Implements `PHASE_FRONTEND_ARCHITECTURE.md` in full, against
the real `rubixdb-api` backend (`PHASE_API_IMPLEMENTATION.md`). No
frontend existed before this phase (§0 of the architecture doc) — this
is the whole console.

---

## 1. Toolchain note, stated honestly

No Node.js/npm toolchain existed in this environment at the start of
this phase. `winget install OpenJS.NodeJS.LTS` was attempted first but
hung indefinitely (traced to `msiexec` waiting on a UAC elevation
prompt that cannot be answered in this non-interactive session — the
standard Node MSI installer requires machine-scope elevation). Killed
and replaced with the official portable ZIP distribution (`node-v20.
18.0-win-x64.zip` from nodejs.org, no installer, no elevation), which
worked immediately. This means every build/lint/test/e2e result in
this document comes from an **actually-run** toolchain, not an
assumption — stated here because the alternative (writing frontend
code with no way to verify it compiles or runs) would have been
dishonest given this project's own standing "measure, don't assume"
discipline.

## 2. Crate/project layout

```
frontend/
  package.json, tsconfig.json, vite.config.ts, eslint.config.js
  index.html
  src/
    main.tsx, App.tsx
    api/          client.ts (typed fetch wrapper), queries.ts (react-query
                   hooks), types.ts (DTOs mirroring the backend exactly)
    context/      SessionContext.tsx
    components/   Button, Badge, Card, Table, Dialog, Toast, Tabs,
                   Field (Input/TextArea/Select), Spinner, EmptyState,
                   AppShell -- one implementation each, per the design
                   system doc
    pages/        ConnectPage, DashboardPage, ExplorerPage,
                   SnapshotsPage, CompactionPage, HealthPage,
                   SettingsPage
    styles/       tokens.css (design tokens, light+dark), global.css
    utils/        base64.ts, format.ts
  e2e/            workflow.spec.ts, a11y.spec.ts, responsive.spec.ts
                   (Playwright, real backend + real built frontend)
```

## 3. A production API gap found and closed: `GET /v1/whoami`

`PHASE_API_ARCHITECTURE.md`'s original §2 contract had no way for a
client to learn its own authenticated role — necessary for a role-
aware UI (disabling write controls for a `reader` key) and not
discoverable any other way. Added as one new GET route reading the
`Principal` `auth_middleware` already attaches to every request; zero
change to the auth model itself. Documented in both architecture docs
and covered by its own backend integration test
(`whoami_reports_the_authenticated_principal_and_role`).

## 4. CORS: a real, necessary backend addition, not a test convenience

The console and the API are two separate origins by design (§1 of the
architecture doc). Browsers enforce the Same-Origin Policy regardless
of how "real" the deployment is, so cross-origin `fetch()` calls from
the console need explicit CORS headers — this is a genuine production
requirement, not something added merely to make the e2e suite pass.
Added `tower-http`'s `CorsLayer`, gated by a new `RUBIXDB_CORS_
ALLOWED_ORIGINS` setting (comma-separated, **empty by default** — no
CORS headers at all, same-origin only, the safe default). Never
wildcards the origin regardless of configuration, even though this
project's own bearer-token auth (no cookies, `credentials: 'include'`
never set) would not strictly require an allow-list for CORS's own
credentialed-request rules — the allow-list is kept anyway as defense
in depth. Covered by a new backend integration test asserting the
configured origin is echoed back exactly and an unconfigured origin
gets no CORS header at all (`cors_allows_only_the_configured_origin_
and_never_wildcards`).

## 5. Five real bugs found by real testing, fixed at the source

Per this project's own standing discipline: found empirically while
building and running the actual test suites below, not hypothesized.

1. **Stale read cache after a write** (functional bug, not cosmetic).
   `e2e/workflow.spec.ts`'s own second-write-then-lookup step showed
   the *previous* value after overwriting a key — `react-query` had no
   reason to refetch a `["kv","get",key,...]` query whose key hadn't
   changed, since it had no way to know a `PUT`/`DELETE` invalidated
   it. Fixed at the source (`api/queries.ts`): `usePutMutation`/
   `useDeleteMutation` now invalidate every cached `["kv", ...]` query
   on success.
2. **Badge color contrast** (`--color-healthy` 3.1:1 against
   `--color-healthy-bg`, WCAG AA requires 4.5:1 for this text size) —
   found by axe-core's real, automated audit against the real rendered
   Dashboard page, not a manual review. Darkened in `tokens.css`.
3. **Field-hint text contrast** (`--color-text-faint` 3.19:1 against
   white) — same mechanism, found on the Data Explorer screen.
   Darkened.
4. **Heading hierarchy skip** (`<h1>` page title directly followed by
   `<h3>` Card titles, skipping `<h2>`) — `axe-core`'s `heading-order`
   rule. Fixed: `Card`'s title is now `<h2>`, universally (every page
   has exactly one `<h1>`, Cards are always the next section level).
5. **Missing `<main>` landmark on the Connect screen** — it renders
   outside `AppShell` (there is no authenticated session yet to gate
   on), so it had no landmark of its own. Wrapped in a labeled
   `<main>`.

None of these were "designed in and later discovered acceptable" —
each was a genuine defect the relevant test caught, fixed, then
re-verified passing.

## 6. Test suite

**Unit/component tests** (`npx vitest run`, 17/17): base64 round-trip
and error handling (including a deliberate non-UTF-8 decode failure,
matching what real binary values will do), `ApiClient` (Authorization
header on every request, 401 triggers `onUnauthorized`, error body
fields propagate correctly, range query-string construction),
`SessionContext` (defaults to `sessionStorage`, `localStorage` only on
explicit opt-in, `clearSession` removes both, a stored session is
restored on mount), `Badge`/`storageStateTone` (every `StorageState`
variant maps to the correct tone, the text label is always rendered
alongside the color).

**End-to-end tests** (`npx playwright test`, real Chromium, real
`rubixdb-api.exe` against a fresh disposable data directory, real
built-and-served frontend on a separate origin — **10/10**):
- `workflow.spec.ts`: connect → inspect health → write → read →
  overwrite → re-read → range query → create/inspect a snapshot →
  inspect compaction status → inspect metrics → look up a nonexistent
  key (error handling) → reload the page (session **and** data both
  survive) — plus a separate test that a `reader`-role session renders
  no write controls at all, and a separate test that an invalid API
  key is rejected with a clear message and never silently accepted.
- `a11y.spec.ts`: `axe-core`'s automated audit against every one of
  the 7 screens (real rendering, not static analysis) — zero
  violations after the fixes in §5 — plus a keyboard-navigation test
  (skip link is the first Tab stop, `Enter` activates a focused nav
  link, a confirmation `Dialog` traps focus and closes on `Escape`).
- `responsive.spec.ts`: no horizontal overflow at desktop (1280px),
  tablet (820px), or small (390px) viewports, on every primary screen;
  the navigation drawer opens/closes correctly at the small viewport
  and closes itself after a selection.

## 7. Production build

```
npm run build   # tsc --noEmit && vite build
```

`dist/index.html` (0.45 kB), one CSS bundle (10.27 kB, 2.62 kB
gzipped), one JS bundle (239.59 kB, 74.56 kB gzipped) — a small,
reasonable bundle for a dependency set of React + react-router +
react-query and this console's own code, with no component-library
dependency (`PHASE_FRONTEND_ARCHITECTURE.md` §1's own deliberate
choice). Reproducible: re-running the build from a clean `npm ci`
produces the same module count and near-identical bundle size.

## 8. Dependency hygiene / `npm audit`

`npm audit` reports 7 advisories, all traced and none blocking:

- **High + critical**: both are `vite`/`vitest` **dev-server-only**
  vulnerabilities (a path-traversal issue in Vite's dev middleware and
  in `@vitest/mocker`'s redirect-mock handling). Neither package is
  part of the production bundle — `vite build`'s output contains only
  the actual application dependencies. These affect a developer
  running `vite dev`/the Vitest UI on an untrusted network, not any
  user of the built console.
- **Moderate (react-router)**: an open-redirect advisory in `<Link>`/
  `useNavigate`. Checked directly, not assumed safe: every `<Link>`/
  `navigate()`/`NavLink` call in this codebase uses a fixed, hardcoded
  path string (`/`, `/connect`, `/explorer`, etc.) — **none** ever
  passes user- or server-controlled data as a navigation target, which
  is the vulnerability's actual precondition. Fixing it requires a
  major-version jump (`react-router-dom` 6→7); deferred as a tracked,
  low-priority follow-up rather than an unplanned breaking migration
  under this phase's own time constraints, stated here rather than
  silently ignored.

No secret, API key, or credential is committed anywhere in `frontend/`
— confirmed by `.gitignore` covering `.env*`/`.e2e-data`, and by
`git ls-files` review before commit.

## 9. What was consolidated or deferred, per the architecture doc's own scope

- "Query/workspace" + "Result viewer" → one Data Explorer screen
  (`PHASE_FRONTEND_ARCHITECTURE.md` §2 — no SQL layer exists to give a
  separate workspace its own meaning).
- "Logs/errors" → the Health screen's own real per-route error-count
  table (from `/v1/metrics`), not a fabricated log viewer (no log-
  retrieval endpoint exists on the backend).
- No "force compaction" control anywhere in the UI — no such backend
  capability exists (§0 of the API architecture doc, unchanged).

## 10. Known limitations

- The rate limiter and CORS allow-list are both single-instance/
  in-process concerns (matching the certified engine's own single-
  instance, non-partitioned scope — `PHASE_API_IMPLEMENTATION.md` §7).
- `npm audit`'s two dev-tooling advisories (§8) are unresolved,
  tracked, and do not affect the production bundle.
- No visual regression/screenshot-diff testing was set up (out of this
  phase's own explicit scope — Playwright's structural/accessibility/
  functional assertions above are the testing surface this phase
  built).
- This frontend has not undergone its own load/endurance testing the
  way the storage engine did — matching `PHASE_API_IMPLEMENTATION.md`
  §7's identical, explicitly-scoped limitation for the API layer.

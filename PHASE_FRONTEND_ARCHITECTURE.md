# RubiXDB — Frontend Architecture

**Date:** 2026-09-22

**Status:** Design for the database-console frontend consuming
`PHASE_API_ARCHITECTURE.md`'s API contract exactly as built
(`PHASE_API_IMPLEMENTATION.md`) — the API contract is the source of
truth this frontend is built against, not the other way around.

---

## 0. Audit: no existing frontend/web stack

Confirmed at the start of the API phase (`PHASE_API_ARCHITECTURE.md`
§0) and re-confirmed here: no `web/`, `frontend/`, `ui/`,
`package.json`, or Node tooling exists anywhere in this repository.
This is a greenfield addition — "the project's established web stack"
does not exist yet, so this document establishes it.

## 1. Stack decision

- **React 18 + TypeScript, built with Vite.** The standard, minimal-
  friction choice for a data-dense single-page admin console; Vite
  gives a fast, reproducible production build (`vite build` → static
  `dist/`, no server-side rendering needed for an authenticated
  internal tool) and a fast dev server for iteration.
- **`react-router-dom`** for client-side routing between the screens
  in §2 below — a single well-justified addition, not a competing
  framework.
- **`@tanstack/react-query`** for server-state (fetching, caching,
  loading/error states) against the REST API — avoids hand-rolling the
  same loading/error/refetch machinery in every screen; it is a data-
  fetching library, not a UI framework, and does not compete with
  anything else in this stack.
- **No component-library dependency** (no MUI/Ant/Chakra). A small,
  purpose-built design system (§ `PHASE_FRONTEND_ARCHITECTURE.md`
  itself, plain CSS with design tokens) keeps the dependency surface
  minimal and the visual language exactly fitted to a dense admin
  console, per this phase's own "do not introduce unnecessary
  frameworks" instruction.
- **No global state library** (no Redux/Zustand) — the only genuinely
  global client state is "which API key/role is connected," held in
  one small React context (§5); everything else is server state
  (react-query) or local component state.
- **Testing**: `vitest` + `@testing-library/react` for component
  tests, `Playwright` for the real end-to-end workflow (Phase 18) —
  driving a real browser against the real built frontend and the real
  `rubixdb-api` backend (itself against a real, disposable `LsmEngine`
  data directory), never a mocked backend for the core workflow.
- **Linting/type-checking**: `eslint` (typescript-eslint, react-hooks
  plugin) + `tsc --noEmit`, both run as part of the test gate (Phase
  22).

## 2. Screens (primary areas)

Mapped directly to what the API contract actually exposes — no screen
here implies a backend capability that doesn't exist.

| Screen | Route | Backend calls | Purpose |
|---|---|---|---|
| Connect | `/connect` | `GET /readyz` (to validate a key before storing it) | Enter an API key, select/confirm the endpoint, establish a session |
| Dashboard | `/` | `/v1/status`, `/v1/compaction/status`, `/v1/metrics` | At-a-glance engine health, storage state, SSTable count, compaction cadence, request volume |
| Data Explorer | `/explorer` | `/v1/kv/*`, `/v1/range`, `/v1/snapshots` | Key lookup (now/historical), `contains`, range query with bound/limit/snapshot selection, result table |
| Snapshots | `/snapshots` | `/v1/snapshots` (all methods) | Create, list, inspect, release held snapshots |
| Compaction | `/compaction` | `/v1/compaction/status`, `/v1/compaction/metrics` | §14 below |
| Health / Storage | `/health` | `/v1/status`, `/v1/metrics` | §15 below |
| Settings | `/settings` | none (local only) | Connection management, logout, theme |

**Consolidation, stated explicitly, not silently narrowed**: "Query/
workspace screen" and "Result viewer" (from the phase brief's own
listed areas) are **one** screen here (Data Explorer) — the API has no
query *language* to give its own workspace (§0/§2 of the API
architecture doc: no SQL layer exists), so "query" here means
"compose a key lookup or range request," and its result table is part
of the same screen, not a separate navigational area. "Logs/errors" is
**not** a separate screen — no log-streaming or log-retrieval endpoint
exists on the backend (out of scope for the API phase); the Health
screen's own per-route error-count table (from `/v1/metrics`'s real
`service.routes[]` data) is the honest substitute, not a fabricated
log viewer.

## 3. Application shell

- **Top bar**: product name, connection indicator (endpoint + role,
  from the session context), a compact storage-state badge (polled
  from `/v1/status` every 10s while any screen is mounted), logout.
- **Left navigation**: the 6 authenticated screens from §2, plus a
  visible role indicator — write-only actions (Data Explorer's put/
  delete, Snapshots' create/release) are disabled with an explicit
  tooltip for a `reader`-role session rather than hidden, so a reader
  understands what exists without being told nothing is there.
- **Content area**: each screen's own layout, described per-screen
  below.
- **Global toast/notification layer**: for the result of a mutating
  action (put succeeded, snapshot released, etc.) and for session-
  level events (401 → "session expired, please reconnect").

## 4. Design system

- **Typography**: one sans-serif system-font stack (`-apple-system,
  Segoe UI, Roboto, sans-serif` — no web-font download, keeps the
  bundle small and avoids a FOUC), one monospace stack (`ui-monospace,
  SFMono-Regular, Consolas, monospace`) reserved for keys/values/hex/
  base64 data specifically — a database console's own data is exactly
  the content class monospace alignment matters for.
- **Spacing**: an 4px-based scale (4/8/12/16/24/32/48px) as CSS custom
  properties (`--space-1` … `--space-6`), used consistently so every
  screen's density reads the same.
- **Color**: CSS custom properties for a light and dark theme (`prefers-
  color-scheme`-driven by default, with an explicit override in
  Settings), a neutral gray scale for structure, and exactly three
  semantic status colors (healthy/green, pressure/amber, full-or-error/
  red) reused everywhere a state needs one (storage-state badge,
  compaction status, error banners) — never a fourth ad hoc color
  introduced screen-by-screen.
- **Core components** (one implementation each, reused everywhere):
  `Button` (primary/secondary/danger/ghost variants, disabled+tooltip
  state for role-gated actions), `Input`/`TextArea` (with a monospace
  variant for key/value entry), `Select`, `Table` (dense, sortable-
  column-ready, empty/loading/error row states built in — every data
  table in this app is one component, not five ad hoc ones), `Card`,
  `Badge` (status pill, using the three semantic colors), `Tabs`,
  `Dialog` (confirmation for destructive actions — delete key, release
  snapshot), `Toast`, `Spinner`/skeleton loading rows, `EmptyState`
  (icon + message + optional action, used whenever a list/table has
  zero rows — never a bare blank area).
- **Accessibility baked into the component layer, not bolted on
  later**: every interactive component carries its own correct ARIA
  role/label by default (a `Button` icon-only variant requires an
  `aria-label` prop at the type level, not optionally); `Dialog` traps
  focus and returns it to the trigger on close; `Table` rows are
  keyboard-navigable; color is never the only signal (the status
  `Badge` always carries a text label alongside its color).

## 5. Security boundary (frontend side of `PHASE_API_ARCHITECTURE.md` §4)

- **Session**: an API key entered on the Connect screen, held in
  React context for the app's lifetime and persisted to
  `sessionStorage` (cleared when the tab closes) by default; an
  explicit "remember this connection" checkbox opts into
  `localStorage` instead — the tradeoff (convenience vs. a credential
  surviving browser close) is the user's own explicit choice, never
  the silent default.
- **No secret ever committed to source** — there is no default/
  example key baked into the frontend; the Connect screen's own
  placeholder text says so explicitly.
- **401 handling**: any API response with status 401 clears the stored
  session and redirects to `/connect` with a "session expired"
  message — centralized in the `react-query` client's own response
  interceptor, not duplicated per screen.
- **403 handling**: shown inline, at the point of the attempted action
  (e.g. a toast: "your reader-role key cannot write") — does **not**
  log the user out, since the session itself is still valid.
- **CSRF**: not applicable, matching the backend's own reasoning
  exactly (`PHASE_API_ARCHITECTURE.md` §4) — this frontend sends the
  API key as an `Authorization: Bearer` header on every request, never
  a cookie, so there is no ambient credential a third-party page could
  ride on.
- **Logout**: clears the stored key from both `sessionStorage` and
  `localStorage` (whichever was used) and returns to `/connect`.

## 6. Responsive / accessibility validation plan

Desktop (≥1280px, the primary target — a dense admin console is
usually used on a real monitor), tablet (~768-1024px: navigation
collapses to an icon rail, tables gain horizontal scroll within their
own `Card` rather than breaking page layout), and a smaller viewport
(~375-414px: navigation becomes a drawer, tables switch to a stacked
card-per-row presentation for the Data Explorer's own result set
specifically, since a dense multi-column table is the one thing that
cannot reasonably compress further). Verified in Phase 17 via
Playwright's own configurable viewport sizes (real rendering, not a
simulated CSS-only check) plus a manual pass. Keyboard navigation,
focus handling, and contrast are verified the same way — Playwright
driving real Tab/Enter/Escape key sequences through the Dialog/Table/
navigation components, plus `axe-core`'s automated accessibility
audit run against each screen as part of the e2e suite.

## 7. What this document does not decide

Deferred to `PHASE_FRONTEND_IMPLEMENTATION.md`: exact component prop
signatures, file layout inside `frontend/src/`, and the specific
Playwright test scenarios (enumerated there against the actual built
screens).

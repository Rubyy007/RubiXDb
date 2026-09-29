# Phase: Relational Database — Frontend SQL Console Architecture (Increment 12)

## 1. Integration, not replacement

The SQL console is one new page (`frontend/src/pages/SqlConsolePage.tsx`)
added to the existing React console, routed at `/sql` and linked from
`AppShell`'s nav — every existing screen (Dashboard, Data Explorer,
Snapshots, Compaction, Health/Storage, Settings) is unchanged (item 116),
verified by re-running the full existing Playwright suite (`workflow.
spec.ts`, `a11y.spec.ts`, `responsive.spec.ts`, `production_validation.
spec.ts`, `visual_regression.spec.ts`) unmodified alongside the new
tests — all 21 tests (18 pre-existing + 3 new) pass together.

## 2. The one SQL execution path (item 42/105)

`ApiClient.sql()` (`frontend/src/api/client.ts`) is the **only** place
this frontend ever sends SQL — a thin `POST /v1/sql` wrapper, nothing
more. There is no second parser/binder/planner/executor anywhere in
this codebase; syntax highlighting/autocomplete were **not** added
(scope discipline, item 43's own "may exist purely as editor
assistance" — this v1 has none, a plain `<textarea>`, since none of the
governing directive's mandatory items require it). The server's own
JSON response is rendered as-is; nothing about a statement's meaning is
ever decided client-side.

Session/transaction state (`sessionId`, whether a transaction is open)
lives in ordinary React component state inside `SqlConsolePage`, not in
`localStorage`/`sessionStorage` and not in a global store — a page
reload starts a fresh, autocommit-only session, exactly like a fresh
CLI process would. This is the honest contract, stated explicitly
rather than left as an implicit limitation: nothing about *durable*
session persistence across a reload is claimed anywhere in this
console.

## 3. Typed values, end to end (item 96)

`frontend/src/api/types.ts::SqlValue` mirrors the server's own tagged
JSON shape exactly (`api/src/sql_params.rs::SqlValueJson`) — the same
type is used for both request parameters (not yet exposed in this v1's
UI, §6) and response cells. `frontend/src/utils/sqlValue.ts::
formatSqlValue` is the **only** place a typed value becomes display
text, mirroring the CLI's own `render.rs` rule-for-rule (one shared
contract across both clients): `BIGINT`/`DECIMAL`'s unscaled part are
already wire-safe strings (never round-tripped through a lossy
JavaScript `Number`), `NULL` renders as the literal text `"NULL"`
(visually distinguished further with italic styling, never confused
with an empty string), `BLOB` renders as a byte-length summary, never
attempted binary-to-text decoding.

## 4. Result rendering and bounded DOM (item 44)

`ResultView`/`RowsView` render `SELECT`'s own typed columns/rows
directly, with **client-side pagination** (200 rows/page) rather than
rendering every row's DOM node at once for a large result — `ExecLimits::
max_result_rows` defaults to 100,000, and rendering that many `<tr>`
elements unpaginated would be exactly the "millions of DOM nodes" item
44 warns against. This is a deliberate, documented v1 scope choice:
true virtualization (windowed rendering, only the visible rows ever in
the DOM) was considered and **not** implemented, to avoid adding a new
dependency (`react-window`/similar) not otherwise justified for this
console's real usage pattern (a human operator paging through results,
not a live-scrolling firehose) — pagination is sufficient to keep DOM
size bounded regardless of result size, and is the smaller, dependency-
free change (item 90's "prefer existing/minimal" applied here).

## 5. Cancellation (item 43/48)

The Cancel button calls `AbortController.abort()` on the in-flight
`fetch` — a real HTTP-level abort, not a UI-only "hide the spinner
while the server keeps working." The server observes the dropped TCP
connection (axum drops the handler future) and reacts via its own
`CancellationToken`/`CancelOnDrop` guard (`PHASE_RELATIONAL_SQL_API_
ARCHITECTURE.md` §5) — the same mechanism that already handles any
other client disconnect, never a frontend-specific cancellation path.

## 6. Security (item 93/94/126)

Every result cell is rendered as a React **text node** — this codebase
never calls `dangerouslySetInnerHTML` anywhere, and the SQL console adds
no exception. React's own escaping means adversarial row content
(`<script>`, `<img onerror=...>`, arbitrary HTML) is displayed as
literal, inert text, never executed or interpreted as markup — verified
directly: `SqlConsolePage.test.tsx::renders adversarial row content as
inert text...` asserts both that the payload text is visible verbatim
*and* that no `<img>` element was actually created anywhere in the
result table's DOM, and the real Playwright E2E suite exercises the
same result-rendering code path against the real server for every
other assertion in `sql_console.spec.ts`. The API key is never placed
in a URL/query parameter, and this page introduces no new place credentials
could leak to the browser console (the same `SessionContext`
storage/lifecycle every other page already uses, unchanged).

## 7. A real, honest scope trim, and the bugs found while proving it wasn't a bug

While writing the real Playwright E2E test for this console
(`e2e/sql_console.spec.ts`), two apparent failures turned out to be test
bugs, not application bugs — both diagnosed by inspecting the real
rendered page snapshot Playwright captured on failure, not assumed:

1. `SqlConsolePage`'s own result heading correctly renders singular
   `"Result (1 row)"` (no trailing `s`) when exactly one row is
   returned — the test's regex had assumed `"1 rows"` (always plural)
   and was fixed to match the correct, existing singular/plural logic.
2. A `GROUP BY` assertion expected 2 groups after an earlier `DELETE`
   in the same test sequence, but that `DELETE` had removed the table's
   only `grp='b'` row, correctly leaving exactly one group (`grp='a'`,
   `COUNT=2`, `SUM=119`) — confirmed correct by hand-computing the
   expected aggregate from the test's own prior `INSERT`/`UPDATE`
   statements, then fixing the test's expectation, not the application.

Both are recorded here because they are exactly the outcome real E2E
testing is supposed to produce: a false alarm caught and traced to its
actual cause before being either dismissed or acted on incorrectly,
never silently "fixed" by loosening an assertion without understanding
why it failed first (item 113's own standard, applied to a UI-layer
finding rather than a backend one this time).

## 8. Known limitations

- No typed-parameter (`$n`) input UI — literal values are typed
  directly into the SQL text (§`PHASE_RELATIONAL_CLI_ARCHITECTURE.md`
  §9's identical limitation; the wire contract already supports it,
  `SqlRequestBody.params`, for a future increment to expose).
- No true virtualization for very large result sets — bounded via
  pagination instead (§4), a documented, deliberate choice, not an
  oversight.
- Query history is in-memory only (component state, capped at 50
  entries), not persisted across a reload — consistent with §2's own
  "session state is not durable across a reload" contract.

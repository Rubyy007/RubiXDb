# Frontend: SQL IDE, multi-statement cells, read-only Catalog (2026-10-11)

Status: IMPLEMENTED and unit/component tested. NOT verified against the real binary in a browser (see "Not tested").
This document is additive; it supersedes nothing. Nothing under the protected engine paths was changed.

## What changed (all under `frontend/`)

| Area | Change |
|---|---|
| SQL Console (`/sql`) | Now the SQL IDE (`src/ide/`): worksheets with tabs, SQL / markdown / transaction cells, concurrent runs, command palette (Ctrl+K, Ctrl+Shift+P, Ctrl+P), right panel (Sessions, Running, History, Schema, Plan). The previous single-statement console remains at `/sql-classic` and is not in the navigation. |
| Multi-statement cells | `POST /v1/sql` accepts exactly one statement per request. A cell is split at top-level `;` (`src/ide/sqlSplit.ts`, ignoring `;` inside `'strings'`, `"identifiers"`, `-- comments` and `/* comments */`); each statement is one request, run in order, stopping at the first failure. A script such as `BEGIN; ...; COMMIT;` carries the `session_id` from statement to statement. Comment-only text is not a statement and cannot be run. |
| History | One entry per executed statement, with the exact text that was sent, its position (`statement 2/3`), outcome code, browser-measured time and row count. Full text is shown (folded behind "Show full statement" when long). The server list (`GET /v1/observability/queries`) is shown separately because the server returns no SQL text. |
| Catalog (`/explorer`) | Opens on **Objects** (first tab), then Point Lookup, Range Query. Schema, table and index lists are read-only: no delete buttons. "Delete this key" on Point Lookup (a key, not a catalog object) is unchanged. |

New API client calls: `instance()`, `observedQueries()`, `observedSessions()`, `authz()`. `SqlEditor` gained optional props only (defaults unchanged).

## Not yet developed (also listed inside the app: command palette -> "Keyboard shortcuts and what is not yet developed")

- Reading as of a pinned snapshot: `POST /v1/sql` has no snapshot parameter.
- Switching role inside the IDE: the role is the connected API key's.
- Format SQL.
- SQL text in server-side History/Running: the API never returns it; previews exist only for statements run in this tab.
- Coloured badges for tier-like values.

## Verified

- `npx tsc --noEmit`, `npx eslint src` (0 errors; 3 warnings that were already present), `npx vitest run`: 147 tests pass (69 before this work).
- `npm run build` succeeds.

## End-to-end results (real `rubixdb gui` release binary, Chromium, real CSP, disposable data folders)

- `frontend/e2e-ide/ide.spec.ts` (new, `playwright.ide.config.ts`, console EMBEDDED in the binary, no on-disk override): 10 of 10 passed on the final build -- clean load with no console/CSP errors; multi-statement cell (semicolon inside a string and inside a comment not split; per-statement history with exact text); failing statement stops the script and the exact API error code is shown; transaction container (rollback discards, commit keeps, server lists the open session); `BEGIN; ...; COMMIT;` and `...; ROLLBACK;` in one cell; Run all with concurrent cells; EXPLAIN plan tab; all six Home templates run cleanly in the IDE; Catalog opens on Objects with no delete button; command palette, Ctrl+Enter, reload persistence, dark mode, 390px layout.
- Existing `e2e-gui` specs run (shell, home, token_handoff, delete_safety, operations): 16 passed, 3 FAILED, all three caused by the intended UI changes and NOT edited: `delete_safety.spec.ts` x2 (object Delete buttons removed) and `home.spec.ts` "every template runs" (looks for the old console's "Run" button; the same behaviour is covered by the new IDE spec).
- A real-browser screenshot at 1280px showed the cell area squeezed by the app sidebar plus both IDE side panels; the IDE's left rail now opens by default only at 1500px+ and closes when the window shrinks to phone width (the right panel opens at 1200px+).

## Not tested

- `e2e-gui` endurance, performance, 100k-row and session-cycle specs, `e2e-gui/cross_browser`, and the whole two-origin `e2e/` suite (it needs the separate `rubixdb-api` debug binary, which this work does not use).
- Firefox/WebKit rendering of the IDE; screen-reader behaviour; the Windows `Ctrl+T` shortcut (browsers reserve it).
- Power-loss and visual-regression suites.

## Open (recorded in `OPEN_ITEMS.md`)

- `frontend/e2e-gui/delete_safety.spec.ts` drives the object Delete buttons that no longer exist; the server-side delete-safety routes and their Rust tests are untouched. The spec needs a maintainer decision (retire the UI part or restore the buttons).
- `frontend/e2e/workflow.spec.ts` and other specs (two-origin suite, not run) that assume Point Lookup is the first Catalog tab, or the old `/sql` console markup (`e2e/sql_console.spec.ts`, `e2e/z_sql_console_redesign.spec.ts`), need to target `/sql-classic` or the new page. Not edited here.
- `frontend/e2e-gui/gui_session_cycles.spec.ts:19` has a pre-existing ESLint `no-undef` error for `process`.

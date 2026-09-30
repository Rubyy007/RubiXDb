# Increment 14, Blocker 12 — Delete Safety

Real implementation across backend, CLI, and frontend — not a
client-side-only confirmation. Increment 13 reported this
`NOT APPLICABLE` solely because no delete-object UI existed anywhere
in the product (`PHASE_RUBIXDB_GUI_ARCHITECTURE.md` §10). This
increment builds it for real.

## 1. SCHEMA / TABLE / INDEX — backend (`api/src/routes/catalog.rs`)

Three new authenticated routes, added this increment:

- `DELETE /v1/catalog/schemas/:schema_id` — body `{ confirm_name }`.
- `DELETE /v1/catalog/tables/by-id/:table_id` — body `{ schema_name,
  table_name }`.
- `DELETE /v1/catalog/indexes/:index_id` — body `{ schema_name,
  table_name, index_name }`.

Each handler re-reads the object's identity **fresh from the live
catalog** at request time and rejects on any mismatch — the client's
confirmation text is never trusted as-is. Each calls the *exact same*
catalog/index-builder primitive `DROP TABLE`/`DROP INDEX` already use
(`CatalogService::drop_table`, `IndexBuilder::drop_index_online`,
`CatalogService::drop_schema`) — no new SQL grammar, no direct
filesystem/index-file manipulation, same `Privilege::Ddl` /
`is_authorized` boundary every SQL DDL statement already uses.

**Real tests**: `api/tests/api_delete_safety.rs`, 12 tests, all real
end-to-end against a real `LsmEngine` + real axum router (no
mocking) — 12/12 pass:

| Case | Result |
|---|---|
| Wrong confirmation (table) | Rejected (400), table survives |
| Partial confirmation (table) | Rejected (400) |
| Empty confirmation (table) | Rejected (400) |
| Exact confirmation (table) | Deleted, verified gone from `list_tables` |
| Stale UI / already-deleted (table) | Second delete 404s, unrelated table untouched |
| Reader role attempts delete | Rejected (404/403), table survives |
| Wrong confirmation (schema) | Rejected (400) |
| Exact confirmation (schema) | Deleted |
| Non-empty schema | Refused (real `CatalogError`, no fabricated `CASCADE`) |
| Wrong confirmation (index) | Rejected (400) |
| Exact confirmation (index) | Deleted; post-delete `SELECT` still correct (planner falls back to scan) |
| Stale UI / already-deleted (index) | Second delete 404s |

## 2. DATABASE / INSTANCE — backend + CLI

No `CREATE DATABASE`/multi-database execution primitive exists in
this architecture (unchanged, documented boundary — one bootstrapped
database per instance), so "database deletion" is, correctly,
**instance deletion** — implemented as a new primitive,
`rubixdb_instance::remove_instance` (`instance/src/lib.rs`):

- Refuses (`NotFound`) for a name with no manifest.
- Refuses (`StillRunning`) whenever the instance's real OS-level lock
  is currently held — proven via the same `InstanceLock::try_acquire`
  primitive already certified, never a PID/staleness heuristic. A
  live instance's storage is never deleted out from under it.
- Only once the lock is confirmed free does it delete the directory.

Exposed via `rubixdb instance drop <NAME> --confirm <NAME>`
(`cli/src/instance_cmd.rs`) — wrong, partial, empty, or missing
`--confirm` is rejected by the CLI itself before the backend
primitive is ever called.

**Real tests**:
- `instance/src/lib.rs` (4 new unit tests): unknown name rejected,
  refuses while locked, deletes once unlocked (directory verified
  gone from disk), never touches a different instance. 4/4 pass.
- `cli/tests/instance_drop_integration.rs` (8 new tests), real
  compiled `rubixdb.exe` subprocesses, real OS locks: wrong/partial/
  empty/missing confirmation all rejected with data surviving; unknown
  name rejected; exact confirmation permanently deletes (directory
  gone, `instance list` no longer reports it); **a real running
  `rubixdb gui --no-browser` process is refused even with correct
  confirmation**; a second, independently-named instance survives
  deleting the first. 8/8 pass.

## 3. Frontend — real Data Explorer "Objects" tab

New tab (`frontend/src/pages/ExplorerPage.tsx`) listing schemas,
tables, and indexes (the backend's existing read-only `/v1/catalog/*`
routes, never rendered by any screen before this increment), each row
with a real delete action for `admin`-role sessions.

**Type-to-confirm, not click-to-confirm**: the delete dialog
(`TypeToConfirmDialog`, built on the existing `Dialog` component)
requires the operator to type the object's exact current name into a
text field before the delete button becomes enabled at all — wrong or
partial text leaves it disabled, matching the mission's own "type the
exact name" requirement literally, not just a yes/no click-through.

**Real Playwright E2E** (`frontend/e2e-gui/delete_safety.spec.ts`),
against the actual `rubixdb gui` product path (real release binary,
real production frontend build, real HTTP, `playwright.gui.config.ts`)
— 2/2 pass:

1. Delete button is disabled until the exact name is typed; wrong text
   keeps it disabled; correct text enables it; confirming actually
   removes the table, verified both in the UI and via a direct,
   independent `GET /v1/catalog/tables` call afterward.
2. **The mission's own named stale-UI scenario, reproduced for real**:
   GUI has the delete dialog open on table A with the correct name
   already typed (armed); a separate, direct API call (simulating
   "another client") deletes table A first; the GUI's stale confirm
   click is safely rejected (a real error is shown, no fabricated
   "Deleted table" success message ever appears), and an unrelated
   second table is proven completely untouched.

## 4. Explicit scope limitation (not hidden)

DATABASE/INSTANCE deletion is implemented and tested at the CLI level
only, not exposed as a GUI button. Reason, not an oversight: this
product hosts exactly one instance per running `rubixdb gui` process
(`PHASE_RUBIXDB_GUI_ARCHITECTURE.md` §2) — a GUI session cannot safely
delete the very instance directory its own live server has open
(`remove_instance` would correctly refuse it anyway, since its own
lock is held). A GUI-side "delete a different, currently-stopped
instance" screen was judged out of this blocker's core safety
requirement (which is preventing accidental data loss, not adding a
new instance-management screen) and left for a future increment if a
real need for it is measured.

## 5. Verdict

**DELETE SAFETY = PASS** — real backend enforcement (never
client-trust-only) for SCHEMA/TABLE/INDEX, real backend + CLI
enforcement for DATABASE/INSTANCE, real frontend type-to-confirm UI
wired to the real backend, and the mission's own named stale-UI/
concurrent-deletion scenario reproduced and proven safe at both the
API level and the actual GUI level. 12 + 4 + 8 + 2 = 26 new real tests
across four layers, all passing, zero mocking.

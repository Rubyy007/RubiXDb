import { test, expect, type Page, type APIRequestContext } from "@playwright/test";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

// Increment 14, Blocker 12 -- delete safety, real end-to-end against
// the real `rubixdb gui` product path (same config/server
// `gui_performance.spec.ts` uses: one real release binary, real
// production frontend build, real HTTP). Backend confirmation-
// enforcement is already proven by `api/tests/api_delete_safety.rs`;
// this file proves the actual GUI flow: the type-to-confirm dialog
// really gates the delete button, and a stale GUI (still showing an
// object another client already deleted) is safely rejected rather
// than fabricating success.

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const API_URL = "http://127.0.0.1:302";
const CREDENTIALS_PATH = path.join(__dirname, "..", ".e2e-gui-data", "default", "credentials.json");

function readAdminKey(): string {
  const raw = fs.readFileSync(CREDENTIALS_PATH, "utf-8");
  return (JSON.parse(raw) as { admin_key: string }).admin_key;
}

async function connect(page: Page, apiKey: string) {
  await page.goto("/connect");
  await expect(page.getByRole("heading", { name: "RubiXDB Console" })).toBeVisible();
  await page.getByLabel("API endpoint").fill(API_URL);
  await page.getByLabel("API key").fill(apiKey);
  await page.getByRole("button", { name: "Connect" }).click();
  await expect(page).toHaveURL("/");
}

async function execSql(request: APIRequestContext, apiKey: string, sql: string) {
  const resp = await request.post(`${API_URL}/v1/sql`, {
    headers: { Authorization: `Bearer ${apiKey}` },
    data: { sql, params: [] },
  });
  if (!resp.ok()) {
    throw new Error(`setup statement failed: ${sql} -> ${resp.status()} ${await resp.text()}`);
  }
}

async function tableIdByName(request: APIRequestContext, apiKey: string, name: string): Promise<number> {
  const resp = await request.get(`${API_URL}/v1/catalog/tables`, {
    headers: { Authorization: `Bearer ${apiKey}` },
  });
  const tables = (await resp.json()) as { table_id: number; name: string }[];
  const found = tables.find((t) => t.name === name);
  if (!found) throw new Error(`table ${name} not found in ${JSON.stringify(tables)}`);
  return found.table_id;
}

async function goToObjectsTab(page: Page) {
  await page.getByRole("link", { name: "Catalog" }).click();
  await expect(page.getByRole("heading", { name: "Data Explorer" })).toBeVisible();
  await page.getByRole("tab", { name: "Objects" }).click();
}

/** The "Tables" card specifically -- both the Tables panel (a table
 * row) and the Indexes panel (that table's own auto-created primary
 * index row, whose "Table" column repeats the table's name) can
 * otherwise both match a bare text search for the table name. */
function tablesCard(page: Page) {
  return page
    .locator("section.card")
    .filter({ has: page.getByRole("heading", { name: "Tables", exact: true }) });
}

test.describe("GUI delete safety -- real rubixdb gui product path", () => {
  test("type-to-confirm gates the delete button, and deletion actually removes the table", async ({
    page,
    request,
  }) => {
    const apiKey = readAdminKey();
    await execSql(request, apiKey, "DROP TABLE IF EXISTS gui_delete_t");
    await execSql(request, apiKey, "CREATE TABLE gui_delete_t (id INTEGER PRIMARY KEY)");

    await connect(page, apiKey);
    await goToObjectsTab(page);

    const row = tablesCard(page).locator("tr", { hasText: "gui_delete_t" });
    await expect(row).toBeVisible();
    await row.getByRole("button", { name: "Delete" }).click();

    const dialogConfirm = page.getByRole("button", { name: "Delete permanently" });
    await expect(dialogConfirm).toBeVisible();
    await expect(dialogConfirm).toBeDisabled();

    const typed = page.getByLabel(/Type "gui_delete_t" to confirm/);
    await typed.fill("wrong_name");
    await expect(dialogConfirm).toBeDisabled();

    await typed.fill("gui_delete_t");
    await expect(dialogConfirm).toBeEnabled();
    await dialogConfirm.click();

    await expect(tablesCard(page).locator("tr", { hasText: "gui_delete_t" })).toHaveCount(0);

    // Real durability check, not just "the UI stopped showing it":
    // re-query the real catalog directly.
    const tablesResp = await request.get(`${API_URL}/v1/catalog/tables`, {
      headers: { Authorization: `Bearer ${apiKey}` },
    });
    const tables = (await tablesResp.json()) as { name: string }[];
    expect(tables.some((t) => t.name === "gui_delete_t")).toBe(false);
  });

  /** Stale UI / concurrent deletion (the mission's own named scenario):
   * the GUI has object A open in its delete dialog; "another client"
   * deletes object A first via the real API; the GUI's own confirm
   * (even with the exact right typed name) must be safely rejected,
   * never a false success. */
  test("a stale delete dialog is safely rejected when another client deletes the object first", async ({
    page,
    request,
  }) => {
    const apiKey = readAdminKey();
    await execSql(request, apiKey, "DROP TABLE IF EXISTS gui_stale_t");
    await execSql(request, apiKey, "DROP TABLE IF EXISTS gui_stale_survivor");
    await execSql(request, apiKey, "CREATE TABLE gui_stale_t (id INTEGER PRIMARY KEY)");
    await execSql(request, apiKey, "CREATE TABLE gui_stale_survivor (id INTEGER PRIMARY KEY)");

    await connect(page, apiKey);
    await goToObjectsTab(page);

    const row = tablesCard(page).locator("tr", { hasText: "gui_stale_t" });
    await expect(row).toBeVisible();
    await row.getByRole("button", { name: "Delete" }).click();

    const dialogConfirm = page.getByRole("button", { name: "Delete permanently" });
    const typed = page.getByLabel(/Type "gui_stale_t" to confirm/);
    await typed.fill("gui_stale_t");
    await expect(dialogConfirm).toBeEnabled();

    // "Another client" deletes the table for real, out from under this
    // still-open dialog, via the exact same real endpoint.
    const staleId = await tableIdByName(request, apiKey, "gui_stale_t");
    const otherClientDelete = await request.delete(`${API_URL}/v1/catalog/tables/by-id/${staleId}`, {
      headers: { Authorization: `Bearer ${apiKey}`, "Content-Type": "application/json" },
      data: { schema_name: "public", table_name: "gui_stale_t" },
    });
    expect(otherClientDelete.ok()).toBe(true);

    // The stale dialog, unaware, now confirms.
    await dialogConfirm.click();

    // Must show a real error, never a fabricated success toast.
    await expect(page.getByText(/not found|does not match|failed/i)).toBeVisible({ timeout: 10_000 });
    await expect(page.getByText(/^Deleted table/)).toHaveCount(0);

    // The unrelated table must be completely untouched by any of this.
    const tablesResp = await request.get(`${API_URL}/v1/catalog/tables`, {
      headers: { Authorization: `Bearer ${apiKey}` },
    });
    const tables = (await tablesResp.json()) as { name: string }[];
    expect(tables.some((t) => t.name === "gui_stale_survivor")).toBe(true);
    expect(tables.some((t) => t.name === "gui_stale_t")).toBe(false);
  });
});

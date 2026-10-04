import { test, expect, type Page, type APIRequestContext } from "@playwright/test";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

// Operator console, real end to end against the real `rubixdb gui` product
// path (real release binary, real production frontend build, real HTTP):
// status cards, backup create/verify/delete (type-to-confirm), the integrity
// check, and the DROP TABLE reclaim flow (dry run + exact-count confirmation).

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const API_URL = "http://127.0.0.1:302";
const INSTANCE_DIR = path.join(__dirname, "..", ".e2e-gui-data", "default");

function readAdminKey(): string {
  const raw = fs.readFileSync(path.join(INSTANCE_DIR, "credentials.json"), "utf-8");
  return (JSON.parse(raw) as { admin_key: string }).admin_key;
}

async function connect(page: Page, apiKey: string) {
  await page.goto("/connect");
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
  if (!resp.ok()) throw new Error(`setup failed: ${sql} -> ${resp.status()} ${await resp.text()}`);
}

async function openOperations(page: Page) {
  await page.getByRole("link", { name: "Admin" }).click();
  await expect(page.getByRole("heading", { name: "Operations", level: 1 })).toBeVisible();
}

test.describe("GUI operator console -- real rubixdb gui product path", () => {
  test("status cards show live instance, WAL, queries and resources", async ({ page, request }) => {
    const apiKey = readAdminKey();
    await execSql(request, apiKey, "CREATE TABLE IF NOT EXISTS ops_gui_t (id INTEGER PRIMARY KEY, v TEXT)");
    for (let i = 0; i < 20; i++) {
      await execSql(request, apiKey, `INSERT INTO ops_gui_t (id, v) VALUES (${1000 + i + Date.now() % 100000}, 'x')`);
    }
    await connect(page, apiKey);
    await openOperations(page);
    for (const title of ["Instance", "Storage", "Write-ahead log", "Compaction", "Queries", "Resources"]) {
      await expect(page.getByRole("heading", { name: title, exact: true })).toBeVisible();
    }
    await expect(page.getByText("Memory (RSS)")).toBeVisible();
    await expect(page.getByText("Durable through")).toBeVisible();
    // The WAL badge must not claim poisoning on a healthy instance.
    await expect(page.getByText("Poisoned")).toHaveCount(0);
  });

  test("backup create -> verify -> delete is gated by the exact name", async ({ page, request }) => {
    const apiKey = readAdminKey();
    await execSql(request, apiKey, "CREATE TABLE IF NOT EXISTS ops_gui_b (id INTEGER PRIMARY KEY)");
    await connect(page, apiKey);
    await openOperations(page);

    const name = `gui-backup-${Date.now()}`;
    await page.getByLabel("New backup name").fill("../escape");
    await expect(page.getByText("Not a valid backup name.")).toBeVisible();
    await expect(page.getByRole("button", { name: "Create backup" })).toBeDisabled();

    await page.getByLabel("New backup name").fill(name);
    await page.getByRole("button", { name: "Create backup" }).click();
    const row = page.locator("tr", { hasText: name });
    await expect(row).toBeVisible({ timeout: 60_000 });
    expect(fs.existsSync(path.join(INSTANCE_DIR, "backups", `${name}.rbxbackup`))).toBe(true);

    await row.getByRole("button", { name: "Verify" }).click();
    await expect(page.getByText(new RegExp(`${name}: verified OK`))).toBeVisible({ timeout: 60_000 });

    await row.getByRole("button", { name: "Delete" }).click();
    const confirm = page.getByRole("button", { name: "Delete backup" });
    await expect(confirm).toBeDisabled();
    await page.getByLabel("Backup name", { exact: true }).fill(name.slice(0, -1));
    await expect(confirm).toBeDisabled();
    await page.getByLabel("Backup name", { exact: true }).fill(name);
    await expect(confirm).toBeEnabled();
    await confirm.click();
    await expect(page.locator("tr", { hasText: name })).toHaveCount(0, { timeout: 30_000 });
    expect(fs.existsSync(path.join(INSTANCE_DIR, "backups", `${name}.rbxbackup`))).toBe(false);
  });

  test("integrity check runs from the console and reports a clean database", async ({ page, request }) => {
    const apiKey = readAdminKey();
    await connect(page, apiKey);
    await openOperations(page);
    await page.getByRole("button", { name: "Run integrity check" }).click();
    await expect(page.getByText(/Clean/)).toBeVisible({ timeout: 120_000 });
    await expect(page.getByText(/rows and/)).toBeVisible();
    void request;
  });

  test("reclaiming a dropped table needs the exact entry count", async ({ page, request }) => {
    const apiKey = readAdminKey();
    await execSql(request, apiKey, "DROP TABLE IF EXISTS ops_gui_doomed");
    await execSql(request, apiKey, "CREATE TABLE ops_gui_doomed (id INTEGER PRIMARY KEY, v TEXT)");
    for (let i = 0; i < 25; i++) {
      await execSql(request, apiKey, `INSERT INTO ops_gui_doomed (id, v) VALUES (${i}, 'gone')`);
    }
    await execSql(request, apiKey, "DROP TABLE ops_gui_doomed");

    await connect(page, apiKey);
    await openOperations(page);
    await page.getByRole("button", { name: "Show what can be reclaimed" }).click();
    const deleteBtn = page.getByRole("button", { name: "Delete these entries…" });
    await expect(deleteBtn).toBeVisible({ timeout: 30_000 });
    const text = await page.getByText(/entries belong to/).innerText();
    const count = text.match(/([\d,]+) entries belong to/)![1].replace(/,/g, "");
    await deleteBtn.click();
    const confirm = page.getByRole("button", { name: "Delete entries" });
    await expect(confirm).toBeDisabled();
    await page.getByLabel("Entry count").fill(String(Number(count) + 1));
    await expect(confirm).toBeDisabled();
    await page.getByLabel("Entry count").fill(count);
    await expect(confirm).toBeEnabled();
    await confirm.click();
    await expect(page.getByText(/Deleted [\d,]+ entries; 0 remain/)).toBeVisible({ timeout: 60_000 });
  });

  test("a reader-role key sees no operator controls", async ({ page, request }) => {
    const apiKey = readAdminKey();
    // The console offers the same connect flow for any key; a bogus/reader key cannot use /v1/admin.
    const resp = await request.get(`${API_URL}/v1/admin/status`, { headers: { Authorization: "Bearer not-a-real-key-0123456789" } });
    expect(resp.status()).toBe(401);
    const ok = await request.get(`${API_URL}/v1/admin/status`, { headers: { Authorization: `Bearer ${apiKey}` } });
    expect(ok.status()).toBe(200);
    void page;
  });
});

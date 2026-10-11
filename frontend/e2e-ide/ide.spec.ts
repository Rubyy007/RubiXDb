import { test, expect, type Locator, type Page } from "@playwright/test";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

// SQL IDE against the REAL release binary with the console embedded in it
// (playwright.ide.config.ts), under the real CSP, on a disposable instances root.

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const INSTANCE_DIR = path.join(__dirname, "..", ".e2e-ide-data", "default");
const SHOTS = path.join(__dirname, "..", "test-results", "ide-shots");
const API = "http://127.0.0.1:302";

function adminKey(): string {
  const raw = fs.readFileSync(path.join(INSTANCE_DIR, "credentials.json"), "utf-8");
  return (JSON.parse(raw) as { admin_key: string }).admin_key;
}

async function sql(request: import("@playwright/test").APIRequestContext, text: string) {
  const res = await request.post(`${API}/v1/sql`, { headers: { Authorization: `Bearer ${adminKey()}` }, data: { sql: text, params: [] } });
  return { status: res.status(), body: await res.json() };
}

function watchErrors(page: Page): string[] {
  const errors: string[] = [];
  page.on("console", (m) => {
    if (m.type() === "error") errors.push(m.text());
  });
  page.on("pageerror", (e) => errors.push(String(e)));
  return errors;
}

async function openIde(page: Page) {
  await page.goto(`/#token=${adminKey()}`);
  await page.getByRole("navigation", { name: "Primary" }).getByRole("link", { name: "SQL Console" }).click();
  await expect(page).toHaveURL("/sql");
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("SQL Console");
}

const cell = (page: Page, n: number): Locator => page.locator(`section[aria-label="Cell ${n}"]`);
const editor = (page: Page, n: number) => page.getByLabel(`Cell ${n} SQL`);
const runBtn = (page: Page, n: number) => page.getByRole("button", { name: `Run cell ${n}` });

async function runCell(page: Page, n: number, text: string) {
  await editor(page, n).fill(text);
  await runBtn(page, n).click();
}

test.describe("SQL IDE -- real rubixdb gui binary, embedded console, real CSP", () => {
  test.beforeAll(() => fs.mkdirSync(SHOTS, { recursive: true }));

  test("loads clean under the CSP with the IDE layout", async ({ page }) => {
    const errors = watchErrors(page);
    await openIde(page);
    await expect(page.getByRole("tablist", { name: "Open worksheets" })).toBeVisible();
    await expect(page.getByRole("toolbar", { name: "Worksheet actions" })).toBeVisible();
    await expect(page.getByRole("complementary", { name: "Context panel" })).toBeVisible();
    await expect(page.getByText("0 running, 0 queued")).toBeVisible();
    await expect(page.getByRole("button", { name: "Run cell 1" })).toBeDisabled();
    await page.screenshot({ path: path.join(SHOTS, "ide-light-1280.png") });
    expect(errors, errors.join("\n")).toEqual([]);
  });

  test("several statements in one cell run in order, each in history exactly", async ({ page, request }) => {
    const errors = watchErrors(page);
    await sql(request, "DROP TABLE IF EXISTS ide_multi");
    await openIde(page);
    await runCell(
      page,
      1,
      [
        "CREATE TABLE ide_multi (id INTEGER PRIMARY KEY, name TEXT);",
        "INSERT INTO ide_multi (id, name) VALUES (1, 'a;b');",
        "INSERT INTO ide_multi (id, name) VALUES (2, 'it''s'); -- trailing; comment",
        "SELECT id, name FROM ide_multi ORDER BY id;",
      ].join("\n"),
    );
    await expect(cell(page, 1)).toHaveAttribute("data-status", "success", { timeout: 20_000 });
    // The cell shows the last statement's rows; the semicolon inside the string literal did not split.
    await expect(cell(page, 1).getByRole("cell", { name: "a;b", exact: true })).toBeVisible();
    await expect(cell(page, 1).getByRole("cell", { name: "it's", exact: true })).toBeVisible();
    const strip = cell(page, 1).getByRole("group", { name: "Statements in this cell" });
    await expect(strip.getByRole("button")).toHaveCount(4);
    await strip.getByRole("button").first().click();
    await expect(cell(page, 1).getByText("OK.")).toBeVisible();

    // Server really holds both rows.
    const count = await sql(request, "SELECT COUNT(*) FROM ide_multi");
    expect(JSON.stringify(count.body)).toContain('"2"');

    // History: one row per statement, newest first, exact text, no trailing semicolon, position shown.
    const rows = page.getByTestId("history-row");
    await expect(rows).toHaveCount(4);
    // A comment after a `;` belongs to the statement that follows it, so it is kept with that statement's text.
    await expect(rows.nth(0).getByLabel("Statement text")).toHaveText("-- trailing; comment SELECT id, name FROM ide_multi ORDER BY id");
    await expect(rows.nth(0)).toContainText("statement 4/4");
    await expect(rows.nth(1).getByLabel("Statement text")).toHaveText("INSERT INTO ide_multi (id, name) VALUES (2, 'it''s')");
    await expect(rows.nth(3).getByLabel("Statement text")).toHaveText("CREATE TABLE ide_multi (id INTEGER PRIMARY KEY, name TEXT)");
    await expect(rows.nth(3)).toContainText("statement 1/4");
    await page.screenshot({ path: path.join(SHOTS, "ide-multi-statement.png") });
    expect(errors, errors.join("\n")).toEqual([]);
    await sql(request, "DROP TABLE IF EXISTS ide_multi");
  });

  test("a failing statement stops the script and is pointed out", async ({ page }) => {
    await openIde(page);
    await runCell(page, 1, "SELECT 1;\n\nSELEC 2;\nSELECT 3;");
    await expect(cell(page, 1)).toHaveAttribute("data-status", "error", { timeout: 20_000 });
    const alert = cell(page, 1).getByRole("alert");
    await expect(alert).toContainText("Statement 2 of 3 failed; the 1 after it did not run.");
    const code = await alert.getAttribute("data-code");
    expect(code, "the API's exact error code is shown").toMatch(/^[A-Z_]+$/);
    await expect(alert.getByText(code as string, { exact: true })).toBeVisible();
    // Statement 1 ran, 2 failed, 3 was never sent: two history rows.
    await expect(page.getByTestId("history-row")).toHaveCount(2);
    await page.screenshot({ path: path.join(SHOTS, "ide-error.png") });
  });

  test("a transaction container: rollback discards, commit keeps, session visible on the server", async ({ page, request }) => {
    await sql(request, "DROP TABLE IF EXISTS ide_txn");
    await sql(request, "CREATE TABLE ide_txn (id INTEGER PRIMARY KEY)");
    await openIde(page);
    await page.getByRole("button", { name: "+ Transaction" }).last().click();
    // The container's first cell is cell 2 (cell 1 is the empty top-level cell).
    await expect(runBtn(page, 2)).toBeDisabled();
    await page.getByRole("button", { name: "Begin" }).click();
    await expect(page.getByText(/^session /)).toBeVisible({ timeout: 15_000 });

    const sessions = await request.get(`${API}/v1/observability/sessions`, { headers: { Authorization: `Bearer ${adminKey()}` } });
    expect(((await sessions.json()) as { total: number }).total).toBe(1);

    await runCell(page, 2, "INSERT INTO ide_txn (id) VALUES (1)");
    await expect(cell(page, 2)).toHaveAttribute("data-status", "success", { timeout: 15_000 });
    await page.getByRole("button", { name: "Rollback" }).first().click();
    await expect(page.getByText(/^session /)).toHaveCount(0, { timeout: 15_000 });
    expect(JSON.stringify((await sql(request, "SELECT COUNT(*) FROM ide_txn")).body)).toContain('"0"');

    await page.getByRole("button", { name: "Begin" }).click();
    await expect(page.getByText(/^session /)).toBeVisible({ timeout: 15_000 });
    await runCell(page, 2, "INSERT INTO ide_txn (id) VALUES (2)");
    await expect(cell(page, 2)).toHaveAttribute("data-status", "success", { timeout: 15_000 });
    await page.getByRole("button", { name: "Commit" }).first().click();
    await expect(page.getByText(/^session /)).toHaveCount(0, { timeout: 15_000 });
    expect(JSON.stringify((await sql(request, "SELECT COUNT(*) FROM ide_txn")).body)).toContain('"1"');
    await sql(request, "DROP TABLE IF EXISTS ide_txn");
  });

  test("BEGIN ... COMMIT inside one cell is one transaction", async ({ page, request }) => {
    await sql(request, "DROP TABLE IF EXISTS ide_script_txn");
    await sql(request, "CREATE TABLE ide_script_txn (id INTEGER PRIMARY KEY)");
    await openIde(page);
    await runCell(page, 1, "BEGIN;\nINSERT INTO ide_script_txn (id) VALUES (1);\nINSERT INTO ide_script_txn (id) VALUES (2);\nCOMMIT;");
    await expect(cell(page, 1)).toHaveAttribute("data-status", "success", { timeout: 20_000 });
    expect(JSON.stringify((await sql(request, "SELECT COUNT(*) FROM ide_script_txn")).body)).toContain('"2"');
    await expect(page.getByText(/open transaction/)).toHaveCount(0);

    // ROLLBACK in a script discards.
    await runCell(page, 1, "BEGIN;\nINSERT INTO ide_script_txn (id) VALUES (3);\nROLLBACK;");
    await expect(cell(page, 1)).toHaveAttribute("data-status", "success", { timeout: 20_000 });
    expect(JSON.stringify((await sql(request, "SELECT COUNT(*) FROM ide_script_txn")).body)).toContain('"2"');
    await sql(request, "DROP TABLE IF EXISTS ide_script_txn");
  });

  test("Run all runs several cells at once and shows each result", async ({ page }) => {
    await openIde(page);
    await editor(page, 1).fill("SELECT 1");
    await page.getByRole("button", { name: "+ SQL cell" }).last().click();
    await editor(page, 2).fill("SELECT 2");
    await page.getByRole("button", { name: "+ SQL cell" }).last().click();
    await editor(page, 3).fill("SELECT 3");
    await page.getByRole("button", { name: /Run all/ }).click();
    for (const n of [1, 2, 3]) await expect(cell(page, n)).toHaveAttribute("data-status", "success", { timeout: 20_000 });
    await expect(page.getByRole("group", { name: "Single value result" })).toHaveCount(3);
  });

  test("EXPLAIN plan tab shows a tree", async ({ page, request }) => {
    await sql(request, "DROP TABLE IF EXISTS ide_plan");
    await sql(request, "CREATE TABLE ide_plan (id INTEGER PRIMARY KEY, v TEXT)");
    await openIde(page);
    await runCell(page, 1, "SELECT id, v FROM ide_plan WHERE id = 1");
    await expect(cell(page, 1)).toHaveAttribute("data-status", "success", { timeout: 20_000 });
    await cell(page, 1).getByRole("tab", { name: "Plan" }).click();
    await expect(cell(page, 1).getByRole("list", { name: "Query plan" })).toBeVisible({ timeout: 15_000 });
    await expect(cell(page, 1).getByRole("list", { name: "Query plan" })).toContainText("Query");
    await sql(request, "DROP TABLE IF EXISTS ide_plan");
  });

  test("Home templates load into the IDE and run cleanly in order", async ({ page, request }) => {
    await sql(request, "DROP TABLE IF EXISTS customers");
    await page.goto(`/#token=${adminKey()}`);
    await expect(page.getByRole("heading", { level: 1 })).toHaveText("Home");
    const titles = ["Create a table", "Insert rows", "Filter and sort", "Add an index", "Group and count", "Explain a query"];
    for (const title of titles) {
      await page.getByRole("button", { name: `Load template: ${title}` }).click();
      await expect(page).toHaveURL("/sql");
      const n = await page.getByRole("textbox", { name: /^Cell \d+ SQL$/ }).count();
      await expect(editor(page, n)).not.toHaveValue("");
      await runBtn(page, n).click();
      await expect(cell(page, n), `template "${title}" must run cleanly`).toHaveAttribute("data-status", "success", { timeout: 20_000 });
      await page.getByRole("link", { name: "Home" }).click();
    }
    await sql(request, "DROP TABLE IF EXISTS customers");
  });

  test("Catalog opens on Objects and offers no delete button", async ({ page, request }) => {
    await sql(request, "DROP TABLE IF EXISTS ide_catalog");
    await sql(request, "CREATE TABLE ide_catalog (id INTEGER PRIMARY KEY)");
    await page.goto(`/#token=${adminKey()}`);
    await page.getByRole("navigation", { name: "Primary" }).getByRole("link", { name: "Catalog" }).click();
    await expect(page).toHaveURL("/explorer");
    const tabs = page.getByRole("tab");
    await expect(tabs).toHaveText(["Objects", "Point Lookup", "Range Query"]);
    await expect(page.getByRole("tab", { name: "Objects" })).toHaveAttribute("aria-selected", "true");
    await expect(page.getByRole("cell", { name: "ide_catalog", exact: true }).first()).toBeVisible();
    await expect(page.getByRole("button", { name: /delete/i })).toHaveCount(0);
    await page.screenshot({ path: path.join(SHOTS, "catalog-objects.png") });
    await page.getByRole("tab", { name: "Point Lookup" }).click();
    await expect(page.getByRole("tab", { name: "Point Lookup" })).toHaveAttribute("aria-selected", "true");
    await sql(request, "DROP TABLE IF EXISTS ide_catalog");
  });

  test("command palette, keyboard run, reload persistence, dark mode and mobile layout", async ({ page }) => {
    const errors = watchErrors(page);
    await openIde(page);
    await page.keyboard.press("Control+k");
    const dialog = page.getByRole("dialog", { name: "Command palette" });
    await expect(dialog).toBeVisible();
    await page.screenshot({ path: path.join(SHOTS, "ide-palette.png") });
    await dialog.getByRole("combobox").fill("dark");
    await dialog.getByRole("combobox").press("Enter");
    await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
    await page.screenshot({ path: path.join(SHOTS, "ide-dark.png") });

    await editor(page, 1).fill("SELECT 42");
    await editor(page, 1).press("Control+Enter");
    await expect(cell(page, 1)).toHaveAttribute("data-status", "success", { timeout: 20_000 });
    await page.waitForTimeout(700);
    await page.reload();
    await expect(editor(page, 1)).toHaveValue("SELECT 42");

    await page.setViewportSize({ width: 390, height: 844 });
    await expect(page.getByLabel("Cell 1 SQL")).toBeVisible();
    await page.screenshot({ path: path.join(SHOTS, "ide-mobile-390.png") });
    expect(errors, errors.join("\n")).toEqual([]);
  });
});

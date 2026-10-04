import { test, expect, type Page } from "@playwright/test";
import path from "node:path";
import { fileURLToPath } from "node:url";

// Shell redesign, Increment 2 -- Home page. Real API (see playwright.config.ts);
// the RAM-card cases intercept /v1/status only to remove or add memory fields
// on the REAL response (the API does not serve memory figures today).

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const axePath = path.join(__dirname, "..", "node_modules", "axe-core", "axe.min.js");
const ADMIN_KEY = "e2e-admin-key-0123456789";
const READER_KEY = "e2e-reader-key-0123456789";
const API_URL = "http://127.0.0.1:8099";

async function connect(page: Page, key = ADMIN_KEY) {
  await page.goto("/connect");
  await page.getByLabel("API endpoint").fill(API_URL);
  await page.getByLabel("API key").fill(key);
  await page.getByRole("button", { name: "Connect" }).click();
  await expect(page).toHaveURL("/");
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("Home");
}

function trackConsoleErrors(page: Page) {
  const errors: string[] = [];
  page.on("console", (m) => {
    if (m.type() === "error") errors.push(m.text());
  });
  page.on("pageerror", (e) => errors.push(String(e)));
  return errors;
}

test.describe("Home page", () => {
  test("renders its sections with one h1, no console errors, and no axe violations", async ({ page }) => {
    const errors = trackConsoleErrors(page);
    const failed: string[] = [];
    page.on("response", (r) => {
      if (r.status() >= 400) failed.push(`${r.status()} ${new URL(r.url()).pathname}`);
    });
    await connect(page);
    await expect(page.getByRole("heading", { level: 1 })).toHaveCount(1);
    for (const h of ["Quick actions", "Recent items", "Start with a template"]) {
      await expect(page.getByRole("heading", { level: 2, name: h })).toBeVisible();
    }
    // Interact first so the audit also covers the non-default states.
    await page.getByRole("tab", { name: "Snapshots" }).click();
    await page.getByRole("button", { name: "Notifications" }).click();
    await page.addScriptTag({ path: axePath });
    const results = await page.evaluate(() =>
      (window as unknown as { axe: { run: () => Promise<{ violations: unknown[] }> } }).axe.run(),
    );
    expect(results.violations, JSON.stringify(results.violations, null, 2)).toEqual([]);
    expect(failed, failed.join(", ")).toEqual([]);
    expect(errors, errors.join("\n")).toEqual([]);
  });

  test("quick actions navigate / act, and only real actions are offered", async ({ page }) => {
    await connect(page);
    const qa = page.getByRole("region", { name: "Quick actions" });
    await expect(qa.locator(".qa-card")).toHaveCount(3);
    for (const absent of ["Invite", "Notebook", "Amazon S3", "cloud storage", "sentiment", "Import data"]) {
      await expect(page.getByText(absent)).toHaveCount(0);
    }

    await qa.getByRole("button", { name: /Create snapshot/ }).click();
    await expect(qa.getByRole("status")).toContainText(/Snapshot created at seq \d+/);

    await qa.getByRole("link", { name: /View backups/ }).click();
    await expect(page).toHaveURL("/operations");

    await page.getByRole("link", { name: "Home" }).click();
    await page.getByRole("region", { name: "Quick actions" }).getByRole("link", { name: /New SQL query/ }).click();
    await expect(page).toHaveURL("/sql");
    await expect(page.getByLabel("SQL")).toHaveValue("");
  });

  test("a reader sees only the quick action it can actually use", async ({ page }) => {
    await connect(page, READER_KEY);
    const qa = page.getByRole("region", { name: "Quick actions" });
    await expect(qa.locator(".qa-card")).toHaveCount(1);
    await expect(qa.getByRole("link", { name: /New SQL query/ })).toBeVisible();
  });

  test("recent items: tabs switch, snapshots and queries are real, empty states are honest", async ({ page }) => {
    await connect(page);
    const recent = page.getByRole("region", { name: "Recent items" });
    for (const t of ["All", "Queries", "Snapshots", "Backups"]) {
      await recent.getByRole("tab", { name: t }).click();
      await expect(recent.getByRole("tab", { name: t })).toHaveAttribute("aria-selected", "true");
    }
    await recent.getByRole("tab", { name: "Queries" }).click();
    await expect(recent.getByText("No queries yet")).toBeVisible();
    await expect(recent.getByText("in memory for this browser session only")).toBeVisible();

    // A real snapshot, created through the API's own path, shows up.
    await page.getByRole("region", { name: "Quick actions" }).getByRole("button", { name: /Create snapshot/ }).click();
    await expect(page.getByRole("region", { name: "Quick actions" }).getByRole("status")).toContainText("Snapshot created");
    await recent.getByRole("tab", { name: "Snapshots" }).click();
    await expect(recent.getByRole("row").nth(1)).toContainText("Snapshot ");
    await recent.getByRole("tab", { name: "All" }).click();
    await expect(recent.getByRole("columnheader")).toHaveText(["Title", "Type", "Viewed", "Updated"]);
  });

  test("recent items render a hostile query title as inert text (no DOM, no script)", async ({ page }) => {
    const payload = "<img src=x onerror=alert(1)>";
    const dialogs: string[] = [];
    page.on("dialog", async (d) => {
      dialogs.push(d.message());
      await d.dismiss();
    });
    await connect(page);
    await page.getByRole("link", { name: "SQL Console" }).click();
    await page.getByLabel("SQL").fill(payload);
    await page.getByRole("button", { name: "Run", exact: true }).click();
    await expect(page.getByRole("alert")).toBeVisible(); // parse error: still recorded in history
    await page.getByRole("link", { name: "Home" }).click(); // client-side nav keeps the in-memory history

    const recent = page.getByRole("region", { name: "Recent items" });
    await recent.getByRole("tab", { name: "Queries" }).click();
    const cell = recent.getByRole("cell", { name: payload });
    await expect(cell).toBeVisible();
    await expect(cell).toHaveText(payload);
    await expect(recent.locator("img")).toHaveCount(0);
    await expect(recent.locator("[onerror]")).toHaveCount(0);
    await page.waitForTimeout(300);
    expect(dialogs).toEqual([]);
    expect(await page.evaluate(() => document.querySelectorAll("img[src='x']").length)).toBe(0);
  });

  test("query history is memory only: only the worksheet text (sessionStorage) is stored, never localStorage, and a reload clears the history", async ({ page }) => {
    await connect(page);
    await page.getByRole("link", { name: "SQL Console" }).click();
    await page.getByLabel("SQL").fill("SELECT 1");
    await page.getByRole("button", { name: "Run", exact: true }).click();
    await expect(page.getByRole("table").first()).toBeVisible();
    // Since the SQL Console redesign the open worksheets' TEXT is kept in sessionStorage (one key,
    // this tab only); the query HISTORY shown on Home is still never stored anywhere.
    await page.waitForTimeout(700);
    const stored = await page.evaluate(() => ({
      local: JSON.stringify(window.localStorage),
      sessionKeys: Object.keys(window.sessionStorage).filter((k) => window.sessionStorage.getItem(k)?.includes("SELECT 1")),
    }));
    expect(stored.local).not.toContain("SELECT 1");
    expect(stored.sessionKeys).toEqual(["rubixdb-console-worksheets"]);
    await page.reload();
    await page.getByRole("link", { name: "Home" }).click();
    await page.getByRole("tab", { name: "Queries" }).click();
    await expect(page.getByText("No queries yet")).toBeVisible();
  });

  test("a template loads its static SQL into the console without running it", async ({ page }) => {
    await connect(page);
    await page.getByRole("button", { name: "Load template: Create a table" }).click();
    await expect(page).toHaveURL("/sql");
    await expect(page.getByLabel("SQL")).toHaveValue(/^CREATE TABLE customers/);
    await expect(page.getByRole("table")).toHaveCount(0); // nothing was executed
  });
});

test.describe("RAM usage card", () => {
  test("shows 'not available' (no number) when /v1/status has no memory fields", async ({ page }) => {
    let seen = 0;
    await page.route("**/v1/status", async (route) => {
      const resp = await route.fetch();
      const body = await resp.json();
      delete body.memory_used_bytes;
      delete body.memory_total_bytes;
      seen += 1;
      await route.fulfill({ response: resp, json: body });
    });
    await connect(page);
    const card = page.locator(".ram-card");
    await expect(card).toContainText("RAM usage");
    await expect(card).toContainText(/not available/i);
    await expect(card).not.toContainText("%");
    await expect(card.locator("svg")).toHaveCount(0);
    expect(seen).toBeGreaterThan(0);
  });

  test("shows the reported figures, status pill and sparkline when /v1/status carries them", async ({ page }) => {
    const GIB = 1024 ** 3;
    await page.route("**/v1/status", async (route) => {
      const resp = await route.fetch();
      const body = await resp.json();
      body.memory_used_bytes = 6 * GIB;
      body.memory_total_bytes = 8 * GIB;
      await route.fulfill({ response: resp, json: body });
    });
    await connect(page);
    const card = page.locator(".ram-card");
    await expect(card).toContainText("75%");
    await expect(card).toContainText("Warning");
    await expect(card).toContainText("6.0 GB / 8.0 GB total");
  });
});

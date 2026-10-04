import { test, expect } from "@playwright/test";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

// Shell redesign, Increment 2 -- Home page against the REAL `rubixdb gui`
// product path (real release binary, real status endpoint, real CSP).

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const INSTANCE_DIR = path.join(__dirname, "..", ".e2e-gui-data", "default");
const API_URL = "http://127.0.0.1:302";

function readAdminKey(): string {
  const raw = fs.readFileSync(path.join(INSTANCE_DIR, "credentials.json"), "utf-8");
  return (JSON.parse(raw) as { admin_key: string }).admin_key;
}

test.describe("Home -- real rubixdb gui product path", () => {
  test("no console/CSP errors; the RAM card reports 'not available' because the real /v1/status has no memory fields", async ({
    page,
    request,
  }) => {
    const errors: string[] = [];
    page.on("console", (m) => {
      if (m.type() === "error") errors.push(m.text());
    });
    page.on("pageerror", (e) => errors.push(String(e)));

    const status = await request.get(`${API_URL}/v1/status`, {
      headers: { Authorization: `Bearer ${readAdminKey()}` },
    });
    const body = (await status.json()) as Record<string, unknown>;
    expect(Object.keys(body).some((k) => /mem|ram|rss/i.test(k))).toBe(false);

    await page.goto(`/#token=${readAdminKey()}`);
    await expect(page.getByRole("heading", { level: 1 })).toHaveText("Home");
    await expect(page.locator(".ram-card")).toContainText(/not available/i);
    await expect(page.locator(".ram-card")).not.toContainText("%");
    // Backups are configured in the real product: the admin sees the section without any failed request.
    await page.getByRole("tab", { name: "Backups" }).click();
    await expect(page.getByText("No backups").or(page.getByRole("table"))).toBeVisible();
    expect(errors, errors.join("\n")).toEqual([]);
  });

  test("every template runs against the real SQL surface, in order, without an error", async ({ page, request }) => {
    const key = readAdminKey();
    await request.post(`${API_URL}/v1/sql`, {
      headers: { Authorization: `Bearer ${key}` },
      data: { sql: "DROP TABLE IF EXISTS customers", params: [] },
    });
    await page.goto(`/#token=${key}`);
    await expect(page.getByRole("heading", { level: 1 })).toHaveText("Home");

    const titles = [
      "Create a table",
      "Insert rows",
      "Filter and sort",
      "Add an index",
      "Group and count",
      "Explain a query",
    ];
    for (const title of titles) {
      await page.getByRole("button", { name: `Load template: ${title}` }).click();
      await expect(page).toHaveURL("/sql");
      await expect(page.getByLabel("SQL")).not.toHaveValue("");
      await page.getByRole("button", { name: "Run", exact: true }).click();
      await expect(page.getByRole("button", { name: "Run", exact: true })).toBeEnabled();
      await expect(page.getByRole("alert"), `template "${title}" must run cleanly`).toHaveCount(0);
      await expect(page.locator(".sqlc-results-body")).not.toContainText("Run a query to see results");
      await page.getByRole("link", { name: "Home" }).click();
    }

    // The query history it produced is on Home, as inert text, newest first.
    await page.getByRole("tab", { name: "Queries" }).click();
    await expect(page.getByRole("cell", { name: /^EXPLAIN SELECT/ })).toBeVisible();
    await request.post(`${API_URL}/v1/sql`, {
      headers: { Authorization: `Bearer ${key}` },
      data: { sql: "DROP TABLE IF EXISTS customers", params: [] },
    });
  });
});

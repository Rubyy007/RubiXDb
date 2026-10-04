import { test, expect, type Page } from "@playwright/test";

// Final single-node certification: database content is UNTRUSTED. Real
// browser, real backend (see playwright.config.ts). Malicious values stored in
// the database must render as inert text -- never execute, never become DOM.

const ADMIN_KEY = "e2e-admin-key-0123456789";
const API_URL = "http://127.0.0.1:8099";

async function connect(page: Page) {
  await page.goto("/connect");
  await page.getByLabel("API endpoint").fill(API_URL);
  await page.getByLabel("API key").fill(ADMIN_KEY);
  await page.getByRole("button", { name: "Connect" }).click();
  await expect(page).toHaveURL("/");
}

async function runSql(page: Page, sql: string) {
  await page.getByLabel("SQL").fill(sql);
  await page.getByRole("button", { name: "Run", exact: true }).click();
}

const PAYLOADS = [
  `<script>window.__xss=1;document.title='pwned'</script>`,
  `<img src=x onerror="window.__xss=2;alert(2)">`,
  `<svg onload="window.__xss=3;alert(3)"></svg>`,
  `"><iframe srcdoc="<script>parent.__xss=4</script>"></iframe>`,
  `<a href="javascript:window.__xss=5">click</a>`,
  `‮evil‬ <b>bold</b> &lt;i&gt; {{7*7}} \${7*7}`,
];

test.describe("frontend security -- untrusted database content, real browser", () => {
  test("malicious stored values render as inert text; no script runs, no DOM injected", async ({ page }) => {
    const dialogs: string[] = [];
    page.on("dialog", async (d) => {
      dialogs.push(d.message());
      await d.dismiss();
    });
    const table = `xss_${Date.now()}`;
    await connect(page);
    await page.getByRole("link", { name: "SQL Console" }).click();
    await runSql(page, `CREATE TABLE ${table} (id INTEGER PRIMARY KEY, v TEXT)`);
    await expect(page.getByText("OK.")).toBeVisible({ timeout: 10_000 });
    for (let i = 0; i < PAYLOADS.length; i++) {
      await runSql(page, `INSERT INTO ${table} (id, v) VALUES (${i}, '${PAYLOADS[i].replace(/'/g, "''")}')`);
      await expect(page.getByText(/INSERT.*1 row/)).toBeVisible({ timeout: 10_000 });
    }
    await runSql(page, `SELECT id, v FROM ${table} ORDER BY id`);
    await expect(page.getByText(/Result \(6 rows\)/)).toBeVisible({ timeout: 10_000 });

    // The raw payload text is visible verbatim (escaped), proving it was rendered as text.
    await expect(page.locator(".table-wrap").getByText(PAYLOADS[0], { exact: true })).toBeVisible();
    await expect(page.locator(".table-wrap").getByText(PAYLOADS[1], { exact: true })).toBeVisible();

    // No injected elements anywhere in the result region or the page.
    const injected = await page.evaluate(() => ({
      scripts: document.querySelectorAll(".table-wrap script, .table-wrap iframe, .table-wrap svg, .table-wrap img").length,
      links: document.querySelectorAll('.table-wrap a[href^="javascript:"]').length,
      bold: document.querySelectorAll(".table-wrap b").length,
      xss: (window as unknown as { __xss?: number }).__xss ?? null,
      title: document.title,
    }));
    expect(injected.scripts).toBe(0);
    expect(injected.links).toBe(0);
    expect(injected.bold).toBe(0);
    expect(injected.xss).toBeNull();
    expect(injected.title).not.toBe("pwned");
    expect(dialogs).toEqual([]);

    // Other views of the same untrusted data (history replay) are inert too.
    await page.reload();
    await expect(page).toHaveURL(/\/(sql|$)/);
    expect(dialogs).toEqual([]);
    await runSql(page, `DROP TABLE ${table}`);
  });

  test("a very large stored value (~900 KB, under the 1 MiB statement limit) renders safely; an over-limit statement is refused cleanly", async ({ page }) => {
    const table = `xssbig_${Date.now()}`;
    await connect(page);
    await page.getByRole("link", { name: "SQL Console" }).click();
    await runSql(page, `CREATE TABLE ${table} (id INTEGER PRIMARY KEY, v TEXT)`);
    await expect(page.getByText("OK.")).toBeVisible({ timeout: 10_000 });
    const big = "<b>".repeat(300_000); // ~900 KB of markup-looking text (limit: 1 MiB per statement)
    await runSql(page, `INSERT INTO ${table} (id, v) VALUES (1, '${big}')`);
    await expect(page.getByText(/INSERT.*1 row/)).toBeVisible({ timeout: 30_000 });
    await runSql(page, `SELECT id, v FROM ${table}`);
    await expect(page.getByText(/Result \(1 rows?\)/)).toBeVisible({ timeout: 30_000 });
    expect(await page.evaluate(() => document.querySelectorAll(".table-wrap b").length)).toBe(0);
    await expect(page.getByRole("button", { name: "Run", exact: true })).toBeEnabled();
    // An over-limit statement (> 1 MiB) is refused by the backend; the UI shows an error and stays usable.
    await runSql(page, `INSERT INTO ${table} (id, v) VALUES (2, '${"<b>".repeat(400_000)}')`);
    await expect(page.getByText(/limit|too large|resource/i).first()).toBeVisible({ timeout: 30_000 });
    await expect(page.getByRole("button", { name: "Run", exact: true })).toBeEnabled();
    await runSql(page, `DROP TABLE ${table}`);
  });
});

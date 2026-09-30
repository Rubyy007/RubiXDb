import { test, expect, type Page, type APIRequestContext } from "@playwright/test";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

// Increment 14, Blocker 11 -- the true 100,000-row GUI case. Real
// production frontend build, real `rubixdb gui` product path.
//
// First inspected the actual production limit rather than guessing or
// bypassing it: `sql/src/exec/mod.rs`'s `execute_query` rejects only
// once an already-accumulated result would grow *past*
// `ExecLimits::max_result_rows` (default 100,000, `sql/src/exec/
// mod.rs:260`) -- the check is `if rows.len() >= limits.max_result_rows`
// evaluated *before* pushing each new tuple, so a result of exactly
// 100,000 rows is legitimately, successfully returned (the 100,001st
// tuple is what trips the limit). This test therefore requests exactly
// 100,000 rows for real, through the real product limit, never a
// weakened one.

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const API_URL = "http://127.0.0.1:302";
const CREDENTIALS_PATH = path.join(__dirname, "..", ".e2e-gui-data", "default", "credentials.json");
const ROW_COUNT = 100_000;
const BATCH_SIZE = 100; // sql/src/limits.rs SqlLimits::max_expression_depth bounds a flat VALUES chain

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
    throw new Error(`setup statement failed: ${sql.slice(0, 120)}... -> ${resp.status()} ${await resp.text()}`);
  }
  return resp.json();
}

test.describe("100,000-row GUI case -- real rubixdb gui product path", () => {
  test("exactly 100,000 rows: execution, transfer, parse, and render, real end to end", async ({
    page,
    request,
  }) => {
    test.setTimeout(10 * 60_000);
    const apiKey = readAdminKey();

    await execSql(request, apiKey, "DROP TABLE IF EXISTS hundred_k_t");
    await execSql(
      request,
      apiKey,
      "CREATE TABLE hundred_k_t (id INTEGER PRIMARY KEY, v TEXT, val INTEGER)",
    );

    const seedStart = Date.now();
    for (let start = 0; start < ROW_COUNT; start += BATCH_SIZE) {
      const end = Math.min(start + BATCH_SIZE, ROW_COUNT);
      const tuples: string[] = [];
      for (let id = start; id < end; id++) tuples.push(`(${id},'row-${id}',${id})`);
      await execSql(request, apiKey, `INSERT INTO hundred_k_t (id, v, val) VALUES ${tuples.join(",")}`);
    }
    console.log(`[100k-gui] seeded ${ROW_COUNT} rows in ${Date.now() - seedStart}ms (setup, not the measured metric)`);

    // Confirm the exact boundary directly against the real,
    // unweakened production limit before ever touching the GUI:
    // exactly 100,000 rows must succeed (proven below via the GUI's
    // own execution); 100,001 must be rejected. Proven here by
    // temporarily inserting one extra row, observing the real
    // `RESOURCE_LIMIT` rejection, then removing it again so the GUI
    // step below queries the real, exact 100,000-row case.
    await execSql(request, apiKey, `INSERT INTO hundred_k_t (id, v, val) VALUES (${ROW_COUNT},'over',${ROW_COUNT})`);
    const overLimitResp = await request.post(`${API_URL}/v1/sql`, {
      headers: { Authorization: `Bearer ${apiKey}` },
      data: { sql: "SELECT id, v, val FROM hundred_k_t", params: [] },
    });
    expect(overLimitResp.status()).toBe(413);
    const overLimitBody = await overLimitResp.json();
    expect(overLimitBody.error.code).toBe("RESOURCE_LIMIT");
    console.log(`[100k-gui] confirmed real 100,001-row rejection: ${JSON.stringify(overLimitBody.error)}`);
    await execSql(request, apiKey, `DELETE FROM hundred_k_t WHERE id = ${ROW_COUNT}`);

    await connect(page, apiKey);
    await page.getByRole("link", { name: "SQL Console" }).click();
    await expect(page.getByRole("heading", { name: "SQL Console" })).toBeVisible();

    const execStart = Date.now();
    await page.getByLabel("SQL").fill("SELECT id, v, val FROM hundred_k_t");
    await page.getByRole("button", { name: "Execute" }).click();
    await expect(page.getByText(new RegExp(`Result \\(${ROW_COUNT} rows\\)`))).toBeVisible({
      timeout: 120_000,
    });
    const executeAndRenderMs = Date.now() - execStart;

    const renderedRows = await page.locator(".table-wrap tbody tr").count();

    const heapBytes = await page.evaluate(() => {
      const mem = (performance as unknown as { memory?: { usedJSHeapSize: number } }).memory;
      return mem ? mem.usedJSHeapSize : null;
    });

    console.log(
      `[100k-gui] execute+render=${executeAndRenderMs}ms renderedDomRows=${renderedRows} jsHeapBytes=${heapBytes}`,
    );

    // Bounded-DOM contract must still hold at the true limit, not just
    // at the smaller sizes already measured in Increment 13.
    expect(renderedRows).toBeLessThanOrEqual(200);

    // Scrolling/pagination must remain responsive at this size.
    const content = page.locator(".app-content");
    const pageStart = Date.now();
    await content.evaluate((el) => {
      el.scrollTop = el.scrollHeight;
    });
    const scrollMs = Date.now() - pageStart;
    console.log(`[100k-gui] scroll-to-bottom=${scrollMs}ms`);

    const nextButton = page.getByRole("button", { name: "Next" });
    await expect(nextButton).toBeEnabled();
    const clickStart = Date.now();
    await nextButton.click();
    await expect(page.getByText("Page 2 of")).toBeVisible();
    console.log(`[100k-gui] pagination click-to-settle=${Date.now() - clickStart}ms`);
  });
});

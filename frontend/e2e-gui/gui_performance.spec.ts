import { test, expect, type Page, type APIRequestContext } from "@playwright/test";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const API_URL = "http://127.0.0.1:302";
const CREDENTIALS_PATH = path.join(
  __dirname,
  "..",
  ".e2e-gui-data",
  "default",
  "credentials.json",
);

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
    throw new Error(`seed statement failed: ${sql} -> ${resp.status()} ${await resp.text()}`);
  }
}

/** Seeds `count` rows directly via the real API (not through the UI --
 * seeding speed is not what this file measures; UI-driven SELECT
 * execution and render timing is). Runs in bounded-size concurrent
 * batches for real wall-clock speed, still one real HTTP request per
 * row (this engine's write executor is single-row INSERT only). */
async function seedRows(request: APIRequestContext, apiKey: string, table: string, count: number) {
  await execSql(request, apiKey, `DROP TABLE IF EXISTS ${table}`);
  await execSql(request, apiKey, `CREATE TABLE ${table} (id INTEGER PRIMARY KEY, v TEXT, val INTEGER)`);
  const batchSize = 32;
  for (let start = 0; start < count; start += batchSize) {
    const end = Math.min(start + batchSize, count);
    const batch = [];
    for (let id = start; id < end; id++) {
      batch.push(execSql(request, apiKey, `INSERT INTO ${table} (id, v, val) VALUES (${id}, 'row-${id}', ${id})`));
    }
    await Promise.all(batch);
  }
}

test.describe("GUI performance -- real rubixdb gui product path, real production frontend build", () => {
  test("page load timing against the real gui-hosted frontend", async ({ page }) => {
    const apiKey = readAdminKey();
    const start = Date.now();
    await connect(page, apiKey);
    const wallClockMs = Date.now() - start;

    const navTiming = await page.evaluate(() => {
      const [nav] = performance.getEntriesByType("navigation") as PerformanceNavigationTiming[];
      return {
        domContentLoaded: nav.domContentLoadedEventEnd - nav.startTime,
        loadEvent: nav.loadEventEnd - nav.startTime,
        responseEnd: nav.responseEnd - nav.startTime,
      };
    });

    console.log(
      `[gui-perf] connect() wall-clock: ${wallClockMs}ms | navigation: responseEnd=${navTiming.responseEnd.toFixed(1)}ms domContentLoaded=${navTiming.domContentLoaded.toFixed(1)}ms load=${navTiming.loadEvent.toFixed(1)}ms`,
    );

    expect(navTiming.loadEvent).toBeGreaterThan(0);
  });

  for (const rowCount of [100, 1000, 10000]) {
    test(`execute + render timing for a ${rowCount}-row result`, async ({ page, request }) => {
      // Seeding cost (not the measured metric) grows with row count
      // and is itself real evidence of the already-documented write-
      // path serialization (`PHASE_RUBIXDB_PERFORMANCE_BASELINE.md`
      // §5/§7: throughput plateaus ~250-300 req/s regardless of
      // concurrency) -- 10,000 individual INSERTs take real minutes,
      // not a test bug, hence the generous per-case timeout here.
      test.setTimeout(rowCount >= 10000 ? 300_000 : 60_000);
      const apiKey = readAdminKey();
      const table = `gui_perf_${rowCount}`;

      const seedStart = Date.now();
      await seedRows(request, apiKey, table, rowCount);
      console.log(`[gui-perf] seeded ${rowCount} rows in ${Date.now() - seedStart}ms (not the measured metric)`);

      await connect(page, apiKey);
      await page.getByRole("link", { name: "SQL Console" }).click();
      await expect(page.getByRole("heading", { name: "SQL Console" })).toBeVisible();

      const execStart = Date.now();
      await page.getByLabel("SQL").fill(`SELECT id, v, val FROM ${table}`);
      await page.getByRole("button", { name: "Run", exact: true }).click();
      await expect(page.getByText(new RegExp(`Result \\(${rowCount} rows\\)`))).toBeVisible({
        timeout: 60_000,
      });
      const executeAndRenderMs = Date.now() - execStart;

      // Bounded-DOM proof (PHASE_RELATIONAL_FRONTEND_SQL_ARCHITECTURE.md
      // §4): regardless of result size, the number of rendered <tr>
      // elements must stay at the pagination page size, never scale
      // with rowCount.
      const renderedRows = await page.locator(".table-wrap tbody tr").count();

      console.log(
        `[gui-perf] rows=${rowCount} execute+render=${executeAndRenderMs}ms rendered_dom_rows=${renderedRows}`,
      );

      expect(renderedRows).toBeLessThanOrEqual(200);

      // Pagination responsiveness, when there is a next page to click.
      const nextButton = page.getByRole("button", { name: "Next" });
      if (rowCount > 200 && (await nextButton.isEnabled().catch(() => false))) {
        const pageStart = Date.now();
        await nextButton.click();
        await page.waitForTimeout(50); // let the re-render settle
        const pageClickMs = Date.now() - pageStart;
        console.log(`[gui-perf] rows=${rowCount} pagination click-to-settle=${pageClickMs}ms`);
      }
    });
  }
});

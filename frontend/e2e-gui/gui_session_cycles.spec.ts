import { test, expect, type Browser, type BrowserContext } from "@playwright/test";
import fs from "node:fs";
import path from "node:path";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";

// GUI memory / lifecycle trend: repeated open -> connect -> query -> navigate
// every page (including Operations) -> close the whole browser context ->
// reopen, against the real `rubixdb gui` product path. Per cycle it samples
// (a) the JS heap of the page just before closing, (b) the total working set
// of every Playwright-launched Chromium process, (c) the server's own RSS,
// threads and handles from /v1/admin/status. It reports first/last/max and a
// least-squares slope; the assertions are declared sanity bounds, not a claim
// of "no leak" beyond the cycles actually run.

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const API_URL = "http://127.0.0.1:302";
const CREDENTIALS_PATH = path.join(__dirname, "..", ".e2e-gui-data", "default", "credentials.json");
const CYCLES = Number(process.env.GUI_CYCLES ?? 40);

function readAdminKey(): string {
  return (JSON.parse(fs.readFileSync(CREDENTIALS_PATH, "utf-8")) as { admin_key: string }).admin_key;
}

function chromiumWorkingSetMb(): number {
  try {
    const out = execFileSync(
      "powershell.exe",
      [
        "-NoProfile",
        "-Command",
        "$s=0; Get-Process | Where-Object { $_.Path -like '*ms-playwright*' } | ForEach-Object { $s += $_.WorkingSet64 }; [math]::Round($s/1MB,1)",
      ],
      { encoding: "utf-8" },
    );
    return Number(out.trim());
  } catch {
    return NaN;
  }
}

function slope(xs: number[], ys: number[]): number {
  const n = xs.length;
  const mx = xs.reduce((a, b) => a + b, 0) / n;
  const my = ys.reduce((a, b) => a + b, 0) / n;
  let num = 0;
  let den = 0;
  for (let i = 0; i < n; i++) {
    num += (xs[i] - mx) * (ys[i] - my);
    den += (xs[i] - mx) ** 2;
  }
  return den === 0 ? 0 : num / den;
}

async function oneSession(browser: Browser, apiKey: string, i: number): Promise<{ heapMb: number }> {
  const ctx: BrowserContext = await browser.newContext();
  const page = await ctx.newPage();
  await page.goto("/connect");
  await page.getByLabel("API endpoint").fill(API_URL);
  await page.getByLabel("API key").fill(apiKey);
  await page.getByRole("button", { name: "Connect" }).click();
  await expect(page).toHaveURL("/");
  // query
  await page.getByRole("link", { name: "SQL Console" }).click();
  await page.getByLabel("SQL").fill(`SELECT id, v FROM gui_cycle_t WHERE id < ${50 + (i % 50)} ORDER BY id`);
  await page.getByRole("button", { name: "Run", exact: true }).click();
  await expect(page.getByRole("table").first()).toBeVisible({ timeout: 30_000 });
  // navigate every page
  for (const name of ["Catalog", "Snapshots", "Compute", "Monitoring", "Admin", "Home"]) {
    await page.getByRole("link", { name }).click();
    await expect(page.getByRole("heading", { level: 1 })).toBeVisible();
  }
  const heapMb = await page.evaluate(() => {
    const m = (performance as unknown as { memory?: { usedJSHeapSize: number } }).memory;
    return m ? m.usedJSHeapSize / 1048576 : NaN;
  });
  await ctx.close();
  return { heapMb };
}

test.describe("GUI session open/close trend -- real rubixdb gui product path", () => {
  test(`${CYCLES} open/query/navigate/close cycles: browser, API and instance memory trends`, async ({ browser, request }) => {
    test.setTimeout(CYCLES * 20_000 + 60_000);
    const apiKey = readAdminKey();
    const sql = async (stmt: string) => {
      const r = await request.post(`${API_URL}/v1/sql`, { headers: { Authorization: `Bearer ${apiKey}` }, data: { sql: stmt, params: [] } });
      if (!r.ok()) throw new Error(`${stmt} -> ${r.status()} ${await r.text()}`);
    };
    await sql("DROP TABLE IF EXISTS gui_cycle_t");
    await sql("CREATE TABLE gui_cycle_t (id INTEGER PRIMARY KEY, v TEXT)");
    await sql("INSERT INTO gui_cycle_t (id, v) VALUES " + Array.from({ length: 200 }, (_, i) => `(${i}, 'v${i}')`).join(","));

    const rows: { i: number; heapMb: number; browserMb: number; serverRssMb: number; threads: number; handles: number }[] = [];
    for (let i = 1; i <= CYCLES; i++) {
      const { heapMb } = await oneSession(browser, apiKey, i);
      const st = await request.get(`${API_URL}/v1/admin/status`, { headers: { Authorization: `Bearer ${apiKey}` } });
      const j = (await st.json()) as { resources: { rss_bytes: number; threads: number; handles: number }; sessions: { active_transactions: number } };
      rows.push({ i, heapMb, browserMb: chromiumWorkingSetMb(), serverRssMb: j.resources.rss_bytes / 1048576, threads: j.resources.threads, handles: j.resources.handles });
    }
    const half = rows.slice(Math.floor(rows.length / 4));
    const xs = half.map((r) => r.i);
    const summary = {
      cycles: CYCLES,
      first: rows[0],
      last: rows[rows.length - 1],
      max: {
        heapMb: Math.max(...rows.map((r) => r.heapMb)),
        browserMb: Math.max(...rows.map((r) => r.browserMb)),
        serverRssMb: Math.max(...rows.map((r) => r.serverRssMb)),
        threads: Math.max(...rows.map((r) => r.threads)),
        handles: Math.max(...rows.map((r) => r.handles)),
      },
      slopePerCycle: {
        heapMb: slope(xs, half.map((r) => r.heapMb)),
        browserMb: slope(xs, half.map((r) => r.browserMb)),
        serverRssMb: slope(xs, half.map((r) => r.serverRssMb)),
        threads: slope(xs, half.map((r) => r.threads)),
        handles: slope(xs, half.map((r) => r.handles)),
      },
    };
    fs.mkdirSync("test-results", { recursive: true });
    fs.writeFileSync(path.join("test-results", "gui_session_cycles.json"), JSON.stringify({ rows, summary }, null, 1));
    console.log("GUI_SESSION_CYCLES " + JSON.stringify(summary));

    // Declared sanity bounds (reported numbers are the evidence).
    expect(summary.last.serverRssMb - summary.first.serverRssMb).toBeLessThan(25);
    expect(summary.last.handles - summary.first.handles).toBeLessThan(15);
    expect(summary.last.threads - summary.first.threads).toBeLessThan(10);
    expect(summary.last.browserMb - rows[Math.min(4, rows.length - 1)].browserMb).toBeLessThan(250);
    const act = await request.get(`${API_URL}/v1/admin/status`, { headers: { Authorization: `Bearer ${apiKey}` } });
    expect(((await act.json()) as { sessions: { active_transactions: number } }).sessions.active_transactions).toBe(0);
  });
});

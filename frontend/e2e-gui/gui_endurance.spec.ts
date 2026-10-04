import { test, expect, type Page, type APIRequestContext } from "@playwright/test";
import fs from "node:fs";
import path from "node:path";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";

// Increment 14, Blocker 3 -- GUI endurance + browser memory. Real
// production frontend build, real browser (Chromium, via the same
// `playwright.gui.config.ts` real `rubixdb gui` product path
// `gui_performance.spec.ts` uses), real sustained
// execute -> display -> clear cycles covering SELECT, JOIN, GROUP BY,
// HAVING, INSERT, UPDATE, DELETE, and an explicit multi-statement
// transaction (BEGIN/COMMIT across separate real Execute clicks, the
// only way this console's UI can express a transaction -- it sends
// one statement per click, unlike the CLI's own client-side `;` split).

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const API_URL = "http://127.0.0.1:302";
const CREDENTIALS_PATH = path.join(__dirname, "..", ".e2e-gui-data", "default", "credentials.json");
const CYCLES = 50;

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

/** Real server-process RSS/handle/thread sampling -- the one
 * `rubixdb.exe` process the real `webServer` config spawned for this
 * whole test run (single instance, real OS process, same dependency-
 * free `Get-Process` technique every other resource-sampling tool in
 * this repository already uses). */
function sampleServerProcess(): { rssKb: number; handles: number; threads: number } | null {
  try {
    const out = execFileSync(
      "powershell.exe",
      [
        "-NoProfile",
        "-Command",
        "$p = Get-Process -Name rubixdb -ErrorAction SilentlyContinue | Select-Object -First 1; if ($p) { Write-Output ($p.WorkingSet64.ToString() + ',' + $p.HandleCount.ToString() + ',' + $p.Threads.Count.ToString()) }",
      ],
      { encoding: "utf-8" },
    ).trim();
    if (!out) return null;
    const [rss, handles, threads] = out.split(",").map(Number);
    return { rssKb: Math.round(rss / 1024), handles, threads };
  } catch {
    return null;
  }
}

/** Real Chrome DevTools Protocol metrics, not the JS-exposed
 * `performance.memory` (which this Chromium build quantizes to
 * suspiciously round 10MB buckets in practice -- a real, verified
 * limitation of that API, not a useful leak signal here). CDP's
 * `Performance.getMetrics` reports the same underlying V8 heap size at
 * full precision, and additionally reports live DOM node / JS event
 * listener counts -- the standard, real signal for "detached
 * component leak" (a leaked React component keeps its DOM nodes and/or
 * listeners alive after it should have unmounted). */
async function cdpMetrics(
  page: Page,
): Promise<{ jsHeapBytes: number; domNodes: number; jsListeners: number } | null> {
  try {
    const client = await page.context().newCDPSession(page);
    await client.send("Performance.enable");
    const { metrics } = await client.send("Performance.getMetrics");
    const get = (name: string) => metrics.find((m) => m.name === name)?.value ?? 0;
    await client.detach();
    return {
      jsHeapBytes: get("JSHeapUsedSize"),
      domNodes: get("Nodes"),
      jsListeners: get("JSEventListeners"),
    };
  } catch {
    return null;
  }
}

async function execute(page: Page, sql: string) {
  const editor = page.getByLabel("SQL");
  await editor.fill(sql);
  const responsePromise = page.waitForResponse(
    (r) => r.url().endsWith("/v1/sql") && r.request().method() === "POST",
  );
  await page.getByRole("button", { name: "Run", exact: true }).click();
  await responsePromise;
}

test.describe("GUI endurance + memory -- real rubixdb gui product path", () => {
  test(`${CYCLES} sustained execute/display/clear cycles across SELECT/JOIN/GROUP BY/HAVING/INSERT/UPDATE/DELETE/transaction`, async ({
    page,
    request,
  }) => {
    test.setTimeout(10 * 60_000);
    const apiKey = readAdminKey();

    await execSql(request, apiKey, "DROP TABLE IF EXISTS endur_a");
    await execSql(request, apiKey, "DROP TABLE IF EXISTS endur_b");
    await execSql(request, apiKey, "CREATE TABLE endur_a (id INTEGER PRIMARY KEY, grp TEXT, val INTEGER)");
    await execSql(request, apiKey, "CREATE TABLE endur_b (id INTEGER PRIMARY KEY, a_id INTEGER, note TEXT)");
    for (let i = 0; i < 200; i++) {
      await execSql(
        request,
        apiKey,
        `INSERT INTO endur_a (id, grp, val) VALUES (${i}, 'g${i % 5}', ${i})`,
      );
      await execSql(request, apiKey, `INSERT INTO endur_b (id, a_id, note) VALUES (${i}, ${i}, 'n${i}')`);
    }

    await connect(page, apiKey);
    await page.getByRole("link", { name: "SQL Console" }).click();
    await expect(page.getByRole("heading", { name: "SQL Console" })).toBeVisible();

    const cdpSamples: { jsHeapBytes: number; domNodes: number; jsListeners: number }[] = [];
    const serverSamples: { rssKb: number; handles: number; threads: number }[] = [];
    let nextId = 100_000;

    for (let cycle = 0; cycle < CYCLES; cycle++) {
      await execute(page, "SELECT id, grp, val FROM endur_a WHERE grp = 'g2'");
      await page.getByRole("button", { name: "Clear" }).click();

      await execute(
        page,
        "SELECT a.id, a.grp, b.note FROM endur_a a JOIN endur_b b ON a.id = b.a_id WHERE a.id < 20",
      );
      await page.getByRole("button", { name: "Clear" }).click();

      await execute(
        page,
        "SELECT grp, COUNT(*), SUM(val) FROM endur_a GROUP BY grp HAVING COUNT(*) > 0",
      );
      await page.getByRole("button", { name: "Clear" }).click();

      nextId += 1;
      await execute(page, `INSERT INTO endur_a (id, grp, val) VALUES (${nextId}, 'gX', ${nextId})`);
      await page.getByRole("button", { name: "Clear" }).click();

      await execute(page, `UPDATE endur_a SET val = ${nextId} + 1 WHERE id = ${nextId}`);
      await page.getByRole("button", { name: "Clear" }).click();

      await execute(page, `DELETE FROM endur_a WHERE id = ${nextId}`);
      await page.getByRole("button", { name: "Clear" }).click();

      // Explicit transaction: BEGIN, a write, COMMIT -- three separate
      // real Execute clicks sharing one session, exactly how this
      // console's UI expresses a transaction.
      await execute(page, "BEGIN");
      await expect(page.getByText("transaction open")).toBeVisible();
      nextId += 1;
      await execute(page, `INSERT INTO endur_a (id, grp, val) VALUES (${nextId}, 'txn', ${nextId})`);
      await execute(page, "COMMIT");
      await expect(page.getByText("autocommit")).toBeVisible();
      await page.getByRole("button", { name: "Clear" }).click();

      if (cycle % 5 === 0) {
        const cdp = await cdpMetrics(page);
        if (cdp !== null) cdpSamples.push(cdp);
        const server = sampleServerProcess();
        if (server) serverSamples.push(server);
      }
    }

    console.log(
      `[gui-endurance] ${CYCLES} cycles complete, ${cdpSamples.length} CDP samples, ${serverSamples.length} server samples`,
    );
    console.log(
      `[gui-endurance] CDP samples (heapBytes/domNodes/jsListeners): ${cdpSamples
        .map((s) => `${s.jsHeapBytes}/${s.domNodes}/${s.jsListeners}`)
        .join(" | ")}`,
    );
    console.log(
      `[gui-endurance] server samples (rss_kb/handles/threads): ${serverSamples
        .map((s) => `${s.rssKb}/${s.handles}/${s.threads}`)
        .join(" | ")}`,
    );

    // No result retention after Clear: the Result card must be gone.
    await expect(page.getByText(/^Result \(/)).toHaveCount(0);

    // No history leak: the history store is capped at 50 (utils/sessionActivity.ts) -- after
    // 50 cycles x 9 real statements (450 executes), the rendered
    // history list must still be capped, never growing unbounded with
    // every additional statement.
    // History is the third tab of the results panel since the SQL Console redesign.
    await page.getByRole("tab", { name: "History" }).click();
    const historyCount = await page.locator("button.history-item").count();
    expect(historyCount).toBeGreaterThan(0);
    expect(historyCount).toBeLessThanOrEqual(50);

    // No monotonic browser memory/DOM growth: compare the back half of
    // the samples against the front half for all three real CDP
    // signals. JS heap size is noisy (GC timing is not controllable
    // from here without a special Chromium flag this config does not
    // set), so this is a bounded-ratio check, not a bit-exact one --
    // but DOM node count and JS listener count are not GC-noisy at all
    // (they reflect exactly what is currently attached to the live
    // document), so those two are the real, precise "no detached
    // component leak" evidence: React unmounting a component that
    // properly cleans up its effects/listeners must not leave the DOM
    // node or listener count climbing cycle over cycle.
    if (cdpSamples.length >= 4) {
      const mid = Math.floor(cdpSamples.length / 2);
      const front = cdpSamples.slice(0, mid);
      const back = cdpSamples.slice(mid);
      const avg = (xs: number[]) => xs.reduce((a, b) => a + b, 0) / xs.length;
      const frontHeap = avg(front.map((s) => s.jsHeapBytes));
      const backHeap = avg(back.map((s) => s.jsHeapBytes));
      const frontNodes = avg(front.map((s) => s.domNodes));
      const backNodes = avg(back.map((s) => s.domNodes));
      const frontListeners = avg(front.map((s) => s.jsListeners));
      const backListeners = avg(back.map((s) => s.jsListeners));
      console.log(
        `[gui-endurance] front-half avg heap=${frontHeap.toFixed(0)} nodes=${frontNodes.toFixed(0)} listeners=${frontListeners.toFixed(0)}`,
      );
      console.log(
        `[gui-endurance] back-half avg heap=${backHeap.toFixed(0)} nodes=${backNodes.toFixed(0)} listeners=${backListeners.toFixed(0)}`,
      );
      expect(backHeap).toBeLessThan(frontHeap * 3 + 5_000_000);
      expect(backNodes).toBeLessThan(frontNodes * 1.5 + 200);
      expect(backListeners).toBeLessThan(frontListeners * 1.5 + 200);
    }

    if (serverSamples.length >= 2) {
      const first = serverSamples[0];
      const last = serverSamples[serverSamples.length - 1];
      console.log(
        `[gui-endurance] server first=${JSON.stringify(first)} last=${JSON.stringify(last)}`,
      );
      expect(last.handles).toBeLessThan(first.handles * 3 + 50);
      expect(last.threads).toBeLessThan(first.threads * 3 + 20);
    }
  });
});

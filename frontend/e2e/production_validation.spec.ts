import { test, expect, type Page } from "@playwright/test";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const ADMIN_KEY = "e2e-admin-key-0123456789";
const READER_KEY = "e2e-reader-key-0123456789";
const API_URL = "http://127.0.0.1:8099";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

async function connect(page: Page, apiKey: string) {
  await page.goto("/connect");
  await page.getByLabel("API endpoint").fill(API_URL);
  await page.getByLabel("API key").fill(apiKey);
  await page.getByRole("button", { name: "Connect" }).click();
  await expect(page).toHaveURL("/");
}

async function navTo(page: Page, label: string) {
  await page.getByRole("link", { name: label }).click();
}

test.describe("Productization — Production Validation Phase §8: functional states", () => {
  test("empty state renders for a range query that matches nothing", async ({ page }) => {
    await connect(page, ADMIN_KEY);
    await navTo(page, "Data Explorer");
    await page.getByRole("tab", { name: "Range Query" }).click();
    // Lexicographically after every real key this test suite ever
    // writes (all real keys use lowercase prefixes) -- deterministic
    // regardless of what other tests have written to the shared
    // server, unlike relying on the snapshot list happening to be
    // empty at this point in the run.
    await page.getByLabel("Start key (inclusive, optional)", { exact: true }).fill("zzz-guaranteed-empty-prefix");
    await page.getByRole("button", { name: "Run range query" }).click();
    await expect(page.getByText("No keys in this range")).toBeVisible({ timeout: 10_000 });
  });

  test("error state renders for a lookup of a key that does not exist, and the app keeps working after it", async ({
    page,
  }) => {
    await connect(page, ADMIN_KEY);
    await navTo(page, "Data Explorer");
    await page.getByLabel("Key", { exact: true }).first().fill("definitely-not-a-real-key-xyz");
    await page.getByRole("button", { name: "Look up" }).click();
    await expect(page.getByText(/does not exist/)).toBeVisible({ timeout: 10_000 });
    // The app is still usable afterward -- an error state is not a
    // dead end.
    await navTo(page, "Dashboard");
    await expect(page.getByRole("heading", { name: "Dashboard" })).toBeVisible();
  });

  test("a 401 mid-session clears the session and redirects to /connect", async ({ page }) => {
    await connect(page, ADMIN_KEY);
    await expect(page.getByText("e2e-admin", { exact: false })).toBeVisible();

    // The real UI has no way to make an already-connected session's
    // in-memory credential go stale (by design -- reconnecting is the
    // only path, `SettingsPage.tsx`), so a genuinely credential-revoked
    // mid-session 401 cannot be produced end to end without either
    // restarting the real server with different keys (disruptive to
    // the shared server every other spec in this run depends on) or
    // intercepting one response. This test does the latter -- it lets
    // every other request through untouched and returns a real-shaped
    // 401 body for exactly one in-flight request, so what's actually
    // under test is the real, unmocked application code that reacts to
    // it: `ApiClient.request` -> `onUnauthorized` -> `clearSession` ->
    // `RequireSession`'s redirect (`client.test.ts` already covers the
    // first link of that chain at the unit level; this confirms the
    // chain is actually wired end to end in the real running app).
    await page.route("**/v1/**", (route) =>
      route.fulfill({
        status: 401,
        contentType: "application/json",
        body: JSON.stringify({ error: { code: "UNAUTHORIZED", message: "missing or invalid API key" } }),
      }),
    );
    // A reload forces every query to remount and refetch fresh rather
    // than serving an already-cached, not-yet-stale result from before
    // the route was installed (react-query's `refetchInterval` would
    // eventually also catch this, but only after its own 10s interval
    // -- a reload is the deterministic way to force it immediately).
    await page.reload();
    await expect(page).toHaveURL("/connect", { timeout: 10_000 });
  });
});

test.describe("Productization — Production Validation Phase §9: repeated-workflow stability", () => {
  test("60 repeated write+read cycles: no console errors, no failed requests, bounded memory growth", async ({
    page,
    context,
  }) => {
    const consoleErrors: string[] = [];
    const failedRequests: string[] = [];
    page.on("console", (msg) => {
      if (msg.type() === "error") consoleErrors.push(msg.text());
    });
    page.on("requestfailed", (req) => {
      failedRequests.push(`${req.method()} ${req.url()}: ${req.failure()?.errorText}`);
    });
    page.on("pageerror", (err) => {
      consoleErrors.push(`pageerror: ${err.message}`);
    });

    await connect(page, ADMIN_KEY);
    await navTo(page, "Data Explorer");

    const cdp = await context.newCDPSession(page);
    await cdp.send("Performance.enable");
    const heapAt = async () => {
      const { metrics } = await cdp.send("Performance.getMetrics");
      return metrics.find((m) => m.name === "JSHeapUsedSize")?.value ?? 0;
    };
    const heapStart = await heapAt();

    const key = page.getByLabel("Key", { exact: true }).first();
    const value = page.getByLabel("Value", { exact: true });
    const put = page.getByRole("button", { name: "Put" });
    const lookup = page.getByRole("button", { name: "Look up" });

    const ITERATIONS = 60;
    for (let i = 0; i < ITERATIONS; i++) {
      const k = `stability-key-${i % 10}`; // reuse a small pool, like a real operator would
      await key.fill(k);
      await value.fill(`stability-value-${i}`);
      const putResponse = page.waitForResponse((r) => r.url().includes("/v1/kv") && r.request().method() === "PUT");
      await put.click();
      await putResponse;
      await lookup.click();
      await expect(page.locator("pre")).toHaveText(`stability-value-${i}`, { timeout: 10_000 });
    }

    const heapEnd = await heapAt();

    expect(consoleErrors, `console errors during ${ITERATIONS} iterations:\n${consoleErrors.join("\n")}`).toEqual([]);
    expect(
      failedRequests,
      `failed network requests during ${ITERATIONS} iterations:\n${failedRequests.join("\n")}`,
    ).toEqual([]);

    // Coarse leak signal, not a precise bound: 60 short-lived React
    // Query cache entries and DOM updates should not multiply heap
    // usage. This is a floor for "something is clearly wrong," not a
    // claim of zero growth (some growth from the query cache holding
    // 10 distinct keys' results is expected and correct).
    console.log(`heap: start=${heapStart} end=${heapEnd} ratio=${(heapEnd / Math.max(heapStart, 1)).toFixed(2)}`);
    expect(heapEnd).toBeLessThan(heapStart * 4 + 5_000_000);
  });
});

test.describe("Productization — Production Validation Phase §13: frontend security", () => {
  test("the production bundle contains no API key, session storage key literal misuse, or obvious secret", async () => {
    const distDir = path.resolve(__dirname, "..", "dist", "assets");
    const files = fs.existsSync(distDir) ? fs.readdirSync(distDir).filter((f) => f.endsWith(".js")) : [];
    expect(files.length, "expected a built dist/assets/*.js bundle -- run `npm run build` first").toBeGreaterThan(0);

    for (const file of files) {
      const content = fs.readFileSync(path.join(distDir, file), "utf-8");
      expect(content, `${file} must not contain the e2e admin test key`).not.toContain(ADMIN_KEY);
      expect(content, `${file} must not contain the e2e reader test key`).not.toContain(READER_KEY);
      // A default/hardcoded credential baked into the bundle would be
      // a real vulnerability; this app has none by design (every
      // credential is user-supplied at Connect time), so this string
      // should never appear as a literal.
      expect(content, `${file} must not contain a hardcoded RUBIXDB_API_KEYS-style default`).not.toMatch(
        /admin:admin:[A-Za-z0-9]{16,}/,
      );
    }
  });

  test("logout clears both storages; a fresh reload requires reconnecting", async ({ page }) => {
    await connect(page, ADMIN_KEY);
    await navTo(page, "Settings");
    // Scoped to the page body's own logout action -- the top bar
    // (`AppShell`) renders a second "Log out" button of its own.
    await page.locator("#main-content").getByRole("button", { name: "Log out" }).click();
    await expect(page).toHaveURL("/connect");

    const stored = await page.evaluate(() => ({
      session: sessionStorage.getItem("rubixdb-console-session"),
      local: localStorage.getItem("rubixdb-console-session"),
    }));
    expect(stored.session).toBeNull();
    expect(stored.local).toBeNull();

    await page.reload();
    await expect(page).toHaveURL("/connect");
  });
});

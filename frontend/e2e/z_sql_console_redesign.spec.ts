import { test, expect, type Page } from "@playwright/test";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

// NOTE: named z_* so it runs after workflow.spec.ts: this file bulk-inserts rows into the shared engine, and
// workflow.spec.ts looks a key up in a paginated raw key listing that extra keys would push off page one.
//
// SQL Console redesign -- real browser, real API (see playwright.config.ts).
// The only interception is a delay on /v1/sql, to hold a query "in flight" long
// enough to prove Stop; every result below comes from the real engine.

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const axePath = path.join(__dirname, "..", "node_modules", "axe-core", "axe.min.js");
const ADMIN_KEY = "e2e-admin-key-0123456789";
const API_URL = "http://127.0.0.1:8099";

async function connect(page: Page) {
  await page.goto("/connect");
  await page.getByLabel("API endpoint").fill(API_URL);
  await page.getByLabel("API key").fill(ADMIN_KEY);
  await page.getByRole("button", { name: "Connect" }).click();
  await expect(page).toHaveURL("/");
  await page.getByRole("link", { name: "SQL Console" }).click();
  await expect(page).toHaveURL("/sql");
}

const run = (page: Page) => page.getByRole("button", { name: "Run", exact: true });
const editor = (page: Page) => page.getByLabel("SQL");

async function exec(page: Page, sql: string) {
  await editor(page).fill(sql);
  await run(page).click();
  await expect(run(page)).toBeEnabled();
}

function errorsOf(page: Page) {
  const errors: string[] = [];
  page.on("console", (m) => {
    if (m.type() === "error") errors.push(m.text());
  });
  page.on("pageerror", (e) => errors.push(String(e)));
  return errors;
}

test.describe("SQL Console", () => {
  test("worksheet tabs: create, switch, close, and restore after reload (sessionStorage only)", async ({ page }) => {
    const errors = errorsOf(page);
    await connect(page);
    const tabs = page.getByRole("tablist", { name: "Worksheets" });
    await expect(tabs.getByRole("tab")).toHaveCount(1);

    await editor(page).fill("SELECT 1 -- first");
    await page.getByRole("button", { name: "Add worksheet" }).click();
    await expect(tabs.getByRole("tab")).toHaveCount(2);
    await expect(editor(page)).toHaveValue("");
    await editor(page).fill("SELECT 2 -- second");

    // The unsaved dot marks text that has not been run.
    await expect(tabs.getByRole("tab", { name: /Untitled 2/ })).toContainText("edited since last run");

    await tabs.getByRole("tab", { name: /Untitled 1/ }).click();
    await expect(editor(page)).toHaveValue("SELECT 1 -- first");

    // Autosave (debounced), then a reload restores both tabs and the active one.
    await page.waitForTimeout(700);
    await page.reload();
    await expect(page.getByRole("tablist", { name: "Worksheets" }).getByRole("tab")).toHaveCount(2);
    await expect(editor(page)).toHaveValue("SELECT 1 -- first");
    const stored = await page.evaluate(() => ({
      session: window.sessionStorage.getItem("rubixdb-console-worksheets"),
      localKeys: Object.keys(window.localStorage),
    }));
    expect(stored.session).toContain("SELECT 2 -- second");
    expect(stored.localKeys.filter((k) => /worksheet/i.test(k))).toEqual([]);
    expect(page.url()).not.toContain("?");

    // Mouse: the x on the tab. Keyboard: Delete on a focused tab. Menu: "Close worksheet".
    await tabs.getByRole("tab", { name: /Untitled 1/ }).locator(".ws-close").click();
    await expect(tabs.getByRole("tab")).toHaveCount(1);
    await expect(editor(page)).toHaveValue("SELECT 2 -- second");
    // Closing the last tab leaves a fresh empty one rather than no editor.
    await page.getByRole("button", { name: "More actions" }).click();
    await page.getByRole("menuitem", { name: "Close worksheet" }).click();
    await expect(tabs.getByRole("tab")).toHaveCount(1);
    await expect(editor(page)).toHaveValue("");
    await page.getByRole("button", { name: "Add worksheet" }).click();
    await tabs.getByRole("tab", { selected: true }).focus();
    await page.keyboard.press("Delete");
    await expect(tabs.getByRole("tab")).toHaveCount(1);
    expect(errors, errors.join("\n")).toEqual([]);
  });

  test("Run executes a query and the results grid renders; Ctrl+Enter also runs; no Run all (single statement only)", async ({
    page,
  }) => {
    const errors = errorsOf(page);
    await connect(page);
    await expect(page.getByRole("button", { name: /Run all/ })).toHaveCount(0);
    await expect(page.getByRole("button", { name: /Format/ })).toHaveCount(0);

    await exec(page, "SELECT 1 AS one, 'two' AS two");
    const results = page.getByRole("tabpanel", { name: "Result view" });
    await expect(results.getByText("Result (1 row)")).toBeVisible();
    await expect(results.getByRole("columnheader", { name: /ONE\s*integer/i })).toBeVisible();
    await expect(results.getByRole("cell", { name: "two" })).toBeVisible();
    await expect(page.locator(".sqlc-status")).toContainText("Ready");
    await expect(page.locator(".sqlc-status")).toContainText(/Last run \d/);

    await editor(page).fill("SELECT 7 AS seven");
    await editor(page).press("Control+Enter");
    await expect(results.getByRole("cell", { name: "7" })).toBeVisible();

    // The API accepts exactly one statement per request, so a second one is refused (and why Run all is absent).
    await exec(page, "SELECT 1; SELECT 2");
    await expect(page.getByRole("alert")).toContainText(/RESOURCE_LIMIT|exactly one statement/);
    expect(errors.filter((e) => !/status of 4\d\d/.test(e)), errors.join("\n")).toEqual([]);
  });

  test("Stop is available only while a query is in flight and cancels it cleanly", async ({ page }) => {
    await connect(page);
    await page.route("**/v1/sql", async (route) => {
      await new Promise((r) => setTimeout(r, 4000));
      await route.continue().catch(() => {});
    });
    await expect(page.getByRole("button", { name: "Stop" })).toHaveCount(0);
    await editor(page).fill("SELECT 1");
    await run(page).click();
    const stop = page.getByRole("button", { name: "Stop" });
    await expect(stop).toBeEnabled();
    await expect(run(page)).toBeDisabled();
    await expect(page.locator(".sqlc-status")).toContainText("Running…");
    await stop.click();
    await expect(page.getByRole("alert")).toHaveText("Cancelled.");
    await expect(stop).toHaveCount(0);
    await expect(run(page)).toBeEnabled();
    await expect(page.locator(".sqlc-status")).toContainText("Failed");
    await page.unroute("**/v1/sql");
    await exec(page, "SELECT 2 AS after_stop");
    await expect(page.getByRole("cell", { name: "2" })).toBeVisible();
  });

  test("pagination: Prev/Next move through the fetched rows, and rows-per-page re-pages client-side", async ({
    page,
  }) => {
    await connect(page);
    const table = `pg_${Date.now()}`;
    await exec(page, `CREATE TABLE ${table} (id INTEGER PRIMARY KEY, v TEXT)`);
    const values = Array.from({ length: 250 }, (_, i) => `(${i + 1}, 'row${i + 1}')`).join(",");
    await exec(page, `INSERT INTO ${table} (id, v) VALUES ${values}`);
    await exec(page, `SELECT id, v FROM ${table} ORDER BY id`);

    const results = page.getByRole("tabpanel", { name: "Result view" });
    await expect(results.getByText("Result (250 rows)")).toBeVisible();
    await expect(results.getByText("Showing 1–200 of 250 rows")).toBeVisible();
    await expect(results.locator("tbody tr")).toHaveCount(200);
    await expect(results.getByRole("button", { name: "Prev" })).toBeDisabled();

    let requests = 0;
    page.on("request", (r) => {
      if (r.url().endsWith("/v1/sql")) requests += 1;
    });
    await results.getByRole("button", { name: "Next" }).click();
    await expect(results.getByText("Showing 201–250 of 250 rows")).toBeVisible();
    await expect(results.locator("tbody tr")).toHaveCount(50);
    await expect(results.locator("tbody tr").first().locator("td").first()).toHaveText("201");
    await expect(results.getByRole("button", { name: "Next" })).toBeDisabled();
    await results.getByRole("button", { name: "Prev" }).click();
    await expect(results.getByText("Showing 1–200 of 250 rows")).toBeVisible();

    await results.getByLabel("Rows per page").selectOption("100");
    await expect(results.getByText("Showing 1–100 of 250 rows")).toBeVisible();
    await expect(results.locator("tbody tr")).toHaveCount(100);
    await results.getByLabel("Rows per page").selectOption("500");
    await expect(results.getByText("Showing 1–250 of 250 rows")).toBeVisible();
    expect(requests, "paging must not re-query the API").toBe(0);

    // The grid scrolls inside its panel; the page itself does not.
    const pageScroll = await page.evaluate(() => ({
      doc: document.documentElement.scrollHeight - document.documentElement.clientHeight,
      main: (() => {
        const m = document.querySelector(".app-content") as HTMLElement;
        return m.scrollHeight - m.clientHeight;
      })(),
    }));
    expect(pageScroll.doc).toBeLessThanOrEqual(1);
    expect(pageScroll.main).toBeLessThanOrEqual(1);
    await exec(page, `DROP TABLE ${table}`);
  });

  test("empty and error states: 'No rows returned', and only the API's sanitized message", async ({ page }) => {
    await connect(page);
    await exec(page, "SELECT 1 AS x WHERE 1 = 0");
    await expect(page.getByText("No rows returned")).toBeVisible();
    await expect(page.getByText("Result (0 rows)")).toBeVisible();

    await exec(page, "SELEKT nonsense");
    const alert = page.getByRole("alert");
    await expect(alert).toBeVisible();
    const text = (await alert.textContent()) ?? "";
    expect(text).toMatch(/^[A-Z_]+: /);
    expect(text).not.toMatch(/stack|at \w+\.|[A-Za-z]:\\|\/src\/|\.rs\b|panicked|Error:/);
    await expect(page.locator(".sqlc-status")).toContainText("Failed");
    // The failure is also in the Query details tab, with the statement.
    await page.getByRole("tab", { name: "Query details" }).click();
    await expect(page.getByText("Failed", { exact: true }).first()).toBeVisible();
    await expect(page.locator(".details-sql")).toHaveText("SELEKT nonsense");
  });

  test("Results / Query details / History tabs switch; history loads a statement back into the editor", async ({
    page,
  }) => {
    await connect(page);
    await exec(page, "SELECT 42 AS answer");
    const views = page.getByRole("tablist", { name: "Result views" });
    for (const t of ["Results", "Query details", "History"]) {
      await views.getByRole("tab", { name: t }).click();
      await expect(views.getByRole("tab", { name: t })).toHaveAttribute("aria-selected", "true");
    }
    await views.getByRole("tab", { name: "Query details" }).click();
    const details = page.getByRole("tabpanel", { name: "Result view" });
    await expect(details.getByText("Succeeded")).toBeVisible();
    await expect(details.getByText("Rows returned")).toBeVisible();
    await expect(details.getByText("Round-trip time")).toBeVisible();
    await expect(details.locator(".details-sql")).toHaveText("SELECT 42 AS answer");
    await expect(details.getByText(/bytes scanned/i)).toHaveCount(0);

    await views.getByRole("tab", { name: "History" }).click();
    await editor(page).fill("");
    const item = page.getByRole("button", { name: /SELECT 42 AS answer/ });
    await expect(item).toBeVisible();
    await expect(item).toContainText("Succeeded");
    await item.click();
    await expect(editor(page)).toHaveValue("SELECT 42 AS answer");
  });

  test("hostile data is inert: a stored <img onerror> value and a hostile column alias render as text", async ({ page }) => {
    const dialogs: string[] = [];
    page.on("dialog", async (d) => {
      dialogs.push(d.message());
      await d.dismiss();
    });
    await connect(page);
    const table = `xssc_${Date.now()}`;
    const payload = "<img src=x onerror=alert(1)>";
    await exec(page, `CREATE TABLE ${table} (id INTEGER PRIMARY KEY, v TEXT)`);
    await exec(page, `INSERT INTO ${table} (id, v) VALUES (1, '${payload}')`);
    await exec(page, `SELECT * FROM ${table}`);
    const results = page.getByRole("tabpanel", { name: "Result view" });
    const cell = results.getByRole("cell", { name: payload });
    await expect(cell).toBeVisible();
    await expect(cell).toHaveText(payload);
    await exec(page, `SELECT id AS "${payload}" FROM ${table}`);
    await expect(results.getByRole("columnheader", { name: new RegExp(payload.replace(/[()]/g, "\\$&"), "i") })).toBeVisible();
    // The same payload typed into the editor / shown in History is text too.
    await editor(page).fill(payload);
    await page.getByRole("tab", { name: "History" }).click();
    await page.waitForTimeout(300);
    expect(await page.evaluate(() => document.querySelectorAll("img[src='x'], .grid img, .history img, .sqled img").length)).toBe(0);
    expect(await page.evaluate(() => document.querySelectorAll("[onerror]").length)).toBe(0);
    expect(dialogs).toEqual([]);
    await exec(page, `DROP TABLE ${table}`);
  });

  test("overflow menu: Copy query and Download results (CSV) are real; hostile text cells cannot become formulas", async ({
    page,
    context,
  }) => {
    await context.grantPermissions(["clipboard-read", "clipboard-write"]);
    await connect(page);
    const table = `csv_${Date.now()}`;
    await exec(page, `CREATE TABLE ${table} (id INTEGER PRIMARY KEY, v TEXT)`);
    await exec(page, `INSERT INTO ${table} (id, v) VALUES (1, '=1+1'), (2, 'plain'), (3, NULL)`);
    await exec(page, `SELECT id, v FROM ${table} ORDER BY id`);

    await page.getByRole("button", { name: "More actions" }).click();
    await expect(page.getByRole("menuitem")).toHaveText(["Copy query", "Download results (CSV)", "Close worksheet"]);
    const [download] = await Promise.all([
      page.waitForEvent("download"),
      page.getByRole("menuitem", { name: "Download results (CSV)" }).click(),
    ]);
    const file = await download.path();
    const csv = fs.readFileSync(file!, "utf-8");
    expect(csv).toBe("id,v\r\n1,'=1+1\r\n2,plain\r\n3,\r\n");

    await page.getByRole("button", { name: "More actions" }).click();
    await page.getByRole("menuitem", { name: "Copy query" }).click();
    expect(await page.evaluate(() => navigator.clipboard.readText())).toBe(`SELECT id, v FROM ${table} ORDER BY id`);

    // Escape closes the menu and returns focus to its button.
    await page.getByRole("button", { name: "More actions" }).click();
    await page.keyboard.press("Escape");
    await expect(page.getByRole("menu")).toHaveCount(0);
    await expect(page.getByRole("button", { name: "More actions" })).toBeFocused();
    await exec(page, `DROP TABLE ${table}`);
  });

  test("editor: line numbers, line:column + character count, Ctrl+/ comments, resize handle, highlighted tokens", async ({
    page,
  }) => {
    await connect(page);
    await editor(page).fill("SELECT 1\nFROM t");
    await expect(page.locator(".sqled-gutter")).toHaveText("1\n2");
    await editor(page).press("Control+End");
    await expect(page.locator(".sqled-footer")).toContainText("Ln 2, Col 7");
    await expect(page.locator(".sqled-footer")).toContainText("15 chars");
    await expect(page.locator(".sqled-footer")).toContainText("autocommit");
    await expect(page.locator(".sqled-hl .tok-kw").first()).toHaveText("SELECT");

    await editor(page).press("Control+/");
    await expect(editor(page)).toHaveValue("SELECT 1\n-- FROM t");
    await expect(page.locator(".sqled-hl .tok-com")).toHaveText("-- FROM t");
    await editor(page).press("Control+/");
    await expect(editor(page)).toHaveValue("SELECT 1\nFROM t");

    const handle = page.getByRole("separator", { name: "Resize editor" });
    const before = Number(await handle.getAttribute("aria-valuenow"));
    await handle.focus();
    await page.keyboard.press("ArrowDown");
    expect(Number(await handle.getAttribute("aria-valuenow"))).toBeGreaterThan(before);
    const box = await page.locator(".sqled").boundingBox();
    expect(Math.round(box!.height)).toBe(before + 24);
  });

  test("session chips show real values; transaction state flips the pill; the page does not scroll", async ({ page }) => {
    await connect(page);
    const chips = page.getByRole("group", { name: "Session context" });
    await expect(chips).toContainText("Database default");
    await expect(chips).toContainText("Role admin");
    await expect(chips).toContainText(/Schema (public|—)/);
    await exec(page, "BEGIN");
    await expect(page.getByText("transaction open")).toBeVisible();
    await exec(page, "ROLLBACK");
    await expect(page.getByText("autocommit")).toBeVisible();
    const over = await page.evaluate(() => document.documentElement.scrollHeight - document.documentElement.clientHeight);
    expect(over).toBeLessThanOrEqual(1);
  });

  test("axe-core: the redesigned page has no violations in its main states", async ({ page }) => {
    await connect(page);
    await exec(page, "SELECT 1 AS a, 'x' AS b");
    await page.addScriptTag({ path: axePath });
    const audit = async (label: string) => {
      const r = await page.evaluate(() =>
        (window as unknown as { axe: { run: () => Promise<{ violations: unknown[] }> } }).axe.run(),
      );
      expect(r.violations, `${label}: ${JSON.stringify(r.violations, null, 2)}`).toEqual([]);
    };
    await audit("results");
    await page.getByRole("tab", { name: "Query details" }).click();
    await audit("details");
    await page.getByRole("tab", { name: "History" }).click();
    await audit("history");
    await page.getByRole("button", { name: "Add worksheet" }).click();
    await editor(page).fill("SELECT 'a' -- c\n/* b */ FROM \"q\" WHERE 1 > 0");
    await page.getByRole("button", { name: "More actions" }).click();
    await audit("menu open + highlighted editor");
    await exec(page, "SELEKT");
    await audit("error state");
  });
});

import { test, expect, type Page } from "@playwright/test";

const ADMIN_KEY = "e2e-admin-key-0123456789";
const API_URL = "http://127.0.0.1:8099";

async function connect(page: Page, apiKey: string) {
  await page.goto("/connect");
  await expect(page.getByRole("heading", { name: "RubiXDB Console" })).toBeVisible();
  await page.getByLabel("API endpoint").fill(API_URL);
  await page.getByLabel("API key").fill(apiKey);
  await page.getByRole("button", { name: "Connect" }).click();
  await expect(page).toHaveURL("/");
}

async function runSql(page: Page, sql: string) {
  await page.getByLabel("SQL").fill(sql);
  await page.getByRole("button", { name: "Run", exact: true }).click();
}

test.describe("SQL console — real backend, real rubixdb-sql engine", () => {
  test("CREATE TABLE, INSERT, SELECT, UPDATE, DELETE, CREATE INDEX, GROUP BY, HAVING, BEGIN/COMMIT/ROLLBACK", async ({
    page,
  }) => {
    const table = `e2e_sql_${Date.now()}`;
    await connect(page, ADMIN_KEY);
    await page.getByRole("link", { name: "SQL Console" }).click();
    await expect(page.getByRole("heading", { name: "SQL Console" })).toBeVisible();
    await expect(page.getByText("autocommit")).toBeVisible();

    // CREATE TABLE
    await runSql(page, `CREATE TABLE ${table} (id INTEGER PRIMARY KEY, grp TEXT, val INTEGER)`);
    await expect(page.getByText("OK.")).toBeVisible({ timeout: 10_000 });

    // INSERT
    await runSql(page, `INSERT INTO ${table} (id, grp, val) VALUES (1, 'a', 10)`);
    await expect(page.getByText(/INSERT.*1 row/)).toBeVisible({ timeout: 10_000 });
    await runSql(page, `INSERT INTO ${table} (id, grp, val) VALUES (2, 'a', 20)`);
    await runSql(page, `INSERT INTO ${table} (id, grp, val) VALUES (3, 'b', 5)`);

    // SELECT
    await runSql(page, `SELECT id, grp, val FROM ${table} ORDER BY id`);
    await expect(page.getByText(/Result \(3 rows\)/)).toBeVisible({ timeout: 10_000 });
    await expect(page.locator(".table-wrap").getByText("a").first()).toBeVisible();

    // UPDATE
    await runSql(page, `UPDATE ${table} SET val = 99 WHERE id = 1`);
    await expect(page.getByText(/UPDATE.*1 row/)).toBeVisible({ timeout: 10_000 });
    await runSql(page, `SELECT val FROM ${table} WHERE id = 1`);
    await expect(page.locator(".table-wrap").getByText("99")).toBeVisible({ timeout: 10_000 });

    // DELETE
    await runSql(page, `DELETE FROM ${table} WHERE id = 3`);
    await expect(page.getByText(/DELETE.*1 row/)).toBeVisible({ timeout: 10_000 });
    await runSql(page, `SELECT id FROM ${table}`);
    await expect(page.getByText(/Result \(2 rows\)/)).toBeVisible({ timeout: 10_000 });

    // CREATE INDEX
    await runSql(page, `CREATE INDEX ${table}_grp_idx ON ${table} (grp)`);
    await expect(page.getByText("OK.")).toBeVisible({ timeout: 10_000 });

    // GROUP BY + HAVING -- the earlier DELETE removed the only grp='b'
    // row (id=3), so only grp='a' (id=1 val=99 after UPDATE, id=2
    // val=20) remains: exactly one group, COUNT=2, SUM=119.
    await runSql(
      page,
      `SELECT grp, COUNT(*), SUM(val) FROM ${table} GROUP BY grp HAVING COUNT(*) >= 1 ORDER BY grp`,
    );
    await expect(page.getByText(/Result \(1 row\)/)).toBeVisible({ timeout: 10_000 });
    await expect(page.locator(".table-wrap").getByRole("cell", { name: "119", exact: true })).toBeVisible();

    // BEGIN / INSERT / SELECT / ROLLBACK / SELECT
    await runSql(page, "BEGIN");
    await expect(page.getByText("transaction open")).toBeVisible({ timeout: 10_000 });
    await runSql(page, `INSERT INTO ${table} (id, grp, val) VALUES (100, 'c', 1)`);
    await expect(page.getByText(/INSERT.*1 row/)).toBeVisible({ timeout: 10_000 });
    await runSql(page, "ROLLBACK");
    await expect(page.getByText("autocommit")).toBeVisible({ timeout: 10_000 });
    await runSql(page, `SELECT id FROM ${table} WHERE id = 100`);
    await expect(page.getByText(/Result \(0 rows\)/)).toBeVisible({ timeout: 10_000 });

    // BEGIN / INSERT / COMMIT / SELECT
    await runSql(page, "BEGIN");
    await expect(page.getByText("transaction open")).toBeVisible({ timeout: 10_000 });
    await runSql(page, `INSERT INTO ${table} (id, grp, val) VALUES (200, 'd', 2)`);
    await runSql(page, "COMMIT");
    await expect(page.getByText("autocommit")).toBeVisible({ timeout: 10_000 });
    await runSql(page, `SELECT id FROM ${table} WHERE id = 200`);
    await expect(page.getByText(/Result \(1 row\)/)).toBeVisible({ timeout: 10_000 });
  });

  test("a parse error renders a stable error, never a blank or crashed page", async ({ page }) => {
    await connect(page, ADMIN_KEY);
    await page.getByRole("link", { name: "SQL Console" }).click();
    await runSql(page, "SELEKT GARBAGE");
    await expect(page.getByRole("alert")).toContainText("PARSE_ERROR", { timeout: 10_000 });
    // The page must remain fully usable afterward.
    await runSql(page, "SELECT 1");
    await expect(page.getByText(/Result \(1 row\)/)).toBeVisible({ timeout: 10_000 });
  });

  test("history replays a previous statement into the editor", async ({ page }) => {
    await connect(page, ADMIN_KEY);
    await page.getByRole("link", { name: "SQL Console" }).click();
    await runSql(page, "SELECT 1");
    await expect(page.getByText(/Result \(1 row\)/)).toBeVisible({ timeout: 10_000 });
    // History is now the third tab of the results panel (was an always-visible card).
    await page.getByRole("tab", { name: "History" }).click();
    await page.getByRole("button", { name: /SELECT 1/ }).click();
    await expect(page.getByLabel("SQL")).toHaveValue("SELECT 1");
  });
});

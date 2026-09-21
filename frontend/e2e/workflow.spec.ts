import { test, expect, type Page } from "@playwright/test";

const ADMIN_KEY = "e2e-admin-key-0123456789";
const READER_KEY = "e2e-reader-key-0123456789";
const API_URL = "http://127.0.0.1:8099";

async function connect(page: Page, apiKey: string) {
  await page.goto("/connect");
  await expect(page.getByRole("heading", { name: "RubiXDB Console" })).toBeVisible();
  await page.getByLabel("API endpoint").fill(API_URL);
  await page.getByLabel("API key").fill(apiKey);
  await page.getByRole("button", { name: "Connect" }).click();
  await expect(page).toHaveURL("/");
}

async function navTo(page: Page, label: string) {
  await page.getByRole("link", { name: label }).click();
}

test.describe("RubiXDB console — full workflow, real backend", () => {
  test("connect, inspect health, write, read, historical read, range, snapshot, compaction, metrics, error handling, reload persistence", async ({
    page,
  }) => {
    // 1. Open application + connect.
    await connect(page, ADMIN_KEY);
    await expect(page.getByText("e2e-admin", { exact: false })).toBeVisible();

    // 2. Inspect health (Dashboard shows storage-state badge). Scoped
    // to the main content, since the top bar shows its own copy of
    // the same badge.
    await expect(page.locator("#main-content").getByText("Healthy")).toBeVisible({
      timeout: 10_000,
    });

    // 3. Write data via the Data Explorer.
    await navTo(page, "Data Explorer");
    const keyText = `e2e-key-${Date.now()}`;
    await page.getByLabel("Key", { exact: true }).first().fill(keyText);
    await page.getByLabel("Value", { exact: true }).fill("hello from playwright");
    await page.getByRole("button", { name: "Put" }).click();
    await expect(page.getByText(/Wrote .* at seq/)).toBeVisible({ timeout: 10_000 });

    // 4. Read data back ("now"). Scoped to the <pre> result display,
    // not the Write form's own textarea (which still shows the same
    // text after submitting).
    await page.getByRole("button", { name: "Look up" }).click();
    await expect(page.locator("pre")).toHaveText("hello from playwright", { timeout: 10_000 });

    // 5. Overwrite and re-read: a real, observable versioning
    // transition exercised end-to-end through the real UI and real
    // backend (the API's own persists-across-restart integration test
    // separately covers get_as_of-at-a-captured-seq directly).
    await page.getByLabel("Value", { exact: true }).fill("updated value");
    await page.getByRole("button", { name: "Put" }).click();
    await expect(page.getByText(/Wrote .* at seq/)).toBeVisible({ timeout: 10_000 });
    await page.getByRole("button", { name: "Look up" }).click();
    await expect(page.locator("pre")).toHaveText("updated value", { timeout: 10_000 });

    // 6. Range query.
    await page.getByRole("tab", { name: "Range Query" }).click();
    await page.getByRole("button", { name: "Run range query" }).click();
    const rangeTable = page.getByRole("table", { name: "Range query results" });
    await expect(rangeTable).toBeVisible({ timeout: 10_000 });
    await expect(rangeTable.getByText(keyText)).toBeVisible();

    // 7. Inspect / create a snapshot.
    await navTo(page, "Snapshots");
    await page.getByRole("button", { name: "New snapshot" }).click();
    await expect(page.getByText(/Snapshot created at seq/)).toBeVisible({ timeout: 10_000 });
    await expect(page.getByRole("table", { name: "Held snapshots" })).toBeVisible();

    // 8. Inspect compaction.
    await navTo(page, "Compaction");
    await expect(page.getByRole("heading", { name: "Automatic trigger" })).toBeVisible();
    await expect(page.getByText("Disabled")).toBeVisible(); // e2e config disables auto-trigger

    // 9. Inspect metrics (Health / Storage page).
    await navTo(page, "Health / Storage");
    await expect(page.getByText("Engine status")).toBeVisible();
    await expect(page.getByRole("table", { name: "Per-route request metrics" })).toBeVisible({
      timeout: 10_000,
    });

    // 10. Handle an error: look up a key that does not exist.
    await navTo(page, "Data Explorer");
    await page.getByRole("tab", { name: "Point Lookup" }).click();
    await page.getByLabel("Key", { exact: true }).first().fill("definitely-does-not-exist");
    await page.getByRole("button", { name: "Look up" }).click();
    await expect(page.getByText(/does not exist/)).toBeVisible({ timeout: 10_000 });

    // 11. Refresh/reload -- session (sessionStorage) and data (the real
    // engine) must both survive.
    await page.reload();
    await expect(page.getByText("e2e-admin", { exact: false })).toBeVisible({ timeout: 10_000 });
    await navTo(page, "Data Explorer");
    await page.getByLabel("Key", { exact: true }).first().fill(keyText);
    await page.getByRole("button", { name: "Look up" }).click();
    await expect(page.locator("pre")).toHaveText("updated value", { timeout: 10_000 });
  });

  test("a reader-role session cannot write, and sees no write controls at all", async ({ page }) => {
    await connect(page, READER_KEY);
    await navTo(page, "Data Explorer");
    // The write Card is not rendered at all for a reader-role session
    // (PHASE_FRONTEND_ARCHITECTURE.md §3: role-gated actions).
    await expect(page.getByRole("button", { name: "Put" })).not.toBeVisible();
  });

  test("an invalid API key is rejected with a clear message, never silently accepted", async ({
    page,
  }) => {
    await page.goto("/connect");
    await page.getByLabel("API endpoint").fill(API_URL);
    await page.getByLabel("API key").fill("totally-invalid-key-0000000000");
    await page.getByRole("button", { name: "Connect" }).click();
    await expect(page.getByText("That API key was not accepted.")).toBeVisible({
      timeout: 10_000,
    });
    await expect(page).toHaveURL("/connect");
  });
});

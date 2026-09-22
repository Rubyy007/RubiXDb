import { test, expect, type Page } from "@playwright/test";

const ADMIN_KEY = "e2e-admin-key-0123456789";
const API_URL = "http://127.0.0.1:8099";

async function connect(page: Page, apiKey: string) {
  await page.goto("/connect");
  await page.getByLabel("API endpoint").fill(API_URL);
  await page.getByLabel("API key").fill(apiKey);
  await page.getByRole("button", { name: "Connect" }).click();
  await expect(page).toHaveURL("/");
}

// Productization — Production Validation Phase §12: a small,
// deterministic screenshot baseline, deliberately scoped to the two
// screens whose content is actually stable run to run. Every other
// screen (Dashboard, Health/Storage, Compaction) renders live
// operational data -- request counts, uptime, SSTable counts,
// timestamps -- that changes on every run by design, so a pixel-diff
// baseline for them would either be permanently flaky or require
// masking most of the page, which is not "a small deterministic set of
// critical screens" any more. Connect (pre-session, no data at all) and
// Settings (its only per-session value, the principal name, is fixed
// by this suite's own fixed API key) are the two screens where a real
// pixel-diff baseline is actually meaningful signal rather than noise.
test.describe("Productization — Production Validation Phase §12: visual regression baseline", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  test("Connect screen matches its baseline", async ({ page }) => {
    await page.goto("/connect");
    await expect(page.getByRole("heading", { name: "RubiXDB Console" })).toBeVisible();
    await expect(page).toHaveScreenshot("connect-screen.png", { maxDiffPixelRatio: 0.01 });
  });

  test("Settings screen matches its baseline", async ({ page }) => {
    await connect(page, ADMIN_KEY);
    await page.getByRole("link", { name: "Settings" }).click();
    await expect(page.getByRole("heading", { name: "Settings" })).toBeVisible();
    await expect(page).toHaveScreenshot("settings-screen.png", { maxDiffPixelRatio: 0.01 });
  });
});

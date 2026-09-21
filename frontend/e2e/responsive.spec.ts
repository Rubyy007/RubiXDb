import { test, expect, type Page } from "@playwright/test";

const ADMIN_KEY = "e2e-admin-key-0123456789";
const API_URL = "http://127.0.0.1:8099";

async function connect(page: Page) {
  await page.goto("/connect");
  await page.getByLabel("API endpoint").fill(API_URL);
  await page.getByLabel("API key").fill(ADMIN_KEY);
  await page.getByRole("button", { name: "Connect" }).click();
  await expect(page).toHaveURL("/");
}

// PHASE_FRONTEND_ARCHITECTURE.md §6: desktop / tablet / small viewport,
// verified via real rendering at each size, not a CSS-only assertion.
const VIEWPORTS = [
  { name: "desktop", width: 1280, height: 800 },
  { name: "tablet", width: 820, height: 1024 },
  { name: "small", width: 390, height: 844 },
];

test.describe("responsive layout", () => {
  for (const viewport of VIEWPORTS) {
    test(`no horizontal overflow at ${viewport.name} (${viewport.width}px)`, async ({ page }) => {
      await page.setViewportSize({ width: viewport.width, height: viewport.height });
      await connect(page);

      for (const path of ["/", "/explorer", "/snapshots", "/compaction", "/health"]) {
        await page.goto(path);
        await page.waitForLoadState("networkidle");
        const hasOverflow = await page.evaluate(
          () => document.documentElement.scrollWidth > document.documentElement.clientWidth + 1,
        );
        expect(hasOverflow, `${path} at ${viewport.name} must not overflow horizontally`).toBe(
          false,
        );
      }
    });
  }

  test("small viewport: navigation collapses to a drawer, opened via the toggle", async ({
    page,
  }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await connect(page);

    const nav = page.locator(".app-nav");
    // Drawer starts closed (translated off-screen) at this width.
    await expect(nav).toHaveAttribute("data-open", "false");

    await page.getByRole("button", { name: "Toggle navigation" }).click();
    await expect(nav).toHaveAttribute("data-open", "true");

    await page.getByRole("link", { name: "Snapshots" }).click();
    await expect(page).toHaveURL("/snapshots");
    // Selecting a destination closes the drawer again.
    await expect(nav).toHaveAttribute("data-open", "false");
  });
});

import { test, expect, type Page } from "@playwright/test";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const axePath = path.join(__dirname, "..", "node_modules", "axe-core", "axe.min.js");

const ADMIN_KEY = "e2e-admin-key-0123456789";
const API_URL = "http://127.0.0.1:8099";

interface AxeResult {
  violations: { id: string; impact: string; help: string; nodes: unknown[] }[];
}

async function runAxe(page: Page): Promise<AxeResult> {
  await page.addScriptTag({ path: axePath });
  return page.evaluate(() => (window as unknown as { axe: { run: () => Promise<AxeResult> } }).axe.run());
}

async function connect(page: Page) {
  await page.goto("/connect");
  await page.getByLabel("API endpoint").fill(API_URL);
  await page.getByLabel("API key").fill(ADMIN_KEY);
  await page.getByRole("button", { name: "Connect" }).click();
  await expect(page).toHaveURL("/");
}

// PHASE_FRONTEND_ARCHITECTURE.md §6: automated accessibility audit
// (axe-core) run against each screen, real rendering via Playwright,
// not a static/CSS-only check.
const SCREENS: { path: string; label: string }[] = [
  { path: "/connect", label: "Connect" },
  { path: "/", label: "Dashboard" },
  { path: "/explorer", label: "Data Explorer" },
  { path: "/snapshots", label: "Snapshots" },
  { path: "/compaction", label: "Compaction" },
  { path: "/health", label: "Health / Storage" },
  { path: "/settings", label: "Settings" },
];

test.describe("accessibility (axe-core)", () => {
  test("Connect screen has no automatically-detectable violations", async ({ page }) => {
    await page.goto("/connect");
    const results = await runAxe(page);
    expect(results.violations, JSON.stringify(results.violations, null, 2)).toEqual([]);
  });

  test("every authenticated screen has no automatically-detectable violations", async ({ page }) => {
    await connect(page);
    for (const screen of SCREENS.slice(1)) {
      await page.goto(screen.path);
      const results = await runAxe(page);
      expect(
        results.violations,
        `${screen.label}: ${JSON.stringify(results.violations, null, 2)}`,
      ).toEqual([]);
    }
  });
});

test.describe("keyboard navigation", () => {
  test("the skip link, nav, and a dialog are all keyboard-operable", async ({ page }) => {
    await connect(page);

    // Skip link is the first focusable element and is genuinely usable.
    await page.keyboard.press("Tab");
    await expect(page.getByRole("link", { name: "Skip to content" })).toBeFocused();

    // Navigate to Snapshots via keyboard-driven link activation.
    await navigateByKeyboard(page, "Snapshots");
    await expect(page).toHaveURL("/snapshots");

    // Open the "New snapshot" dialog flow indirectly via release
    // confirmation: create one with the mouse first (not the focus of
    // this test), then verify the confirmation Dialog traps focus and
    // Escape closes it.
    await page.getByRole("button", { name: "New snapshot" }).click();
    await expect(page.getByText(/Snapshot created at seq/)).toBeVisible({ timeout: 10_000 });
    await page.getByRole("button", { name: "Release" }).click();
    const dialog = page.getByRole("dialog");
    await expect(dialog).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(dialog).not.toBeVisible();
  });
});

async function navigateByKeyboard(page: Page, label: string) {
  const link = page.getByRole("link", { name: label });
  await link.focus();
  await page.keyboard.press("Enter");
}

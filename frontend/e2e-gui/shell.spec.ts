import { test, expect } from "@playwright/test";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

// Shell redesign, Increment 1 -- against the real `rubixdb gui` product
// path, so the shell is exercised under the real Phase 7 CSP (no inline
// style/script allowed): the logo image must load, every existing page must
// be reachable from the left rail, and nothing may raise a console error
// (a CSP violation is reported there).

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const INSTANCE_DIR = path.join(__dirname, "..", ".e2e-gui-data", "default");

function readAdminKey(): string {
  const raw = fs.readFileSync(path.join(INSTANCE_DIR, "credentials.json"), "utf-8");
  return (JSON.parse(raw) as { admin_key: string }).admin_key;
}

const ROUTES: { link: string; path: string; h1: string }[] = [
  { link: "Home", path: "/", h1: "Home" },
  { link: "SQL Console", path: "/sql", h1: "SQL Console" },
  { link: "Monitoring", path: "/health", h1: "Health / Storage" },
  { link: "Catalog", path: "/explorer", h1: "Data Explorer" },
  { link: "Governance & security", path: "/settings", h1: "Settings" },
  { link: "Compute", path: "/compaction", h1: "Compaction" },
  { link: "Admin", path: "/operations", h1: "Operations" },
  { link: "Snapshots", path: "/snapshots", h1: "Snapshots" },
];

test.describe("app shell -- real rubixdb gui product path, under the CSP", () => {
  test("every page is reachable from the rail, inside one <main>, with one <h1>, and no console/CSP errors", async ({
    page,
  }) => {
    const errors: string[] = [];
    page.on("console", (m) => {
      if (m.type() === "error") errors.push(m.text());
    });
    page.on("pageerror", (e) => errors.push(String(e)));

    await page.goto(`/#token=${readAdminKey()}`);
    const primary = page.getByRole("navigation", { name: "Primary" });
    await expect(primary).toBeVisible();

    for (const r of ROUTES) {
      await primary.getByRole("link", { name: r.link }).click();
      await expect(page).toHaveURL(r.path);
      await expect(page.getByRole("main")).toHaveCount(1);
      await expect(page.getByRole("heading", { level: 1 })).toHaveText(r.h1);
      await expect(primary.getByRole("link", { name: r.link })).toHaveAttribute("aria-current", "page");
    }

    // The logo is a real, same-origin image that the CSP let through.
    const logo = page.locator("img.rail-logo");
    await expect(logo).toBeVisible();
    expect(await logo.evaluate((i: HTMLImageElement) => i.naturalWidth)).toBeGreaterThan(0);

    expect(errors, errors.join("\n")).toEqual([]);
  });

  test("features rubiXDb does not have are omitted, not shown disabled", async ({ page }) => {
    await page.goto(`/#token=${readAdminKey()}`);
    const primary = page.getByRole("navigation", { name: "Primary" });
    await expect(primary).toBeVisible();
    for (const absent of ["Ingestion", "Transformation", "AI & ML", "Apps", "Marketplace", "Data sharing", "Postgres"]) {
      await expect(primary.getByText(absent)).toHaveCount(0);
    }
    await expect(primary.getByRole("link")).toHaveCount(ROUTES.length);
  });

  test("the rail collapses to icons and every link keeps its accessible name", async ({ page }) => {
    await page.goto(`/#token=${readAdminKey()}`);
    const primary = page.getByRole("navigation", { name: "Primary" });
    await expect(primary).toBeVisible();
    await page.getByRole("button", { name: "Collapse sidebar" }).click();
    await expect(page.getByRole("button", { name: "Expand sidebar" })).toHaveAttribute("aria-expanded", "false");
    for (const r of ROUTES) {
      await expect(primary.getByRole("link", { name: r.link })).toHaveCount(1);
    }
    await primary.getByRole("link", { name: "SQL Console" }).click();
    await expect(page).toHaveURL("/sql");
    await page.getByRole("button", { name: "Expand sidebar" }).click();
    await expect(page.getByRole("button", { name: "Collapse sidebar" })).toBeVisible();
  });

  test("top bar: search is UI-only (no request is made), profile shows the real role", async ({ page }) => {
    await page.goto(`/#token=${readAdminKey()}`);
    const search = page.getByPlaceholder("Search your database, tables, and SQL history");
    await expect(search).toBeVisible();
    const requests: string[] = [];
    page.on("request", (r) => requests.push(r.url()));
    await search.fill("anything");
    await search.press("Enter");
    await page.waitForTimeout(300);
    expect(requests.filter((u) => /search|query|sql/i.test(new URL(u).pathname))).toEqual([]);
    await expect(page.locator(".rail-profile-role")).toHaveText("admin");
  });
});

import { defineConfig, devices } from "@playwright/test";
import path from "node:path";
import { fileURLToPath } from "node:url";

// Increment 14, Blocker 10 -- cross-browser GUI timing. Same real
// `rubixdb gui` product path as `playwright.gui.config.ts` (one real
// release binary hosting both API and frontend on the same origin),
// but exercising every realistically available browser engine in this
// environment instead of Chromium only. A separate config (not just
// added projects to `playwright.gui.config.ts`) so the existing heavy
// Chromium-only performance/endurance suites are not accidentally
// tripled in run time by browsers they were never meant to sweep.
const PORT = 302;
const INSTANCES_ROOT = path.join(
  path.dirname(fileURLToPath(import.meta.url)),
  ".e2e-crossbrowser-data",
);
const __dirname = path.dirname(fileURLToPath(import.meta.url));

export default defineConfig({
  testDir: "./e2e-gui",
  testMatch: /cross_browser\.spec\.ts/,
  fullyParallel: false,
  retries: 0,
  workers: 1,
  reporter: [["list"]],
  timeout: 60_000,
  use: {
    baseURL: `http://127.0.0.1:${PORT}`,
    trace: "retain-on-failure",
  },
  projects: [
    { name: "chromium", use: { ...devices["Desktop Chrome"] } },
    { name: "firefox", use: { ...devices["Desktop Firefox"] } },
    { name: "webkit", use: { ...devices["Desktop Safari"] } },
  ],
  webServer: {
    command: `"${path.join(__dirname, "..", "target", "release", "rubixdb.exe")}" gui --no-browser`,
    cwd: __dirname,
    env: {
      RUBIXDB_INSTANCES_ROOT: INSTANCES_ROOT,
      RUBIXDB_FRONTEND_DIST: path.join(__dirname, "dist"),
    },
    url: `http://127.0.0.1:${PORT}/healthz`,
    reuseExistingServer: false,
    timeout: 30_000,
  },
});

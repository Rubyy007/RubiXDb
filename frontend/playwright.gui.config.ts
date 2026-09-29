import { defineConfig, devices } from "@playwright/test";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

// Increment 13 GUI/frontend performance pass -- the REAL `rubixdb gui`
// product path: one real compiled binary (release build) hosting both
// the API and the real `npm run build` frontend on the SAME origin
// (PHASE_RUBIXDB_GUI_ARCHITECTURE.md §6), not the separately-hosted
// two-origin shape `playwright.config.ts` exercises. A fresh,
// disposable instances root per run.
const PORT = 302;
const INSTANCES_ROOT = path.join(__dirname, ".e2e-gui-data");

export default defineConfig({
  testDir: "./e2e-gui",
  fullyParallel: false,
  retries: 0,
  workers: 1,
  reporter: [["list"]],
  timeout: 120_000,
  use: {
    baseURL: `http://127.0.0.1:${PORT}`,
    trace: "retain-on-failure",
  },
  projects: [
    {
      name: "chromium",
      use: { ...devices["Desktop Chrome"] },
    },
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

import { defineConfig, devices } from "@playwright/test";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

// SQL IDE against the REAL release binary with the console EMBEDDED in it (no
// RUBIXDB_FRONTEND_DIST override, unlike playwright.gui.config.ts), on a fresh,
// disposable instances root. Build the frontend, then the binary, first:
//   npm run build && cargo build --release -p rubixdb-cli
const PORT = 302;
const INSTANCES_ROOT = path.join(__dirname, ".e2e-ide-data");

export default defineConfig({
  testDir: "./e2e-ide",
  fullyParallel: false,
  retries: 0,
  workers: 1,
  reporter: [["list"]],
  timeout: 120_000,
  outputDir: path.join(__dirname, "test-results", "ide"),
  use: { baseURL: `http://127.0.0.1:${PORT}`, trace: "retain-on-failure" },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
  webServer: {
    command: `"${path.join(__dirname, "..", "target", "release", "rubixdb.exe")}" gui --no-browser`,
    cwd: __dirname,
    env: { RUBIXDB_INSTANCES_ROOT: INSTANCES_ROOT },
    url: `http://127.0.0.1:${PORT}/healthz`,
    reuseExistingServer: false,
    timeout: 30_000,
  },
});

import { defineConfig, devices } from "@playwright/test";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

// Real backend + real frontend, two separate origins -- exercising the
// actual CORS configuration (PHASE_FRONTEND_ARCHITECTURE.md §5/§18),
// not a mocked API. A fresh, disposable data directory per test run.
const API_PORT = 8099;
const FRONTEND_PORT = 5173;
const DATA_DIR = path.join(__dirname, ".e2e-data");

export default defineConfig({
  testDir: "./e2e",
  fullyParallel: false,
  retries: 0,
  workers: 1,
  reporter: [["list"]],
  use: {
    baseURL: `http://127.0.0.1:${FRONTEND_PORT}`,
    trace: "retain-on-failure",
  },
  projects: [
    {
      name: "chromium",
      use: { ...devices["Desktop Chrome"] },
    },
  ],
  webServer: [
    {
      command: `"${path.join(__dirname, "..", "target", "debug", "rubixdb-api.exe")}"`,
      cwd: __dirname,
      env: {
        RUBIXDB_DATA_DIR: DATA_DIR,
        RUBIXDB_LISTEN_ADDR: `127.0.0.1:${API_PORT}`,
        RUBIXDB_API_KEYS: `e2e-admin:admin:e2e-admin-key-0123456789,e2e-reader:reader:e2e-reader-key-0123456789`,
        RUBIXDB_CORS_ALLOWED_ORIGINS: `http://127.0.0.1:${FRONTEND_PORT},http://localhost:${FRONTEND_PORT}`,
        RUBIXDB_COMPACTION_AUTO_TRIGGER: "false",
      },
      url: `http://127.0.0.1:${API_PORT}/healthz`,
      reuseExistingServer: false,
      timeout: 30_000,
    },
    {
      command: `npm run preview -- --port ${FRONTEND_PORT} --strictPort --host 127.0.0.1`,
      cwd: __dirname,
      url: `http://127.0.0.1:${FRONTEND_PORT}`,
      reuseExistingServer: false,
      timeout: 30_000,
    },
  ],
});

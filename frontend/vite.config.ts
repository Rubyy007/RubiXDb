import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";

// RubiXDB console build config. Dev server proxies /v1, /readyz,
// /healthz to the local rubixdb-api instance so the console can be
// developed against a real backend without a CORS layer on the API
// itself (RUBIXDB_API_PROXY_TARGET overrides the default for a
// non-default port/host).
const apiProxyTarget = process.env.RUBIXDB_API_PROXY_TARGET ?? "http://127.0.0.1:8080";

export default defineConfig({
  plugins: [react()],
  server: {
    port: 5173,
    proxy: {
      "/v1": apiProxyTarget,
      "/healthz": apiProxyTarget,
      "/readyz": apiProxyTarget,
    },
  },
  build: {
    outDir: "dist",
    sourcemap: true,
  },
  test: {
    environment: "jsdom",
    globals: true,
    setupFiles: ["./src/tests/setup.ts"],
    exclude: ["**/node_modules/**", "**/e2e/**"],
  },
});

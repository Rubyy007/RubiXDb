import { test, expect, type Page } from "@playwright/test";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

// Phase 7 D-3 / O-2 -- GUI credential handoff, real end to end against the
// real `rubixdb gui` product path (real release binary, real production
// frontend build served WITH the Phase 7 security headers/CSP, real HTTP).
// `rubixdb gui` opens http://127.0.0.1:302/#token=<key>; the SPA must
// authenticate without a paste, remove the fragment, and keep the key in the
// tab's sessionStorage only.

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const INSTANCE_DIR = path.join(__dirname, "..", ".e2e-gui-data", "default");
const STORAGE_KEY = "rubixdb-console-session";

function readAdminKey(): string {
  const raw = fs.readFileSync(path.join(INSTANCE_DIR, "credentials.json"), "utf-8");
  return (JSON.parse(raw) as { admin_key: string }).admin_key;
}

async function storage(page: Page) {
  return page.evaluate((k) => {
    return {
      session: window.sessionStorage.getItem(k),
      local: window.localStorage.getItem(k),
      localLength: window.localStorage.length,
    };
  }, STORAGE_KEY);
}

test.describe("GUI token handoff -- real rubixdb gui product path", () => {
  test("a #token URL authenticates without a paste, scrubs the fragment, and uses sessionStorage only", async ({
    page,
  }) => {
    const key = readAdminKey();
    const consoleErrors: string[] = [];
    page.on("console", (m) => {
      if (m.type() === "error") consoleErrors.push(m.text());
    });
    page.on("pageerror", (e) => consoleErrors.push(String(e)));

    await page.goto(`/#token=${key}`);
    // Authenticated: the app shell is shown (Log out is only in the shell); no Connect form.
    await expect(page.getByRole("button", { name: "Log out" })).toBeVisible();
    await expect(page.getByLabel("API key")).toHaveCount(0);

    // The fragment (and therefore the key) is gone from the address bar.
    expect(page.url()).not.toContain("#");
    expect(page.url()).not.toContain(key);
    expect(await page.evaluate(() => window.location.hash)).toBe("");

    // The key is held in sessionStorage -- and NOT in localStorage.
    const st = await storage(page);
    expect(st.session).not.toBeNull();
    expect(JSON.parse(st.session as string).apiKey).toBe(key);
    expect(st.local).toBeNull();
    expect(st.localLength).toBe(0);

    // The console works under the CSP: a real authenticated page of data.
    await page.getByRole("link", { name: "SQL Console" }).click();
    await expect(page).toHaveURL(/\/sql$/);

    // No CSP violation / console error produced by any of the above.
    expect(consoleErrors.filter((e) => /Content Security Policy|Refused to/i.test(e))).toEqual([]);
  });

  test("a fresh tab without the fragment is NOT authenticated (sessionStorage is per tab)", async ({
    browser,
  }) => {
    const key = readAdminKey();
    const context = await browser.newContext();
    const first = await context.newPage();
    await first.goto(`/#token=${key}`);
    await expect(first.getByRole("button", { name: "Log out" })).toBeVisible();

    const second = await context.newPage();
    await second.goto("/");
    await expect(second).toHaveURL(/\/connect$/);
    await expect(second.getByLabel("API key")).toBeVisible();
    expect((await storage(second)).session).toBeNull();
    // Reloading the first tab keeps its own session (and still no fragment).
    await first.reload();
    await expect(first.getByRole("button", { name: "Log out" })).toBeVisible();
    expect(first.url()).not.toContain("#");
    await context.close();
  });

  test("a wrong or malformed token is scrubbed from the URL and falls back to the Connect page", async ({
    page,
  }) => {
    for (const bad of ["f".repeat(64), "short", "not a token at all!!"]) {
      await page.goto(`/#token=${encodeURIComponent(bad)}`);
      await expect(page).toHaveURL(/\/connect$/);
      await expect(page.getByLabel("API key")).toBeVisible();
      expect(page.url()).not.toContain("#");
      expect(page.url()).not.toContain(bad);
      const st = await storage(page);
      expect(st.session).toBeNull();
      expect(st.local).toBeNull();
    }
  });

  test("Remember is opt-in, carries an explicit warning, and is the only way the key reaches localStorage", async ({
    page,
  }) => {
    const key = readAdminKey();
    await page.goto("/connect");
    const warning = page.getByRole("note");
    await expect(warning).toContainText("localStorage");
    await expect(warning).toContainText(/Warning/i);
    // The warning sits above the checkbox.
    const wBox = await warning.boundingBox();
    const cBox = await page.getByLabel("Remember this connection on this device").boundingBox();
    expect(wBox && cBox && wBox.y < cBox.y).toBeTruthy();

    // Unticked connect: sessionStorage only.
    await page.getByLabel("API endpoint").fill("http://127.0.0.1:302");
    await page.getByLabel("API key").fill(key);
    await page.getByRole("button", { name: "Connect" }).click();
    await expect(page).toHaveURL("/");
    let st = await storage(page);
    expect(st.session).not.toBeNull();
    expect(st.local).toBeNull();

    // Ticked connect: localStorage (explicit opt-in).
    await page.getByRole("button", { name: "Log out" }).click();
    await page.goto("/connect");
    await page.getByLabel("API endpoint").fill("http://127.0.0.1:302");
    await page.getByLabel("API key").fill(key);
    await page.getByLabel("Remember this connection on this device").check();
    await page.getByRole("button", { name: "Connect" }).click();
    await expect(page).toHaveURL("/");
    st = await storage(page);
    expect(st.local).not.toBeNull();
    expect(st.session).toBeNull();
    await page.getByRole("button", { name: "Log out" }).click();
    st = await storage(page);
    expect(st.local).toBeNull();
  });

  test("the served console carries the security headers and its own assets load under the CSP", async ({
    page,
    request,
  }) => {
    const resp = await request.get("/");
    const csp = resp.headers()["content-security-policy"];
    expect(csp).toContain("script-src 'self'");
    expect(csp).toContain("frame-ancestors 'none'");
    expect(csp).not.toContain("unsafe-inline");
    expect(resp.headers()["x-content-type-options"]).toBe("nosniff");
    const violations: string[] = [];
    page.on("console", (m) => {
      if (/Content Security Policy|Refused to/i.test(m.text())) violations.push(m.text());
    });
    await page.goto("/connect");
    await expect(page.getByLabel("API key")).toBeVisible();
    expect(violations).toEqual([]);
  });

  test("control: the browser really enforces the CSP (an injected inline script and inline style are blocked)", async ({
    page,
  }) => {
    const violations: string[] = [];
    page.on("console", (m) => {
      if (/Content Security Policy|Refused to/i.test(m.text())) violations.push(m.text());
    });
    await page.goto("/connect");
    await expect(page.getByLabel("API key")).toBeVisible();
    const ran = await page.evaluate(() => {
      const s = document.createElement("script");
      s.textContent = "window.__csp_inline_ran = true;";
      document.body.appendChild(s);
      document.body.setAttribute("style", "background: red");
      return (window as unknown as { __csp_inline_ran?: boolean }).__csp_inline_ran === true;
    });
    expect(ran).toBe(false);
    await expect.poll(() => violations.length).toBeGreaterThan(0);
  });
});

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { effectiveTheme, initTheme, setThemePref, storedTheme, subscribeTheme, toggleTheme } from "./theme";

const KEY = "rubixdb-console-theme";
const root = document.documentElement;

function mockMedia(dark: boolean, reduced = false) {
  const listeners = new Set<() => void>();
  vi.stubGlobal("matchMedia", (q: string) => ({
    matches: q.includes("prefers-reduced-motion") ? reduced : q.includes("prefers-color-scheme: dark") ? dark : false,
    addEventListener: (_: string, cb: () => void) => listeners.add(cb),
    removeEventListener: (_: string, cb: () => void) => listeners.delete(cb),
  }));
  return listeners;
}

describe("theme", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    window.localStorage.clear();
    root.removeAttribute("data-theme");
    root.classList.remove("theme-switching");
    mockMedia(false);
  });
  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it("applies the saved theme at startup, on any page (it used to be applied only by Settings)", () => {
    window.localStorage.setItem(KEY, "dark");
    initTheme();
    expect(root.getAttribute("data-theme")).toBe("dark");
    expect(effectiveTheme()).toBe("dark");
  });

  it("ignores a corrupt saved value and falls back to the system setting", () => {
    window.localStorage.setItem(KEY, "purple");
    initTheme();
    expect(root.hasAttribute("data-theme")).toBe(false);
    expect(storedTheme()).toBe("system");
  });

  it("setThemePref applies, remembers and announces the choice", () => {
    const heard = vi.fn();
    const off = subscribeTheme(heard);
    setThemePref("dark");
    expect(root.getAttribute("data-theme")).toBe("dark");
    expect(window.localStorage.getItem(KEY)).toBe("dark");
    expect(heard).toHaveBeenCalledTimes(1);
    setThemePref("system");
    expect(root.hasAttribute("data-theme")).toBe(false);
    expect(window.localStorage.getItem(KEY)).toBeNull();
    off();
    setThemePref("light");
    expect(heard).toHaveBeenCalledTimes(2);
  });

  it("cross-fades: the theme-switching class is on during a switch and gone after it", () => {
    setThemePref("dark");
    expect(root.classList.contains("theme-switching")).toBe(true);
    vi.advanceTimersByTime(200);
    expect(root.classList.contains("theme-switching")).toBe(true);
    vi.advanceTimersByTime(200);
    expect(root.classList.contains("theme-switching")).toBe(false);
  });

  it("two quick switches keep the fade on until the last one has finished", () => {
    setThemePref("dark");
    vi.advanceTimersByTime(300);
    setThemePref("light");
    vi.advanceTimersByTime(300);
    expect(root.classList.contains("theme-switching")).toBe(true);
    vi.advanceTimersByTime(100);
    expect(root.classList.contains("theme-switching")).toBe(false);
  });

  it("does not animate for people who asked for reduced motion, but still switches", () => {
    mockMedia(false, true);
    setThemePref("dark");
    expect(root.classList.contains("theme-switching")).toBe(false);
    expect(root.getAttribute("data-theme")).toBe("dark");
  });

  it("toggleTheme flips from what is showing, including when that comes from the system", () => {
    mockMedia(true);
    expect(effectiveTheme()).toBe("dark");
    expect(toggleTheme()).toBe("light");
    expect(root.getAttribute("data-theme")).toBe("light");
    expect(toggleTheme()).toBe("dark");
  });

  it("follows the operating system while the preference is system, and not otherwise", () => {
    const listeners = mockMedia(false);
    const heard = vi.fn();
    subscribeTheme(heard);
    const stop = initTheme();
    expect(listeners.size).toBe(1);
    [...listeners][0]();
    expect(heard).toHaveBeenCalledTimes(1);
    expect(root.classList.contains("theme-switching")).toBe(true);
    window.localStorage.setItem(KEY, "light");
    [...listeners][0]();
    expect(heard).toHaveBeenCalledTimes(1); // a fixed choice is not changed by the system
    stop();
    expect(listeners.size).toBe(0);
  });

  it("still works when storage is unavailable", () => {
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("denied");
    });
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("denied");
    });
    expect(() => setThemePref("dark")).not.toThrow();
    expect(root.getAttribute("data-theme")).toBe("dark");
    expect(storedTheme()).toBe("system");
  });
});

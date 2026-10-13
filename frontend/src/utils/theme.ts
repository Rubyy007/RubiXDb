/** Light / dark / system theme, shared by the whole console (top-bar toggle, Settings, SQL IDE).
 *
 * The preference lives in `localStorage` under one key and is applied as `data-theme` on <html>
 * (`system` removes the attribute, so the `prefers-color-scheme` rules decide). `initTheme()` applies it
 * before the first render so every page -- not only Settings -- starts in the saved theme.
 *
 * A switch cross-fades colours: <html> gets the `theme-switching` class for a moment, and
 * `global.css` turns on colour transitions for everything under it (skipped for reduced motion). */

export type ThemePref = "system" | "light" | "dark";

const KEY = "rubixdb-console-theme";
const EVENT = "rubixdb-theme-change";
/** Longer than the 280 ms transition in global.css, so the class never cuts a fade short. */
const SWITCH_MS = 360;

function readStored(): ThemePref {
  try {
    const v = window.localStorage.getItem(KEY);
    return v === "light" || v === "dark" ? v : "system";
  } catch {
    return "system";
  }
}

export function storedTheme(): ThemePref {
  return readStored();
}

export function effectiveTheme(): "light" | "dark" {
  const attr = document.documentElement.getAttribute("data-theme");
  if (attr === "dark" || attr === "light") return attr;
  return window.matchMedia?.("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

let switchTimer: number | undefined;

/** Fade colours for a moment. No-op for people who asked for reduced motion (the CSS also guards this). */
function fadeColours() {
  if (window.matchMedia?.("(prefers-reduced-motion: reduce)").matches) return;
  const root = document.documentElement;
  root.classList.add("theme-switching");
  window.clearTimeout(switchTimer);
  switchTimer = window.setTimeout(() => root.classList.remove("theme-switching"), SWITCH_MS);
}

function paint(pref: ThemePref) {
  const root = document.documentElement;
  if (pref === "system") root.removeAttribute("data-theme");
  else root.setAttribute("data-theme", pref);
}

/** Choose a theme: applies it (with the fade), remembers it, and tells every listener. */
export function setThemePref(pref: ThemePref) {
  fadeColours();
  paint(pref);
  try {
    if (pref === "system") window.localStorage.removeItem(KEY);
    else window.localStorage.setItem(KEY, pref);
  } catch {
    /* storage unavailable: the theme still applies for this page view */
  }
  window.dispatchEvent(new Event(EVENT));
}

/** Flip between light and dark from whatever is showing now (what a toggle button means). */
export function toggleTheme(): "light" | "dark" {
  const next = effectiveTheme() === "dark" ? "light" : "dark";
  setThemePref(next);
  return next;
}

/** Apply the saved preference and follow the operating system while the preference is "system". Call once,
 * before the first render. Returns a function that undoes the listeners (for tests). */
export function initTheme(): () => void {
  paint(readStored());
  const mq = window.matchMedia?.("(prefers-color-scheme: dark)");
  const onSystem = () => {
    if (readStored() === "system") {
      fadeColours();
      window.dispatchEvent(new Event(EVENT));
    }
  };
  mq?.addEventListener?.("change", onSystem);
  return () => mq?.removeEventListener?.("change", onSystem);
}

/** For `useSyncExternalStore`: re-render when the theme changes anywhere (this tab or another). */
export function subscribeTheme(cb: () => void): () => void {
  const onStorage = (e: StorageEvent) => {
    if (e.key === KEY || e.key === null) {
      paint(readStored());
      cb();
    }
  };
  window.addEventListener(EVENT, cb);
  window.addEventListener("storage", onStorage);
  return () => {
    window.removeEventListener(EVENT, cb);
    window.removeEventListener("storage", onStorage);
  };
}

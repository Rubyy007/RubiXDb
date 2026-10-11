/** Light/dark toggle shared by the SQL IDE. Writes the same key and attribute as Settings, so the
 * two stay in agreement. */

const KEY = "rubixdb-console-theme";

export function effectiveTheme(): "light" | "dark" {
  const attr = document.documentElement.getAttribute("data-theme");
  if (attr === "dark" || attr === "light") return attr;
  return window.matchMedia?.("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

export function toggleTheme(): "light" | "dark" {
  const next = effectiveTheme() === "dark" ? "light" : "dark";
  document.documentElement.setAttribute("data-theme", next);
  try {
    window.localStorage.setItem(KEY, next);
  } catch {
    /* storage unavailable: the theme still applies for this page */
  }
  return next;
}

import { useSyncExternalStore } from "react";
import { effectiveTheme, storedTheme, subscribeTheme, type ThemePref } from "./theme";

/** The saved preference ("system" | "light" | "dark"), live across the console. */
export function useThemePref(): ThemePref {
  return useSyncExternalStore(subscribeTheme, storedTheme, () => "system" as ThemePref);
}

/** The theme actually showing now ("light" | "dark"), live -- also when "system" follows the OS. */
export function useEffectiveTheme(): "light" | "dark" {
  return useSyncExternalStore(subscribeTheme, effectiveTheme, () => "light" as const);
}

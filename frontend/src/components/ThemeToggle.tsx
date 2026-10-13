import { toggleTheme } from "../utils/theme";
import { useEffectiveTheme } from "../utils/useTheme";
import { Icon } from "./Icon";

/** One button, on every page: switches between light and dark with a short colour cross-fade. */
export function ThemeToggle() {
  const shown = useEffectiveTheme();
  const next = shown === "dark" ? "light" : "dark";
  return (
    <button
      type="button"
      className="shell-icon-btn theme-toggle"
      aria-label={`Switch to ${next} theme`}
      title={`Switch to ${next} theme`}
      data-theme-now={shown}
      onClick={() => void toggleTheme()}
    >
      <Icon name={shown === "dark" ? "sun" : "moon"} />
    </button>
  );
}

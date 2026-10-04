// Inline stroke icons for the shell. Hand-drawn paths, no icon library and
// no external asset: nothing here needs a CSP exception. Always decorative
// (aria-hidden); the control that contains an icon carries the accessible name.

const PATHS = {
  home: "M3 11l9-8 9 8M5 10v10h14V10",
  sql: "M4 5h16v14H4z M8 10l3 2-3 2 M13 15h4",
  monitoring: "M3 12h4l3-8 4 16 3-8h4",
  catalog:
    "M4 6c0-1.7 3.6-3 8-3s8 1.3 8 3-3.6 3-8 3-8-1.3-8-3z M4 6v12c0 1.7 3.6 3 8 3s8-1.3 8-3V6 M4 12c0 1.7 3.6 3 8 3s8-1.3 8-3",
  shield: "M12 3l8 3v6c0 5-3.5 8-8 9-4.5-1-8-4-8-9V6z",
  compute:
    "M7 7h10v10H7z M10 10h4v4h-4z M9 3v4M15 3v4M9 17v4M15 17v4M3 9h4M3 15h4M17 9h4M17 15h4",
  admin: "M4 6h10M18 6h2M4 12h4M12 12h8M4 18h12M20 18h0 M14 4v4M8 10v4M16 16v4",
  snapshots: "M12 3l9 5-9 5-9-5z M3 13l9 5 9-5",
  plus: "M12 5v14M5 12h14",
  search: "M11 4a7 7 0 100 14 7 7 0 000-14z M20 20l-4-4",
  chevronLeft: "M15 6l-6 6 6 6",
  chevronRight: "M9 6l6 6-6 6",
  bell: "M6 16V11a6 6 0 1112 0v5l2 2H4z M10 21h4",
  logout: "M9 4H5v16h4 M16 8l4 4-4 4 M20 12H9",
  menu: "M4 6h16M4 12h16M4 18h16",
  play: "M8 5l11 7-11 7z",
  stop: "M6 6h12v12H6z",
  more: "M5 12h.01M12 12h.01M19 12h.01",
  close: "M6 6l12 12M18 6L6 18",
} as const;

export type IconName = keyof typeof PATHS;

export function Icon({ name }: { name: IconName }) {
  return (
    <svg
      className="icon"
      width="18"
      height="18"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.8"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      focusable="false"
    >
      <path d={PATHS[name]} />
    </svg>
  );
}

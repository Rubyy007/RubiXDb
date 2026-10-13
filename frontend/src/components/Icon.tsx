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
  clock: "M12 7v5l3 2 M12 3a9 9 0 100 18 9 9 0 000-18z",
  table: "M3 5h18v14H3z M3 10h18M9 5v14",
  plan: "M5 5h6v4H5z M13 15h6v4h-6z M8 9v8h5",
  link: "M10 14a4 4 0 005.7 0l3-3a4 4 0 00-5.7-5.7l-1 1 M14 10a4 4 0 00-5.7 0l-3 3a4 4 0 005.7 5.7l1-1",
  reset: "M4 12a8 8 0 103 6.2 M4 19v-5h5",
  runAll: "M3 5l8 7-8 7z M12 5l8 7-8 7z",
  runAbove: "M6 10l7 5-7 5z M17 15V4 M13 8l4-4 4 4",
  moon: "M20 14.5A8 8 0 019.5 4a8 8 0 1010.5 10.5z",
  sun: "M12 8a4 4 0 100 8 4 4 0 000-8z M12 2v2 M12 20v2 M4.9 4.9l1.4 1.4 M17.7 17.7l1.4 1.4 M2 12h2 M20 12h2 M4.9 19.1l1.4-1.4 M17.7 6.3l1.4-1.4",
  refresh: "M20 12a8 8 0 01-14.3 4.9 M4 12A8 8 0 0118.3 7.1 M18 3v4.5h-4.5 M6 21v-4.5h4.5",
  panelRight: "M4 5h16v14H4z M15 5v14",
  panelLeft: "M4 5h16v14H4z M9 5v14",
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

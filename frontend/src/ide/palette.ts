export interface PaletteAction {
  id: string;
  label: string;
  group: string;
  shortcut?: string;
  disabled?: boolean;
  run: () => void;
}

/** Subsequence fuzzy score: higher is better, -1 means no match. Consecutive matches and matches at
 * word starts score higher; an exact substring beats a scattered match. */
export function fuzzyScore(query: string, text: string): number {
  const q = query.trim().toLowerCase();
  if (q === "") return 0;
  const t = text.toLowerCase();
  const sub = t.indexOf(q);
  if (sub >= 0) return 1000 - sub * 10 - (t.length - q.length);
  let score = 0;
  let ti = 0;
  let prev = -2;
  for (const ch of q) {
    const found = t.indexOf(ch, ti);
    if (found < 0) return -1;
    score += 10;
    if (found === prev + 1) score += 15;
    if (found === 0 || t[found - 1] === " ") score += 12;
    prev = found;
    ti = found + 1;
  }
  return score - (t.length - q.length);
}

export function filterActions<T extends { label: string }>(actions: T[], query: string): T[] {
  if (query.trim() === "") return actions;
  return actions
    .map((a) => ({ a, s: fuzzyScore(query, a.label) }))
    .filter((x) => x.s >= 0)
    .sort((x, y) => y.s - x.s)
    .map((x) => x.a);
}

/** "Did you mean ...?" for names the server could not find. Pure: the candidates come from the catalog the page
 * already loaded (which the API filtered for this key), so a suggestion can never reveal an object the key
 * may not see. */

function distance(a: string, b: string): number {
  const x = a.toLowerCase();
  const y = b.toLowerCase();
  let prev = Array.from({ length: y.length + 1 }, (_, j) => j);
  for (let i = 1; i <= x.length; i++) {
    const cur = [i];
    for (let j = 1; j <= y.length; j++) {
      cur[j] = Math.min(prev[j] + 1, cur[j - 1] + 1, prev[j - 1] + (x[i - 1] === y[j - 1] ? 0 : 1));
    }
    prev = cur;
  }
  return prev[y.length];
}

/** The closest candidates to `name`, best first. A candidate must be within roughly a third of the word's
 * length in edits (at least 1), or contain / be contained in the word, and is never the name itself. */
export function suggestNames(name: string, candidates: readonly string[], max = 3): string[] {
  const n = name.trim().replace(/^"|"$/g, "");
  if (n.length < 2) return [];
  const limit = Math.max(1, Math.floor(n.length / 3));
  const seen = new Set<string>();
  const scored: { c: string; d: number }[] = [];
  for (const c of candidates) {
    const key = c.toLowerCase();
    if (seen.has(key) || key === n.toLowerCase()) continue;
    seen.add(key);
    const d = distance(n, c);
    const related = key.startsWith(n.toLowerCase()) || n.toLowerCase().startsWith(key);
    if (d <= limit || (related && Math.abs(c.length - n.length) <= 4)) scored.push({ c, d });
  }
  return scored.sort((a, b) => a.d - b.d || a.c.localeCompare(b.c)).slice(0, max).map((s) => s.c);
}

/** What kind of object a NOT_FOUND error was about, from the server's message ("the referenced table was not found"). */
export function missingKind(message: string): "table" | "schema" | "database" | "index" | "object" | null {
  const m = /referenced (table|schema|database|index|object) was not found/i.exec(message);
  return m ? (m[1].toLowerCase() as "table" | "schema" | "database" | "index" | "object") : null;
}

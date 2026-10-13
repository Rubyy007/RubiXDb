/** Pure logic behind the Catalog "Schema design" diagram: which tables are related, and where to draw them.
 *
 * rubiXDB has NO foreign keys (`FOREIGN KEY` is refused at CREATE TABLE and the catalog API stores and
 * returns none), so a relationship line here is never a fact about the database. It is an inference from
 * names and types, drawn dashed and labelled as such. The rules are deliberately strict -- a missing line
 * is better than a false one:
 *
 *   A. `<stem>_id` / `<stem>id` column, and a table named after the stem (singular or plural), whose
 *      primary key is a single column of the same type.
 *   B. A column named like the singular of a table (`department` -> `departments`), same type as that
 *      table's single-column primary key.
 *   C. A column with the same name and type as another table's single-column primary key (not `id`).
 *
 * Only tables of the same schema are linked, and a table's own single-column primary key is never treated
 * as a reference. Nothing is read from data and nothing is stored. */

export interface ErdColumn {
  name: string;
  dataType: string;
  nullable: boolean;
  primaryKey: boolean;
}

export interface ErdTable {
  /** `schema.table`, unique. */
  key: string;
  schema: string;
  name: string;
  /** Empty when the columns could not be read (the card says so). */
  columns: ErdColumn[];
}

export type LinkRule = "id-suffix" | "table-name" | "same-key-name";

export interface ErdLink {
  id: string;
  /** The referencing side. */
  from: { table: string; column: string };
  /** The referenced primary key. */
  to: { table: string; column: string };
  rule: LinkRule;
  /** One sentence for the tooltip and the relationships list. */
  reason: string;
}

const lc = (s: string) => s.toLowerCase();

/** Spellings of `stem` that a table could be named: `order` -> order, orders, orderes; `category` -> categories. */
function variants(stem: string): string[] {
  const s = lc(stem);
  const out = [s, `${s}s`, `${s}es`];
  if (s.endsWith("y")) out.push(`${s.slice(0, -1)}ies`);
  return out;
}

const soleKey = (t: ErdTable): ErdColumn | null => {
  const pk = t.columns.filter((c) => c.primaryKey);
  return pk.length === 1 ? pk[0] : null;
};

export function inferLinks(tables: ErdTable[]): ErdLink[] {
  const links = new Map<string, ErdLink>();
  const add = (l: ErdLink) => {
    const existing = links.get(l.id);
    const rank = { "id-suffix": 0, "table-name": 1, "same-key-name": 2 } as const;
    if (!existing || rank[l.rule] < rank[existing.rule]) links.set(l.id, l);
  };

  for (const t of tables) {
    const own = soleKey(t);
    for (const c of t.columns) {
      if (own && own.name === c.name) continue; // a table's only key is its identity, not a reference
      const cname = lc(c.name);
      const stemMatch = /^(.+?)_?id$/.exec(cname);
      for (const u of tables) {
        if (u.key === t.key || u.schema !== t.schema) continue;
        const pk = soleKey(u);
        if (!pk || lc(pk.dataType) !== lc(c.dataType)) continue;
        const uname = lc(u.name);
        let rule: LinkRule | null = null;
        let reason = "";
        if (cname !== "id" && stemMatch && variants(stemMatch[1]).includes(uname)) {
          rule = "id-suffix";
          reason = `${t.name}.${c.name} ends in "id" and "${stemMatch[1]}" names the table ${u.name}`;
        } else if (variants(cname).includes(uname)) {
          rule = "table-name";
          reason = `${t.name}.${c.name} is named like the table ${u.name}`;
        } else if (cname !== "id" && cname.length >= 3 && cname === lc(pk.name)) {
          rule = "same-key-name";
          reason = `${t.name}.${c.name} has the name and type of the primary key of ${u.name}`;
        }
        if (rule) {
          add({
            id: `${t.key}.${c.name}->${u.key}.${pk.name}`,
            from: { table: t.key, column: c.name },
            to: { table: u.key, column: pk.name },
            rule,
            reason: `${reason} (${pk.dataType}); inferred from names, not enforced by the database`,
          });
        }
      }
    }
  }
  return [...links.values()];
}

// ---- layout ----------------------------------------------------------------------------------------

export const CARD_W = 248;
export const HEAD_H = 40;
export const ROW_H = 26;
export const MORE_H = 24;
/** Columns shown on a card before "+N more" (columns that carry a link are always shown). */
export const MAX_ROWS = 14;

export interface Pos {
  x: number;
  y: number;
}
export interface Box extends Pos {
  w: number;
  h: number;
}

/** Which columns a card draws, in table order: the first `MAX_ROWS`, plus any column that carries a link. */
export function visibleColumns(t: ErdTable, links: ErdLink[]): { shown: ErdColumn[]; hidden: number } {
  const linked = new Set<string>();
  for (const l of links) {
    if (l.from.table === t.key) linked.add(l.from.column);
    if (l.to.table === t.key) linked.add(l.to.column);
  }
  const shown = t.columns.filter((c, i) => i < MAX_ROWS || linked.has(c.name));
  return { shown, hidden: t.columns.length - shown.length };
}

export function cardHeight(t: ErdTable, links: ErdLink[]): number {
  const { shown, hidden } = visibleColumns(t, links);
  const rows = Math.max(1, shown.length); // an empty card still shows its "columns unavailable" row
  return HEAD_H + rows * ROW_H + (hidden > 0 ? MORE_H : 0) + 6;
}

/** y of the centre of column `name` on a card at `box.y` (the card's middle when the column is not drawn). */
export function columnY(t: ErdTable, links: ErdLink[], boxY: number, name: string): number {
  const { shown } = visibleColumns(t, links);
  const i = shown.findIndex((c) => c.name === name);
  return i < 0 ? boxY + cardHeight(t, links) / 2 : boxY + HEAD_H + i * ROW_H + ROW_H / 2;
}

const GAP_X = 120;
const GAP_Y = 36;
// Between two schema bands: more than a frame's padding + title (28 + 30 + 28) so frames never touch.
const GROUP_GAP = 132;
const ISOLATED_GAP = 56;
const ISOLATED_PER_ROW = 4;

/** Automatic placement, deterministic for the same input. One band per schema, stacked top to bottom.
 * Inside a band, related tables sit in columns -- a table that references others is left of the tables it
 * references -- and tables without any inferred relationship follow in a grid below them. */
export function layoutTables(tables: ErdTable[], links: ErdLink[]): Record<string, Box> {
  const out: Record<string, Box> = {};
  const schemas = [...new Set(tables.map((t) => t.schema))];
  let top = 0;
  for (const schema of schemas) {
    const group = tables.filter((t) => t.schema === schema);
    const keys = new Set(group.map((t) => t.key));
    const mine = links.filter((l) => keys.has(l.from.table) && keys.has(l.to.table));
    const touched = new Set(mine.flatMap((l) => [l.from.table, l.to.table]));
    const related = group.filter((t) => touched.has(t.key));
    const isolated = group.filter((t) => !touched.has(t.key));

    // rank 0 = references nobody (a "parent"); a table is one rank above the highest parent it references.
    const rank: Record<string, number> = {};
    const rankOf = (k: string, seen: Set<string>): number => {
      if (rank[k] !== undefined) return rank[k];
      if (seen.has(k)) return 0; // a cycle: stop descending
      seen.add(k);
      const parents = mine.filter((l) => l.from.table === k).map((l) => l.to.table);
      const r = parents.length ? 1 + Math.max(...parents.map((p) => rankOf(p, seen))) : 0;
      seen.delete(k);
      rank[k] = r;
      return r;
    };
    for (const t of related) rankOf(t.key, new Set());
    const maxRank = Math.max(0, ...related.map((t) => rank[t.key]));

    let bottom = top;
    for (let col = 0; col <= maxRank; col++) {
      let y = top;
      for (const t of related.filter((x) => maxRank - rank[x.key] === col)) {
        const h = cardHeight(t, links);
        out[t.key] = { x: col * (CARD_W + GAP_X), y, w: CARD_W, h };
        y += h + GAP_Y;
      }
      bottom = Math.max(bottom, y - GAP_Y);
    }
    if (isolated.length > 0) {
      let y = related.length > 0 ? bottom + ISOLATED_GAP : top;
      for (let i = 0; i < isolated.length; i += ISOLATED_PER_ROW) {
        const row = isolated.slice(i, i + ISOLATED_PER_ROW);
        row.forEach((t, j) => {
          out[t.key] = { x: j * (CARD_W + GAP_X / 2), y, w: CARD_W, h: cardHeight(t, links) };
        });
        y += Math.max(...row.map((t) => cardHeight(t, links))) + GAP_Y;
      }
      bottom = Math.max(bottom, y - GAP_Y);
    }
    top = (group.length > 0 ? bottom : top) + GROUP_GAP;
  }
  return out;
}

export interface GroupFrame extends Box {
  schema: string;
}

/** A frame around each schema's cards, from where the cards actually are (so dragging a card moves it). */
export function groupFrames(tables: ErdTable[], boxes: Record<string, Box>, pad = 28, head = 30): GroupFrame[] {
  const frames: GroupFrame[] = [];
  for (const schema of [...new Set(tables.map((t) => t.schema))]) {
    const bs = tables.filter((t) => t.schema === schema).map((t) => boxes[t.key]).filter(Boolean);
    if (bs.length === 0) continue;
    const x0 = Math.min(...bs.map((b) => b.x));
    const y0 = Math.min(...bs.map((b) => b.y));
    const x1 = Math.max(...bs.map((b) => b.x + b.w));
    const y1 = Math.max(...bs.map((b) => b.y + b.h));
    frames.push({ schema, x: x0 - pad, y: y0 - pad - head, w: x1 - x0 + pad * 2, h: y1 - y0 + pad * 2 + head });
  }
  return frames;
}

/** SVG path for a link between two cards: leaves the side that faces the other card, ends with a horizontal
 * run into the referenced key. `fromY` / `toY` are the column rows' centres. */
export function linkPath(from: Box, fromY: number, to: Box, toY: number): { d: string; start: Pos; end: Pos } {
  const fromCentre = from.x + from.w / 2;
  const toCentre = to.x + to.w / 2;
  let sx: number;
  let ex: number;
  let c1: number;
  let c2: number;
  if (Math.abs(fromCentre - toCentre) < CARD_W * 0.6) {
    // Same column: loop out of the right-hand side of both cards.
    sx = from.x + from.w;
    ex = to.x + to.w;
    const bulge = 56;
    c1 = sx + bulge;
    c2 = ex + bulge;
  } else if (toCentre > fromCentre) {
    sx = from.x + from.w;
    ex = to.x;
    const k = Math.max(40, (ex - sx) / 2);
    c1 = sx + k;
    c2 = ex - k;
  } else {
    sx = from.x;
    ex = to.x + to.w;
    const k = Math.max(40, (sx - ex) / 2);
    c1 = sx - k;
    c2 = ex + k;
  }
  const f = (n: number) => Math.round(n * 10) / 10;
  return {
    d: `M ${f(sx)} ${f(fromY)} C ${f(c1)} ${f(fromY)}, ${f(c2)} ${f(toY)}, ${f(ex)} ${f(toY)}`,
    start: { x: sx, y: fromY },
    end: { x: ex, y: toY },
  };
}

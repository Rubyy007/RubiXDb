/** Completion for `db.`, `schema.` and `table.` typed in a cell. Pure functions; the catalog lists
 * come from the existing `/v1/catalog/*` queries and columns from `/v1/catalog/tables/:name`. */

export interface CatalogLists {
  databases: { id: number; name: string }[];
  schemas: { id: number; databaseId: number; name: string }[];
  tables: { id: number; schemaId: number; name: string }[];
}

export interface CompletionContext {
  /** Dotted path typed before the final dot, e.g. ["default", "public"]. */
  base: string[];
  /** What follows the final dot (may be empty). */
  partial: string;
  /** Offset in the text where `partial` starts. */
  start: number;
}

export function completionContext(text: string, caret: number): CompletionContext | null {
  const before = text.slice(0, caret);
  const m = /([A-Za-z_]\w*(?:\.[A-Za-z_]\w*)*)\.(\w*)$/.exec(before);
  if (!m) return null;
  return { base: m[1].split("."), partial: m[2], start: caret - m[2].length };
}

export type Resolved =
  | { kind: "schemas"; items: string[] }
  | { kind: "tables"; items: string[] }
  | { kind: "columns"; table: string };

export function resolveBase(base: string[], cat: CatalogLists): Resolved | null {
  const lower = (s: string) => s.toLowerCase();
  const db = cat.databases.find((d) => lower(d.name) === lower(base[0]));
  if (base.length === 1) {
    if (db) return { kind: "schemas", items: cat.schemas.filter((s) => s.databaseId === db.id).map((s) => s.name) };
    const schema = cat.schemas.find((s) => lower(s.name) === lower(base[0]));
    if (schema) return { kind: "tables", items: cat.tables.filter((t) => t.schemaId === schema.id).map((t) => t.name) };
    const table = cat.tables.find((t) => lower(t.name) === lower(base[0]));
    return table ? { kind: "columns", table: table.name } : null;
  }
  if (base.length === 2 && db) {
    const schema = cat.schemas.find((s) => s.databaseId === db.id && lower(s.name) === lower(base[1]));
    if (schema) return { kind: "tables", items: cat.tables.filter((t) => t.schemaId === schema.id).map((t) => t.name) };
  }
  // schema.table. or db.schema.table. -> columns of the last segment when it is a known table.
  const last = base[base.length - 1];
  const table = cat.tables.find((t) => lower(t.name) === lower(last));
  return table ? { kind: "columns", table: table.name } : null;
}

export function filterItems(items: string[], partial: string, max = 12): string[] {
  const p = partial.toLowerCase();
  return items
    .filter((x) => x.toLowerCase().startsWith(p))
    .sort((a, b) => a.localeCompare(b))
    .slice(0, max);
}

/** Replace the typed partial with the chosen completion. */
export function applyCompletion(text: string, ctx: CompletionContext, choice: string): { text: string; caret: number } {
  const end = ctx.start + ctx.partial.length;
  const next = text.slice(0, ctx.start) + choice + text.slice(end);
  return { text: next, caret: ctx.start + choice.length };
}

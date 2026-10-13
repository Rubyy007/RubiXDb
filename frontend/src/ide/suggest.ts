/** Context-aware suggestions for the SQL IDE editor. Pure functions: the catalog and the column lists are
 * passed in, nothing is fetched here.
 *
 *  - after `FROM` / `JOIN` / `INTO` / `UPDATE` / `DROP TABLE` ...  -> tables (and schemas, CTE names)
 *  - in a select list, `WHERE`, `ON`, `SET`, `GROUP BY` ...         -> columns of the tables the statement uses
 *    (aliases included), then functions and keywords
 *  - `table.` / `alias.` / `schema.` / `db.schema.`                   -> columns / tables / schemas
 *  - elsewhere inside a statement                                     -> the keyword that usually comes next
 *  - at the start of a statement                                      -> statement starters
 *
 * Nothing is suggested inside strings or comments, and a suggestion that is exactly what was typed is
 * dropped, so a finished word never keeps a popup open that would swallow Enter. */

import { completionContext, resolveBase, type CatalogLists } from "./completion";
import { insideLiteral } from "./autoUpper";
import { CLAUSE_KEYWORDS, FUNCTIONS, STATEMENT_SNIPPETS } from "./keywords";

export type SuggestKind = "keyword" | "function" | "table" | "column" | "schema" | "database" | "snippet";

export interface Suggestion {
  label: string;
  /** Text that replaces what was typed. */
  insert: string;
  kind: SuggestKind;
  detail?: string;
}

export interface ColumnInfo {
  name: string;
  type?: string;
}

export interface SuggestResult {
  items: Suggestion[];
  /** The range replaced when a suggestion is accepted: the partial word before the caret. */
  start: number;
  end: number;
  partial: string;
  /** True when the list opened without the user typing a letter (after `FROM `, `WHERE `, ...). */
  auto: boolean;
}

export interface SuggestInput {
  text: string;
  caret: number;
  catalog: CatalogLists;
  /** Columns of a catalog table, or `undefined` while they have not been loaded. */
  columns: (table: string) => ColumnInfo[] | undefined;
  /** Ctrl+Space: also offer a list when nothing has been typed. */
  manual?: boolean;
}

const MAX_ITEMS = 12;
const WORD_CHAR = /[A-Za-z0-9_]/;

// ---- a small lexer ------------------------------------------------------------------------------------

interface Tok {
  /** Upper-cased word, the punctuation character, or a placeholder for a literal. */
  t: string;
  kind: "word" | "num" | "str" | "p";
  raw: string;
}

function lex(src: string): Tok[] {
  const out: Tok[] = [];
  const n = src.length;
  let i = 0;
  while (i < n) {
    const c = src[i];
    const d = src[i + 1];
    if (/\s/.test(c)) i += 1;
    else if (c === "-" && d === "-") {
      const j = src.indexOf("\n", i);
      i = j < 0 ? n : j;
    } else if (c === "/" && d === "*") {
      const j = src.indexOf("*/", i + 2);
      i = j < 0 ? n : j + 2;
    } else if (c === "'") {
      let j = i + 1;
      while (j < n) {
        if (src[j] === "'") {
          if (src[j + 1] === "'") j += 2;
          else break;
        } else j += 1;
      }
      out.push({ t: "'", kind: "str", raw: src.slice(i, j + 1) });
      i = j + 1;
    } else if (c === '"') {
      const j = src.indexOf('"', i + 1);
      const end = j < 0 ? n : j;
      const raw = src.slice(i + 1, end);
      out.push({ t: raw, kind: "word", raw });
      i = end + 1;
    } else if (/[0-9]/.test(c)) {
      let j = i + 1;
      while (j < n && /[0-9.]/.test(src[j])) j += 1;
      out.push({ t: "0", kind: "num", raw: src.slice(i, j) });
      i = j;
    } else if (/[A-Za-z_]/.test(c)) {
      let j = i + 1;
      while (j < n && WORD_CHAR.test(src[j])) j += 1;
      const raw = src.slice(i, j);
      out.push({ t: raw.toUpperCase(), kind: "word", raw });
      i = j;
    } else {
      out.push({ t: c, kind: "p", raw: c });
      i += 1;
    }
  }
  return out;
}

/** Start of the statement that contains `offset`: just after the last top-level `;` before it. */
function statementStart(text: string, offset: number): number {
  let start = 0;
  let i = 0;
  while (i < offset) {
    const c = text[i];
    const d = text[i + 1];
    if (c === "-" && d === "-") {
      const j = text.indexOf("\n", i);
      i = j < 0 ? text.length : j;
    } else if (c === "/" && d === "*") {
      const j = text.indexOf("*/", i + 2);
      i = j < 0 ? text.length : j + 2;
    } else if (c === "'" || c === '"') {
      let j = i + 1;
      while (j < text.length) {
        if (text[j] === c) {
          if (text[j + 1] === c) j += 2;
          else break;
        } else j += 1;
      }
      i = j + 1;
    } else {
      if (c === ";") start = i + 1;
      i += 1;
    }
  }
  return start;
}

function statementEnd(text: string, offset: number): number {
  let i = offset;
  while (i < text.length) {
    const c = text[i];
    const d = text[i + 1];
    if (c === "-" && d === "-") {
      const j = text.indexOf("\n", i);
      i = j < 0 ? text.length : j;
    } else if (c === "/" && d === "*") {
      const j = text.indexOf("*/", i + 2);
      i = j < 0 ? text.length : j + 2;
    } else if (c === "'" || c === '"') {
      let j = i + 1;
      while (j < text.length) {
        if (text[j] === c) {
          if (text[j + 1] === c) j += 2;
          else break;
        } else j += 1;
      }
      i = j + 1;
    } else if (c === ";") return i;
    else i += 1;
  }
  return text.length;
}

// ---- which tables a statement uses -----------------------------------------------------------------------

export interface TableUse {
  /** The catalog table's name. */
  table: string;
  alias?: string;
}

const NOT_ALIAS = new Set([
  "WHERE", "JOIN", "INNER", "LEFT", "RIGHT", "FULL", "CROSS", "OUTER", "ON", "USING", "GROUP", "ORDER", "HAVING", "LIMIT", "OFFSET",
  "UNION", "INTERSECT", "EXCEPT", "SET", "VALUES", "SELECT", "AS", "LATERAL", "QUALIFY", "WINDOW", "RETURNING", "FROM", "INTO",
]);

/** Tables named after FROM / JOIN / UPDATE / INTO (and `CREATE INDEX .. ON t`) in the statement around `caret`,
 * matched against the catalog; with their aliases. */
export function referencedTables(text: string, caret: number, cat: CatalogLists): TableUse[] {
  const s0 = statementStart(text, caret);
  const toks = lex(text.slice(s0, statementEnd(text, caret)));
  const first = toks[0]?.t;
  const out: TableUse[] = [];
  const lower = (x: string) => x.toLowerCase();
  const find = (path: string[]): string | null => {
    const name = path[path.length - 1];
    const schema = path.length >= 2 ? path[path.length - 2] : null;
    const hit = cat.tables.find((t) => {
      if (lower(t.name) !== lower(name)) return false;
      if (!schema) return true;
      return cat.schemas.some((s) => s.id === t.schemaId && lower(s.name) === lower(schema));
    });
    return hit ? hit.name : null;
  };
  const readName = (i: number): { path: string[]; next: number } | null => {
    if (!toks[i] || toks[i].kind !== "word") return null;
    const path = [toks[i].raw];
    let j = i + 1;
    while (toks[j]?.t === "." && toks[j + 1]?.kind === "word") {
      path.push(toks[j + 1].raw);
      j += 2;
    }
    return { path, next: j };
  };
  for (let i = 0; i < toks.length; i++) {
    const kw = toks[i].kind === "word" ? toks[i].t : "";
    const takes = kw === "FROM" || kw === "JOIN" || kw === "UPDATE" || kw === "INTO" || (kw === "ON" && first === "CREATE");
    if (!takes) continue;
    let at = i + 1;
    for (;;) {
      const nm = readName(at);
      if (!nm) break;
      let next = nm.next;
      let alias: string | undefined;
      if (toks[next]?.t === "AS" && toks[next + 1]?.kind === "word") {
        alias = toks[next + 1].raw;
        next += 2;
      } else if (toks[next]?.kind === "word" && !NOT_ALIAS.has(toks[next].t)) {
        alias = toks[next].raw;
        next += 1;
      }
      const table = find(nm.path);
      if (table) out.push({ table, alias });
      // `FROM a, b`: a comma list continues; JOIN / UPDATE / INTO name one table.
      if (kw === "FROM" && toks[next]?.t === ",") at = next + 1;
      else break;
    }
  }
  return out;
}

/** Names of `WITH name AS (` common table expressions in the statement. */
function cteNames(text: string, caret: number): string[] {
  const s0 = statementStart(text, caret);
  const toks = lex(text.slice(s0, statementEnd(text, caret)));
  const out: string[] = [];
  for (let i = 0; i < toks.length - 2; i++) {
    if (toks[i].kind !== "word" || toks[i + 1].t !== "AS" || toks[i + 2].t !== "(") continue;
    const before = toks[i - 1]?.t;
    if (before === "WITH" || before === "," || before === "RECURSIVE") out.push(toks[i].raw);
  }
  return out;
}

/** Tables whose columns the editor should load before it can suggest columns here. */
export function neededTables(text: string, caret: number, cat: CatalogLists): string[] {
  const names = new Set(referencedTables(text, caret, cat).map((u) => u.table));
  const ctx = completionContext(text, caret);
  if (ctx) {
    const r = resolveBase(ctx.base, cat);
    if (r?.kind === "columns") names.add(r.table);
    const alias = ctx.base.length === 1 ? referencedTables(text, caret, cat).find((u) => u.alias && u.alias.toLowerCase() === ctx.base[0].toLowerCase()) : undefined;
    if (alias) names.add(alias.table);
  }
  return [...names];
}

// ---- what is expected at the caret -------------------------------------------------------------------------

type Mode = "start" | "table" | "column" | "values" | "after" | "ddl-name";

const COLUMN_STARTERS = new Set(["SELECT", "WHERE", "AND", "OR", "ON", "HAVING", "BY", "SET", "WHEN", "THEN", "ELSE", "CASE", "NOT", "DISTINCT", "QUALIFY", "USING", "IN", "LIKE", "BETWEEN", "IS", "OVER"]);
const OPERATORS = new Set(["=", "<", ">", "!", "+", "-", "*", "/", "%", "|", "(", ","]);
const CLAUSES = new Set(["SELECT", "FROM", "JOIN", "WHERE", "GROUP", "ORDER", "HAVING", "SET", "INTO", "UPDATE", "VALUES", "ON", "LIMIT", "OFFSET", "QUALIFY", "UNION", "INTERSECT", "EXCEPT", "WITH"]);

/** Nearest clause keyword before the end of `toks`, skipping over whole `( ... )` groups. */
function nearestClause(toks: Tok[]): string | null {
  for (let i = toks.length - 1; i >= 0; i--) {
    if (toks[i].t === ")") {
      let depth = 1;
      while (--i >= 0 && depth > 0) {
        if (toks[i].t === ")") depth += 1;
        else if (toks[i].t === "(") depth -= 1;
      }
      i += 1;
      continue;
    }
    if (toks[i].kind === "word" && CLAUSES.has(toks[i].t)) return toks[i].t;
  }
  return null;
}

function modeAt(toks: Tok[]): { mode: Mode; clause: string | null } {
  if (toks.length === 0) return { mode: "start", clause: null };
  const last = toks[toks.length - 1];
  const first = toks[0].t;
  const clause = nearestClause(toks);
  const word = last.kind === "word" ? last.t : "";
  const prev = toks[toks.length - 2]?.t;

  if (word === "FROM" || word === "JOIN" || word === "UPDATE" || word === "INTO") return { mode: "table", clause };
  if (word === "TABLE") return { mode: first === "DROP" || first === "ALTER" ? "table" : "ddl-name", clause };
  if (word === "EXISTS" && prev === "IF") return { mode: first === "DROP" ? "table" : "ddl-name", clause };
  if (word === "ON" && first === "CREATE") return { mode: "table", clause };
  if (last.t === "," && clause === "FROM") return { mode: "table", clause };
  if (word === "INDEX" || word === "SCHEMA" || word === "DATABASE") return { mode: first === "DROP" ? "table" : "ddl-name", clause };

  if (clause === "VALUES") return { mode: "values", clause };

  if (word === "IS") return { mode: "after", clause: "IS" };
  if (word && COLUMN_STARTERS.has(word)) return { mode: "column", clause };
  // `SELECT *` / `count(*)`: the star is a whole item, what follows is a keyword.
  if (last.t === "*" && (prev === "SELECT" || prev === "," || prev === "DISTINCT" || prev === "(")) return { mode: "after", clause };
  if (last.kind === "p" && OPERATORS.has(last.t)) {
    // `FROM (` / `JOIN (` opens a subquery; a comma after a FROM item was handled above.
    if (clause === "FROM" || clause === "JOIN") return { mode: last.t === "(" ? "start" : "after", clause };
    return { mode: "column", clause };
  }
  return { mode: "after", clause };
}

/** Keywords that most often follow, by clause. Listed first; the rest of `CLAUSE_KEYWORDS` follows. */
const NEXT: Record<string, string[]> = {
  SELECT: ["FROM", "AS", "DISTINCT", "WHERE"],
  FROM: ["WHERE", "GROUP BY", "ORDER BY", "LIMIT", "JOIN", "LEFT JOIN", "INNER JOIN", "AS", "UNION"],
  JOIN: ["ON", "USING", "WHERE", "LEFT JOIN", "INNER JOIN", "GROUP BY", "ORDER BY"],
  ON: ["AND", "OR", "WHERE", "JOIN", "LEFT JOIN", "GROUP BY", "ORDER BY"],
  WHERE: ["AND", "OR", "GROUP BY", "ORDER BY", "LIMIT", "IN", "LIKE", "BETWEEN", "IS NULL", "IS NOT NULL", "NOT"],
  GROUP: ["HAVING", "ORDER BY", "LIMIT"],
  HAVING: ["AND", "OR", "ORDER BY", "LIMIT"],
  ORDER: ["ASC", "DESC", "LIMIT", "OFFSET"],
  LIMIT: ["OFFSET"],
  SET: ["WHERE", "AND"],
  UPDATE: ["SET"],
  INTO: ["VALUES", "SELECT"],
  VALUES: [],
  IS: ["NULL", "NOT NULL", "TRUE", "FALSE"],
};

const DDL_ONLY = new Set(["PRIMARY KEY", "NOT NULL", "UNIQUE", "DEFAULT", "IF EXISTS", "IF NOT EXISTS", "ADD COLUMN", "RENAME TO"]);
const CASE_ONLY = new Set(["WHEN", "THEN", "ELSE", "END"]);

/** Keywords that fit here: CASE words only inside a CASE, DDL words only in DDL, INSERT words only in INSERT. */
function keywordItems(clause: string | null, partial: string, toks: Tok[]): Suggestion[] {
  const preferred = (clause ? NEXT[clause] : undefined) ?? [];
  const first = toks[0]?.t;
  const ddl = first === "CREATE" || first === "ALTER" || first === "DROP";
  const caseDepth = toks.filter((t) => t.t === "CASE").length - toks.filter((t) => t.t === "END").length;
  const fits = (k: string) => {
    if (CASE_ONLY.has(k) && caseDepth <= 0) return false;
    if (DDL_ONLY.has(k) && !ddl && !preferred.includes(k)) return false;
    if ((k === "VALUES" || k === "INTO") && first !== "INSERT") return false;
    if (k === "SET" && first !== "UPDATE" && first !== "ALTER") return false;
    return true;
  };
  const all = [...preferred, ...CLAUSE_KEYWORDS.filter((k) => !preferred.includes(k))].filter(fits);
  const p = partial.toUpperCase();
  return all.filter((k) => k.startsWith(p)).map((k) => ({ label: k, insert: `${k} `, kind: "keyword" as const }));
}

// ---- the entry point ---------------------------------------------------------------------------------------

const same = (a: string, b: string) => a.toLowerCase() === b.toLowerCase();

function dropExact(items: Suggestion[], partial: string): Suggestion[] {
  return items.filter((i) => !same(i.label, partial) && !same(i.insert.trim(), partial));
}

export function suggest({ text, caret, catalog, columns, manual = false }: SuggestInput): SuggestResult | null {
  if (caret < 0 || caret > text.length) return null;
  if (insideLiteral(text, caret)) return null;
  // Editing in the middle of a word: no list (it would fight the letters already there).
  if (caret < text.length && WORD_CHAR.test(text[caret])) return null;

  const before = text.slice(0, caret);
  const m = /[A-Za-z_][A-Za-z0-9_]*$/.exec(before);
  const partial = m ? m[0] : "";
  const start = caret - partial.length;
  // `12abc`: the letters belong to a number.
  if (partial && start > 0 && /[0-9]/.test(text[start - 1])) return null;

  const done = (items: Suggestion[], auto: boolean): SuggestResult | null => {
    const kept = dropExact(items, partial).slice(0, MAX_ITEMS);
    return kept.length ? { items: kept, start, end: caret, partial, auto } : null;
  };

  const uses = referencedTables(text, caret, catalog);
  const lower = (s: string) => s.toLowerCase();
  const tableByName = (name: string) => catalog.tables.find((t) => lower(t.name) === lower(name));
  const schemaName = (id: number) => catalog.schemas.find((s) => s.id === id)?.name ?? "";
  const colsOf = (table: string): Suggestion[] =>
    (columns(table) ?? []).map((c) => ({ label: c.name, insert: c.name, kind: "column" as const, detail: c.type ? `${table} · ${c.type}` : table }));

  // ---- dotted: `x.`  -------------------------------------------------------------------------------------
  if (start > 0 && text[start - 1] === ".") {
    const ctx = completionContext(text, caret);
    if (!ctx) return null;
    const p = lower(ctx.partial);
    let list: Suggestion[] = [];
    const alias = ctx.base.length === 1 ? uses.find((u) => u.alias && lower(u.alias) === lower(ctx.base[0])) : undefined;
    if (alias) list = colsOf(alias.table);
    else {
      const r = resolveBase(ctx.base, catalog);
      if (r?.kind === "columns") list = colsOf(r.table);
      else if (r?.kind === "tables") list = r.items.map((name) => ({ label: name, insert: name, kind: "table" as const }));
      else if (r?.kind === "schemas") list = r.items.map((name) => ({ label: name, insert: name, kind: "schema" as const }));
    }
    const items = list.filter((x) => lower(x.label).startsWith(p));
    if (list.length && list[0].kind !== "column") items.sort((a, b) => a.label.localeCompare(b.label));
    if (!items.length) return null;
    return { items: items.slice(0, MAX_ITEMS), start: ctx.start, end: caret, partial: ctx.partial, auto: partial === "" };
  }

  const stmt0 = statementStart(text, start);
  const toks = lex(text.slice(stmt0, start));
  const { mode, clause } = modeAt(toks);

  // `INSERT INTO t (` / `CREATE INDEX i ON t (`: the columns of that table, minus the ones already listed.
  const open = /\b(?:INSERT\s+INTO|ON)\s+((?:"[^"]+"|[A-Za-z_]\w*)(?:\s*\.\s*(?:"[^"]+"|[A-Za-z_]\w*))*)\s*\(([^()]*)$/i.exec(text.slice(stmt0, start));
  if (open && (toks[0]?.t === "INSERT" || toks[0]?.t === "CREATE")) {
    const t = tableByName(open[1].split(".").pop()!.replace(/"/g, "").trim());
    if (t) {
      const used = new Set(open[2].split(",").map((s) => lower(s.trim())).filter(Boolean));
      const items = colsOf(t.name).filter((c) => !used.has(lower(c.label)) && lower(c.label).startsWith(lower(partial)));
      if (partial || manual || items.length) return done(items, partial === "");
    }
  }

  if (mode === "table") {
    const ctes = cteNames(text, caret).map((n) => ({ label: n, insert: n, kind: "table" as const, detail: "CTE" }));
    const dup = new Map<string, number>();
    for (const t of catalog.tables) dup.set(lower(t.name), (dup.get(lower(t.name)) ?? 0) + 1);
    type Cand = Suggestion & { bare?: string };
    const tables: Cand[] = catalog.tables.map((t) => {
      const sch = schemaName(t.schemaId);
      const qualify = (dup.get(lower(t.name)) ?? 0) > 1 && sch;
      return { label: qualify ? `${sch}.${t.name}` : t.name, insert: qualify ? `${sch}.${t.name}` : t.name, kind: "table" as const, detail: sch, bare: t.name };
    });
    const schemas = catalog.schemas.map((s) => ({ label: s.name, insert: `${s.name}.`, kind: "schema" as const, detail: "schema" }));
    const p = lower(partial);
    // Tables first, then CTEs, then schemas; alphabetical inside each group.
    const rank = (x: Suggestion) => (x.kind === "schema" ? 2 : x.detail === "CTE" ? 1 : 0);
    const items: Suggestion[] = ([...tables, ...ctes, ...schemas] as Cand[])
      .filter((x: Cand) => lower(x.label).startsWith(p) || lower(x.bare ?? "").startsWith(p))
      .sort((a, b) => rank(a) - rank(b) || a.label.localeCompare(b.label))
      .map(({ label, insert, kind, detail }) => ({ label, insert, kind, detail }));
    if (!partial && !manual && !items.length) return null;
    return done(items, partial === "");
  }

  if (mode === "ddl-name" || mode === "values") {
    // A new name or a literal is expected: nothing to suggest from the catalog.
    if (!manual) return null;
  }

  if (mode === "start") {
    if (!partial && !manual) return null;
    const p = partial.toUpperCase();
    const items = STATEMENT_SNIPPETS.filter((s) => s.label.startsWith(p)).map((s) => ({ label: s.label, insert: s.insert, kind: "snippet" as const, detail: s.detail }));
    return done(items, false);
  }

  if (mode === "column") {
    // Columns of every table the statement uses; a name found in several tables is offered qualified.
    const seen = new Map<string, number>();
    const mine: { c: Suggestion; qual: string }[] = [];
    for (const u of uses) {
      for (const c of colsOf(u.table)) {
        seen.set(lower(c.label), (seen.get(lower(c.label)) ?? 0) + 1);
        mine.push({ c, qual: u.alias ?? u.table });
      }
    }
    const p = lower(partial);
    const columnItems = mine
      .map(({ c, qual }) => ((seen.get(lower(c.label)) ?? 0) > 1 ? { ...c, label: `${qual}.${c.label}`, insert: `${qual}.${c.label}` } : c))
      .filter((c) => lower(c.label).startsWith(p) || (p && lower(c.label.split(".").pop() ?? "").startsWith(p)));
    const long = partial.length >= 2 || manual;
    const fnItems = long ? FUNCTIONS.filter((f) => f.startsWith(partial.toUpperCase()) && f !== "EXISTS" && f !== "CAST").map((f) => ({ label: f, insert: `${f}(`, kind: "function" as const })) : [];
    const kwItems = long ? keywordItems(clause, partial, toks).filter((k) => ["NOT", "NULL", "CASE", "DISTINCT", "EXISTS", "TRUE", "FALSE", "AS", "FROM", "WHERE", "AND", "OR", "IN", "LIKE", "BETWEEN", "IS NULL", "IS NOT NULL", "ASC", "DESC", "ORDER BY", "GROUP BY", "LIMIT", "OFFSET", "HAVING", "ON", "JOIN", "LEFT JOIN", "INNER JOIN"].includes(k.label)) : [];
    // With nothing typed, only columns open the list on their own (after SELECT / WHERE / AND ...).
    const afterWord = toks[toks.length - 1]?.kind === "word";
    if (!partial && !manual && !(afterWord && columnItems.length)) return null;
    return done([...columnItems, ...fnItems, ...kwItems], partial === "");
  }

  // "after": the keyword that usually comes next. Needs two typed letters (or Ctrl+Space).
  if (partial.length < 2 && !manual) return null;
  return done(keywordItems(clause, partial, toks), false);
}

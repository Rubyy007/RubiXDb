import type { SqlColumnMeta, SqlValue } from "../api/types";
import { formatSqlValue } from "../utils/sqlValue";

/** Client-side conversions of a result set that is already in memory. No request is made. */

function jsonValue(v: SqlValue): unknown {
  switch (v.type) {
    case "null":
      return null;
    case "boolean":
    case "integer":
    case "real":
    case "double":
      return v.value;
    default:
      // bigint, decimal, text, date, time, timestamp, blob: strings, so no precision is lost.
      return formatSqlValue(v);
  }
}

export function toJson(columns: SqlColumnMeta[], rows: SqlValue[][]): string {
  const objects = rows.map((row) => {
    const o: Record<string, unknown> = {};
    columns.forEach((c, i) => {
      o[c.name] = jsonValue(row[i] ?? { type: "null" });
    });
    return o;
  });
  return JSON.stringify(objects, null, 2);
}

const PLAIN_IDENT = /^[A-Za-z_][A-Za-z0-9_]*$/;

function ident(name: string): string {
  return PLAIN_IDENT.test(name) ? name : `"${name.replace(/"/g, '""')}"`;
}

function quote(s: string): string {
  return `'${s.replace(/'/g, "''")}'`;
}

/** SQL literal for a value, or null when the value has no literal form (blob). */
export function sqlLiteral(v: SqlValue): string | null {
  switch (v.type) {
    case "null":
      return "NULL";
    case "boolean":
      return v.value ? "TRUE" : "FALSE";
    case "integer":
    case "real":
    case "double":
      return Number.isFinite(v.value) ? String(v.value) : null;
    case "bigint":
      return v.value;
    case "decimal":
      return formatSqlValue(v);
    case "text":
    case "date":
    case "time":
    case "timestamp":
      return quote(v.value);
    case "blob":
      return null;
  }
}

/** Table name guess for generated INSERTs: the first identifier after FROM in the cell's SQL. */
export function guessTable(sql: string): string {
  const m = /\bFROM\s+("[^"]+"|[A-Za-z_][A-Za-z0-9_.]*)/i.exec(sql);
  return m ? m[1] : "table_name";
}

export interface InsertScript {
  text: string;
  /** Cells with no literal form (blobs), written as NULL; the caller tells the user. */
  skipped: number;
}

export function toInserts(table: string, columns: SqlColumnMeta[], rows: SqlValue[][]): InsertScript {
  let skipped = 0;
  const cols = columns.map((c) => ident(c.name)).join(", ");
  const lines = rows.map((row) => {
    const vals = columns.map((_, i) => {
      const lit = sqlLiteral(row[i] ?? { type: "null" });
      if (lit === null) {
        skipped++;
        return "NULL";
      }
      return lit;
    });
    return `INSERT INTO ${table} (${cols}) VALUES (${vals.join(", ")});`;
  });
  return { text: lines.join("\n") + (lines.length ? "\n" : ""), skipped };
}

/** A single-cell result is shown as a number card instead of a grid. */
export function scalarOf(columns: SqlColumnMeta[], rows: SqlValue[][]): { label: string; value: string } | null {
  if (columns.length !== 1 || rows.length !== 1) return null;
  return { label: columns[0].name, value: formatSqlValue(rows[0][0] ?? { type: "null" }) };
}

// ---- profile -------------------------------------------------------------------------------------

export interface ColumnProfile {
  name: string;
  type: string | null;
  nulls: number;
  min: string | null;
  max: string | null;
}

export interface Profile {
  rows: number;
  textColumn: { name: string; distinct: number } | null;
  columns: ColumnProfile[];
}

function numericKey(v: SqlValue): number | null {
  switch (v.type) {
    case "integer":
    case "real":
    case "double":
      return v.value;
    case "bigint":
    case "decimal": {
      const n = Number(formatSqlValue(v));
      return Number.isFinite(n) ? n : null;
    }
    default:
      return null;
  }
}

/** Summary computed from the rows the cell already holds (no follow-up request). Numeric min/max
 * compare as JavaScript numbers, so bigint/decimal ordering is approximate beyond 2^53 while the
 * value shown is still the exact original text. */
export function profileRows(columns: SqlColumnMeta[], rows: SqlValue[][]): Profile {
  const textIdx = columns.findIndex((c) => (c.type ?? "").toLowerCase() === "text");
  let textColumn: Profile["textColumn"] = null;
  if (textIdx >= 0) {
    const seen = new Set<string>();
    for (const r of rows) {
      const v = r[textIdx];
      if (v && v.type === "text") seen.add(v.value);
    }
    textColumn = { name: columns[textIdx].name, distinct: seen.size };
  }
  const cols: ColumnProfile[] = columns.map((c, i) => {
    let nulls = 0;
    let min: { k: number; v: SqlValue } | null = null;
    let max: { k: number; v: SqlValue } | null = null;
    for (const r of rows) {
      const v = r[i];
      if (!v || v.type === "null") {
        nulls++;
        continue;
      }
      const k = numericKey(v);
      if (k === null) continue;
      if (!min || k < min.k) min = { k, v };
      if (!max || k > max.k) max = { k, v };
    }
    return {
      name: c.name,
      type: c.type,
      nulls,
      min: min ? formatSqlValue(min.v) : null,
      max: max ? formatSqlValue(max.v) : null,
    };
  });
  return { rows: rows.length, textColumn, columns: cols };
}

// ---- chart ---------------------------------------------------------------------------------------

export interface ChartData {
  label: string;
  points: { label: string; value: number }[];
}

/** One numeric column plotted against the first column's labels (at most 50 bars). Null when the
 * result has no numeric column. */
export function chartOf(columns: SqlColumnMeta[], rows: SqlValue[][]): ChartData | null {
  if (rows.length === 0) return null;
  const numIdx = columns.findIndex((_, i) => i > 0 && rows.some((r) => r[i] && numericKey(r[i]) !== null));
  const idx = numIdx >= 0 ? numIdx : columns.findIndex((_, i) => rows.some((r) => r[i] && numericKey(r[i]) !== null));
  if (idx < 0) return null;
  const points = rows.slice(0, 50).flatMap((r, n) => {
    const k = r[idx] ? numericKey(r[idx]) : null;
    if (k === null) return [];
    const label = idx === 0 ? String(n + 1) : formatSqlValue(r[0] ?? { type: "null" });
    return [{ label, value: k }];
  });
  return points.length ? { label: columns[idx].name, points } : null;
}

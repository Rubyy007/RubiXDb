import type { SqlColumnMeta, SqlValue } from "../api/types";
import { formatSqlValue } from "./sqlValue";

/** CSV text for already-fetched rows. Text cells that a spreadsheet would
 * evaluate as a formula (=, +, -, @, tab, CR) are prefixed with an apostrophe
 * so a hostile stored value cannot execute when the file is opened. Numbers
 * and other typed values are written as-is. NULL is an empty field. */
export function toCsv(columns: SqlColumnMeta[], rows: SqlValue[][]): string {
  const field = (v: SqlValue): string => {
    if (v.type === "null") return "";
    let s = formatSqlValue(v);
    if (v.type === "text" && /^[=+\-@\t\r]/.test(s)) s = `'${s}`;
    return /[",\r\n]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s;
  };
  const header = columns.map((c) => {
    const s = /^[=+\-@\t\r]/.test(c.name) ? `'${c.name}` : c.name;
    return /[",\r\n]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s;
  });
  const lines = [header.join(",")];
  for (const row of rows) lines.push(row.map(field).join(","));
  return lines.join("\r\n") + "\r\n";
}

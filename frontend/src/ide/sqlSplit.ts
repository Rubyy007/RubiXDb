/** Statement-boundary splitting for a cell that holds more than one statement.
 *
 * `POST /v1/sql` takes exactly one statement per request (the server's parser refuses a script), so a
 * cell with several `;`-separated statements is split here and each statement is sent as its own
 * request, in order. This is lexical only -- it never judges whether SQL is valid; the server does.
 *
 * A `;` ends a statement only at the top level. It is ignored inside:
 *   - 'single-quoted strings' (a doubled '' is an escaped quote and stays inside the string),
 *   - "double-quoted identifiers" (a doubled "" likewise),
 *   - -- line comments and
 *   - /* block comments *\/.
 * A piece that holds only whitespace and comments is not a statement and is dropped. Each statement
 * keeps its text exactly as typed (comments inside it included), trimmed of surrounding whitespace,
 * without the terminating `;`. */

export interface SqlStatement {
  /** The statement as it is sent to the server. */
  text: string;
  /** Offset of `text` inside the cell text. */
  offset: number;
  /** 1-based line and column in the cell where `text` begins. */
  startLine: number;
  startCol: number;
}

export function splitStatements(script: string): SqlStatement[] {
  const out: SqlStatement[] = [];
  const n = script.length;
  let segStart = 0;
  let hasContent = false;

  const flush = (end: number) => {
    if (hasContent) {
      const raw = script.slice(segStart, end);
      const lead = raw.length - raw.trimStart().length;
      const text = raw.trim();
      const offset = segStart + lead;
      const before = script.slice(0, offset);
      const lastNl = before.lastIndexOf("\n");
      out.push({
        text,
        offset,
        startLine: (before.match(/\n/g)?.length ?? 0) + 1,
        startCol: offset - (lastNl + 1) + 1,
      });
    }
    hasContent = false;
  };

  let i = 0;
  while (i < n) {
    const c = script[i];
    const d = script[i + 1];
    if (c === "'" || c === '"') {
      hasContent = true;
      i++;
      while (i < n) {
        if (script[i] === c) {
          if (script[i + 1] === c) {
            i += 2;
            continue;
          }
          i++;
          break;
        }
        i++;
      }
      continue;
    }
    if (c === "-" && d === "-") {
      while (i < n && script[i] !== "\n") i++;
      continue;
    }
    if (c === "/" && d === "*") {
      i += 2;
      while (i < n && !(script[i] === "*" && script[i + 1] === "/")) i++;
      i += 2;
      continue;
    }
    if (c === ";") {
      flush(i);
      segStart = i + 1;
      i++;
      continue;
    }
    if (!/\s/.test(c)) hasContent = true;
    i++;
  }
  flush(n);
  return out;
}

/** True when the text holds at least one statement (not just blanks, `;` and comments). */
export function hasSql(script: string): boolean {
  return splitStatements(script).length > 0;
}

/** Position in the cell for a line/column the server reported relative to one statement. */
export function cellPosition(stmt: Pick<SqlStatement, "startLine" | "startCol">, line: number, col: number): { line: number; col: number } {
  return line <= 1 ? { line: stmt.startLine, col: stmt.startCol + Math.max(0, col - 1) } : { line: stmt.startLine + line - 1, col };
}

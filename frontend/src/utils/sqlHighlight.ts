/** Tiny hand-rolled SQL tokenizer for the console editor's highlight layer.
 * No dependency. It only classifies text; the editor renders every token as a
 * React text node, so nothing here can become markup. */

export type TokKind = "kw" | "str" | "num" | "com" | "id" | "plain";
export interface Tok {
  kind: TokKind;
  text: string;
}

const KEYWORDS = new Set(
  (
    "SELECT FROM WHERE GROUP BY HAVING ORDER LIMIT OFFSET INSERT INTO VALUES UPDATE SET DELETE CREATE TABLE INDEX " +
    "DROP ALTER ON AS AND OR NOT NULL IS IN LIKE BETWEEN JOIN INNER LEFT RIGHT OUTER CROSS DISTINCT UNIQUE PRIMARY " +
    "KEY BEGIN COMMIT ROLLBACK EXPLAIN IF EXISTS ASC DESC COUNT SUM AVG MIN MAX CASE WHEN THEN ELSE END INTEGER " +
    "BIGINT TEXT BOOLEAN REAL DOUBLE DECIMAL DATE TIME TIMESTAMP BLOB TRUE FALSE SCHEMA DATABASE DEFAULT"
  ).split(" "),
);

/** Beyond this the editor shows plain text: highlighting is a nicety, never a cost. */
export const HIGHLIGHT_LIMIT = 200_000;

const WORD = /[A-Za-z_]/;
const WORD_CHAR = /[A-Za-z0-9_]/;
const DIGIT = /[0-9]/;

export function tokenize(src: string): Tok[] {
  if (src.length > HIGHLIGHT_LIMIT) return [{ kind: "plain", text: src }];
  const out: Tok[] = [];
  const push = (kind: TokKind, text: string) => {
    if (!text) return;
    const last = out[out.length - 1];
    if (last && last.kind === kind && kind === "plain") last.text += text;
    else out.push({ kind, text });
  };
  let i = 0;
  const n = src.length;
  while (i < n) {
    const c = src[i];
    if (c === "-" && src[i + 1] === "-") {
      let j = src.indexOf("\n", i);
      if (j < 0) j = n;
      push("com", src.slice(i, j));
      i = j;
    } else if (c === "/" && src[i + 1] === "*") {
      const j = src.indexOf("*/", i + 2);
      const end = j < 0 ? n : j + 2;
      push("com", src.slice(i, end));
      i = end;
    } else if (c === "'") {
      let j = i + 1;
      while (j < n) {
        if (src[j] === "'") {
          if (src[j + 1] === "'") j += 2;
          else break;
        } else j += 1;
      }
      const end = Math.min(n, j + 1);
      push("str", src.slice(i, end));
      i = end;
    } else if (c === '"') {
      const j = src.indexOf('"', i + 1);
      const end = j < 0 ? n : j + 1;
      push("id", src.slice(i, end));
      i = end;
    } else if (DIGIT.test(c)) {
      let j = i + 1;
      while (j < n && (DIGIT.test(src[j]) || (src[j] === "." && DIGIT.test(src[j + 1] ?? "")))) j += 1;
      push("num", src.slice(i, j));
      i = j;
    } else if (WORD.test(c)) {
      let j = i + 1;
      while (j < n && WORD_CHAR.test(src[j])) j += 1;
      const word = src.slice(i, j);
      push(KEYWORDS.has(word.toUpperCase()) ? "kw" : "plain", word);
      i = j;
    } else {
      push("plain", c);
      i += 1;
    }
  }
  return out;
}

/** 1-based line and column of a caret offset. */
export function caretPosition(text: string, offset: number): { line: number; col: number } {
  const before = text.slice(0, Math.max(0, Math.min(offset, text.length)));
  const lastNl = before.lastIndexOf("\n");
  let line = 1;
  for (let k = before.indexOf("\n"); k >= 0; k = before.indexOf("\n", k + 1)) line += 1;
  return { line, col: before.length - lastNl };
}

/** Toggle `-- ` on every line touched by [start, end]. Returns new text and selection. */
export function toggleLineComment(
  text: string,
  start: number,
  end: number,
): { text: string; start: number; end: number } {
  const lineStart = text.lastIndexOf("\n", start - 1) + 1;
  let lineEnd = text.indexOf("\n", end > start ? end - 1 : end);
  if (lineEnd < 0) lineEnd = text.length;
  const block = text.slice(lineStart, lineEnd);
  const lines = block.split("\n");
  const nonEmpty = lines.filter((l) => l.trim().length > 0);
  const allCommented = nonEmpty.length > 0 && nonEmpty.every((l) => /^\s*--/.test(l));
  let startDelta = 0;
  let totalDelta = 0;
  const next = lines.map((l, idx) => {
    if (allCommented) {
      const m = /^(\s*)-- ?/.exec(l);
      if (!m) return l;
      const removed = m[0].length - m[1].length;
      if (idx === 0) startDelta = -removed;
      totalDelta -= removed;
      return m[1] + l.slice(m[0].length);
    }
    if (l.trim().length === 0) return l;
    if (idx === 0) startDelta = 3;
    totalDelta += 3;
    return "-- " + l;
  });
  return {
    text: text.slice(0, lineStart) + next.join("\n") + text.slice(lineEnd),
    start: Math.max(lineStart, start + startDelta),
    end: Math.max(lineStart, end + totalDelta),
  };
}

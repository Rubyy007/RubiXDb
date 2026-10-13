/** Decides when the word the user just finished typing should become UPPERCASE.
 *
 * Called with the text and the caret just BEFORE a separator (space, newline, `(`, `,`, ...) is inserted.
 * If the word that ends exactly at the caret is a SQL keyword used as a keyword, it returns the range to
 * replace and the uppercase text; otherwise `null` and the text is left exactly as typed.
 *
 * It never touches: text inside 'strings', "quoted identifiers", -- comments or block comments; a word after
 * or before a dot (`t.date`); a word that is a table or column the catalog knows (`keepCase`); a word that is
 * not in the strict lists of `keywords.ts` (so `select name from t` leaves `name` alone). */

import { FUNCTION_SET, RESERVED, TYPE_SET } from "./keywords";

export interface UpperEdit {
  start: number;
  end: number;
  upper: string;
}

const WORD_CHAR = /[A-Za-z0-9_]/;

/** True when `offset` is inside a string, quoted identifier or comment of `text`. */
export function insideLiteral(text: string, offset: number): boolean {
  let i = 0;
  const n = text.length;
  while (i < n && i < offset) {
    const c = text[i];
    const d = text[i + 1];
    if (c === "-" && d === "-") {
      const j = text.indexOf("\n", i);
      const end = j < 0 ? n : j;
      if (offset <= end) return true;
      i = end;
    } else if (c === "/" && d === "*") {
      const j = text.indexOf("*/", i + 2);
      const end = j < 0 ? n : j + 2;
      if (offset < end || j < 0) return true;
      i = end;
    } else if (c === "'" || c === '"') {
      let j = i + 1;
      while (j < n) {
        if (text[j] === c) {
          if (text[j + 1] === c) j += 2;
          else break;
        } else j += 1;
      }
      // The caret is inside when it is before the closing quote (or the quote never closes).
      if (j >= n || offset <= j) return true;
      i = j + 1;
    } else {
      i += 1;
    }
  }
  return false;
}

/** Words before `start`, nearest last, skipping whitespace; punctuation is returned as its own entry. */
function previousTokens(text: string, start: number, count: number): string[] {
  const out: string[] = [];
  let i = start;
  while (out.length < count && i > 0) {
    while (i > 0 && /\s/.test(text[i - 1])) i -= 1;
    if (i === 0) break;
    if (WORD_CHAR.test(text[i - 1])) {
      let j = i;
      while (j > 0 && WORD_CHAR.test(text[j - 1])) j -= 1;
      out.push(text.slice(j, i));
      i = j;
    } else {
      out.push(text[i - 1]);
      i -= 1;
    }
  }
  return out;
}

/** True where a column TYPE is expected: `CREATE TABLE t (name <here>`, `, name <here>` inside it, or
 * `ALTER TABLE t ADD COLUMN name <here>`. */
export function typePosition(text: string, wordStart: number): boolean {
  // `name` is the word right before the one being typed, `before` the token before that.
  const [name, before] = previousTokens(text, wordStart, 2);
  if (!name || !WORD_CHAR.test(name[0]) || !before) return false;
  const stmtStart = text.lastIndexOf(";", wordStart - 1) + 1;
  const stmt = text.slice(stmtStart, wordStart);
  if (/^(?:column|add)$/i.test(before)) return /^\s*ALTER\s+TABLE\b/i.test(stmt); // ADD [COLUMN] name <type>
  if (before === "(" || before === ",") return /^\s*CREATE\s+TABLE\b/i.test(stmt) && depthAt(stmt) === 1;
  return false;
}

function depthAt(stmt: string): number {
  let d = 0;
  for (let i = 0; i < stmt.length; i++) {
    const c = stmt[i];
    if (c === "(") d += 1;
    else if (c === ")") d -= 1;
    else if (c === "'" || c === '"') {
      const j = stmt.indexOf(c, i + 1);
      if (j < 0) return d;
      i = j;
    }
  }
  return d;
}

/**
 * @param typed the separator about to be inserted ("" for none, e.g. when the caret just left the word)
 * @param keepCase true for words that must never be changed (a table or column the catalog has)
 */
export function upperEdit(text: string, caret: number, typed: string, keepCase: (word: string) => boolean = () => false): UpperEdit | null {
  if (caret <= 0 || caret > text.length) return null;
  // Only a separator finishes a word. A dot makes it a qualified name; a quote or digit is part of something else.
  if (typed === "" || WORD_CHAR.test(typed) || typed === "." || typed === '"' || typed === "$") return null;
  // The word ends at the caret -- or, for `(`, just before spaces that precede it (`max (age)`; only a
  // function can be uppercased that way, every other keyword was already finished by the space).
  let end = caret;
  if (typed === "(") while (end > 0 && (text[end - 1] === " " || text[end - 1] === "\t")) end -= 1;
  if (end <= 0 || !WORD_CHAR.test(text[end - 1])) return null;
  let start = end;
  while (start > 0 && WORD_CHAR.test(text[start - 1])) start -= 1;
  const word = text.slice(start, end);
  if (/^[0-9]/.test(word)) return null;
  const upper = word.toUpperCase();
  if (word === upper) return null;
  if (insideLiteral(text, start)) return null;
  // `t.date`, `schema.table`: a part of a qualified name, never a keyword.
  if (text[start - 1] === ".") return null;
  if (keepCase(word.toLowerCase())) return null;

  // A call: the separator is `(`, possibly after spaces.
  if (FUNCTION_SET.has(upper) && !RESERVED.has(upper)) return typed === "(" ? { start, end, upper } : null;
  // Everything else must be finished by the separator itself, with no spaces in between.
  if (end !== caret) return null;
  // Everything in RESERVED is a keyword on its own.
  if (RESERVED.has(upper)) return { start, end, upper };
  if (upper === "KEY") {
    const [prev] = previousTokens(text, start, 1);
    return prev && /^(primary|foreign)$/i.test(prev) ? { start, end, upper } : null;
  }
  if (TYPE_SET.has(upper)) return typePosition(text, start) ? { start, end, upper } : null;
  return null;
}

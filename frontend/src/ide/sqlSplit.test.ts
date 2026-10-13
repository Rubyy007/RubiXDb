import { describe, expect, it } from "vitest";
import { cellPosition, hasSql, isolateStatement, splitStatements, statementIndexAt } from "./sqlSplit";

const texts = (s: string) => splitStatements(s).map((x) => x.text);

describe("splitStatements", () => {
  it("splits on top-level semicolons and drops the terminator", () => {
    expect(texts("SELECT 1; SELECT 2;")).toEqual(["SELECT 1", "SELECT 2"]);
    expect(texts("SELECT 1")).toEqual(["SELECT 1"]);
  });

  it("drops empty pieces and trims whitespace", () => {
    expect(texts("  ;\nSELECT 1 ;  \n\n;  SELECT 2  ")).toEqual(["SELECT 1", "SELECT 2"]);
    expect(texts("")).toEqual([]);
    expect(texts(" ; ;; ")).toEqual([]);
  });

  it("does not split inside strings, including escaped quotes", () => {
    expect(texts("INSERT INTO t VALUES ('a;b'); SELECT 1;")).toEqual(["INSERT INTO t VALUES ('a;b')", "SELECT 1"]);
    expect(texts("SELECT 'it''s; fine'; SELECT 2")).toEqual(["SELECT 'it''s; fine'", "SELECT 2"]);
  });

  it("does not split inside quoted identifiers", () => {
    expect(texts('SELECT "a;b" FROM t; SELECT "x""y;z"')).toEqual(['SELECT "a;b" FROM t', 'SELECT "x""y;z"']);
  });

  it("ignores semicolons and quotes inside comments", () => {
    expect(texts("SELECT 1 -- done; really\n; SELECT 2")).toEqual(["SELECT 1 -- done; really", "SELECT 2"]);
    expect(texts("SELECT /* a; b */ 1; SELECT 2")).toEqual(["SELECT /* a; b */ 1", "SELECT 2"]);
    expect(texts("-- it's a note\nSELECT 1; SELECT 2")).toEqual(["-- it's a note\nSELECT 1", "SELECT 2"]);
  });

  it("drops a piece that is only comments", () => {
    expect(texts("SELECT 1; -- trailing note")).toEqual(["SELECT 1"]);
    expect(texts("/* only a comment */")).toEqual([]);
    expect(hasSql("-- nothing here\n;")).toBe(false);
    expect(hasSql("SELECT 1")).toBe(true);
  });

  it("an unterminated string or block comment runs to the end without splitting", () => {
    expect(texts("SELECT 'oops; SELECT 2")).toEqual(["SELECT 'oops; SELECT 2"]);
    expect(texts("SELECT 1 /* oops; SELECT 2")).toEqual(["SELECT 1 /* oops; SELECT 2"]);
  });

  it("reports where each statement starts in the cell", () => {
    const s = splitStatements("SELECT 1;\n\n  SELECT 2\n  FROM t;");
    expect(s[0]).toMatchObject({ startLine: 1, startCol: 1, offset: 0 });
    expect(s[1]).toMatchObject({ startLine: 3, startCol: 3, text: "SELECT 2\n  FROM t" });
  });

  it("maps a server line/column back to the cell", () => {
    const s = splitStatements("SELECT 1;\n\n  SELECT 2\n  FROM t;")[1];
    expect(cellPosition(s, 1, 8)).toEqual({ line: 3, col: 10 });
    expect(cellPosition(s, 2, 5)).toEqual({ line: 4, col: 5 });
  });

  it("keeps CRLF text intact", () => {
    expect(texts("SELECT 1;\r\nSELECT 2;\r\n")).toEqual(["SELECT 1", "SELECT 2"]);
  });
});

describe("statementIndexAt: which statement the caret belongs to", () => {
  const script = "SELECT 1;\nSELECT 2;\n\n-- note\nSELECT 3";
  const at = (marker: string, delta = 0) => statementIndexAt(script, script.indexOf(marker) + delta);

  it("picks the statement the caret is inside", () => {
    expect(at("SELECT 1", 3)).toBe(0);
    expect(at("SELECT 2", 3)).toBe(1);
    expect(at("SELECT 3", 3)).toBe(2);
  });

  it("includes the start, the end and the terminating semicolon of a statement", () => {
    expect(at("SELECT 1")).toBe(0);
    expect(at("SELECT 1", "SELECT 1".length)).toBe(0); // just before ;
    expect(at("SELECT 1", "SELECT 1;".length)).toBe(0); // just after ; on the same line
    expect(at("SELECT 2", "SELECT 2;".length)).toBe(1);
  });

  it("a caret on the line break or blank line after a statement belongs to the NEXT one", () => {
    expect(statementIndexAt(script, "SELECT 1;\n".length)).toBe(1); // start of line 2 == start of statement 2
    expect(statementIndexAt(script, "SELECT 1;\nSELECT 2;\n".length)).toBe(2); // the blank line
  });

  it("a comment above a statement is part of that statement", () => {
    expect(at("-- note", 3)).toBe(2);
  });

  it("clamps before the first statement and after the last, and reports none for an empty cell", () => {
    expect(statementIndexAt("\n\n  SELECT 1;  SELECT 2", 0)).toBe(0);
    expect(statementIndexAt(script, script.length + 50)).toBe(2);
    expect(statementIndexAt("SELECT 1;   \n  ", 14)).toBe(0);
    expect(statementIndexAt("-- only a note;", 3)).toBe(-1);
    expect(statementIndexAt("", 0)).toBe(-1);
  });

  it("ignores a semicolon inside a string or a comment", () => {
    const s = "INSERT INTO t VALUES ('a;b'); -- x; y\nSELECT 2";
    expect(statementIndexAt(s, s.indexOf("a;b") + 3)).toBe(0);
    expect(statementIndexAt(s, s.indexOf("SELECT 2") + 2)).toBe(1);
  });
});

describe("isolateStatement: send only one statement, keep its position", () => {
  it("re-splits to exactly that statement, at the same line and column", () => {
    const script = "SELECT 1;\n\n  SELECT 2 +\n    3;\nSELECT 4";
    const all = splitStatements(script);
    for (let k = 0; k < all.length; k++) {
      const one = splitStatements(isolateStatement(script, all[k]));
      expect(one).toHaveLength(1);
      expect(one[0]).toMatchObject({ text: all[k].text, startLine: all[k].startLine, startCol: all[k].startCol });
    }
  });

  it("keeps a statement's own semicolon-in-string and comments intact", () => {
    const script = "SELECT 1;\n-- why\nINSERT INTO t VALUES ('a;b')";
    const second = splitStatements(script)[1];
    expect(isolateStatement(script, second).trim()).toBe("-- why\nINSERT INTO t VALUES ('a;b')");
  });
});

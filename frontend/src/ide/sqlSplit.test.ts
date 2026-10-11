import { describe, expect, it } from "vitest";
import { cellPosition, hasSql, splitStatements } from "./sqlSplit";

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

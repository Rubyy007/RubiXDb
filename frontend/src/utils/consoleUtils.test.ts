import { beforeEach, describe, expect, it } from "vitest";
import { caretPosition, tokenize, toggleLineComment } from "./sqlHighlight";
import { MAX_TABS, freshState, loadWorksheets, saveWorksheets } from "./worksheets";
import { toCsv } from "./csv";

describe("tokenize", () => {
  const kinds = (s: string) => tokenize(s).filter((t) => t.kind !== "plain").map((t) => `${t.kind}:${t.text}`);

  it("classifies keywords, strings, numbers, comments and quoted identifiers", () => {
    expect(kinds("select 1, 'a''b' from \"T x\" -- note\n/* c */")).toEqual([
      "kw:select",
      "num:1",
      "str:'a''b'",
      "kw:from",
      'id:"T x"',
      "com:-- note",
      "com:/* c */",
    ]);
  });
  it("never loses or reorders text (hostile input stays text)", () => {
    for (const s of ["<img src=x onerror=alert(1)>", "'unterminated", "/* open", '"open', "a\n\n--\n", ""]) {
      expect(tokenize(s).map((t) => t.text).join("")).toBe(s);
    }
  });
  it("does not highlight words inside identifiers", () => {
    expect(kinds("selection fromage")).toEqual([]);
  });
  it("falls back to plain text for very large input", () => {
    expect(tokenize("select ".repeat(40_000))).toHaveLength(1);
  });
});

describe("caretPosition", () => {
  it("is 1-based line and column", () => {
    expect(caretPosition("", 0)).toEqual({ line: 1, col: 1 });
    expect(caretPosition("ab\ncd", 4)).toEqual({ line: 2, col: 2 });
    expect(caretPosition("ab\n", 3)).toEqual({ line: 2, col: 1 });
  });
});

describe("toggleLineComment", () => {
  it("comments, then uncomments, the selected lines", () => {
    const a = toggleLineComment("select 1\nfrom t", 0, 15);
    expect(a.text).toBe("-- select 1\n-- from t");
    const b = toggleLineComment(a.text, a.start, a.end);
    expect(b.text).toBe("select 1\nfrom t");
  });
  it("only touches the lines the caret is on", () => {
    expect(toggleLineComment("a\nb\nc", 2, 2).text).toBe("a\n-- b\nc");
  });
  it("leaves blank lines alone", () => {
    expect(toggleLineComment("a\n\nb", 0, 4).text).toBe("-- a\n\n-- b");
  });
});

describe("worksheet storage", () => {
  beforeEach(() => window.sessionStorage.clear());

  it("round-trips through sessionStorage only", () => {
    const s = freshState();
    s.tabs[0].sql = "SELECT 1";
    expect(saveWorksheets(s)).toBe(true);
    expect(loadWorksheets().tabs[0].sql).toBe("SELECT 1");
    expect(window.localStorage.length).toBe(0);
  });
  it("treats corrupt or hostile stored data as untrusted and falls back", () => {
    for (const raw of ["not json", "null", "{}", '{"tabs":[]}', '{"tabs":[{"id":"x","name":1,"sql":2}]}']) {
      window.sessionStorage.setItem("rubixdb-console-worksheets", raw);
      expect(loadWorksheets().tabs).toHaveLength(1);
    }
  });
  it("caps the tab count and repairs the active id and counter", () => {
    const tabs = Array.from({ length: 40 }, (_, i) => ({ id: `ws-${i + 1}`, name: `n${i}`, sql: "" }));
    window.sessionStorage.setItem("rubixdb-console-worksheets", JSON.stringify({ tabs, activeId: "nope", counter: 0 }));
    const s = loadWorksheets();
    expect(s.tabs).toHaveLength(MAX_TABS);
    expect(s.activeId).toBe("ws-1");
    expect(s.counter).toBeGreaterThanOrEqual(MAX_TABS);
  });
});

describe("toCsv", () => {
  const cols = [
    { name: "id", type: "integer", nullable: false },
    { name: "v", type: "text", nullable: true },
  ];
  it("quotes, escapes and writes NULL as an empty field", () => {
    const csv = toCsv(cols, [
      [{ type: "integer", value: -5 }, { type: "text", value: 'say "hi", ok' }],
      [{ type: "integer", value: 2 }, { type: "null" }],
    ]);
    expect(csv).toBe('id,v\r\n-5,"say ""hi"", ok"\r\n2,\r\n');
  });
  it("neutralises spreadsheet formulas in text cells but leaves negative numbers alone", () => {
    const csv = toCsv(cols, [[{ type: "integer", value: -1 }, { type: "text", value: "=HYPERLINK(\"http://x\")" }]]);
    expect(csv).toContain("-1,\"'=HYPERLINK(");
    expect(toCsv(cols, [[{ type: "integer", value: 1 }, { type: "text", value: "@cmd" }]])).toContain("1,'@cmd");
  });
});

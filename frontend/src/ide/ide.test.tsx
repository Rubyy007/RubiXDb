import { describe, expect, it, vi } from "vitest";
import { render } from "@testing-library/react";
import type { SqlColumnMeta, SqlResponseBody, SqlValue } from "../api/types";
import { ApiRequestError } from "../api/types";
import { applyCompletion, completionContext, filterItems, resolveBase, type CatalogLists } from "./completion";
import { chartOf, guessTable, profileRows, scalarOf, sqlLiteral, toInserts, toJson } from "./exports";
import { renderMarkdown } from "./markdown";
import { filterActions, fuzzyScore } from "./palette";
import { hiddenByCollapse, parsePlan } from "./plan";
import { Runner, implicitSessionKey, type HistoryEntry } from "./runner";

const text = (value: string): SqlValue => ({ type: "text", value });
const int = (value: number): SqlValue => ({ type: "integer", value });
const cols = (...names: [string, string][]): SqlColumnMeta[] => names.map(([name, type]) => ({ name, type, nullable: true }));

describe("plan parser", () => {
  const plan = "Query (max_parameter=0)\n  Projection [id, name]\n    Filter (tier = 'PRO')\n      Scan table_id=3\n";
  it("builds nodes with depth, name and detail", () => {
    const nodes = parsePlan(plan);
    expect(nodes.map((n) => [n.depth, n.name])).toEqual([[0, "Query"], [1, "Projection"], [2, "Filter"], [3, "Scan"]]);
    expect(nodes[1].detail).toBe("[id, name]");
  });
  it("hides descendants of a collapsed node", () => {
    const nodes = parsePlan(plan);
    expect(hiddenByCollapse(nodes, 3, new Set([1]))).toBe(true);
    expect(hiddenByCollapse(nodes, 1, new Set([1]))).toBe(false);
  });
  it("keeps a join node name together", () => {
    const n = parsePlan("INNER Join (NestedLoop) on a = b")[0];
    expect(n.name).toBe("INNER Join");
    expect(n.detail).toBe("(NestedLoop) on a = b");
  });
});

describe("markdown", () => {
  it("never turns note text into HTML", () => {
    const { container } = render(<div>{renderMarkdown("<img src=x onerror=alert(1)>\n\n[click](javascript:alert(1))\n\n**bold** and `code`")}</div>);
    expect(container.querySelector("img")).toBeNull();
    expect(container.querySelector("a")).toBeNull();
    expect(container.textContent).toContain("<img src=x onerror=alert(1)>");
    expect(container.querySelector("strong")?.textContent).toBe("bold");
    expect(container.querySelector("code")?.textContent).toBe("code");
  });
  it("renders headings, lists and fenced code", () => {
    const { container } = render(<div>{renderMarkdown("# T\n- a\n- b\n\n```\nSELECT 1\n```")}</div>);
    expect(container.querySelector("h3")?.textContent).toBe("T");
    expect(container.querySelectorAll("li")).toHaveLength(2);
    expect(container.querySelector("pre")?.textContent).toBe("SELECT 1");
  });
});

describe("exports", () => {
  const columns = cols(["id", "bigint"], ["name", "text"]);
  const rows: SqlValue[][] = [
    [{ type: "bigint", value: "9007199254740993" }, text("O'Neil")],
    [{ type: "bigint", value: "2" }, { type: "null" }],
  ];
  it("keeps big integers exact in JSON and quotes text in INSERTs", () => {
    expect(JSON.parse(toJson(columns, rows))[0]).toEqual({ id: "9007199254740993", name: "O'Neil" });
    const ins = toInserts("customers", columns, rows);
    expect(ins.text).toBe(
      "INSERT INTO customers (id, name) VALUES (9007199254740993, 'O''Neil');\nINSERT INTO customers (id, name) VALUES (2, NULL);\n",
    );
    expect(ins.skipped).toBe(0);
  });
  it("reports cells that have no literal form", () => {
    expect(sqlLiteral({ type: "blob", value_b64: "AA==" })).toBeNull();
    expect(toInserts("t", cols(["b", "blob"]), [[{ type: "blob", value_b64: "AA==" }]]).skipped).toBe(1);
  });
  it("guesses the table and detects scalar results", () => {
    expect(guessTable("select * from public.orders where x = 1")).toBe("public.orders");
    expect(guessTable("SELECT 1")).toBe("table_name");
    expect(scalarOf(cols(["count", "bigint"]), [[{ type: "bigint", value: "311" }]])).toEqual({ label: "count", value: "311" });
    expect(scalarOf(columns, rows)).toBeNull();
  });
  it("profiles rows already in memory", () => {
    const p = profileRows(cols(["name", "text"], ["n", "integer"]), [[text("a"), int(5)], [text("a"), int(2)], [text("b"), { type: "null" }]]);
    expect(p.rows).toBe(3);
    expect(p.textColumn).toEqual({ name: "name", distinct: 2 });
    expect(p.columns[1]).toMatchObject({ nulls: 1, min: "2", max: "5" });
  });
  it("plots one numeric column against the first column", () => {
    const c = chartOf(cols(["tier", "text"], ["n", "integer"]), [[text("a"), int(3)], [text("b"), int(7)]]);
    expect(c?.points).toEqual([{ label: "a", value: 3 }, { label: "b", value: 7 }]);
    expect(chartOf(cols(["t", "text"]), [[text("a")]])).toBeNull();
  });
});

describe("completion", () => {
  const cat: CatalogLists = {
    databases: [{ id: 1, name: "default" }],
    schemas: [{ id: 10, databaseId: 1, name: "public" }],
    tables: [{ id: 100, schemaId: 10, name: "customers" }, { id: 101, schemaId: 10, name: "orders" }],
  };
  it("finds the dotted context before the caret", () => {
    const t = "SELECT * FROM default.pub";
    expect(completionContext(t, t.length)).toEqual({ base: ["default"], partial: "pub", start: t.length - 3 });
    expect(completionContext("SELECT 1", 8)).toBeNull();
  });
  it("resolves db, schema and table prefixes", () => {
    expect(resolveBase(["default"], cat)).toEqual({ kind: "schemas", items: ["public"] });
    expect(resolveBase(["default", "public"], cat)).toEqual({ kind: "tables", items: ["customers", "orders"] });
    expect(resolveBase(["public"], cat)).toEqual({ kind: "tables", items: ["customers", "orders"] });
    expect(resolveBase(["customers"], cat)).toEqual({ kind: "columns", table: "customers" });
    expect(resolveBase(["nope"], cat)).toBeNull();
  });
  it("filters and applies a choice", () => {
    expect(filterItems(["orders", "events", "customers"], "or")).toEqual(["orders"]);
    const ctx = completionContext("FROM public.cu", 14)!;
    expect(applyCompletion("FROM public.cu", ctx, "customers")).toEqual({ text: "FROM public.customers", caret: 21 });
  });
});

describe("palette fuzzy match", () => {
  it("matches subsequences and prefers substrings", () => {
    expect(fuzzyScore("rall", "Run all")).toBeGreaterThan(0);
    expect(fuzzyScore("zzz", "Run all")).toBe(-1);
    expect(fuzzyScore("run", "Run all")).toBeGreaterThan(fuzzyScore("ral", "Run all"));
    const acts = [{ label: "New worksheet" }, { label: "Run all" }, { label: "Toggle dark mode" }];
    expect(filterActions(acts, "dark")).toEqual([{ label: "Toggle dark mode" }]);
    expect(filterActions(acts, "")).toHaveLength(3);
  });
});

// ---- runner ------------------------------------------------------------------------------------

type Pending = { req: { sql: string; session_id?: string | null }; signal: AbortSignal; ok: (r: SqlResponseBody) => void; fail: (e: unknown) => void };

function harness(max = 2) {
  const calls: Pending[] = [];
  const history: HistoryEntry[] = [];
  const runner = new Runner({
    maxConcurrent: max,
    onHistory: (e) => history.push(e),
    send: (req, signal) =>
      new Promise<SqlResponseBody>((ok, fail) => {
        calls.push({ req, signal, ok, fail });
        signal.addEventListener("abort", () => fail(new DOMException("aborted", "AbortError")));
      }),
  });
  return { runner, calls, history };
}
const rows = (n: number): SqlResponseBody => ({ session_id: null, result: { kind: "rows", columns: cols(["x", "integer"]), rows: Array.from({ length: n }, (_, i) => [int(i)]), row_count: n } });
const flush = () => new Promise((r) => setTimeout(r, 0));

describe("runner: autocommit", () => {
  it("runs up to the limit at once and queues the rest", async () => {
    const { runner, calls } = harness(2);
    for (const id of ["a", "b", "c"]) void runner.runAutocommit({ cellId: id, sheetId: "w-1", sql: `SELECT ${id}` });
    expect(calls).toHaveLength(2);
    expect(runner.getState().queued).toBe(1);
    expect(runner.getState().runs.c.status).toBe("queued");
    calls[0].ok(rows(1));
    await flush();
    expect(calls).toHaveLength(3);
    expect(runner.getState().runs.a.status).toBe("success");
    expect(runner.getState().queued).toBe(0);
  });

  it("sends no session id for autocommit and records history", async () => {
    const { runner, calls, history } = harness();
    const p = runner.runAutocommit({ cellId: "a", sheetId: "w-1", sql: "SELECT 1" });
    expect(calls[0].req).toEqual({ sql: "SELECT 1" });
    calls[0].ok(rows(3));
    await p;
    expect(history[0]).toMatchObject({ ok: true, rows: 3, sql: "SELECT 1" });
  });

  it("keeps the exact API error code", async () => {
    const { runner, calls } = harness();
    const p = runner.runAutocommit({ cellId: "a", sheetId: "w-1", sql: "SELEC" });
    calls[0].fail(new ApiRequestError(400, { error: { code: "PARSE_ERROR", message: "the SQL text could not be parsed", detail: "expected FROM" } }));
    const run = await p;
    expect(run?.status).toBe("error");
    expect(run?.error).toEqual({ code: "PARSE_ERROR", message: "the SQL text could not be parsed", detail: "expected FROM", http: 400 });
  });

  it("cancel aborts a running request and drops a queued one", async () => {
    const { runner, calls } = harness(1);
    const a = runner.runAutocommit({ cellId: "a", sheetId: "w-1", sql: "SELECT 1" });
    const b = runner.runAutocommit({ cellId: "b", sheetId: "w-1", sql: "SELECT 2" });
    runner.cancel("b");
    expect((await b)?.status).toBe("cancelled");
    runner.cancel("a");
    const run = await a;
    expect(calls[0].signal.aborted).toBe(true);
    expect(run?.status).toBe("cancelled");
    expect(run?.error?.code).toBe("CANCELLED");
    expect(calls).toHaveLength(1);
  });

  it("stopAll cancels everything", async () => {
    const { runner } = harness(1);
    const ps = ["a", "b", "c"].map((id) => runner.runAutocommit({ cellId: id, sheetId: "w-1", sql: "SELECT 1" }));
    runner.stopAll();
    const done = await Promise.all(ps);
    expect(done.map((d) => d?.status)).toEqual(["cancelled", "cancelled", "cancelled"]);
    expect(runner.getState().inFlight).toHaveLength(0);
  });

  it("ignores empty SQL and a cell that is already running", async () => {
    const { runner, calls } = harness();
    expect(await runner.runAutocommit({ cellId: "a", sheetId: "w-1", sql: "   " })).toBeNull();
    void runner.runAutocommit({ cellId: "a", sheetId: "w-1", sql: "SELECT 1" });
    expect(await runner.runAutocommit({ cellId: "a", sheetId: "w-1", sql: "SELECT 1" })).toBeNull();
    expect(calls).toHaveLength(1);
  });

  it("reset clears finished outputs only", async () => {
    const { runner, calls } = harness();
    const p = runner.runAutocommit({ cellId: "a", sheetId: "w-1", sql: "SELECT 1" });
    void runner.runAutocommit({ cellId: "b", sheetId: "w-1", sql: "SELECT 2" });
    calls[0].ok(rows(1));
    await p;
    runner.reset();
    expect(Object.keys(runner.getState().runs)).toEqual(["b"]);
  });
});

describe("runner: transactions", () => {
  const begin = async (h: ReturnType<typeof harness>, key = "t-1") => {
    const p = h.runner.begin(key, "w-1");
    h.calls[h.calls.length - 1].ok({ session_id: "S1", result: { kind: "begin" } });
    await p;
  };

  it("BEGIN stores the session and cells send its id", async () => {
    const h = harness();
    await begin(h);
    expect(h.runner.getState().sessions["t-1"].sessionId).toBe("S1");
    const p = h.runner.runInSession({ cellId: "a", sheetId: "w-1", sql: "SELECT 1", sessionKey: "t-1" });
    expect(h.calls[h.calls.length - 1].req).toEqual({ sql: "SELECT 1", session_id: "S1" });
    expect(h.runner.getState().sessions["t-1"].busyCell).toBe("a");
    h.calls[h.calls.length - 1].ok({ session_id: "S1", result: rows(1).result });
    expect((await p).outcome).toBe("done");
    expect(h.runner.getState().sessions["t-1"].busyCell).toBeNull();
  });

  it("refuses to run without a session", async () => {
    const { runner } = harness();
    expect((await runner.runInSession({ cellId: "a", sheetId: "w-1", sql: "SELECT 1", sessionKey: "t-1" })).outcome).toBe("no-session");
  });

  it("blocks a second cell while the session is busy, then frees it", async () => {
    const h = harness();
    await begin(h);
    const first = h.runner.runInSession({ cellId: "a", sheetId: "w-1", sql: "SELECT 1", sessionKey: "t-1" });
    const second = await h.runner.runInSession({ cellId: "b", sheetId: "w-1", sql: "SELECT 2", sessionKey: "t-1" });
    expect(second.outcome).toBe("blocked");
    expect(h.runner.getState().runs.b.blockedBy).toEqual({ sessionId: "S1", cellId: "a" });
    h.calls[h.calls.length - 1].ok({ session_id: "S1", result: rows(1).result });
    await first;
    expect(h.runner.getState().runs.b.blockedBy).toBeUndefined();
  });

  it("allows only one open transaction per tab", async () => {
    const h = harness();
    await begin(h, "t-1");
    const before = h.calls.length;
    const r = await h.runner.begin("t-2", "w-1");
    expect(r.ok).toBe(false);
    expect(h.calls).toHaveLength(before);
    expect(h.runner.getState().notices["t-2"]).toContain("Another transaction is open");
  });

  it("COMMIT ends the session; a failed COMMIT keeps it and shows the code", async () => {
    const h = harness();
    await begin(h);
    const bad = h.runner.finish("t-1", "w-1", "COMMIT");
    h.calls[h.calls.length - 1].fail(new ApiRequestError(409, { error: { code: "CONFLICT_ERROR", message: "a transaction conflict occurred" } }));
    expect((await bad).ok).toBe(false);
    expect(h.runner.getState().sessions["t-1"]).toBeDefined();
    expect(h.runner.getState().notices["t-1"]).toContain("CONFLICT_ERROR");
    const good = h.runner.finish("t-1", "w-1", "COMMIT");
    expect(h.calls[h.calls.length - 1].req).toEqual({ sql: "COMMIT", session_id: "S1" });
    h.calls[h.calls.length - 1].ok({ session_id: null, result: { kind: "commit" } });
    expect((await good).ok).toBe(true);
    expect(h.runner.getState().sessions["t-1"]).toBeUndefined();
  });

  it("SESSION_NOT_FOUND forgets the session", async () => {
    const h = harness();
    await begin(h);
    const p = h.runner.runInSession({ cellId: "a", sheetId: "w-1", sql: "SELECT 1", sessionKey: "t-1" });
    h.calls[h.calls.length - 1].fail(new ApiRequestError(404, { error: { code: "SESSION_NOT_FOUND", message: "gone" } }));
    await p;
    expect(h.runner.getState().sessions["t-1"]).toBeUndefined();
    expect(h.runner.getState().runs.a.error?.code).toBe("SESSION_NOT_FOUND");
  });

  it("adopts a session opened by a BEGIN typed in an autocommit cell", async () => {
    const h = harness();
    const p = h.runner.runAutocommit({ cellId: "a", sheetId: "w-9", sql: "BEGIN" });
    h.calls[0].ok({ session_id: "S9", result: { kind: "begin" } });
    await p;
    expect(h.runner.getState().sessions[implicitSessionKey("w-9")].sessionId).toBe("S9");
  });

  it("runSequence stops at the first failure", async () => {
    const h = harness();
    await begin(h);
    const seq = h.runner.runSequence(
      [
        { cellId: "a", sheetId: "w-1", sql: "UPDATE x" },
        { cellId: "b", sheetId: "w-1", sql: "UPDATE y" },
      ],
      "t-1",
    );
    const n = h.calls.length;
    h.calls[n - 1].fail(new ApiRequestError(409, { error: { code: "CONFLICT_ERROR", message: "c" } }));
    await seq;
    expect(h.calls).toHaveLength(n);
    expect(h.runner.getState().runs.b).toBeUndefined();
  });

  it("notifies subscribers", async () => {
    const { runner } = harness();
    const fn = vi.fn();
    runner.subscribe(fn);
    void runner.runAutocommit({ cellId: "a", sheetId: "w-1", sql: "SELECT 1" });
    expect(fn).toHaveBeenCalled();
  });
});

import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter } from "react-router-dom";
import type { SqlRequestBody, SqlResponseBody } from "../api/types";
import { ApiRequestError } from "../api/types";
import { IdePage } from "./IdePage";
import { Runner, type HistoryEntry } from "./runner";

const ok = (n: number): SqlResponseBody => ({
  session_id: null,
  result: { kind: "rows", columns: [{ name: "x", type: "integer", nullable: false }], rows: [[{ type: "integer", value: n }], [{ type: "integer", value: n + 1 }]], row_count: 2 },
});
const wrote: SqlResponseBody = { session_id: null, result: { kind: "write", statement: "INSERT", rows_affected: 1 } };

function harness() {
  const sent: SqlRequestBody[] = [];
  const queue: Array<SqlResponseBody | Error> = [];
  const history: HistoryEntry[] = [];
  const runner = new Runner({
    onHistory: (e) => history.push(e),
    send: async (req) => {
      sent.push(req);
      const next = queue.shift();
      if (next instanceof Error) throw next;
      return next ?? ok(1);
    },
  });
  return { runner, sent, queue, history };
}

describe("runner: a cell with several statements", () => {
  it("sends each statement as its own request, in order", async () => {
    const h = harness();
    h.queue.push(wrote, ok(10));
    const run = await h.runner.runAutocommit({ cellId: "a", sheetId: "w-1", sql: "INSERT INTO t VALUES (1);\nSELECT x FROM t;" });
    expect(h.sent).toEqual([{ sql: "INSERT INTO t VALUES (1)" }, { sql: "SELECT x FROM t" }]);
    expect(run?.status).toBe("success");
    expect(run?.parts).toHaveLength(2);
    expect(run?.statementCount).toBe(2);
    // The cell's visible result is the last statement's.
    expect(run?.response?.result.kind).toBe("rows");
  });

  it("stops at the first failure and says which statement failed", async () => {
    const h = harness();
    h.queue.push(wrote, new ApiRequestError(400, { error: { code: "PARSE_ERROR", message: "the SQL text could not be parsed", detail: "expected FROM, found EOF at line 1 col 8" } }));
    const run = await h.runner.runAutocommit({ cellId: "a", sheetId: "w-1", sql: "INSERT INTO t VALUES (1);\nSELECT;\nSELECT 3;" });
    expect(h.sent).toHaveLength(2);
    expect(run?.status).toBe("error");
    expect(run?.error?.code).toBe("PARSE_ERROR");
    expect(run?.error?.statement).toMatchObject({ index: 1, count: 3, text: "SELECT", startLine: 2, startCol: 1 });
    expect(run?.parts?.map((p) => p.status)).toEqual(["success", "error"]);
  });

  it("records one history entry per statement with the exact text sent", async () => {
    const h = harness();
    h.queue.push(wrote, ok(5));
    await h.runner.runAutocommit({ cellId: "a", sheetId: "w-1", sql: "INSERT INTO t VALUES ('a;b');  SELECT x\n  FROM t ;" });
    expect(h.history.map((e) => [e.sql, e.index, e.count, e.ok, e.rows])).toEqual([
      ["INSERT INTO t VALUES ('a;b')", 1, 2, true, 1],
      ["SELECT x\n  FROM t", 2, 2, true, 2],
    ]);
    expect(new Set(h.history.map((e) => e.seq)).size).toBe(2);
  });

  it("records the failed statement as a failure and nothing after it", async () => {
    const h = harness();
    h.queue.push(new ApiRequestError(409, { error: { code: "CONFLICT_ERROR", message: "a transaction conflict occurred" } }));
    await h.runner.runAutocommit({ cellId: "a", sheetId: "w-1", sql: "UPDATE t SET a = 1; SELECT 1;" });
    expect(h.history).toHaveLength(1);
    expect(h.history[0]).toMatchObject({ ok: false, code: "CONFLICT_ERROR", sql: "UPDATE t SET a = 1", index: 1, count: 2 });
  });

  it("carries the session through BEGIN; ...; COMMIT; in one autocommit cell", async () => {
    const h = harness();
    h.queue.push({ session_id: "S1", result: { kind: "begin" } }, { session_id: "S1", result: wrote.result }, { session_id: null, result: { kind: "commit" } });
    const run = await h.runner.runAutocommit({ cellId: "a", sheetId: "w-1", sql: "BEGIN; INSERT INTO t VALUES (1); COMMIT;" });
    expect(h.sent).toEqual([{ sql: "BEGIN" }, { sql: "INSERT INTO t VALUES (1)", session_id: "S1" }, { sql: "COMMIT", session_id: "S1" }]);
    expect(run?.status).toBe("success");
    // The script closed its own transaction, so the tab holds no open session.
    expect(h.runner.getState().sessions).toEqual({});
  });

  it("keeps a transaction left open by a script so it can be committed or rolled back", async () => {
    const h = harness();
    h.queue.push({ session_id: "S2", result: { kind: "begin" } }, { session_id: "S2", result: wrote.result });
    await h.runner.runAutocommit({ cellId: "a", sheetId: "w-1", sql: "BEGIN; INSERT INTO t VALUES (1);" });
    expect(h.runner.getState().sessions["sess-w-1"].sessionId).toBe("S2");
  });

  it("ignores text that is only comments and semicolons", async () => {
    const h = harness();
    expect(await h.runner.runAutocommit({ cellId: "a", sheetId: "w-1", sql: "-- nothing\n;;" })).toBeNull();
    expect(h.sent).toHaveLength(0);
  });
});

// ---- the page ------------------------------------------------------------------------------------

const sqlMock = vi.fn<(req: SqlRequestBody, signal?: AbortSignal) => Promise<SqlResponseBody>>();
vi.mock("../api/queries", () => ({
  useApiClient: () => ({ sql: sqlMock, describeTable: vi.fn() }),
  invalidateCatalogQueries: () => {},
  useDatabasesQuery: () => ({ data: [], isError: false, isLoading: false }),
  useSchemasQuery: () => ({ data: [], isError: false, isLoading: false }),
  useTablesQuery: () => ({ data: [], isError: false, isLoading: false }),
  useInstanceQuery: () => ({ data: { instance_id: "x", name: "default" } }),
  useObservedQueriesQuery: () => ({ data: { queries: [], returned: 0, active: 0, untracked_active: 0, max_records: 200 }, isError: false }),
  useObservedSessionsQuery: () => ({ data: { sessions: [], returned: 0, total: 0, truncated: false, max_records: 200 }, isError: false }),
  useTableDescriptionQuery: () => ({ data: undefined, isLoading: false, isError: false }),
  useIndexesQuery: () => ({ data: [] }),
  useAuthzQuery: () => ({ data: undefined, isError: false }),
}));
vi.mock("../context/SessionContext", () => ({
  useSession: () => ({ session: { role: "admin", principalName: "p", baseUrl: "http://127.0.0.1:302" } }),
}));

function renderPage() {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <MemoryRouter>
      <QueryClientProvider client={queryClient}>
        <IdePage />
      </QueryClientProvider>
    </MemoryRouter>,
  );
}

describe("IdePage: several statements in one cell", () => {
  beforeEach(() => {
    window.sessionStorage.clear();
    sqlMock.mockReset();
  });

  it("runs both statements, lets you switch between their results, and fills History exactly", async () => {
    sqlMock.mockResolvedValueOnce({ session_id: null, result: { kind: "ddl" } });
    sqlMock.mockResolvedValueOnce(ok(7));
    renderPage();
    fireEvent.change(screen.getByLabelText("Cell 1 SQL"), { target: { value: "CREATE TABLE t (x INTEGER);\nSELECT 7;" } });
    fireEvent.click(screen.getByRole("button", { name: "Run cell 1" }));
    const strip = await screen.findByRole("group", { name: "Statements in this cell" });
    expect(sqlMock.mock.calls.map((c) => c[0])).toEqual([{ sql: "CREATE TABLE t (x INTEGER)" }, { sql: "SELECT 7" }]);
    // The last statement's rows are showing; the first statement's outcome is one click away.
    expect(await screen.findByText("Showing 1–2 of 2 rows")).toBeInTheDocument();
    fireEvent.click(within(strip).getAllByRole("button")[0]);
    expect(screen.getByText("OK.")).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "History" }));
    const rows = await screen.findAllByTestId("history-row");
    expect(rows).toHaveLength(2);
    // Newest first, each with its own statement text, position and outcome.
    expect(within(rows[0]).getByLabelText("Statement text")).toHaveTextContent("SELECT 7");
    expect(rows[0]).toHaveTextContent("statement 2/2");
    expect(within(rows[1]).getByLabelText("Statement text")).toHaveTextContent("CREATE TABLE t (x INTEGER)");
    expect(rows[1]).toHaveTextContent("statement 1/2");
  });

  it("points the failing statement out and jumps to its line in the cell", async () => {
    sqlMock.mockResolvedValueOnce(ok(1));
    sqlMock.mockRejectedValueOnce(
      new ApiRequestError(400, { error: { code: "PARSE_ERROR", message: "the SQL text could not be parsed", detail: "expected FROM, found EOF at line 1 col 8" } }),
    );
    renderPage();
    fireEvent.change(screen.getByLabelText("Cell 1 SQL"), { target: { value: "SELECT 1;\n\nSELECT 2 +;\nSELECT 3;" } });
    fireEvent.click(screen.getByRole("button", { name: "Run cell 1" }));
    const alert = await screen.findByRole("alert");
    expect(within(alert).getByText("PARSE_ERROR")).toBeInTheDocument();
    expect(alert).toHaveTextContent("Statement 2 of 3 failed; the 1 after it did not run.");
    // The server said line 1 of the statement it received; that statement starts on line 3 of the cell.
    fireEvent.click(within(alert).getByRole("button", { name: /Jump to line 3/ }));
    await waitFor(() => expect((screen.getByLabelText("Cell 1 SQL") as HTMLTextAreaElement).selectionStart).toBe("SELECT 1;\n\n".length + 7));
    expect(sqlMock).toHaveBeenCalledTimes(2);
  });

  it("a trailing semicolon is a single statement and is not sent", async () => {
    sqlMock.mockResolvedValueOnce(ok(1));
    renderPage();
    fireEvent.change(screen.getByLabelText("Cell 1 SQL"), { target: { value: "SELECT 1;" } });
    fireEvent.click(screen.getByRole("button", { name: "Run cell 1" }));
    await screen.findByText("Showing 1–2 of 2 rows");
    expect(sqlMock.mock.calls[0][0]).toEqual({ sql: "SELECT 1" });
    expect(screen.queryByRole("group", { name: "Statements in this cell" })).toBeNull();
  });

  it("a cell holding only a comment cannot be run", () => {
    renderPage();
    fireEvent.change(screen.getByLabelText("Cell 1 SQL"), { target: { value: "-- just a note;" } });
    expect(screen.getByRole("button", { name: "Run cell 1" })).toBeDisabled();
  });
});

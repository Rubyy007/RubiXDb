import { beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter } from "react-router-dom";
import type { SqlRequestBody, SqlResponseBody } from "../api/types";
import { ApiRequestError } from "../api/types";
import { IdePage } from "./IdePage";

const sqlMock = vi.fn<(req: SqlRequestBody, signal?: AbortSignal) => Promise<SqlResponseBody>>();
const invalidateMock = vi.fn();
let fetching = false;
const UPDATED_AT = new Date(2026, 9, 13, 21, 30, 15).getTime();

vi.mock("../api/queries", () => ({
  useApiClient: () => ({ sql: sqlMock, describeTable: vi.fn() }),
  invalidateCatalogQueries: (...a: unknown[]) => invalidateMock(...a),
  useDatabasesQuery: () => ({ data: [{ database_id: 1, name: "default" }], isError: false, isLoading: false, isFetching: fetching, dataUpdatedAt: UPDATED_AT }),
  useSchemasQuery: () => ({ data: [{ schema_id: 1, database_id: 1, name: "public" }], isError: false, isLoading: false, isFetching: fetching, dataUpdatedAt: UPDATED_AT }),
  useTablesQuery: () => ({
    data: [
      { table_id: 1, schema_id: 1, name: "customers" },
      { table_id: 2, schema_id: 1, name: "orders" },
    ],
    isError: false,
    isLoading: false,
    isFetching: fetching,
    dataUpdatedAt: UPDATED_AT,
  }),
  useInstanceQuery: () => ({ data: { instance_id: "abc", name: "default" } }),
  useObservedQueriesQuery: () => ({ data: { queries: [], returned: 0, active: 0, untracked_active: 0, max_records: 200 }, isError: false }),
  useObservedSessionsQuery: () => ({ data: { sessions: [], returned: 0, total: 0, truncated: false, max_records: 200 }, isError: false }),
  useTableDescriptionQuery: () => ({ data: undefined, isLoading: false, isError: false }),
  useIndexesQuery: () => ({ data: [] }),
  useAuthzQuery: () => ({ data: undefined, isError: false }),
}));
vi.mock("../context/SessionContext", () => ({
  useSession: () => ({ session: { role: "admin", principalName: "p", baseUrl: "http://127.0.0.1:302" } }),
}));

function rows(columns: [string, string][], data: unknown[][]): SqlResponseBody {
  return {
    session_id: null,
    result: {
      kind: "rows",
      columns: columns.map(([name, type]) => ({ name, type, nullable: true })),
      rows: data.map((r) => r.map((v) => (typeof v === "number" ? { type: "integer" as const, value: v } : { type: "text" as const, value: String(v) }))),
      row_count: data.length,
    },
  };
}

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

const editor = (n = 1) => screen.getByLabelText(`Cell ${n} SQL`);
const type = (text: string, n = 1) => fireEvent.change(editor(n), { target: { value: text } });
const run = (n = 1) => fireEvent.click(screen.getByRole("button", { name: `Run cell ${n}` }));

describe("IdePage", () => {
  beforeEach(() => {
    window.sessionStorage.clear();
    sqlMock.mockReset();
    // jsdom does not implement these two; the page must still work without them.
    window.confirm = vi.fn(() => true);
  });

  it("starts with one SQL cell, the toolbar and an honest status strip", () => {
    renderPage();
    expect(editor()).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /Run all/ })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /Stop/ })).toBeDisabled();
    expect(screen.getByText("0 running, 0 queued")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Run cell 1" })).toBeDisabled();
    expect(screen.getByText("default:302")).toBeInTheDocument();
  });

  it("runs a cell and shows typed rows with the response unchanged", async () => {
    sqlMock.mockResolvedValueOnce(rows([["id", "integer"], ["name", "text"]], [[1, "Ada"], [2, "<b>x</b>"]]));
    renderPage();
    type("SELECT id, name FROM customers");
    run();
    expect(sqlMock).toHaveBeenCalledWith({ sql: "SELECT id, name FROM customers" }, expect.any(AbortSignal));
    expect(await screen.findByText("Ada")).toBeInTheDocument();
    // Result text is a text node, never markup.
    expect(screen.getByText("<b>x</b>")).toBeInTheDocument();
    expect(document.querySelector("b")).toBeNull();
    expect(screen.getByRole("tab", { name: /Results \(2 rows/ })).toBeInTheDocument();
  });

  it("shows a single value as a number card", async () => {
    sqlMock.mockResolvedValueOnce(rows([["count", "bigint"]], [[311]]));
    renderPage();
    type("SELECT COUNT(*) FROM orders");
    run();
    const card = await screen.findByRole("group", { name: "Single value result" });
    expect(within(card).getByText("311")).toBeInTheDocument();
  });

  it("shows the exact error code, message and detail, and jumps to the line", async () => {
    sqlMock.mockRejectedValueOnce(
      new ApiRequestError(400, { error: { code: "PARSE_ERROR", message: "the SQL text could not be parsed", detail: "expected FROM, found ORDER at line 3, col 1" } }),
    );
    renderPage();
    type("SELECT id\nFROM\nORDER BY id");
    run();
    const alert = await screen.findByRole("alert");
    expect(alert).toHaveAttribute("data-code", "PARSE_ERROR");
    expect(within(alert).getByText("PARSE_ERROR")).toBeInTheDocument();
    expect(within(alert).getByText("the SQL text could not be parsed")).toBeInTheDocument();
    expect(within(alert).getByText(/HTTP 400/)).toBeInTheDocument();
    fireEvent.click(within(alert).getByRole("button", { name: "Jump to line 3" }));
    expect((editor() as HTMLTextAreaElement).selectionStart).toBe("SELECT id\nFROM\n".length);
  });

  it("Stop aborts the request and marks the cell cancelled", async () => {
    sqlMock.mockImplementationOnce(
      (_req, signal) => new Promise((_ok, fail) => signal?.addEventListener("abort", () => fail(new DOMException("aborted", "AbortError")))),
    );
    renderPage();
    type("SELECT * FROM events");
    run();
    expect(await screen.findByText(/Request in flight/)).toBeInTheDocument();
    expect(screen.getByText("1 running, 0 queued")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: /^Stop/ }));
    const alert = await screen.findByRole("alert");
    expect(within(alert).getByText("CANCELLED")).toBeInTheDocument();
    expect(screen.getByText("0 running, 0 queued")).toBeInTheDocument();
  });

  it("opens the command palette with Ctrl+K and filters by fuzzy text", () => {
    renderPage();
    fireEvent.keyDown(window, { key: "k", ctrlKey: true });
    const dialog = screen.getByRole("dialog", { name: "Command palette" });
    fireEvent.change(within(dialog).getByRole("combobox"), { target: { value: "dark" } });
    expect(within(dialog).getByRole("option", { name: /Toggle dark mode/ })).toBeInTheDocument();
    expect(within(dialog).queryByRole("option", { name: /Run all/ })).toBeNull();
    fireEvent.keyDown(dialog, { key: "Escape" });
    expect(screen.queryByRole("dialog", { name: "Command palette" })).toBeNull();
  });

  it("runs a transaction container: BEGIN, a cell on the session, then COMMIT", async () => {
    renderPage();
    fireEvent.click(screen.getAllByRole("button", { name: "+ Transaction" })[0]);
    // The container's child cell is cell 2 and cannot run before BEGIN.
    expect(screen.getByRole("button", { name: "Run cell 2" })).toBeDisabled();
    sqlMock.mockResolvedValueOnce({ session_id: "S-1", result: { kind: "begin" } });
    fireEvent.click(screen.getByRole("button", { name: "Begin" }));
    expect(await screen.findByText(/session S-1/)).toBeInTheDocument();
    expect(sqlMock).toHaveBeenLastCalledWith({ sql: "BEGIN" }, expect.any(AbortSignal));

    type("UPDATE orders SET status = 'x' WHERE id = 1", 2);
    sqlMock.mockResolvedValueOnce({ session_id: "S-1", result: { kind: "write", statement: "UPDATE", rows_affected: 1 } });
    run(2);
    expect(sqlMock).toHaveBeenLastCalledWith({ sql: "UPDATE orders SET status = 'x' WHERE id = 1", session_id: "S-1" }, expect.any(AbortSignal));
    expect(await screen.findByText(/1 row affected/)).toBeInTheDocument();

    sqlMock.mockResolvedValueOnce({ session_id: null, result: { kind: "commit" } });
    fireEvent.click(screen.getAllByRole("button", { name: "Commit" })[0]);
    await waitFor(() => expect(sqlMock).toHaveBeenLastCalledWith({ sql: "COMMIT", session_id: "S-1" }, expect.any(AbortSignal)));
    await waitFor(() => expect(screen.queryByText(/session S-1/)).toBeNull());
  });

  it("with autocommit off, a plain cell runs inside a worksheet transaction", async () => {
    renderPage();
    fireEvent.click(screen.getByRole("checkbox", { name: "Autocommit" }));
    type("SELECT 1");
    sqlMock.mockResolvedValueOnce({ session_id: "S-9", result: { kind: "begin" } });
    sqlMock.mockResolvedValueOnce({ ...rows([["x", "integer"]], [[1]]), session_id: "S-9" });
    run();
    await waitFor(() => expect(sqlMock).toHaveBeenCalledTimes(2));
    expect(sqlMock.mock.calls[0][0]).toEqual({ sql: "BEGIN" });
    expect(sqlMock.mock.calls[1][0]).toEqual({ sql: "SELECT 1", session_id: "S-9" });
  });

  it("keeps worksheet text across a reload", async () => {
    const first = renderPage();
    type("SELECT 42");
    await act(async () => {
      await new Promise((r) => setTimeout(r, 600));
    });
    first.unmount();
    renderPage();
    expect((editor() as HTMLTextAreaElement).value).toBe("SELECT 42");
  });

  it("renders scratchpad notes as text, never HTML", () => {
    renderPage();
    const pad = screen.getByRole("region", { name: "Scratchpad · markdown" });
    fireEvent.click(within(pad).getByRole("button", { name: "Edit" }));
    fireEvent.change(within(pad).getByRole("textbox"), { target: { value: "<img src=x onerror=alert(1)> **ok**" } });
    fireEvent.click(within(pad).getByRole("button", { name: "Done" }));
    expect(pad.querySelector("img")).toBeNull();
    expect(pad.querySelector("strong")?.textContent).toBe("ok");
  });

  it("lists the features that are not yet developed", () => {
    renderPage();
    fireEvent.keyDown(window, { key: "k", ctrlKey: true });
    fireEvent.click(screen.getByRole("option", { name: /Keyboard shortcuts/ }));
    const dialog = screen.getByRole("dialog", { name: /Keyboard shortcuts/ });
    expect(within(dialog).getByText("Not yet developed")).toBeInTheDocument();
    expect(within(dialog).getByText(/pinned snapshot/)).toBeInTheDocument();
  });

  it("creates, renames and closes worksheets", () => {
    renderPage();
    fireEvent.click(screen.getByRole("button", { name: /New worksheet \(Ctrl\+T\)/ }));
    expect(screen.getAllByRole("tab", { name: /Untitled/ })).toHaveLength(2);
    const title = screen.getByLabelText("Worksheet title");
    fireEvent.change(title, { target: { value: "Revenue" } });
    fireEvent.blur(title);
    expect(screen.getByRole("tab", { name: /Revenue/ })).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Close worksheet Revenue" }));
    expect(screen.queryByRole("tab", { name: /Revenue/ })).toBeNull();
  });
});

describe("IdePage: refresh the databases section", () => {
  const openRail = () => fireEvent.click(screen.getByRole("button", { name: /Expand left rail/ }));
  beforeEach(() => {
    window.sessionStorage.clear();
    sqlMock.mockReset();
    invalidateMock.mockReset();
    fetching = false;
  });

  it("has a Refresh databases button in the Databases section that re-reads the catalog", () => {
    renderPage();
    openRail();
    const section = screen.getByRole("heading", { name: "Databases" }).closest("section") as HTMLElement;
    const btn = within(section).getByRole("button", { name: "Refresh databases" });
    expect(invalidateMock).not.toHaveBeenCalled();
    fireEvent.click(btn);
    expect(invalidateMock).toHaveBeenCalledTimes(1);
  });

  it("says when the catalog was last read, and that it is refreshing while it is", () => {
    const first = renderPage();
    openRail();
    expect(screen.getByText(`Updated ${new Date(UPDATED_AT).toLocaleTimeString()}`)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Refresh databases" })).toHaveAttribute("aria-busy", "false");
    first.unmount();
    fetching = true;
    renderPage();
    openRail();
    expect(screen.getByText("Refreshing…")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Refresh databases" })).toHaveAttribute("aria-busy", "true");
  });
});

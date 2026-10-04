import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { SqlConsolePage } from "./SqlConsolePage";
import type { SqlResponseBody } from "../api/types";

const sqlMock = vi.fn<(req: unknown, signal?: AbortSignal) => Promise<SqlResponseBody>>();

vi.mock("../api/queries", () => ({
  useApiClient: () => ({ sql: sqlMock }),
  invalidateCatalogQueries: () => {},
  useDatabasesQuery: () => ({ data: [{ database_id: 1, name: "default" }], isError: false }),
  useSchemasQuery: () => ({ data: [{ schema_id: 1, database_id: 1, name: "public" }], isError: false }),
}));
vi.mock("../context/SessionContext", () => ({
  useSession: () => ({ session: { role: "admin", principalName: "p" } }),
}));

function renderPage() {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={queryClient}>
      <SqlConsolePage />
    </QueryClientProvider>,
  );
}

function typeSql(text: string) {
  fireEvent.change(screen.getByLabelText("SQL"), { target: { value: text } });
}

function clickExecute() {
  fireEvent.click(screen.getByRole("button", { name: "Run" }));
}

describe("SqlConsolePage", () => {
  beforeEach(() => {
    // worksheet text is kept in sessionStorage; start every test from a fresh browser tab
    window.sessionStorage.clear();
  });

  it("renders the editor and a disabled Execute button with empty input", () => {
    renderPage();
    expect(screen.getByLabelText("SQL")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Run" })).toBeDisabled();
  });

  it("executes a query and renders typed rows including NULL", async () => {
    sqlMock.mockResolvedValueOnce({
      session_id: null,
      result: {
        kind: "rows",
        columns: [{ name: "id", type: "integer", nullable: false }, { name: "name", type: "text", nullable: true }],
        rows: [
          [{ type: "integer", value: 1 }, { type: "text", value: "alice" }],
          [{ type: "integer", value: 2 }, { type: "null" }],
        ],
        row_count: 2,
      },
    });
    renderPage();
    typeSql("SELECT id, name FROM t");
    clickExecute();

    await waitFor(() => expect(screen.getByText("alice")).toBeInTheDocument());
    expect(screen.getByText("NULL")).toBeInTheDocument();
    expect(screen.getByText(/Result \(2 rows\)/)).toBeInTheDocument();
    expect(sqlMock).toHaveBeenCalledWith(
      { sql: "SELECT id, name FROM t", session_id: null },
      expect.any(AbortSignal),
    );
  });

  it("renders adversarial row content as inert text, never as executed HTML (item 93/94/126)", async () => {
    sqlMock.mockResolvedValueOnce({
      session_id: null,
      result: {
        kind: "rows",
        columns: [{ name: "payload", type: "text", nullable: false }],
        rows: [[{ type: "text", value: "<img src=x onerror=alert(1)>" }]],
        row_count: 1,
      },
    });
    renderPage();
    typeSql("SELECT payload FROM t");
    clickExecute();

    await waitFor(() => expect(screen.getByText("<img src=x onerror=alert(1)>")).toBeInTheDocument());
    // React renders this as a text node, not as markup -- there must be
    // no actual <img> element created from result data anywhere in the
    // result table.
    expect(document.querySelectorAll(".table-wrap img").length).toBe(0);
  });

  it("shows a BEGIN response as an open transaction and includes session_id on the next call", async () => {
    sqlMock.mockResolvedValueOnce({ session_id: "sess-1", result: { kind: "begin" } });
    renderPage();
    typeSql("BEGIN");
    clickExecute();

    await waitFor(() => expect(screen.getByText("transaction open")).toBeInTheDocument());

    sqlMock.mockResolvedValueOnce({
      session_id: "sess-1",
      result: { kind: "write", statement: "INSERT", rows_affected: 1 },
    });
    typeSql("INSERT INTO t (id) VALUES (1)");
    clickExecute();

    await waitFor(() =>
      expect(sqlMock).toHaveBeenLastCalledWith(
        { sql: "INSERT INTO t (id) VALUES (1)", session_id: "sess-1" },
        expect.any(AbortSignal),
      ),
    );
  });

  it("renders a stable API error without a raw stack trace", async () => {
    const { ApiRequestError } = await import("../api/types");
    sqlMock.mockRejectedValueOnce(
      new ApiRequestError(400, { error: { code: "PARSE_ERROR", message: "the SQL text could not be parsed" } }),
    );
    renderPage();
    typeSql("GARBAGE");
    clickExecute();

    await waitFor(() =>
      expect(screen.getByRole("alert")).toHaveTextContent("PARSE_ERROR: the SQL text could not be parsed"),
    );
  });

  it("Clear resets the editor and result", async () => {
    sqlMock.mockResolvedValueOnce({
      session_id: null,
      result: { kind: "rows", columns: [], rows: [], row_count: 0 },
    });
    renderPage();
    typeSql("SELECT 1");
    clickExecute();
    await waitFor(() => expect(screen.getByText(/Result \(0 rows\)/)).toBeInTheDocument());

    fireEvent.click(screen.getByRole("button", { name: "Clear" }));
    expect(screen.getByLabelText("SQL")).toHaveValue("");
    expect(screen.queryByText(/Result \(0 rows\)/)).not.toBeInTheDocument();
  });
});

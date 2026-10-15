import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter } from "react-router-dom";
import type { SqlRequestBody, SqlResponseBody } from "../api/types";
import { ApiRequestError } from "../api/types";
import { IdePage } from "./IdePage";

const sqlMock = vi.fn<(req: SqlRequestBody, signal?: AbortSignal) => Promise<SqlResponseBody>>();
const UPDATED_AT = new Date(2026, 9, 13, 21, 30, 15).getTime();
const ok = { isError: false, isLoading: false, isFetching: false, dataUpdatedAt: UPDATED_AT };

vi.mock("../api/queries", () => ({
  useApiClient: () => ({ sql: sqlMock, describeTable: vi.fn().mockResolvedValue({ table_id: 1, name: "t", columns: [] }) }),
  invalidateCatalogQueries: vi.fn(),
  useDatabasesQuery: () => ({ data: [{ database_id: 1, name: "default" }], ...ok }),
  useSchemasQuery: () => ({
    data: [
      { schema_id: 1, database_id: 1, name: "public" },
      { schema_id: 2, database_id: 1, name: "analytics" },
    ],
    ...ok,
  }),
  useTablesQuery: () => ({
    data: [
      { table_id: 1, schema_id: 1, name: "customers", schema_name: "public" },
      { table_id: 2, schema_id: 2, name: "users", schema_name: "analytics" },
    ],
    ...ok,
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

const editor = () => screen.getByLabelText("Cell 1 SQL");
const runSql = (sql: string) => {
  fireEvent.change(editor(), { target: { value: sql } });
  fireEvent.click(screen.getByRole("button", { name: "Run cell 1" }));
};
const ddl = (statement: string, message: string, changed = true): SqlResponseBody => ({
  session_id: null,
  result: { kind: "ddl", statement, message, changed, object: "x" },
});

describe("guided results and errors", () => {
  beforeEach(() => {
    window.sessionStorage.clear();
    window.localStorage.clear();
    sqlMock.mockReset();
    window.confirm = vi.fn(() => true);
  });

  it("says what completed instead of a bare OK", async () => {
    sqlMock.mockResolvedValueOnce(ddl("CREATE SCHEMA", "Schema default.analytics created."));
    renderPage();
    runSql("CREATE SCHEMA analytics");
    expect(await screen.findByText("Query execution completed")).toBeInTheDocument();
    expect(screen.getByText("Schema default.analytics created.")).toBeInTheDocument();
    expect(screen.queryByText("OK.")).toBeNull();
  });

  it("marks an IF NOT EXISTS no-op as completed with nothing changed", async () => {
    sqlMock.mockResolvedValueOnce(ddl("CREATE SCHEMA", "Schema default.analytics already exists; nothing changed.", false));
    renderPage();
    runSql("CREATE SCHEMA IF NOT EXISTS analytics");
    expect(await screen.findByText("Query execution completed — nothing changed")).toBeInTheDocument();
    expect(screen.getByText(/already exists; nothing changed/)).toBeInTheDocument();
  });

  it("shows a 409 with its code, status and the server's hint", async () => {
    sqlMock.mockRejectedValueOnce(
      new ApiRequestError(409, {
        error: { code: "ALREADY_EXISTS", message: 'schema "analytics" already exists', hint: "Pick a different name, or add IF NOT EXISTS." },
      }),
    );
    renderPage();
    runSql("CREATE SCHEMA analytics");
    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent("ALREADY_EXISTS");
    expect(alert).toHaveTextContent('schema "analytics" already exists');
    expect(alert).toHaveTextContent("HTTP 409");
    expect(screen.getByTestId("error-hint")).toHaveTextContent("What to do: Pick a different name, or add IF NOT EXISTS.");
  });

  it("shows the CREATE DATABASE refusal verbatim as a 400", async () => {
    const message =
      "CREATE DATABASE is disabled. rubiXDb v1 runs on a single database ('default'). Use CREATE SCHEMA <schema_name> to organize your tables.";
    sqlMock.mockRejectedValueOnce(new ApiRequestError(400, { error: { code: "UNSUPPORTED", message } }));
    renderPage();
    runSql("CREATE DATABASE sales");
    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent(message);
    expect(alert).toHaveTextContent("HTTP 400");
  });

  it("offers a did-you-mean from the loaded catalog for a missing table or schema", async () => {
    sqlMock.mockRejectedValueOnce(
      new ApiRequestError(404, { error: { code: "NOT_FOUND", message: "the referenced table was not found", detail: "usrs" } }),
    );
    renderPage();
    runSql("SELECT * FROM usrs");
    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent("Did you mean users?");

    sqlMock.mockRejectedValueOnce(
      new ApiRequestError(404, { error: { code: "NOT_FOUND", message: "the referenced schema was not found", detail: "analytcs" } }),
    );
    runSql("USE SCHEMA analytcs");
    await waitFor(() => expect(screen.getByRole("alert")).toHaveTextContent("Did you mean analytics?"));
  });

  it("USE SCHEMA shows the active schema and sends it with the next statement", async () => {
    sqlMock.mockResolvedValueOnce({
      session_id: null,
      result: { kind: "use_schema", database: "default", schema: "analytics", message: "Now using schema default.analytics." },
    });
    renderPage();
    // Before any USE SCHEMA the chip shows the server's default (the first schema).
    expect(screen.getByText("default.public")).toBeInTheDocument();
    runSql("USE SCHEMA analytics");
    expect(await screen.findByText("Now using schema default.analytics.")).toBeInTheDocument();
    await waitFor(() => expect(screen.getByText("default.analytics")).toBeInTheDocument());
    expect(window.localStorage.getItem("rubixdb-ide-schema")).toBe("analytics");

    sqlMock.mockResolvedValueOnce({ session_id: null, result: { kind: "rows", columns: [], rows: [], row_count: 0 } });
    runSql("SELECT * FROM users");
    await waitFor(() => expect(sqlMock).toHaveBeenCalledTimes(2));
    expect(sqlMock).toHaveBeenLastCalledWith({ sql: "SELECT * FROM users", schema: "analytics" }, expect.any(AbortSignal));
  });

  it("remembers the chosen schema across a reload", () => {
    window.localStorage.setItem("rubixdb-ide-schema", "analytics");
    renderPage();
    expect(screen.getByText("default.analytics")).toBeInTheDocument();
  });
});

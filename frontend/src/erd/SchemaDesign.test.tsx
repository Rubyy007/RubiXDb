import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { TableDescription } from "../api/types";
import { SchemaDesign } from "./SchemaDesign";

const describeTable = vi.fn<(name: string) => Promise<TableDescription>>();
let tables: { table_id: number; schema_id: number; name: string }[] = [];
const schemas = [
  { schema_id: 1, database_id: 1, name: "public" },
  { schema_id: 2, database_id: 1, name: "ex1" },
];

vi.mock("../api/queries", () => ({
  useApiClient: () => ({ describeTable }),
  useSchemasQuery: () => ({ data: schemas, isLoading: false, isError: false }),
  useTablesQuery: () => ({ data: tables, isLoading: false, isError: false }),
}));
vi.mock("../context/SessionContext", () => ({
  useSession: () => ({ session: { role: "admin", principalName: "p", baseUrl: "http://127.0.0.1:302" } }),
}));

const col = (ordinal: number, name: string, data_type: string, primary_key = false) => ({ ordinal, name, data_type, nullable: !primary_key, primary_key });
const DESCRIPTIONS: Record<string, TableDescription> = {
  employees: { table_id: 2, name: "employees", columns: [col(0, "id", "integer", true), col(1, "name", "text"), col(2, "department", "text")] },
  departments: { table_id: 3, name: "departments", columns: [col(0, "dept_name", "text", true), col(1, "budget", "decimal")] },
  solo: { table_id: 4, name: "solo", columns: [col(0, "id", "integer", true)] },
};

function renderIt() {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={queryClient}>
      <SchemaDesign />
    </QueryClientProvider>,
  );
}

describe("SchemaDesign", () => {
  beforeEach(() => {
    window.localStorage.clear();
    describeTable.mockReset();
    describeTable.mockImplementation(async (name) => DESCRIPTIONS[name]);
    tables = [
      { table_id: 2, schema_id: 1, name: "employees" },
      { table_id: 3, schema_id: 1, name: "departments" },
      { table_id: 4, schema_id: 1, name: "solo" },
    ];
  });

  it("draws every table as a card with its columns, types and primary key", async () => {
    renderIt();
    const card = await screen.findByRole("article", { name: /Table public\.departments, 2 columns, 1 inferred relationship/ });
    expect(within(card).getByText("dept_name")).toBeInTheDocument();
    expect(within(card).getByText("budget")).toBeInTheDocument();
    expect(within(card).getAllByText("PK")).toHaveLength(1);
    expect(screen.getAllByRole("article")).toHaveLength(3);
  });

  it("infers employees.department -> departments.dept_name, says it is inferred, and lists it", async () => {
    renderIt();
    const row = await screen.findByTestId("erd-link-row");
    expect(row).toHaveTextContent("public.employees.department");
    expect(row).toHaveTextContent("public.departments.dept_name");
    expect(row).toHaveTextContent("table name");
    expect(screen.getByRole("status")).toHaveTextContent("3 tables · 1 inferred relationship");
    expect(screen.getAllByText(/no foreign keys/i).length).toBeGreaterThan(0);
    expect(document.querySelectorAll(".erd-link")).toHaveLength(1);
  });

  it("hides the lines but keeps the cards when Show relationships is turned off", async () => {
    renderIt();
    await screen.findByTestId("erd-link-row");
    fireEvent.click(screen.getByRole("checkbox", { name: "Show relationships" }));
    expect(document.querySelectorAll(".erd-link")).toHaveLength(0);
    expect(screen.getAllByRole("article")).toHaveLength(3);
  });

  it("is honest about schemas whose tables the API does not list", async () => {
    renderIt();
    expect(await screen.findByRole("note")).toHaveTextContent("No tables are listed for: ex1");
  });

  it("selecting a line explains it", async () => {
    renderIt();
    await screen.findByTestId("erd-link-row");
    expect(document.querySelector(".erd-picked")).toBeNull();
    fireEvent.click(document.querySelector(".erd-link") as Element);
    await waitFor(() => expect(document.querySelector(".erd-picked")).not.toBeNull());
    const strip = document.querySelector(".erd-picked") as HTMLElement;
    expect(strip).toHaveTextContent("public.employees.department → public.departments.dept_name");
    expect(strip).toHaveTextContent("inferred from names, not enforced by the database");
    // Selecting the same line again closes it.
    fireEvent.click(document.querySelector(".erd-link") as Element);
    await waitFor(() => expect(document.querySelector(".erd-picked")).toBeNull());
  });

  it("a table can be moved with the arrow keys and the position is remembered", async () => {
    renderIt();
    const card = await screen.findByRole("article", { name: /Table public\.solo/ });
    const left = () => Number.parseFloat((screen.getByRole("article", { name: /Table public\.solo/ }) as HTMLElement).style.left);
    const before = left();
    card.focus();
    fireEvent.keyDown(card, { key: "ArrowRight" });
    fireEvent.keyDown(card, { key: "ArrowRight", shiftKey: true });
    expect(left()).toBe(before + 50);
    const stored = JSON.parse(window.localStorage.getItem("rubixdb-erd-layout-v1:http://127.0.0.1:302") ?? "{}");
    expect(stored["public.solo"].x).toBe(before + 50);
  });

  it("shows a card that could not be described instead of dropping it", async () => {
    describeTable.mockImplementation(async (name) => {
      if (name === "solo") throw new Error("boom");
      return DESCRIPTIONS[name];
    });
    renderIt();
    expect(await screen.findByText("columns unavailable")).toBeInTheDocument();
    expect(screen.getAllByRole("article")).toHaveLength(3);
    await waitFor(() => expect(screen.getByText(/Columns could not be read for 1 table/)).toBeInTheDocument());
  });

  it("says so when there are no tables", async () => {
    tables = [];
    renderIt();
    expect(await screen.findByText(/No tables to draw yet/)).toBeInTheDocument();
    expect(screen.queryByRole("article")).toBeNull();
  });

  it("ignores garbage in the saved layout", async () => {
    window.localStorage.setItem("rubixdb-erd-layout-v1:http://127.0.0.1:302", '{"public.solo":{"x":"a","y":null},"public.nope":5}');
    renderIt();
    await screen.findByRole("article", { name: /Table public\.solo/ });
    expect(screen.getAllByRole("article")).toHaveLength(3);
  });
});

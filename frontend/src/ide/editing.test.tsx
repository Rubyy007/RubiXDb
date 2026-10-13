import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter } from "react-router-dom";
import type { SqlRequestBody, SqlResponseBody, TableDescription } from "../api/types";
import { ApiRequestError } from "../api/types";
import { IdePage } from "./IdePage";

const sqlMock = vi.fn<(req: SqlRequestBody, signal?: AbortSignal) => Promise<SqlResponseBody>>();
const describeMock = vi.fn<(name: string) => Promise<TableDescription>>();

vi.mock("../api/queries", () => ({
  useApiClient: () => ({ sql: sqlMock, describeTable: describeMock }),
  invalidateCatalogQueries: () => {},
  useDatabasesQuery: () => ({ data: [{ database_id: 1, name: "default" }], isError: false, isLoading: false }),
  useSchemasQuery: () => ({ data: [{ schema_id: 1, database_id: 1, name: "public" }], isError: false, isLoading: false }),
  useTablesQuery: () => ({
    data: [
      { table_id: 1, schema_id: 1, name: "customers" },
      { table_id: 2, schema_id: 1, name: "orders" },
    ],
    isError: false,
    isLoading: false,
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

const col = (ordinal: number, name: string, data_type: string, primary_key = false) => ({ ordinal, name, data_type, nullable: !primary_key, primary_key });
const TABLES: Record<string, TableDescription> = {
  customers: { table_id: 1, name: "customers", columns: [col(0, "id", "integer", true), col(1, "name", "text"), col(2, "email", "text")] },
  orders: { table_id: 2, name: "orders", columns: [col(0, "id", "integer", true), col(1, "customer_id", "integer"), col(2, "total", "decimal")] },
};
const ok: SqlResponseBody = {
  session_id: null,
  result: { kind: "rows", columns: [{ name: "x", type: "integer", nullable: false }], rows: [[{ type: "integer", value: 1 }]], row_count: 1 },
};

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

const editor = (n = 1) => screen.getByLabelText(`Cell ${n} SQL`) as HTMLTextAreaElement;
const typeText = (text: string, n = 1) => {
  const ta = editor(n);
  fireEvent.change(ta, { target: { value: text } });
  ta.setSelectionRange(text.length, text.length);
  return ta;
};
/** The suggestion list only: the page has other options too (the cell-type select). */
const list = () => screen.getByRole("listbox", { name: "Suggestions" });
const options = () => within(list()).queryAllByRole("option").map((o) => o.getAttribute("aria-label") ?? "");
const pick = async (name: RegExp) => within(await screen.findByRole("listbox", { name: "Suggestions" })).findByRole("option", { name });

/** What the browser sends before it inserts a typed character. */
function beforeInput(ta: HTMLTextAreaElement, data: string) {
  const ev = new InputEvent("beforeinput", { inputType: "insertText", data, bubbles: true, cancelable: true });
  ta.dispatchEvent(ev);
  return ev;
}

describe("suggestions in a cell", () => {
  beforeEach(() => {
    window.sessionStorage.clear();
    window.localStorage.clear();
    sqlMock.mockReset();
    describeMock.mockReset();
    describeMock.mockImplementation(async (name) => TABLES[name]);
  });

  it("suggests a table after FROM for what was typed, with its schema", async () => {
    renderPage();
    typeText("select * from cu");
    const list = await screen.findByRole("listbox", { name: "Suggestions" });
    expect(within(list).getByRole("option", { name: /customers/ })).toBeInTheDocument();
    expect(within(list).queryByRole("option", { name: /orders/ })).toBeNull();
    expect(list).toHaveTextContent("public");
  });

  it("Tab inserts the suggestion", async () => {
    renderPage();
    const ta = typeText("select * from cu");
    await screen.findByRole("listbox", { name: "Suggestions" });
    fireEvent.keyDown(ta, { key: "Tab" });
    expect(ta.value).toBe("select * from customers");
    expect(screen.queryByRole("listbox", { name: "Suggestions" })).toBeNull();
  });

  it("clicking a suggestion inserts it", async () => {
    renderPage();
    const ta = typeText("select * from ord");
    fireEvent.mouseDown(await pick(/orders/));
    expect(ta.value).toBe("select * from orders");
  });

  it("suggests the columns of the table in the statement, with their types", async () => {
    renderPage();
    typeText("select * from orders where cu");
    const opt = await pick(/customer_id/);
    expect(opt).toHaveTextContent("orders · integer");
    expect(describeMock).toHaveBeenCalledWith("orders");
  });

  it("suggests columns after an alias and a dot", async () => {
    renderPage();
    typeText("select * from customers c where c.em");
    expect(await pick(/email/)).toBeInTheDocument();
    expect(options().some((o) => /^name,/.test(o))).toBe(false); // only what starts with "em"
  });

  it("moving the caret through unchanged text does not open a list", async () => {
    renderPage();
    const ta = typeText("select * from customers");
    expect(screen.queryByRole("listbox", { name: "Suggestions" })).toBeNull();
    ta.setSelectionRange(16, 16);
    fireEvent.keyUp(ta, { key: "ArrowLeft" });
    fireEvent.click(ta);
    expect(screen.queryByRole("listbox", { name: "Suggestions" })).toBeNull();
  });

  it("a list that opened by itself does not swallow Enter; choosing with the arrow keys does", async () => {
    renderPage();
    const ta = typeText("select * from ");
    await screen.findByRole("listbox", { name: "Suggestions" });
    // Enter without choosing: not prevented (the browser inserts its new line), list closes, nothing inserted.
    expect(fireEvent.keyDown(ta, { key: "Enter" })).toBe(true);
    expect(ta.value).toBe("select * from ");
    expect(screen.queryByRole("listbox", { name: "Suggestions" })).toBeNull();
    // Again, but this time the user moves down to a row first.
    typeText("select * from  ");
    typeText("select * from ");
    await screen.findByRole("listbox", { name: "Suggestions" });
    fireEvent.keyDown(ta, { key: "ArrowDown" });
    fireEvent.keyDown(ta, { key: "ArrowDown" });
    expect(within(list()).getByRole("option", { selected: true })).toHaveTextContent("orders");
    expect(fireEvent.keyDown(ta, { key: "Enter" })).toBe(false);
    expect(ta.value).toBe("select * from orders");
  });

  it("Escape closes the list without leaving the editor", async () => {
    renderPage();
    const ta = typeText("select * from cu");
    await screen.findByRole("listbox", { name: "Suggestions" });
    ta.focus();
    fireEvent.keyDown(ta, { key: "Escape" });
    expect(screen.queryByRole("listbox", { name: "Suggestions" })).toBeNull();
    expect(document.activeElement).toBe(ta);
  });

  it("Ctrl+Space offers statements in an empty cell", async () => {
    renderPage();
    const ta = editor();
    expect(screen.queryByRole("listbox", { name: "Suggestions" })).toBeNull();
    fireEvent.keyDown(ta, { key: " ", ctrlKey: true });
    expect(await pick(/SELECT \* FROM/)).toBeInTheDocument();
  });

  it("offers the keyword that usually comes next", async () => {
    renderPage();
    typeText("select * from orders wh");
    expect(await pick(/WHERE/)).toBeInTheDocument();
    expect(options().some((o) => /WHEN/.test(o))).toBe(false);
  });

  it("says nothing inside a string", () => {
    renderPage();
    typeText("select 'cu");
    expect(screen.queryByRole("listbox", { name: "Suggestions" })).toBeNull();
  });
});

describe("keywords become UPPERCASE as they are typed", () => {
  beforeEach(() => {
    window.sessionStorage.clear();
    window.localStorage.clear();
    sqlMock.mockReset();
    describeMock.mockImplementation(async (name) => TABLES[name]);
  });

  it("uppercases a finished keyword and puts the caret after the typed separator", async () => {
    renderPage();
    const ta = editor();
    fireEvent.change(ta, { target: { value: "select" } });
    ta.focus();
    ta.setSelectionRange(6, 6);
    const ev = beforeInput(ta, " ");
    expect(ev.defaultPrevented).toBe(true); // the editor made the whole edit itself
    await waitFor(() => expect(ta.value).toBe("SELECT "));
    expect(ta.selectionStart).toBe(7);
  });

  it("leaves names that are not keywords alone", () => {
    renderPage();
    const ta = editor();
    fireEvent.change(ta, { target: { value: "SELECT name" } });
    ta.focus();
    ta.setSelectionRange(11, 11);
    expect(beforeInput(ta, " ").defaultPrevented).toBe(false);
    expect(ta.value).toBe("SELECT name");
  });

  it("leaves a keyword inside a string alone", () => {
    renderPage();
    const ta = editor();
    fireEvent.change(ta, { target: { value: "SELECT 'from" } });
    ta.focus();
    ta.setSelectionRange(12, 12);
    expect(beforeInput(ta, " ").defaultPrevented).toBe(false);
  });

  it("never changes the name of a table", () => {
    renderPage();
    // `orders` is a table; a table that happened to be called like a keyword would be kept the same way.
    const ta = editor();
    fireEvent.change(ta, { target: { value: "SELECT * FROM orders" } });
    ta.focus();
    ta.setSelectionRange(20, 20);
    expect(beforeInput(ta, " ").defaultPrevented).toBe(false);
    expect(ta.value).toBe("SELECT * FROM orders");
  });

  it("can be turned off with the toolbar switch, and the choice is remembered", () => {
    renderPage();
    const sw = screen.getByRole("checkbox", { name: "UPPERCASE keywords" });
    expect(sw).toBeChecked();
    fireEvent.click(sw);
    expect(window.localStorage.getItem("rubixdb-ide-auto-upper")).toBe("off");
    const ta = editor();
    fireEvent.change(ta, { target: { value: "select" } });
    ta.focus();
    ta.setSelectionRange(6, 6);
    expect(beforeInput(ta, " ").defaultPrevented).toBe(false);
    expect(ta.value).toBe("select");
  });

  it("starts switched off when it was switched off before", () => {
    window.localStorage.setItem("rubixdb-ide-auto-upper", "off");
    renderPage();
    expect(screen.getByRole("checkbox", { name: "UPPERCASE keywords" })).not.toBeChecked();
  });

  it("does not touch a selection or text typed through composition", () => {
    renderPage();
    const ta = editor();
    fireEvent.change(ta, { target: { value: "select" } });
    ta.focus();
    ta.setSelectionRange(0, 6); // a selection: typing replaces it
    expect(beforeInput(ta, " ").defaultPrevented).toBe(false);
  });
});

describe("an error stays in the cell and the statement that caused it", () => {
  beforeEach(() => {
    window.sessionStorage.clear();
    window.localStorage.clear();
    sqlMock.mockReset();
    describeMock.mockImplementation(async (name) => TABLES[name]);
  });
  const parseError = new ApiRequestError(400, {
    error: { code: "PARSE_ERROR", message: "the SQL text could not be parsed", detail: "Expected: an SQL statement, found: SELEC at Line: 1, Column: 1" },
  });
  // Fails by the text sent, not by call order: Run all starts the cells at the same time.
  const failOnSelec = () =>
    sqlMock.mockImplementation(async (req) => {
      if (/^SELEC\b/.test(req.sql)) throw parseError;
      return ok;
    });

  it("marks the line and word of the failing statement, and only in that cell", async () => {
    failOnSelec();
    renderPage();
    typeText("SELECT 1;\nSELEC broken;\nSELECT 3");
    fireEvent.click(screen.getAllByRole("button", { name: "+ SQL cell" }).at(-1) as HTMLElement);
    typeText("SELECT 9", 2);
    fireEvent.click(screen.getByRole("button", { name: /^Run all/ }));
    const alert = await screen.findByRole("alert");
    const cell1 = screen.getByRole("region", { name: "Cell 1" });
    const cell2 = screen.getByRole("region", { name: "Cell 2" });
    expect(cell1.contains(alert)).toBe(true);
    // the failing statement is statement 2, on line 2 of the cell
    expect(cell1.querySelectorAll(".sqled-errband")).toHaveLength(1);
    expect(cell1.querySelectorAll(".sqled-squiggle")).toHaveLength(1);
    expect(cell1.querySelector(".sqled-ln-err")?.textContent).toContain("2");
    expect((cell1.querySelector(".sqled-errband") as HTMLElement).style.top).toBe(`${12 + 20}px`);
    expect((cell1.querySelector(".sqled-squiggle") as HTMLElement).style.width).toBe("5ch"); // "SELEC"
    // the other cell is untouched
    expect(cell2.querySelector("[role=alert]")).toBeNull();
    expect(cell2.querySelector(".sqled-errband")).toBeNull();
    await waitFor(() => expect(cell2).toHaveAttribute("data-status", "success")); // it ran fine; the other cell's error is not its own
    // the server's "Line: 1, Column: 1" is understood, so there is a link to the line
    expect(within(alert).getByRole("button", { name: /Jump to line 2/ })).toBeInTheDocument();
  });

  it("keeps the marker while other parts of the cell change and drops it once the failing statement changes", async () => {
    failOnSelec();
    renderPage();
    const ta = typeText("SELECT 1;\nSELEC broken;\nSELECT 3");
    fireEvent.click(screen.getByRole("button", { name: /^Run all/ }));
    await screen.findByRole("alert");
    expect(document.querySelectorAll(".sqled-errband")).toHaveLength(1);
    fireEvent.change(ta, { target: { value: "SELECT 1;\nSELEC broken;\nSELECT 3 + 4" } });
    expect(document.querySelectorAll(".sqled-errband")).toHaveLength(1);
    fireEvent.change(ta, { target: { value: "SELECT 1;\nSELECT broken;\nSELECT 3 + 4" } });
    expect(document.querySelectorAll(".sqled-errband")).toHaveLength(0);
  });

  it("a successful run shows no marker", async () => {
    sqlMock.mockResolvedValue(ok);
    renderPage();
    typeText("SELECT 1");
    fireEvent.click(screen.getByRole("button", { name: "Run cell 1" }));
    await waitFor(() => expect(screen.getByRole("region", { name: "Cell 1" })).toHaveAttribute("data-status", "success"));
    expect(document.querySelector(".sqled-errband")).toBeNull();
  });
});

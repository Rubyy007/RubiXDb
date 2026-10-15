import { describe, expect, it, vi } from "vitest";
import type { SqlRequestBody, SqlResponseBody } from "../api/types";
import { ApiRequestError } from "../api/types";
import type { CatalogLists } from "./completion";
import { resolveBase, tableKey } from "./completion";
import { Runner } from "./runner";
import { STATEMENT_SNIPPETS, UNSUPPORTED_STATEMENTS } from "./keywords";
import { neededTables, referencedTables, suggest, type ColumnInfo } from "./suggest";

// One database, three schemas, tables spread over them. `users` exists in two schemas on purpose.
const catalog: CatalogLists = {
  databases: [{ id: 1, name: "default" }],
  schemas: [
    { id: 1, databaseId: 1, name: "public" },
    { id: 2, databaseId: 1, name: "analytics" },
    { id: 3, databaseId: 1, name: "app_data" },
  ],
  tables: [
    { id: 1, schemaId: 1, name: "customers" },
    { id: 2, schemaId: 2, name: "events" },
    { id: 3, schemaId: 2, name: "users" },
    { id: 4, schemaId: 3, name: "users" },
    { id: 5, schemaId: 3, name: "orders" },
  ],
};
const COLS: Record<string, ColumnInfo[]> = {
  users: [{ name: "id", type: "integer" }, { name: "email", type: "text" }],
  "app_data.users": [{ name: "id", type: "integer" }, { name: "login", type: "text" }],
  "analytics.events": [{ name: "event_id", type: "bigint" }],
  customers: [{ name: "cid", type: "integer" }],
};

/** `|` marks the caret. */
function at(marked: string, active: string | null = "analytics", manual = false) {
  const caret = marked.indexOf("|");
  const text = marked.replace("|", "");
  return suggest({ text, caret, catalog, columns: (t) => COLS[t], manual, activeSchema: active });
}
const labels = (marked: string, active: string | null = "analytics", manual = false) => at(marked, active, manual)?.items.map((i) => i.label) ?? [];

describe("tables under the active schema", () => {
  it("offers the active schema's tables bare, then other schemas' tables as schema.table, then schemas", () => {
    expect(labels("SELECT * FROM |")).toEqual([
      "events",
      "users",
      "app_data.orders",
      "app_data.users",
      "public.customers",
      "analytics",
      "app_data",
      "public",
    ]);
    // A schema is inserted with its dot, ready for the table name.
    expect(at("SELECT * FROM |")!.items.find((i) => i.label === "analytics")?.insert).toBe("analytics.");
  });

  it("changes with the active schema", () => {
    expect(labels("SELECT * FROM |", "public").slice(0, 4)).toEqual(["customers", "analytics.events", "analytics.users", "app_data.orders"]);
  });

  it("completes a schema.table path by typing the schema name", () => {
    const l = labels("SELECT * FROM pub|");
    expect(l).toEqual(["public.customers", "public"]);
    expect(at("SELECT * FROM pub|")!.items.find((i) => i.label === "public.customers")?.insert).toBe("public.customers");
    expect(at("SELECT * FROM pub|")!.items.find((i) => i.label === "public")?.insert).toBe("public.");
  });

  it("works after JOIN, UPDATE and INSERT INTO too", () => {
    for (const lead of ["SELECT * FROM users u JOIN ", "UPDATE ", "INSERT INTO "]) {
      expect(labels(`${lead}|`)).toContain("events");
      expect(labels(`${lead}|`)).toContain("app_data.orders");
    }
  });

  it("without an active schema keeps bare names and qualifies only duplicates", () => {
    const r = suggest({ text: "SELECT * FROM ", caret: 14, catalog, columns: () => undefined });
    const l = r!.items.map((i) => i.label);
    expect(l).toContain("customers");
    expect(l).toContain("analytics.users");
    expect(l).toContain("app_data.users");
    expect(l).not.toContain("users");
  });
});

describe("USE SCHEMA", () => {
  it("offers SCHEMA after USE, and never DATABASE", () => {
    expect(labels("USE |")).toEqual(["SCHEMA"]);
    expect(labels("use sch|")).toEqual(["SCHEMA"]);
    expect(labels("USE dat|")).toEqual([]);
    expect(labels("USE |", "analytics", true)).not.toContain("DATABASE");
  });

  it("lists the schemas after USE SCHEMA, the active one first, plus the database prefix", () => {
    const r = at("USE SCHEMA |")!;
    expect(r.items.map((i) => i.label)).toEqual(["analytics", "app_data", "public", "default."]);
    expect(r.items[0].detail).toBe("active schema");
    expect(r.auto).toBe(true);
  });

  it("filters and inserts a schema name", () => {
    const r = at("USE SCHEMA ap|")!;
    expect(r.items.map((i) => i.label)).toEqual(["app_data"]);
    expect(r.items[0].insert).toBe("app_data");
  });

  it("completes default.<schema> and default.<schema>.<table>", () => {
    expect(labels("USE SCHEMA default.|")).toEqual(["analytics", "app_data", "public"]);
    expect(labels("SELECT * FROM default.|")).toEqual(["analytics", "app_data", "public"]);
    expect(labels("SELECT * FROM default.analytics.|")).toEqual(["events", "users"]);
    expect(labels("SELECT * FROM app_data.|")).toEqual(["orders", "users"]);
  });

  it("is a statement starter, so it shows up while typing", () => {
    expect(labels("us|")).toEqual(["USE SCHEMA"]);
    expect(STATEMENT_SNIPPETS.map((s) => s.label)).toContain("CREATE SCHEMA");
    expect(STATEMENT_SNIPPETS.map((s) => s.label)).toContain("USE SCHEMA");
  });
});

describe("CREATE / USE DATABASE are never suggested", () => {
  it("is absent from every statement list and keyword list", () => {
    expect(UNSUPPORTED_STATEMENTS).toEqual(["CREATE DATABASE", "USE DATABASE"]);
    expect(STATEMENT_SNIPPETS.map((s) => s.label)).not.toContain("CREATE DATABASE");
    expect(STATEMENT_SNIPPETS.map((s) => s.label)).not.toContain("USE DATABASE");
    expect(labels("create d|")).toEqual([]);
    expect(labels("|", "analytics", true).join(" ")).not.toMatch(/DATABASE/);
    expect(labels("CREATE |", "analytics", true).join(" ")).not.toMatch(/DATABASE/);
  });
});

describe("CREATE SCHEMA / CREATE TABLE", () => {
  it("offers IF NOT EXISTS once an I is typed, and stays quiet while a new name is expected", () => {
    expect(labels("CREATE SCHEMA i|")).toEqual(["IF NOT EXISTS"]);
    expect(labels("CREATE TABLE if|")).toEqual(["IF NOT EXISTS"]);
    expect(labels("CREATE SCHEMA |")).toEqual([]);
    expect(labels("CREATE SCHEMA a|")).toEqual([]);
  });
});

describe("columns of tables outside the active schema", () => {
  it("names a table by schema.table unless it is in the active schema", () => {
    expect(tableKey(catalog, catalog.tables[2], "analytics")).toBe("users");
    expect(tableKey(catalog, catalog.tables[3], "analytics")).toBe("app_data.users");
    expect(tableKey(catalog, catalog.tables[3], null)).toBe("users");
  });

  it("resolves a bare name in the active schema first", () => {
    expect(referencedTables("SELECT * FROM users", 19, catalog, "analytics").map((u) => u.table)).toEqual(["users"]);
    expect(referencedTables("SELECT * FROM users", 19, catalog, "app_data").map((u) => u.table)).toEqual(["users"]);
    expect(referencedTables("SELECT * FROM app_data.users", 28, catalog, "analytics").map((u) => u.table)).toEqual(["app_data.users"]);
    expect(referencedTables("SELECT * FROM default.app_data.users u", 38, catalog, "analytics")).toEqual([{ table: "app_data.users", alias: "u" }]);
  });

  it("asks for the columns of the qualified table and then suggests them", () => {
    const text = "SELECT  FROM app_data.users";
    const caret = 7;
    expect(neededTables(text, caret, catalog, "analytics")).toEqual(["app_data.users"]);
    const r = suggest({ text, caret, catalog, columns: (t) => COLS[t], activeSchema: "analytics", manual: true });
    expect(r!.items.map((i) => i.label)).toContain("login");
    expect(r!.items.map((i) => i.label)).not.toContain("email");
  });

  it("completes columns after a schema-qualified table and an alias", () => {
    expect(labels("SELECT app_data.users.|")).toEqual(["id", "login"]);
    expect(labels("SELECT u.| FROM app_data.users u")).toEqual(["id", "login"]);
    expect(resolveBase(["app_data", "users"], catalog, "analytics")).toEqual({ kind: "columns", table: "app_data.users" });
    expect(resolveBase(["default", "analytics", "events"], catalog, "analytics")).toEqual({ kind: "columns", table: "events" });
  });

  it("lists the columns of the table named in INSERT INTO schema.table (", () => {
    expect(labels("INSERT INTO app_data.users (|")).toEqual(["id", "login"]);
  });
});

// ---- the schema the runner carries --------------------------------------------------------------------------

const useSchemaResponse = (schema: string): SqlResponseBody => ({
  session_id: null,
  result: { kind: "use_schema", database: "default", schema, message: `Now using schema default.${schema}.` },
});
const rowsResponse: SqlResponseBody = { session_id: null, result: { kind: "rows", columns: [], rows: [], row_count: 0 } };

describe("the runner carries the chosen schema", () => {
  it("sends nothing extra until USE SCHEMA, then sends the schema with every statement", async () => {
    const send = vi.fn<(r: SqlRequestBody, s: AbortSignal) => Promise<SqlResponseBody>>();
    const onSchemaChange = vi.fn();
    const runner = new Runner({ send, onSchemaChange });

    send.mockResolvedValueOnce(rowsResponse);
    await runner.runAutocommit({ cellId: "c1", sheetId: "s", sql: "SELECT 1" });
    expect(send.mock.calls[0][0]).toEqual({ sql: "SELECT 1" });
    expect(runner.getState().schema).toBeNull();

    send.mockResolvedValueOnce(useSchemaResponse("analytics"));
    await runner.runAutocommit({ cellId: "c2", sheetId: "s", sql: "USE SCHEMA analytics" });
    expect(runner.getState().schema).toBe("analytics");
    expect(onSchemaChange).toHaveBeenCalledWith("analytics");

    send.mockResolvedValueOnce(rowsResponse);
    await runner.runAutocommit({ cellId: "c3", sheetId: "s", sql: "SELECT * FROM users" });
    expect(send.mock.calls[2][0]).toEqual({ sql: "SELECT * FROM users", schema: "analytics" });
  });

  it("applies a USE SCHEMA in the middle of a cell to the statements after it", async () => {
    const send = vi.fn<(r: SqlRequestBody, s: AbortSignal) => Promise<SqlResponseBody>>();
    const runner = new Runner({ send });
    send.mockResolvedValueOnce(useSchemaResponse("app_data")).mockResolvedValueOnce(rowsResponse);
    await runner.runAutocommit({ cellId: "c", sheetId: "s", sql: "USE SCHEMA app_data; SELECT * FROM users;" });
    expect(send.mock.calls[0][0]).toEqual({ sql: "USE SCHEMA app_data" });
    expect(send.mock.calls[1][0]).toEqual({ sql: "SELECT * FROM users", schema: "app_data" });
  });

  it("starts in a remembered schema and does not change it when USE SCHEMA fails", async () => {
    const send = vi.fn<(r: SqlRequestBody, s: AbortSignal) => Promise<SqlResponseBody>>();
    const runner = new Runner({ send, initialSchema: "analytics" });
    expect(runner.getState().schema).toBe("analytics");
    send.mockRejectedValueOnce(
      new ApiRequestError(404, { error: { code: "NOT_FOUND", message: "the referenced schema was not found", detail: "nope", hint: "Check the spelling" } }),
    );
    const run = await runner.runAutocommit({ cellId: "c", sheetId: "s", sql: "USE SCHEMA nope" });
    expect(run?.status).toBe("error");
    expect(run?.error).toMatchObject({ code: "NOT_FOUND", http: 404, hint: "Check the spelling" });
    expect(runner.getState().schema).toBe("analytics");
  });

  it("can go back to the server's default schema", () => {
    const runner = new Runner({ send: vi.fn(), initialSchema: "analytics" });
    runner.setSchema(null);
    expect(runner.getState().schema).toBeNull();
  });
});

import { describe, expect, it } from "vitest";
import type { CatalogLists } from "./completion";
import { neededTables, referencedTables, suggest, type ColumnInfo } from "./suggest";

const catalog: CatalogLists = {
  databases: [{ id: 1, name: "default" }],
  schemas: [
    { id: 1, databaseId: 1, name: "public" },
    { id: 2, databaseId: 1, name: "sales" },
  ],
  tables: [
    { id: 1, schemaId: 1, name: "employees" },
    { id: 2, schemaId: 1, name: "departments" },
    { id: 3, schemaId: 1, name: "fct_orders" },
    { id: 4, schemaId: 2, name: "invoices" },
  ],
};
const COLS: Record<string, ColumnInfo[]> = {
  employees: [{ name: "id", type: "integer" }, { name: "name", type: "text" }, { name: "department", type: "text" }, { name: "salary", type: "decimal" }],
  departments: [{ name: "dept_name", type: "text" }, { name: "location", type: "text" }, { name: "budget", type: "decimal" }, { name: "id", type: "integer" }],
  fct_orders: [{ name: "order_id", type: "bigint" }, { name: "customer_id", type: "text" }, { name: "status", type: "text" }],
};
const columns = (t: string) => COLS[t];

/** `|` marks the caret. */
function at(marked: string, opts: { manual?: boolean; cols?: (t: string) => ColumnInfo[] | undefined } = {}) {
  const caret = marked.indexOf("|");
  const text = marked.replace("|", "");
  return suggest({ text, caret, catalog, columns: opts.cols ?? columns, manual: opts.manual });
}
const labels = (marked: string, o?: Parameters<typeof at>[1]) => at(marked, o)?.items.map((i) => i.label) ?? [];

describe("statement start", () => {
  it("offers statement starters for what was typed", () => {
    expect(labels("sel|")).toEqual(["SELECT * FROM", "SELECT"]);
    expect(labels("cre|")).toEqual(["CREATE TABLE", "CREATE INDEX", "CREATE UNIQUE INDEX", "CREATE SCHEMA"]);
    expect(labels("al|")).toEqual(["ALTER TABLE"]);
    expect(at("sel|")?.items[0]).toMatchObject({ kind: "snippet", insert: "SELECT * FROM " });
  });
  it("starts again after a semicolon", () => {
    expect(labels("SELECT 1; dr|")).toEqual(["DROP TABLE", "DROP INDEX"]);
  });
  it("is quiet when nothing has been typed, unless asked", () => {
    expect(at("|")).toBeNull();
    expect(labels("|", { manual: true }).length).toBeGreaterThan(5);
  });
});

describe("tables", () => {
  it("lists tables right after FROM, without typing anything, and marks the list as automatic", () => {
    const r = at("SELECT * FROM |")!;
    expect(r.auto).toBe(true);
    // Tables first (alphabetical), then schemas.
    expect(r.items.map((i) => i.label)).toEqual(["departments", "employees", "fct_orders", "invoices", "public", "sales"]);
  });
  it("filters by what was typed and replaces only the typed part", () => {
    const r = at("SELECT * FROM emp|")!;
    expect(r.items.map((i) => i.label)).toEqual(["employees"]);
    expect(r).toMatchObject({ start: 14, end: 17, partial: "emp", auto: false });
  });
  it("works after JOIN, INTO, UPDATE, DROP TABLE and a comma in FROM", () => {
    expect(labels("SELECT * FROM employees e JOIN dep|")).toEqual(["departments"]);
    expect(labels("INSERT INTO emp|")).toEqual(["employees"]);
    expect(labels("UPDATE emp|")).toEqual(["employees"]);
    expect(labels("DROP TABLE fct|")).toEqual(["fct_orders"]);
    expect(labels("DROP TABLE IF EXISTS fct|")).toEqual(["fct_orders"]);
    expect(labels("SELECT * FROM employees, dep|")).toEqual(["departments"]);
    expect(labels("ALTER TABLE emp|")).toEqual(["employees"]);
    expect(labels("CREATE INDEX i ON emp|")).toEqual(["employees"]);
  });
  it("offers schemas too (they continue with a dot) and tables of other schemas", () => {
    expect(at("SELECT * FROM sa|")?.items).toEqual([{ label: "sales", insert: "sales.", kind: "schema", detail: "schema" }]);
    expect(labels("SELECT * FROM inv|")).toEqual(["invoices"]);
  });
  it("offers CTE names declared in the statement", () => {
    const l = labels("WITH recent AS (SELECT 1) SELECT * FROM re|");
    expect(l).toEqual(["recent"]);
    expect(at("WITH recent AS (SELECT 1) SELECT * FROM re|")?.items[0].detail).toBe("CTE");
  });
  it("does not offer tables when a new name is expected", () => {
    expect(at("CREATE TABLE emp|")).toBeNull();
    expect(at("CREATE TABLE |")).toBeNull();
  });
  it("qualifies a table name that exists in two schemas", () => {
    const cat: CatalogLists = { ...catalog, tables: [...catalog.tables, { id: 9, schemaId: 2, name: "employees" }] };
    const r = suggest({ text: "SELECT * FROM emp", caret: 17, catalog: cat, columns })!;
    // Typing the bare name finds both, and each is inserted qualified so the choice is unambiguous.
    expect(r.items.map((i) => i.insert)).toEqual(["public.employees", "sales.employees"]);
  });
});

describe("columns", () => {
  it("offers the columns of the tables the statement uses, wherever the caret is", () => {
    expect(labels("SELECT na| FROM employees")).toEqual(["name"]);
    expect(labels("SELECT * FROM employees WHERE sal|")).toEqual(["salary"]);
    expect(labels("SELECT * FROM employees ORDER BY na|")).toEqual(["name"]);
    expect(labels("SELECT * FROM employees GROUP BY dep|")).toEqual(["department"]);
    expect(labels("UPDATE employees SET sal|")).toEqual(["salary"]);
    expect(labels("SELECT * FROM employees WHERE salary > 10 AND na|")).toEqual(["name"]);
    expect(labels("SELECT * FROM employees WHERE salary > sa|")).toEqual(["salary"]);
  });
  it("shows the table and type next to a column", () => {
    expect(at("SELECT sal| FROM employees")?.items[0]).toMatchObject({ label: "salary", kind: "column", detail: "employees · decimal" });
  });
  it("opens by itself after WHERE / AND / SELECT when the table is known, but not after an operator", () => {
    const r = at("SELECT * FROM employees WHERE |")!;
    expect(r.auto).toBe(true);
    // The table's own column order: id first.
    expect(r.items.map((i) => i.label)).toEqual(["id", "name", "department", "salary"]);
    expect(at("SELECT | FROM employees")).not.toBeNull();
    expect(at("SELECT * FROM employees WHERE salary > |")).toBeNull();
  });
  it("uses the columns of every joined table, and qualifies a name that appears in both", () => {
    const l = labels("SELECT * FROM employees e JOIN departments d ON e.department = d.dept_name WHERE id|");
    expect(l).toEqual(["e.id", "d.id"]);
    // Columns come before functions (LOWER also starts with "lo").
    expect(labels("SELECT lo| FROM employees e JOIN departments d ON 1=1")[0]).toBe("location");
  });
  it("completes alias. and table. with that table's columns", () => {
    expect(labels("SELECT e.| FROM employees e")).toEqual(["id", "name", "department", "salary"]);
    expect(labels("SELECT e.sa| FROM employees AS e")).toEqual(["salary"]);
    expect(labels("SELECT employees.na| FROM employees")).toEqual(["name"]);
    expect(labels("SELECT * FROM public.employees WHERE public.employees.na|")).toEqual(["name"]);
  });
  it("completes schema. with its tables", () => {
    expect(labels("SELECT * FROM sales.|")).toEqual(["invoices"]);
    expect(labels("SELECT * FROM default.|")).toEqual(["public", "sales"]);
  });
  it("INSERT INTO t ( and CREATE INDEX .. ON t ( list the table's columns, minus those already used", () => {
    expect(labels("INSERT INTO employees (|")).toEqual(["id", "name", "department", "salary"]);
    expect(labels("INSERT INTO employees (id, na|")).toEqual(["name"]);
    expect(labels("INSERT INTO employees (id, |")).toEqual(["name", "department", "salary"]);
    expect(labels("CREATE INDEX i ON employees (sa|")).toEqual(["salary"]);
  });
  it("offers functions and the next keywords after two letters", () => {
    expect(labels("SELECT co| FROM employees")).toContain("COUNT");
    expect(at("SELECT co| FROM employees")?.items.find((i) => i.label === "COUNT")).toMatchObject({ insert: "COUNT(", kind: "function" });
    // After IS the likely words are NULL / NOT NULL, not the NULLIF function.
    expect(labels("SELECT * FROM employees WHERE salary IS NU|")).toEqual(["NULL"]);
    expect(labels("SELECT * FROM employees WHERE salary IS NO|")).toEqual(["NOT NULL", "NOT"]);
  });
  it("waits quietly while a table's columns are still loading", () => {
    expect(at("SELECT na| FROM employees", { cols: () => undefined })).toBeNull();
  });
});

describe("keywords inside a statement", () => {
  it("suggests the clause that usually comes next, first", () => {
    expect(labels("SELECT * FROM employees wh|")).toEqual(["WHERE"]);
    expect(labels("SELECT * FROM employees gr|")).toEqual(["GROUP BY"]);
    expect(labels("SELECT * FROM employees or|")).toEqual(["ORDER BY"]);
    expect(labels("SELECT * FROM employees le|")).toEqual(["LEFT JOIN"]);
    expect(labels("SELECT * FROM employees WHERE age = 1 an|")).toEqual(["AND"]);
    expect(labels("SELECT id fr|")).toEqual(["FROM"]);
    expect(labels("SELECT * fr|")).toEqual(["FROM"]);
    expect(at("SELECT * FROM employees wh|")?.items[0]).toMatchObject({ kind: "keyword", insert: "WHERE " });
  });
  it("needs two typed letters", () => {
    expect(at("SELECT * FROM employees w|")).toBeNull();
  });
  it("puts the likely word first when several match", () => {
    expect(labels("SELECT * FROM employees WHERE a = 1 li|")).toEqual(["LIMIT", "LIKE"]);
  });
  it("drops a suggestion that is exactly what was typed, so a finished word never holds a popup open", () => {
    // OR is what was typed; only the longer ORDER BY is still worth offering.
    expect(labels("SELECT * FROM employees WHERE a = 1 or|")).toEqual(["ORDER BY"]);
    expect(at("SELECT * FROM employees WHERE a = 1 and|")).toBeNull();
  });
  it("offers CASE words only inside a CASE, DDL words only in DDL, and INSERT words only in INSERT", () => {
    expect(labels("SELECT * FROM employees wh|")).toEqual(["WHERE"]);
    expect(labels("SELECT CASE WHEN a THEN b el| FROM t")).toEqual(["ELSE"]);
    expect(labels("SELECT CASE WHEN a THEN b END wh| FROM t")).toEqual(["WHERE"]);
    expect(labels("SELECT * FROM employees pr|")).toEqual([]);
    expect(labels("CREATE TABLE t (a integer pr|")).toEqual(["PRIMARY KEY"]);
    expect(labels("INSERT INTO employees va|")).toEqual(["VALUES"]);
    expect(labels("SELECT * FROM employees va|")).toEqual([]);
  });
});

describe("when to stay quiet", () => {
  it("inside strings and comments", () => {
    expect(at("SELECT 'emp|'")).toBeNull();
    expect(at("SELECT 1 -- emp|")).toBeNull();
    expect(at("SELECT /* emp| */ 1")).toBeNull();
  });
  it("in the middle of a word, and in a number", () => {
    expect(at("SELECT * FROM em|ployees")).toBeNull();
    expect(at("SELECT 12ab|")).toBeNull();
  });
  it("when the word is already complete", () => {
    expect(at("SELECT * FROM employees|")).toBeNull();
    expect(at("SELECT * FROM employees WHERE|")).toBeNull();
    expect(at("SELECT * FROM employees WHERE name|")).toBeNull();
  });
  it("for a bad caret", () => {
    expect(suggest({ text: "abc", caret: 9, catalog, columns })).toBeNull();
    expect(suggest({ text: "abc", caret: -1, catalog, columns })).toBeNull();
  });
});

describe("referencedTables and neededTables", () => {
  it("reads FROM lists, joins, aliases, schema-qualified names and only the current statement", () => {
    const text = "SELECT 1 FROM departments; SELECT * FROM public.employees AS e, fct_orders o JOIN departments d ON 1=1 WHERE x";
    expect(referencedTables(text, text.length, catalog)).toEqual([
      { table: "employees", alias: "e" },
      { table: "fct_orders", alias: "o" },
      { table: "departments", alias: "d" },
    ]);
  });
  it("ignores names that are not in the catalog and words that are not aliases", () => {
    expect(referencedTables("SELECT * FROM nowhere WHERE 1", 29, catalog)).toEqual([]);
    expect(referencedTables("SELECT * FROM employees WHERE 1", 31, catalog)).toEqual([{ table: "employees", alias: undefined }]);
    expect(referencedTables("SELECT * FROM employees LEFT JOIN departments ON 1=1", 30, catalog).map((u) => u.alias)).toEqual([undefined, undefined]);
  });
  it("lists the tables whose columns must be loaded, including the table before a dot", () => {
    expect(neededTables("SELECT employees.na FROM departments", 18, catalog).sort()).toEqual(["departments", "employees"]);
    expect(neededTables("SELECT e.na FROM employees e", 11, catalog)).toEqual(["employees"]);
  });
});

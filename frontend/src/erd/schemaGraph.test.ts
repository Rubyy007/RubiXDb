import { describe, expect, it } from "vitest";
import { CARD_W, cardHeight, columnY, groupFrames, inferLinks, layoutTables, linkPath, visibleColumns, MAX_ROWS, type ErdColumn, type ErdTable } from "./schemaGraph";

const col = (name: string, dataType: string, primaryKey = false): ErdColumn => ({ name, dataType, nullable: !primaryKey, primaryKey });
const table = (name: string, columns: ErdColumn[], schema = "public"): ErdTable => ({ key: `${schema}.${name}`, schema, name, columns });

const customers = table("customers", [col("id", "integer", true), col("name", "text")]);
const departments = table("departments", [col("dept_name", "text", true), col("budget", "decimal")]);

const pairs = (ts: ErdTable[]) => inferLinks(ts).map((l) => `${l.from.table}.${l.from.column}->${l.to.table}.${l.to.column}:${l.rule}`);

describe("inferLinks", () => {
  it("rule A: <stem>_id next to a table named after the stem, same type as its single primary key", () => {
    const orders = table("orders", [col("id", "integer", true), col("customer_id", "integer")]);
    expect(pairs([customers, orders])).toEqual(["public.orders.customer_id->public.customers.id:id-suffix"]);
  });

  it("rule A also reads customerid and plural / -ies table names", () => {
    const categories = table("categories", [col("id", "integer", true)]);
    const items = table("items", [col("id", "integer", true), col("categoryid", "integer"), col("customer_id", "integer")]);
    expect(pairs([categories, customers, items]).sort()).toEqual([
      "public.items.categoryid->public.categories.id:id-suffix",
      "public.items.customer_id->public.customers.id:id-suffix",
    ]);
  });

  it("rule B: a column named like the singular of a table (employees.department -> departments)", () => {
    const employees = table("employees", [col("id", "integer", true), col("department", "text")]);
    expect(pairs([employees, departments])).toEqual(["public.employees.department->public.departments.dept_name:table-name"]);
  });

  it("rule C: the same name and type as another table's primary key", () => {
    const people = table("people", [col("pid", "integer", true), col("dept_name", "text")]);
    expect(pairs([people, departments])).toEqual(["public.people.dept_name->public.departments.dept_name:same-key-name"]);
  });

  it("never links when the types differ", () => {
    const orders = table("orders", [col("id", "integer", true), col("customer_id", "text")]);
    expect(pairs([customers, orders])).toEqual([]);
  });

  it("never links to a table whose primary key is composite or missing", () => {
    const composite = table("customers", [col("a", "integer", true), col("b", "integer", true)]);
    const keyless = table("customers", [col("id", "integer")], "other");
    const orders = table("orders", [col("id", "integer", true), col("customer_id", "integer")]);
    expect(pairs([composite, orders])).toEqual([]);
    expect(pairs([keyless, orders])).toEqual([]);
  });

  it("never treats a table's own single primary key, a bare id, or an unrelated name as a reference", () => {
    const a = table("a", [col("id", "integer", true)]);
    const b = table("b", [col("id", "integer", true)]);
    const orders = table("fct_orders", [col("order_id", "bigint", true), col("customer_id", "text"), col("status", "text")]);
    expect(pairs([a, b])).toEqual([]);
    expect(pairs([orders, table("lonely", [col("id", "integer", true)])])).toEqual([]);
  });

  it("only links tables of the same schema", () => {
    const orders = table("orders", [col("id", "integer", true), col("customer_id", "integer")], "sales");
    expect(pairs([customers, orders])).toEqual([]);
  });

  it("keeps the strongest rule once per column pair, with a reason that says it is inferred", () => {
    const emp = table("emp", [col("id", "integer", true), col("customer_id", "integer")]);
    const [l] = inferLinks([customers, emp]);
    expect(l.rule).toBe("id-suffix");
    expect(l.reason).toContain("inferred from names, not enforced by the database");
    expect(inferLinks([customers, emp])).toHaveLength(1);
  });

  it("is empty for no tables and for tables whose columns could not be read", () => {
    expect(inferLinks([])).toEqual([]);
    expect(inferLinks([table("x", []), table("y", [])])).toEqual([]);
  });
});

describe("layoutTables", () => {
  const employees = table("employees", [col("id", "integer", true), col("department", "text")]);
  const solo = table("solo", [col("id", "integer", true)]);
  const ts = [employees, departments, solo];

  it("puts a referencing table left of the table it references, and unrelated tables below", () => {
    const links = inferLinks(ts);
    const box = layoutTables(ts, links);
    expect(box["public.employees"].x).toBeLessThan(box["public.departments"].x);
    expect(box["public.solo"].y).toBeGreaterThan(Math.max(box["public.employees"].y + box["public.employees"].h, box["public.departments"].y + box["public.departments"].h));
  });

  it("is deterministic and never overlaps two cards", () => {
    const many = Array.from({ length: 11 }, (_, i) => table(`t${i}`, [col("id", "integer", true), col("x", "text")]));
    const all = [...ts, ...many];
    const links = inferLinks(all);
    const a = layoutTables(all, links);
    expect(layoutTables(all, links)).toEqual(a);
    const boxes = Object.values(a);
    for (let i = 0; i < boxes.length; i++)
      for (let j = i + 1; j < boxes.length; j++) {
        const p = boxes[i];
        const q = boxes[j];
        const overlap = p.x < q.x + q.w && q.x < p.x + p.w && p.y < q.y + q.h && q.y < p.y + p.h;
        expect(overlap, `${i} overlaps ${j}`).toBe(false);
      }
  });

  it("stacks schemas into separate bands that do not overlap", () => {
    const other = table("orders", [col("id", "integer", true)], "sales");
    const all = [employees, departments, other];
    const box = layoutTables(all, inferLinks(all));
    const pub = [box["public.employees"], box["public.departments"]];
    expect(box["sales.orders"].y).toBeGreaterThan(Math.max(...pub.map((b) => b.y + b.h)));
    const frames = groupFrames(all, box);
    expect(frames.map((f) => f.schema)).toEqual(["public", "sales"]);
    expect(frames[1].y).toBeGreaterThan(frames[0].y + frames[0].h);
  });

  it("survives a reference cycle", () => {
    const x = table("xs", [col("id", "integer", true), col("y_id", "integer")]);
    const y = table("ys", [col("id", "integer", true), col("x_id", "integer")]);
    const box = layoutTables([x, y], inferLinks([x, y]));
    expect(Object.keys(box).sort()).toEqual(["public.xs", "public.ys"]);
  });

  it("handles no tables", () => {
    expect(layoutTables([], [])).toEqual({});
  });
});

describe("card geometry", () => {
  const wide = table("wide", [col("id", "integer", true), ...Array.from({ length: 20 }, (_, i) => col(`c${i}`, "text"))]);

  it("caps the columns drawn but always draws a column that carries a link", () => {
    const ref = table("refs", [col("id", "integer", true), ...Array.from({ length: 20 }, (_, i) => col(`c${i}`, "text")), col("wide_id", "integer")]);
    const links = inferLinks([wide, ref]);
    expect(links.map((l) => l.from.column)).toContain("wide_id");
    const { shown, hidden } = visibleColumns(ref, links);
    expect(shown.length).toBe(MAX_ROWS + 1);
    expect(shown.at(-1)?.name).toBe("wide_id");
    expect(hidden).toBe(ref.columns.length - shown.length);
  });

  it("anchors a link at the middle of its column row, and at the card middle when the column is not drawn", () => {
    const y = columnY(wide, [], 100, "c0");
    expect(y).toBeGreaterThan(100);
    expect(columnY(wide, [], 100, "does_not_exist")).toBe(100 + cardHeight(wide, []) / 2);
  });

  it("an empty card (columns unreadable) still has a height", () => {
    expect(cardHeight(table("e", []), [])).toBeGreaterThan(0);
  });

  it("linkPath leaves the facing sides, and loops out the right for cards in the same column", () => {
    const left = { x: 0, y: 0, w: CARD_W, h: 100 };
    const right = { x: 500, y: 0, w: CARD_W, h: 100 };
    const lr = linkPath(left, 20, right, 40);
    expect(lr.start.x).toBe(CARD_W);
    expect(lr.end.x).toBe(500);
    const rl = linkPath(right, 20, left, 40);
    expect(rl.start.x).toBe(500);
    expect(rl.end.x).toBe(CARD_W);
    const same = linkPath(left, 20, { ...left, y: 200 }, 220);
    expect(same.start.x).toBe(CARD_W);
    expect(same.end.x).toBe(CARD_W);
    expect(same.d).toMatch(/^M [\d.]+ [\d.]+ C /);
  });
});

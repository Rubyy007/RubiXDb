import { describe, expect, it } from "vitest";
import { insideLiteral, typePosition, upperEdit } from "./autoUpper";

/** Types `text` one character at a time like a user, applying every uppercase edit the way the editor does. */
function typeIt(text: string, keepCase: (w: string) => boolean = () => false): string {
  let out = "";
  for (const ch of text) {
    const e = upperEdit(out, out.length, ch, keepCase);
    if (e) out = out.slice(0, e.start) + e.upper + out.slice(e.end);
    out += ch;
  }
  return out;
}

describe("upperEdit: keywords become UPPERCASE when the word is finished", () => {
  it("uppercases the keywords of a typed query and nothing else", () => {
    expect(typeIt("select name, age from employees where age > 30 order by name desc ")).toBe(
      "SELECT name, age FROM employees WHERE age > 30 ORDER BY name DESC ",
    );
  });

  it("works for mixed case and for every kind of separator", () => {
    expect(typeIt("SeLeCt ")).toBe("SELECT ");
    expect(typeIt("select\n")).toBe("SELECT\n");
    expect(typeIt("select*")).toBe("SELECT*");
    expect(typeIt("x in(1) and(y) ")).toBe("x IN(1) AND(y) ");
    expect(typeIt("a;")).toBe("a;");
    expect(typeIt("commit;")).toBe("COMMIT;");
  });

  it("does nothing until the word is finished (no separator typed)", () => {
    expect(upperEdit("select", 6, "")).toBeNull();
    expect(upperEdit("select", 6, "e")).toBeNull();
    expect(upperEdit("select", 6, "1")).toBeNull();
  });

  it("does not uppercase words that are not keywords", () => {
    expect(typeIt("select name, department, salary from employees ")).toBe("SELECT name, department, salary FROM employees ");
    expect(typeIt("select id, status, value, key, total from orders ")).toBe("SELECT id, status, value, key, total FROM orders ");
  });

  it("leaves a word alone when it is the name of a table or column the catalog has", () => {
    const keep = (w: string) => w === "index" || w === "end";
    expect(typeIt("select index, end from t ", keep)).toBe("SELECT index, end FROM t ");
    expect(typeIt("select index from t ")).toBe("SELECT INDEX FROM t ");
  });

  it("never touches strings, quoted identifiers or comments", () => {
    expect(typeIt("select 'select from where' ")).toBe("SELECT 'select from where' ");
    expect(typeIt('select "from" ')).toBe('SELECT "from" ');
    expect(typeIt("-- select the rows\nselect ")).toBe("-- select the rows\nSELECT ");
    expect(typeIt("/* drop table */ select ")).toBe("/* drop table */ SELECT ");
    expect(typeIt("select 'it''s a select ")).toBe("SELECT 'it''s a select ");
  });

  it("never touches a part of a qualified name", () => {
    expect(typeIt("select t.date, t.index from public.table ")).toBe("SELECT t.date, t.index FROM public.table ");
  });

  it("uppercases a function only when it is called", () => {
    expect(typeIt("select count(*), max (age) from t ")).toBe("SELECT COUNT(*), MAX (age) FROM t ");
    expect(typeIt("select count, max from t ")).toBe("SELECT count, max FROM t ");
  });

  it("uppercases a type only where a type is expected", () => {
    expect(typeIt("create table t (id integer primary key, name text not null, born date) ")).toBe(
      "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT NOT NULL, born DATE) ",
    );
    expect(typeIt("select date, text, time from t ")).toBe("SELECT date, text, time FROM t ");
    expect(typeIt("create table t (date date) ")).toBe("CREATE TABLE t (date DATE) ");
    expect(typeIt("create table t (price decimal(10, 2), n integer) ")).toBe("CREATE TABLE t (price DECIMAL(10, 2), n INTEGER) ");
    expect(typeIt("alter table t add column age integer ")).toBe("ALTER TABLE t ADD COLUMN age INTEGER ");
  });

  it("uppercases KEY only after PRIMARY or FOREIGN", () => {
    expect(typeIt("create table t (id integer primary key ")).toBe("CREATE TABLE t (id INTEGER PRIMARY KEY ");
    expect(typeIt("select key from t ")).toBe("SELECT key FROM t ");
  });

  it("ignores words that are already uppercase and anything that is not a word", () => {
    expect(upperEdit("SELECT", 6, " ")).toBeNull();
    expect(upperEdit("select 1", 8, " ")).toBeNull();
    expect(upperEdit("select ", 7, " ")).toBeNull();
    expect(upperEdit("", 0, " ")).toBeNull();
    expect(upperEdit("select", 99, " ")).toBeNull();
    expect(upperEdit("t1select", 8, " ")).toBeNull();
    expect(upperEdit("select", 6, ".")).toBeNull();
  });

  it("returns the exact range so the caret never moves", () => {
    expect(upperEdit("select * from t where", 21, " ")).toEqual({ start: 16, end: 21, upper: "WHERE" });
  });
});

describe("insideLiteral", () => {
  it("knows strings, quoted identifiers and comments, and where they end", () => {
    const t = "a 'x y' b \"p q\" -- c d\ne /* f g */ h";
    expect(insideLiteral(t, t.indexOf("x y") + 1)).toBe(true);
    expect(insideLiteral(t, t.indexOf(" b "))).toBe(false); // right after the closing quote
    expect(insideLiteral(t, t.indexOf("b") + 1)).toBe(false);
    expect(insideLiteral(t, t.indexOf("p q") + 1)).toBe(true);
    expect(insideLiteral(t, t.indexOf("c d") + 1)).toBe(true);
    expect(insideLiteral(t, t.indexOf("e "))).toBe(false); // the next line
    expect(insideLiteral(t, t.indexOf("f g") + 1)).toBe(true);
    expect(insideLiteral(t, t.indexOf(" h") + 1)).toBe(false);
    expect(insideLiteral("select 'open", 10)).toBe(true);
    expect(insideLiteral("select /* open", 12)).toBe(true);
  });
});

describe("typePosition", () => {
  it("is true only in a column definition", () => {
    expect(typePosition("create table t (id ", 19)).toBe(true);
    expect(typePosition("create table t (id int, name ", 29)).toBe(true);
    expect(typePosition("select id ", 10)).toBe(false);
    expect(typePosition("create table t (", 16)).toBe(false);
    expect(typePosition("create table t (a decimal(10, 2", 31)).toBe(false);
  });
});

/** SQL words the IDE knows about: which ones it may uppercase on its own, and what it offers as suggestions.
 *
 * The auto-uppercase lists are deliberately SHORT and strict. A word is only uppercased automatically when
 * it can hardly be a table or column name, so typing `select name from t` never turns `name` into `NAME`
 * (and a table or column the catalog actually has is never touched, whatever it is called). Nothing here
 * affects what is sent to the server: SQL keywords are case-insensitive, this is purely how the text looks. */

/** Words that are always keywords. Uppercased as soon as the word is finished. */
export const RESERVED: ReadonlySet<string> = new Set(
  (
    "SELECT FROM WHERE GROUP BY HAVING ORDER LIMIT OFFSET INSERT INTO VALUES UPDATE SET DELETE CREATE TABLE INDEX " +
    "DROP ALTER ADD COLUMN RENAME TO ON AS AND OR NOT NULL IS IN LIKE BETWEEN JOIN INNER LEFT RIGHT FULL OUTER CROSS " +
    "USING DISTINCT UNIQUE PRIMARY BEGIN COMMIT ROLLBACK EXPLAIN IF EXISTS ASC DESC CASE WHEN THEN ELSE END " +
    "WITH RECURSIVE UNION INTERSECT EXCEPT ALL ANY OVER PARTITION LATERAL QUALIFY TRUE FALSE SCHEMA DATABASE DEFAULT"
  ).split(" "),
);

/** Function names: uppercased only when a `(` follows, because only then is it certainly a call. */
export const FUNCTIONS: readonly string[] = [
  "COUNT", "SUM", "AVG", "MIN", "MAX", "COALESCE", "NULLIF", "CAST", "UPPER", "LOWER", "LENGTH", "TRIM", "SUBSTR", "SUBSTRING",
  "REPLACE", "ABS", "ROUND", "FLOOR", "CEIL", "CONCAT", "NOW", "ROW_NUMBER", "RANK", "DENSE_RANK", "PERCENT_RANK", "CUME_DIST",
  "NTILE", "LAG", "LEAD", "FIRST_VALUE", "LAST_VALUE", "NTH_VALUE", "GROUPING", "EXISTS",
];

/** Column types: uppercased only where a type is expected (see `typePosition` in `autoUpper.ts`). */
export const TYPES: readonly string[] = ["INTEGER", "BIGINT", "SMALLINT", "TEXT", "BOOLEAN", "REAL", "DOUBLE", "DECIMAL", "DATE", "TIME", "TIMESTAMP", "BLOB", "VARCHAR"];

export const FUNCTION_SET: ReadonlySet<string> = new Set(FUNCTIONS);
export const TYPE_SET: ReadonlySet<string> = new Set(TYPES);

/** Words that, when they are the last word before the one being typed, start a name that is NOT a keyword. */
export const CLAUSE_STARTERS: ReadonlySet<string> = new Set(["SELECT", "WHERE", "AND", "OR", "ON", "HAVING", "SET", "BY", "WHEN", "THEN", "ELSE", "CASE", "DISTINCT", "NOT", "BETWEEN", "IN", "LIKE", "IS", "USING", "QUALIFY", "OVER", "PARTITION"]);

/** Suggestions for the start of a statement. `insert` is what is typed; `caret` is where the caret ends up. */
export interface Snippet {
  label: string;
  insert: string;
  detail: string;
}

export const STATEMENT_SNIPPETS: readonly Snippet[] = [
  { label: "SELECT * FROM", insert: "SELECT * FROM ", detail: "read rows" },
  { label: "SELECT", insert: "SELECT ", detail: "read rows" },
  { label: "INSERT INTO", insert: "INSERT INTO ", detail: "add rows" },
  { label: "UPDATE", insert: "UPDATE ", detail: "change rows" },
  { label: "DELETE FROM", insert: "DELETE FROM ", detail: "remove rows" },
  { label: "CREATE TABLE", insert: "CREATE TABLE ", detail: "new table" },
  { label: "CREATE INDEX", insert: "CREATE INDEX ", detail: "new index" },
  { label: "CREATE UNIQUE INDEX", insert: "CREATE UNIQUE INDEX ", detail: "new unique index" },
  { label: "CREATE SCHEMA", insert: "CREATE SCHEMA ", detail: "new schema in default" },
  { label: "USE SCHEMA", insert: "USE SCHEMA ", detail: "switch the working schema" },
  { label: "ALTER TABLE", insert: "ALTER TABLE ", detail: "change a table" },
  { label: "DROP TABLE", insert: "DROP TABLE ", detail: "remove a table" },
  { label: "DROP INDEX", insert: "DROP INDEX ", detail: "remove an index" },
  { label: "WITH", insert: "WITH ", detail: "common table expression" },
  { label: "EXPLAIN", insert: "EXPLAIN ", detail: "show the plan" },
  { label: "BEGIN", insert: "BEGIN", detail: "start a transaction" },
  { label: "COMMIT", insert: "COMMIT", detail: "keep the transaction" },
  { label: "ROLLBACK", insert: "ROLLBACK", detail: "discard the transaction" },
];

/** Statements the server refuses (rubiXDb v1 has one database, `default`). They are never suggested; typing one
 * is answered by the server with a 400 and the way forward (CREATE SCHEMA / USE SCHEMA). */
export const UNSUPPORTED_STATEMENTS: readonly string[] = ["CREATE DATABASE", "USE DATABASE"];

/** Keywords and phrases offered inside a statement, in the order they are usually wanted. */
export const CLAUSE_KEYWORDS: readonly string[] = [
  "FROM", "WHERE", "GROUP BY", "HAVING", "ORDER BY", "LIMIT", "OFFSET", "JOIN", "INNER JOIN", "LEFT JOIN", "RIGHT JOIN", "FULL JOIN",
  "CROSS JOIN", "ON", "USING", "AS", "AND", "OR", "NOT", "IN", "LIKE", "BETWEEN", "IS NULL", "IS NOT NULL", "EXISTS", "DISTINCT",
  "CASE", "WHEN", "THEN", "ELSE", "END", "ASC", "DESC", "UNION", "UNION ALL", "INTERSECT", "EXCEPT", "VALUES", "SET", "INTO",
  "PRIMARY KEY", "NOT NULL", "UNIQUE", "DEFAULT", "IF EXISTS", "IF NOT EXISTS", "OVER", "PARTITION BY", "QUALIFY", "TRUE", "FALSE", "NULL",
  "ADD COLUMN", "RENAME TO",
];

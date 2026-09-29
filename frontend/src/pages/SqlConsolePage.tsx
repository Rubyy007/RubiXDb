import { useCallback, useMemo, useRef, useState, type KeyboardEvent } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useApiClient, invalidateCatalogQueries } from "../api/queries";
import { ApiRequestError } from "../api/types";
import type { SqlResponseBody, SqlValue } from "../api/types";
import { formatSqlValue, isNullValue } from "../utils/sqlValue";
import { Card } from "../components/Card";
import { Button } from "../components/Button";
import { TextArea } from "../components/Field";
import { Badge } from "../components/Badge";
import { EmptyState } from "../components/EmptyState";

const PAGE_SIZE = 200;
const HISTORY_LIMIT = 50;

interface HistoryEntry {
  sql: string;
  ranAt: number;
  ok: boolean;
}

/** SQL console state -- item 22/108: session/transaction state lives
 * here, in ordinary React component state, never silently assumed
 * durable across a page reload (a reload starts a fresh, autocommit-
 * only browser session, exactly like a fresh CLI process would --
 * `PHASE_RELATIONAL_FRONTEND_SQL_ARCHITECTURE.md` §3 states this
 * explicitly as the honest contract, not a limitation to hide). */
export function SqlConsolePage() {
  const client = useApiClient();
  const queryClient = useQueryClient();

  const [sqlText, setSqlText] = useState("");
  const [sessionId, setSessionId] = useState<string | null>(null);
  const [result, setResult] = useState<SqlResponseBody | null>(null);
  const [errorMessage, setErrorMessage] = useState<string | null>(null);
  const [running, setRunning] = useState(false);
  const [history, setHistory] = useState<HistoryEntry[]>([]);
  const [page, setPage] = useState(0);
  const abortRef = useRef<AbortController | null>(null);

  const inTransaction = sessionId !== null;

  const execute = useCallback(async () => {
    if (!client || !sqlText.trim() || running) return;
    const controller = new AbortController();
    abortRef.current = controller;
    setRunning(true);
    setErrorMessage(null);
    setPage(0);
    const sqlToRun = sqlText;
    try {
      const response = await client.sql(
        { sql: sqlToRun, session_id: sessionId },
        controller.signal,
      );
      setResult(response);
      setSessionId(response.session_id);
      setHistory((h) => [{ sql: sqlToRun, ranAt: Date.now(), ok: true }, ...h].slice(0, HISTORY_LIMIT));
      if (response.result.kind === "ddl" || response.result.kind === "write") {
        invalidateCatalogQueries(queryClient);
      }
    } catch (err) {
      if (err instanceof DOMException && err.name === "AbortError") {
        setErrorMessage("Cancelled.");
      } else if (err instanceof ApiRequestError) {
        setErrorMessage(`${err.code}: ${err.message}${err.detail ? ` (${err.detail})` : ""}`);
        if (err.code === "SESSION_NOT_FOUND") {
          // The server no longer knows this session (expired or the
          // transaction already ended some other way) -- forget it
          // client-side too rather than keep retrying a dead id.
          setSessionId(null);
        }
      } else {
        setErrorMessage(err instanceof Error ? err.message : "Request failed");
      }
      setHistory((h) => [{ sql: sqlToRun, ranAt: Date.now(), ok: false }, ...h].slice(0, HISTORY_LIMIT));
    } finally {
      setRunning(false);
      abortRef.current = null;
    }
  }, [client, sqlText, sessionId, running, queryClient]);

  function cancel() {
    // item 48: a real HTTP-level abort -- the server observes the
    // dropped connection and cancels the running statement via its own
    // `CancellationToken`, never merely a UI-side "hide the spinner."
    abortRef.current?.abort();
  }

  function clear() {
    setSqlText("");
    setResult(null);
    setErrorMessage(null);
  }

  function onEditorKeyDown(e: KeyboardEvent<HTMLTextAreaElement>) {
    if ((e.ctrlKey || e.metaKey) && e.key === "Enter") {
      e.preventDefault();
      void execute();
    }
  }

  return (
    <div className="stack">
      <div className="spread">
        <h1 style={{ fontSize: "var(--font-size-2xl)" }}>SQL Console</h1>
        {inTransaction ? (
          <Badge tone="pressure">transaction open</Badge>
        ) : (
          <Badge tone="neutral">autocommit</Badge>
        )}
      </div>

      <Card title="Editor">
        <div className="stack">
          <TextArea
            label="SQL"
            value={sqlText}
            onChange={(e) => setSqlText(e.target.value)}
            onKeyDown={onEditorKeyDown}
            mono
            rows={8}
            placeholder="SELECT * FROM t WHERE id = 1"
            hint="Ctrl+Enter (Cmd+Enter on Mac) runs the statement. Use BEGIN/COMMIT/ROLLBACK for an explicit transaction spanning multiple statements."
          />
          <div className="row">
            <Button variant="primary" onClick={() => void execute()} disabled={!sqlText.trim() || running}>
              {running ? "Running…" : "Execute"}
            </Button>
            <Button variant="ghost" onClick={cancel} disabled={!running}>
              Cancel
            </Button>
            <Button variant="ghost" onClick={clear} disabled={running}>
              Clear
            </Button>
            {inTransaction && (
              <span className="text-muted" style={{ fontSize: "var(--font-size-sm)" }}>
                session: <span className="text-mono">{sessionId}</span>
              </span>
            )}
          </div>
        </div>
      </Card>

      {errorMessage && (
        <Card title="Error">
          <p className="field-error" role="alert">
            {errorMessage}
          </p>
        </Card>
      )}

      {result && !errorMessage && <ResultView result={result} page={page} onPageChange={setPage} />}

      {history.length > 0 && (
        <Card title="History">
          <ul className="stack" style={{ listStyle: "none", padding: 0, margin: 0 }}>
            {history.map((h, i) => (
              <li key={i}>
                <button
                  type="button"
                  className="btn btn-ghost"
                  style={{ textAlign: "left", width: "100%", justifyContent: "flex-start" }}
                  onClick={() => setSqlText(h.sql)}
                  title="Click to load into the editor"
                >
                  <span aria-hidden="true">{h.ok ? "✓" : "✗"}</span>{" "}
                  <span className="text-mono">{h.sql.length > 120 ? `${h.sql.slice(0, 120)}…` : h.sql}</span>
                </button>
              </li>
            ))}
          </ul>
        </Card>
      )}
    </div>
  );
}

function ResultView({
  result,
  page,
  onPageChange,
}: {
  result: SqlResponseBody;
  page: number;
  onPageChange: (p: number) => void;
}) {
  const { result: r } = result;
  switch (r.kind) {
    case "rows":
      return <RowsView columns={r.columns} rows={r.rows} rowCount={r.row_count} page={page} onPageChange={onPageChange} />;
    case "write":
      return (
        <Card title="Result">
          <p>
            <strong>{r.statement}</strong> — {r.rows_affected} row{r.rows_affected === 1 ? "" : "s"} affected.
          </p>
        </Card>
      );
    case "ddl":
      return (
        <Card title="Result">
          <p>OK.</p>
        </Card>
      );
    case "explain":
      return (
        <Card title="Plan">
          <pre className="text-mono" style={{ whiteSpace: "pre-wrap" }}>
            {r.plan_text}
          </pre>
        </Card>
      );
    case "begin":
      return (
        <Card title="Result">
          <p>Transaction started.</p>
        </Card>
      );
    case "commit":
      return (
        <Card title="Result">
          <p>Transaction committed.</p>
        </Card>
      );
    case "rollback":
      return (
        <Card title="Result">
          <p>Transaction rolled back.</p>
        </Card>
      );
    default:
      return null;
  }
}

function RowsView({
  columns,
  rows,
  rowCount,
  page,
  onPageChange,
}: {
  columns: { name: string; type: string | null; nullable: boolean }[];
  rows: SqlValue[][];
  rowCount: number;
  page: number;
  onPageChange: (p: number) => void;
}) {
  // item 44: bounded rendering -- client-side pagination rather than
  // rendering every row's DOM node at once, without pulling in a new
  // virtualization dependency for this console's v1 (a documented,
  // deliberate scope choice, `PHASE_RELATIONAL_FRONTEND_SQL_
  // ARCHITECTURE.md` §4).
  const totalPages = Math.max(1, Math.ceil(rows.length / PAGE_SIZE));
  const clampedPage = Math.min(page, totalPages - 1);
  const pageRows = useMemo(
    () => rows.slice(clampedPage * PAGE_SIZE, clampedPage * PAGE_SIZE + PAGE_SIZE),
    [rows, clampedPage],
  );

  if (rowCount === 0) {
    return (
      <Card title="Result (0 rows)">
        <EmptyState title="No rows" />
      </Card>
    );
  }

  return (
    <Card title={`Result (${rowCount} row${rowCount === 1 ? "" : "s"})`}>
      <div className="table-wrap">
        <table className="table">
          <caption className="visually-hidden">Query result</caption>
          <thead>
            <tr>
              {columns.map((c) => (
                <th key={c.name} scope="col">
                  {c.name}
                  {c.type && <span className="text-muted"> ({c.type})</span>}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {pageRows.map((row, i) => (
              <tr key={clampedPage * PAGE_SIZE + i} tabIndex={0}>
                {row.map((cell, j) => (
                  <td key={j} className="text-mono">
                    {/* React text nodes only -- never dangerouslySetInnerHTML.
                        Adversarial SQL result content (HTML/script tags/URLs)
                        renders as inert text, never executable markup
                        (item 93/94/126). */}
                    {isNullValue(cell) ? (
                      <span className="text-muted" style={{ fontStyle: "italic" }}>
                        NULL
                      </span>
                    ) : (
                      formatSqlValue(cell)
                    )}
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {totalPages > 1 && (
        <div className="row spread" style={{ marginTop: "var(--space-3)" }}>
          <Button variant="ghost" onClick={() => onPageChange(clampedPage - 1)} disabled={clampedPage === 0}>
            Previous
          </Button>
          <span className="text-muted">
            Page {clampedPage + 1} of {totalPages}
          </span>
          <Button
            variant="ghost"
            onClick={() => onPageChange(clampedPage + 1)}
            disabled={clampedPage >= totalPages - 1}
          >
            Next
          </Button>
        </div>
      )}
    </Card>
  );
}

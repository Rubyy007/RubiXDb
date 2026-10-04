import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useApiClient, invalidateCatalogQueries, useDatabasesQuery, useSchemasQuery } from "../api/queries";
import { useSession } from "../context/SessionContext";
import { ApiRequestError } from "../api/types";
import type { SqlResponseBody } from "../api/types";
import { Badge } from "../components/Badge";
import { Button } from "../components/Button";
import { Icon } from "../components/Icon";
import { Tabs } from "../components/Tabs";
import { SqlEditor } from "../components/console/SqlEditor";
import { WorksheetTabs } from "../components/console/WorksheetTabs";
import { ResultsGrid } from "../components/console/ResultsGrid";
import { OverflowMenu } from "../components/console/OverflowMenu";
import { queryTitle, recordQuery, takePendingSql, useRecentQueries } from "../utils/sessionActivity";
import {
  MAX_TABS,
  loadWorksheets,
  newWorksheet,
  saveWorksheets,
  type Worksheet,
  type WorksheetState,
} from "../utils/worksheets";
import { toCsv } from "../utils/csv";
import "../styles/console.css";

const DEFAULT_PAGE_SIZE = 200;
const DEFAULT_EDITOR_HEIGHT = 200;
const SAVE_DELAY_MS = 400;

interface RunInfo {
  status: "succeeded" | "failed";
  sql: string;
  ms: number;
  kind: string;
  /** Rows returned (SELECT) or affected (writes); null when the statement has no count. */
  count: number | null;
  countLabel: "Rows returned" | "Rows affected";
}

interface TabRuntime {
  result: SqlResponseBody | null;
  error: string | null;
  running: boolean;
  run: RunInfo | null;
  page: number;
}

const EMPTY_RUNTIME: TabRuntime = { result: null, error: null, running: false, run: null, page: 0 };

type ResultView = "results" | "details" | "history";
const RESULT_TABS = [
  { id: "results", label: "Results" },
  { id: "details", label: "Query details" },
  { id: "history", label: "History" },
];

function clock(ms: number): string {
  return new Date(ms).toLocaleTimeString();
}

/** SQL console -- worksheet tabs, editor, results. Session/transaction state lives here, in ordinary
 * React state, never silently assumed durable across a page reload (a reload starts a fresh,
 * autocommit-only browser session, exactly like a fresh CLI process would --
 * `PHASE_RELATIONAL_FRONTEND_SQL_ARCHITECTURE.md` §3). Worksheet TEXT (not results, not
 * credentials) is kept in sessionStorage so a reload restores the open tabs. */
export function SqlConsolePage() {
  const client = useApiClient();
  const queryClient = useQueryClient();
  const { session } = useSession();
  const databases = useDatabasesQuery();
  const schemas = useSchemasQuery();
  const recent = useRecentQueries();

  const [ws, setWs] = useState<WorksheetState>(() => {
    const loaded = loadWorksheets();
    // A template chosen on Home is dropped into the active worksheet if it is empty,
    // otherwise into a new one.
    const pending = takePendingSql();
    if (!pending) return loaded;
    const active = loaded.tabs.find((t) => t.id === loaded.activeId);
    if (active && active.sql.trim() === "") {
      return { ...loaded, tabs: loaded.tabs.map((t) => (t.id === active.id ? { ...t, sql: pending } : t)) };
    }
    if (loaded.tabs.length >= MAX_TABS) return loaded;
    const counter = loaded.counter + 1;
    const tab = newWorksheet(counter, pending);
    return { tabs: [...loaded.tabs, tab], activeId: tab.id, counter };
  });
  const [runtime, setRuntime] = useState<Record<string, TabRuntime>>({});
  const [sessionId, setSessionId] = useState<string | null>(null);
  const [runningTab, setRunningTab] = useState<string | null>(null);
  const [view, setView] = useState<ResultView>("results");
  const [pageSize, setPageSize] = useState<number>(DEFAULT_PAGE_SIZE);
  const [editorHeight, setEditorHeight] = useState(DEFAULT_EDITOR_HEIGHT);
  const [savedAt, setSavedAt] = useState<number | null>(null);
  const [lastRunMs, setLastRunMs] = useState<number | null>(null);
  const abortRef = useRef<AbortController | null>(null);
  const firstSave = useRef(true);

  const active: Worksheet = ws.tabs.find((t) => t.id === ws.activeId) ?? ws.tabs[0];
  const rt = runtime[active.id] ?? EMPTY_RUNTIME;
  const inTransaction = sessionId !== null;
  const anyRunning = runningTab !== null;

  // Autosave the worksheet text (debounced). Skips the initial mount.
  useEffect(() => {
    if (firstSave.current) {
      firstSave.current = false;
      return;
    }
    const timer = window.setTimeout(() => {
      if (saveWorksheets(ws)) setSavedAt(Date.now());
    }, SAVE_DELAY_MS);
    return () => window.clearTimeout(timer);
  }, [ws]);

  const patch = useCallback((id: string, p: Partial<TabRuntime>) => {
    setRuntime((r) => ({ ...r, [id]: { ...(r[id] ?? EMPTY_RUNTIME), ...p } }));
  }, []);

  const setSql = useCallback((sql: string) => {
    setWs((s) => ({ ...s, tabs: s.tabs.map((t) => (t.id === s.activeId ? { ...t, sql } : t)) }));
  }, []);

  const execute = useCallback(async () => {
    if (!client || !active.sql.trim() || runningTab !== null) return;
    const tabId = active.id;
    const sqlToRun = active.sql;
    const controller = new AbortController();
    abortRef.current = controller;
    setRunningTab(tabId);
    patch(tabId, { running: true, error: null, page: 0 });
    setView("results");
    const t0 = performance.now();
    try {
      const response = await client.sql({ sql: sqlToRun, session_id: sessionId }, controller.signal);
      const ms = performance.now() - t0;
      const r = response.result;
      const count = r.kind === "rows" ? r.row_count : r.kind === "write" ? r.rows_affected : null;
      setSessionId(response.session_id);
      setLastRunMs(ms);
      patch(tabId, {
        result: response,
        running: false,
        run: {
          status: "succeeded",
          sql: sqlToRun,
          ms,
          kind: r.kind,
          count,
          countLabel: r.kind === "write" ? "Rows affected" : "Rows returned",
        },
      });
      recordQuery(sqlToRun, true, { ms, rows: count });
      if (r.kind === "ddl" || r.kind === "write") invalidateCatalogQueries(queryClient);
    } catch (err) {
      const ms = performance.now() - t0;
      let message: string;
      if (err instanceof DOMException && err.name === "AbortError") {
        message = "Cancelled.";
      } else if (err instanceof ApiRequestError) {
        // Only the API's own sanitized code/message/detail -- never the raw error object.
        message = `${err.code}: ${err.message}${err.detail ? ` (${err.detail})` : ""}`;
        if (err.code === "SESSION_NOT_FOUND") {
          // The server no longer knows this session (expired or the transaction already ended
          // some other way) -- forget it client-side too rather than keep retrying a dead id.
          setSessionId(null);
        }
      } else {
        message = err instanceof Error ? err.message : "Request failed";
      }
      setLastRunMs(ms);
      patch(tabId, {
        error: message,
        running: false,
        run: { status: "failed", sql: sqlToRun, ms, kind: "error", count: null, countLabel: "Rows returned" },
      });
      recordQuery(sqlToRun, false, { ms });
    } finally {
      setRunningTab(null);
      abortRef.current = null;
    }
  }, [client, active.id, active.sql, runningTab, sessionId, queryClient, patch]);

  function stop() {
    // A real HTTP-level abort -- the server observes the dropped connection and cancels the
    // running statement via its own `CancellationToken`, never merely a UI-side "hide the spinner."
    abortRef.current?.abort();
  }

  function clear() {
    setSql("");
    patch(active.id, { result: null, error: null, run: null, page: 0 });
  }

  // --- worksheet tabs ---
  function addTab() {
    setWs((s) => {
      if (s.tabs.length >= MAX_TABS) return s;
      const counter = s.counter + 1;
      const tab = newWorksheet(counter);
      return { tabs: [...s.tabs, tab], activeId: tab.id, counter };
    });
  }
  function closeTab(id: string) {
    setWs((s) => {
      const idx = s.tabs.findIndex((t) => t.id === id);
      if (idx < 0) return s;
      if (s.tabs.length === 1) {
        const counter = s.counter + 1;
        const tab = newWorksheet(counter);
        return { tabs: [tab], activeId: tab.id, counter };
      }
      const tabs = s.tabs.filter((t) => t.id !== id);
      const activeId = s.activeId === id ? tabs[Math.min(idx, tabs.length - 1)].id : s.activeId;
      return { ...s, tabs, activeId };
    });
    setRuntime((r) => Object.fromEntries(Object.entries(r).filter(([k]) => k !== id)));
  }

  const dirty = useMemo(() => {
    const d = new Set<string>();
    for (const t of ws.tabs) {
      const run = runtime[t.id]?.run;
      if (t.sql.trim() !== "" && (!run || run.sql !== t.sql)) d.add(t.id);
    }
    return d;
  }, [ws.tabs, runtime]);

  // --- overflow menu: only actions that really work ---
  const rowsResult = rt.result && rt.result.result.kind === "rows" ? rt.result.result : null;

  async function copyQuery() {
    try {
      await navigator.clipboard.writeText(active.sql);
    } catch {
      /* clipboard unavailable or denied: nothing to do */
    }
  }

  function downloadResults() {
    if (!rowsResult) return;
    const blob = new Blob([toCsv(rowsResult.columns, rowsResult.rows)], { type: "text/csv;charset=utf-8" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = `rubixdb-results-${Date.now()}.csv`;
    document.body.appendChild(a);
    a.click();
    a.remove();
    window.setTimeout(() => URL.revokeObjectURL(url), 1000);
  }

  // --- derived display values ---
  const dbName = databases.isError ? "—" : (databases.data?.[0]?.name ?? "…");
  const schemaName = schemas.isError
    ? "—"
    : schemas.data
      ? (schemas.data.find((s) => s.name === "public")?.name ?? "—")
      : "…";

  const status: "ready" | "running" | "failed" = rt.running ? "running" : rt.run?.status === "failed" ? "failed" : "ready";
  const statusText = { ready: "Ready", running: "Running…", failed: "Failed" }[status];

  return (
    <div className="sqlc">
      <h1 className="visually-hidden">SQL Console</h1>

      <WorksheetTabs
        tabs={ws.tabs}
        activeId={ws.activeId}
        dirty={dirty}
        canAdd={ws.tabs.length < MAX_TABS}
        onSelect={(id) => setWs((s) => ({ ...s, activeId: id }))}
        onClose={closeTab}
        onAdd={addTab}
      />

      <div className="sqlc-toolbar">
        <div className="sqlc-chips" aria-label="Session context" role="group">
          <span className="chip">
            <span className="chip-k">Database</span>{" "}
            <span className="chip-v">{dbName}</span>
          </span>
          <span className="chip">
            <span className="chip-k">Schema</span>{" "}
            <span className="chip-v">{schemaName}</span>
          </span>
          <span className="chip">
            <span className="chip-k">Role</span>{" "}
            <span className="chip-v">{session?.role ?? "—"}</span>
          </span>
        </div>
        <div className="sqlc-actions">
          <Button variant="primary" onClick={() => void execute()} disabled={!active.sql.trim() || anyRunning}>
            <Icon name="play" />
            Run
          </Button>
          {anyRunning && (
            <Button variant="secondary" onClick={stop}>
              <Icon name="stop" />
              Stop
            </Button>
          )}
        </div>
        <div className="sqlc-right">
          <Button variant="ghost" onClick={clear} disabled={anyRunning}>
            Clear
          </Button>
          <OverflowMenu
            items={[
              { id: "copy", label: "Copy query", disabled: !active.sql, onSelect: () => void copyQuery() },
              { id: "download", label: "Download results (CSV)", disabled: !rowsResult, onSelect: downloadResults },
              { id: "close", label: "Close worksheet", onSelect: () => closeTab(active.id) },
            ]}
          />
        </div>
      </div>

      <div className="sqlc-panel" role="tabpanel" aria-labelledby={`tab-${active.id}`}>
        <SqlEditor
          value={active.sql}
          onChange={setSql}
          onRun={() => void execute()}
          height={editorHeight}
          onHeightChange={setEditorHeight}
          footerLeft={
            inTransaction ? <Badge tone="pressure">transaction open</Badge> : <Badge tone="neutral">autocommit</Badge>
          }
        />
      </div>

      <div className="sqlc-results">
        <Tabs tabs={RESULT_TABS} active={view} onChange={(id) => setView(id as ResultView)} label="Result views" />
        {rt.running && <div className="sqlc-progress" aria-hidden="true" />}
        <div className="sqlc-results-body" role="tabpanel" aria-label="Result view">
          {view === "results" && <ResultsView rt={rt} pageSize={pageSize} onPageSize={(n) => { setPageSize(n); patch(active.id, { page: 0 }); }} onPage={(p) => patch(active.id, { page: p })} />}
          {view === "details" && <DetailsView rt={rt} />}
          {view === "history" && <HistoryView entries={recent} onLoad={setSql} />}
        </div>
      </div>

      <div className="sqlc-status" role="status">
        <span className="sqlc-status-left">
          <span className={`dot dot-${status}`} aria-hidden="true" />
          {statusText}
        </span>
        <span className="sqlc-status-right">
          {lastRunMs !== null && <span>Last run {formatMs(lastRunMs)}</span>}
          <span>{savedAt !== null ? `Saved ${clock(savedAt)}` : "Saved —"}</span>
        </span>
      </div>
    </div>
  );
}

function formatMs(ms: number): string {
  return ms < 1000 ? `${Math.max(1, Math.round(ms))} ms` : `${(ms / 1000).toFixed(2)} s`;
}

function ResultsView({
  rt,
  pageSize,
  onPageSize,
  onPage,
}: {
  rt: TabRuntime;
  pageSize: number;
  onPageSize: (n: number) => void;
  onPage: (p: number) => void;
}) {
  if (rt.error) {
    return (
      <div className="sqlc-message">
        <p className="field-error" role="alert">
          {rt.error}
        </p>
      </div>
    );
  }
  if (rt.running) {
    return (
      <div className="sqlc-message">
        <p className="text-muted">Running query…</p>
      </div>
    );
  }
  if (!rt.result) {
    return (
      <div className="sqlc-message">
        <p className="text-muted">Run a query to see results here.</p>
      </div>
    );
  }
  const r = rt.result.result;
  switch (r.kind) {
    case "rows":
      return (
        <ResultsGrid
          columns={r.columns}
          rows={r.rows}
          rowCount={r.row_count}
          page={rt.page}
          pageSize={pageSize}
          onPageChange={onPage}
          onPageSizeChange={onPageSize}
        />
      );
    case "write":
      return (
        <div className="sqlc-message">
          <p>
            <strong>{r.statement}</strong> — {r.rows_affected} row{r.rows_affected === 1 ? "" : "s"} affected.
          </p>
        </div>
      );
    case "ddl":
      return (
        <div className="sqlc-message">
          <p>OK.</p>
        </div>
      );
    case "explain":
      return (
        <div className="sqlc-message">
          <pre className="text-mono sqlc-plan">{r.plan_text}</pre>
        </div>
      );
    case "begin":
      return (
        <div className="sqlc-message">
          <p>Transaction started.</p>
        </div>
      );
    case "commit":
      return (
        <div className="sqlc-message">
          <p>Transaction committed.</p>
        </div>
      );
    case "rollback":
      return (
        <div className="sqlc-message">
          <p>Transaction rolled back.</p>
        </div>
      );
    default:
      return null;
  }
}

function DetailsView({ rt }: { rt: TabRuntime }) {
  const run = rt.run;
  if (rt.running) {
    return (
      <dl className="details">
        <dt>Status</dt>
        <dd>Running</dd>
      </dl>
    );
  }
  if (!run) {
    return (
      <div className="sqlc-message">
        <p className="text-muted">No query has run in this worksheet yet.</p>
      </div>
    );
  }
  // Only facts the console really has. The API returns no server-side timing or bytes scanned, so the
  // time shown is the browser-measured request round trip and is labelled as such.
  return (
    <dl className="details">
      <dt>Status</dt>
      <dd>{run.status === "succeeded" ? "Succeeded" : "Failed"}</dd>
      {run.status === "succeeded" && run.count !== null && (
        <>
          <dt>{run.countLabel}</dt>
          <dd>{run.count.toLocaleString()}</dd>
        </>
      )}
      <dt>Round-trip time</dt>
      <dd>{formatMs(run.ms)}</dd>
      <dt>Result kind</dt>
      <dd>{run.kind}</dd>
      <dt>Statement</dt>
      <dd>
        <pre className="text-mono details-sql">{run.sql}</pre>
      </dd>
    </dl>
  );
}

function HistoryView({
  entries,
  onLoad,
}: {
  entries: ReturnType<typeof useRecentQueries>;
  onLoad: (sql: string) => void;
}) {
  if (entries.length === 0) {
    return (
      <div className="sqlc-message">
        <p className="text-muted">No query history yet</p>
      </div>
    );
  }
  return (
    <ul className="history">
      {entries.map((h, i) => (
        <li key={`${h.ranAt}-${i}`}>
          <button type="button" className="history-item" onClick={() => onLoad(h.sql)} title="Load into the editor">
            <span className="history-time">{clock(h.ranAt)}</span>
            <span className="history-sql text-mono">{queryTitle(h.sql, 120)}</span>
            <span className={h.ok ? "history-ok" : "history-fail"}>{h.ok ? "Succeeded" : "Failed"}</span>
          </button>
        </li>
      ))}
    </ul>
  );
}

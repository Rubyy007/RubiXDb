import { useEffect, useMemo, useState } from "react";
import type { SqlColumnMeta } from "../api/types";
import { ResultsGrid } from "../components/console/ResultsGrid";
import { toCsv } from "../utils/csv";
import { queryTitle } from "../utils/sessionActivity";
import { BarChart } from "./BarChart";
import { formatMs } from "./format";
import { chartOf, guessTable, profileRows, scalarOf, toInserts, toJson } from "./exports";
import { useIde } from "./ideContext";
import type { IdeCell } from "./model";
import { PlanTree } from "./PlanTree";
import type { CellRun } from "./runner";

type Tab = "results" | "details" | "plan" | "logs";

/** Chip label for a statement: its first line that is not a comment (the full text stays in the tooltip). */
function stmtLabel(text: string, max: number): string {
  const line = text.split(/\r?\n/).find((l) => l.trim() !== "" && !l.trim().startsWith("--"));
  return queryTitle(line ?? text, max);
}

async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    return false;
  }
}

function download(name: string, text: string, type: string) {
  const url = URL.createObjectURL(new Blob([text], { type }));
  const a = document.createElement("a");
  a.href = url;
  a.download = name;
  document.body.appendChild(a);
  a.click();
  a.remove();
  window.setTimeout(() => URL.revokeObjectURL(url), 1000);
}

/** Result tabs under a finished cell: Results, Query details, Plan, Logs. Only what the API really
 * returned is shown; the one time value is the browser-measured round trip and is labelled so. */
export function OutputPanel({ cell, run }: { cell: IdeCell; run: CellRun }) {
  const ide = useIde();
  const [tab, setTab] = useState<Tab>("results");
  const [pageSize, setPageSize] = useState(100);
  const [page, setPage] = useState(0);
  const [showChart, setShowChart] = useState(false);
  const [showProfile, setShowProfile] = useState(false);
  const [note, setNote] = useState("");

  // A cell with several statements keeps one outcome per statement; show the chosen one (the last by default).
  const parts = run.parts ?? [];
  const [sel, setSel] = useState<number | null>(null);
  const idx = sel !== null && sel < parts.length ? sel : parts.length - 1;
  const part = idx >= 0 ? parts[idx] : undefined;
  const ms = part ? part.ms : run.ms;
  const result = part?.response?.result ?? run.response?.result;
  const rows = result && result.kind === "rows" ? result : null;
  const plan = ide.plans[cell.id];

  useEffect(() => {
    if (tab === "plan" && !plan) ide.requestPlan(cell.id);
    // requestPlan is stable enough for this one-shot trigger; re-running on every render would loop.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tab, plan, cell.id]);

  const chart = useMemo(() => (rows ? chartOf(rows.columns, rows.rows) : null), [rows]);
  const profile = useMemo(() => (rows && showProfile ? profileRows(rows.columns, rows.rows) : null), [rows, showProfile]);

  if (!result) return null;

  const summary =
    result.kind === "rows"
      ? `${result.row_count.toLocaleString()} row${result.row_count === 1 ? "" : "s"}`
      : result.kind === "write"
        ? `${result.statement} · ${result.rows_affected} affected`
        : result.kind;
  const label = `Results (${summary}${ms !== undefined ? ` · ${formatMs(ms)}` : ""})`;
  const tabs: { id: Tab; label: string }[] = [
    { id: "results", label },
    { id: "details", label: "Query details" },
    { id: "plan", label: "Plan" },
    { id: "logs", label: "Logs" },
  ];

  async function copy(kind: "json" | "insert") {
    if (!rows) return;
    if (kind === "json") {
      setNote((await copyText(toJson(rows.columns, rows.rows))) ? "Copied as JSON." : "Copy failed: the browser denied clipboard access.");
      return;
    }
    const ins = toInserts(guessTable(cell.text), rows.columns, rows.rows);
    const ok = await copyText(ins.text);
    setNote(
      ok
        ? `Copied ${rows.rows.length} INSERT statement${rows.rows.length === 1 ? "" : "s"}${ins.skipped ? `; ${ins.skipped} blob value(s) written as NULL` : ""}.`
        : "Copy failed: the browser denied clipboard access.",
    );
  }

  return (
    <div className="ide-out">
      {parts.length > 1 && (
        <div className="ide-stmts" role="group" aria-label="Statements in this cell">
          {parts.map((pt, i) => (
            <button
              key={pt.index}
              type="button"
              className="ide-stmt"
              aria-pressed={i === idx}
              data-status={pt.status}
              title={pt.text}
              onClick={() => setSel(i)}
            >
              <span className="ide-stmt-n">{pt.index + 1}</span>
              <span className="ide-stmt-sql text-mono">{stmtLabel(pt.text, 36)}</span>
              <span className="ide-stmt-ms text-mono">{formatMs(pt.ms)}</span>
            </button>
          ))}
        </div>
      )}
      <div className="ide-out-tabs" role="tablist" aria-label={`Cell ${ide.flat.find((f) => f.cell.id === cell.id)?.num ?? ""} output`}>
        {tabs.map((t) => (
          <button
            key={t.id}
            type="button"
            role="tab"
            aria-selected={tab === t.id}
            className="ide-out-tab"
            onClick={() => setTab(t.id)}
          >
            {t.label}
          </button>
        ))}
      </div>

      {tab === "results" && (
        <div className="ide-out-body">
          <ResultBody result={result} ms={ms} page={page} pageSize={pageSize} setPage={setPage} setPageSize={(n) => { setPageSize(n); setPage(0); }} />
          {rows && (
            <>
              <div className="ide-out-actions">
                <button type="button" className="btn btn-secondary" onClick={() => download(`rubixdb-results-${Date.now()}.csv`, toCsv(rows.columns, rows.rows), "text/csv;charset=utf-8")}>
                  Download CSV
                </button>
                <button type="button" className="btn btn-secondary" onClick={() => void copy("json")}>
                  Copy as JSON
                </button>
                <button type="button" className="btn btn-secondary" onClick={() => void copy("insert")}>
                  Copy as INSERT
                </button>
                <button type="button" className="btn btn-secondary" aria-pressed={showChart} disabled={!chart} title={chart ? undefined : "No numeric column to plot"} onClick={() => setShowChart((v) => !v)}>
                  Chart
                </button>
                <button type="button" className="btn btn-secondary" aria-pressed={showProfile} onClick={() => setShowProfile((v) => !v)}>
                  Profile
                </button>
                <span className="ide-out-note" role="status">
                  {note}
                </span>
              </div>
              {showChart && chart && <BarChart data={chart} />}
              {profile && <ProfilePanel profile={profile} total={rows.row_count} />}
            </>
          )}
        </div>
      )}

      {tab === "details" && (
        <div className="ide-out-body">
          <dl className="details">
            <dt>Status</dt>
            <dd>Succeeded</dd>
            {result.kind === "rows" && (
              <>
                <dt>Rows returned</dt>
                <dd>{result.row_count.toLocaleString()}</dd>
              </>
            )}
            {result.kind === "write" && (
              <>
                <dt>Rows affected</dt>
                <dd>{result.rows_affected.toLocaleString()}</dd>
              </>
            )}
            <dt>Round-trip time</dt>
            <dd>{ms !== undefined ? `${formatMs(ms)} (measured in this browser)` : "—"}</dd>
            <dt>Statement class</dt>
            <dd>{result.kind === "write" ? result.statement : result.kind}</dd>
            {parts.length > 1 && (
              <>
                <dt>Statements</dt>
                <dd>
                  Showing {idx + 1} of {run.statementCount ?? parts.length}. They ran in order, one request each.
                </dd>
                <dt>Whole cell</dt>
                <dd>{run.ms !== undefined ? formatMs(run.ms) : "—"} browser round trips combined</dd>
              </>
            )}
            <dt>Session</dt>
            <dd className="text-mono">{run.sessionId ?? "none (autocommit)"}</dd>
            <dt>Snapshot</dt>
            <dd>Not yet developed — POST /v1/sql has no snapshot parameter.</dd>
          </dl>
        </div>
      )}

      {tab === "plan" && (
        <div className="ide-out-body">
          {!plan || plan.status === "loading" ? (
            <p className="text-muted">Running EXPLAIN…</p>
          ) : plan.status === "error" ? (
            <p className="field-error" role="alert">
              {plan.error?.code}: {plan.error?.message}
              {plan.error?.detail ? ` (${plan.error.detail})` : ""}
            </p>
          ) : (
            <>
              {plan.sql !== cell.text && (
                <p className="text-muted">
                  The cell text changed after this plan was made.{" "}
                  <button type="button" className="ide-link" onClick={() => ide.requestPlan(cell.id)}>
                    Explain again
                  </button>
                </p>
              )}
              <PlanTree text={plan.text ?? ""} />
            </>
          )}
        </div>
      )}

      {tab === "logs" && (
        <div className="ide-out-body">
          <dl className="details">
            <dt>Server message</dt>
            <dd>None. The API returns no log text for a statement.</dd>
            <dt>Error detail</dt>
            <dd>None. The statement succeeded.</dd>
            <dt>Timing</dt>
            <dd>Browser round trip {ms !== undefined ? formatMs(ms) : "—"}. The API reports no server-side breakdown.</dd>
            <dt>Started</dt>
            <dd>{run.startedAt ? new Date(run.startedAt).toLocaleTimeString() : "—"}</dd>
          </dl>
        </div>
      )}
    </div>
  );
}

function ResultBody({
  result,
  ms,
  page,
  pageSize,
  setPage,
  setPageSize,
}: {
  result: NonNullable<CellRun["response"]>["result"];
  ms: number | undefined;
  page: number;
  pageSize: number;
  setPage: (n: number) => void;
  setPageSize: (n: number) => void;
}) {
  switch (result.kind) {
    case "rows": {
      const scalar = scalarOf(result.columns, result.rows);
      if (scalar) {
        return (
          <div className="ide-scalar" role="group" aria-label="Single value result">
            <strong className="text-mono">{scalar.value}</strong>
            <span className="text-mono">{scalar.label}</span>
          </div>
        );
      }
      if (result.row_count === 0) return <EmptyGrid columns={result.columns} ms={ms} />;
      return (
        <ResultsGrid
          columns={result.columns}
          rows={result.rows}
          rowCount={result.row_count}
          page={page}
          pageSize={pageSize}
          onPageChange={setPage}
          onPageSizeChange={setPageSize}
        />
      );
    }
    case "write":
      return (
        <p className="ide-write">
          <strong>{result.statement}</strong> — {result.rows_affected.toLocaleString()} row{result.rows_affected === 1 ? "" : "s"} affected.
        </p>
      );
    case "ddl":
      return <p className="ide-write">OK.</p>;
    case "explain":
      return <PlanTree text={result.plan_text} />;
    case "begin":
      return <p className="ide-write">Transaction started.</p>;
    case "commit":
      return <p className="ide-write">Transaction committed.</p>;
    case "rollback":
      return <p className="ide-write">Transaction rolled back.</p>;
    default:
      return null;
  }
}

function EmptyGrid({ columns, ms }: { columns: SqlColumnMeta[]; ms: number | undefined }) {
  return (
    <div>
      <div className="table-wrap grid-scroll">
        <table className="grid">
          <caption className="visually-hidden">Query result: no rows</caption>
          <thead>
            <tr>
              <th scope="col" className="grid-rownum">
                #
              </th>
              {columns.map((c, i) => (
                <th key={`${i}-${c.name}`} scope="col">
                  <span className="grid-colname">{c.name}</span>
                  {c.type && <span className="grid-type">{c.type}</span>}
                </th>
              ))}
            </tr>
          </thead>
        </table>
      </div>
      <p className="ide-write text-muted">0 rows{ms !== undefined ? ` · ${formatMs(ms)}` : ""}</p>
    </div>
  );
}

function ProfilePanel({ profile, total }: { profile: ReturnType<typeof profileRows>; total: number }) {
  return (
    <div className="ide-profile" role="group" aria-label="Data profile">
      <p className="text-muted">
        Computed in this browser from the {profile.rows.toLocaleString()} row{profile.rows === 1 ? "" : "s"} this cell holds
        {total > profile.rows ? ` (of ${total.toLocaleString()})` : ""}. No query is sent.
      </p>
      <table className="grid">
        <thead>
          <tr>
            <th scope="col">Column</th>
            <th scope="col">Nulls</th>
            <th scope="col">Min</th>
            <th scope="col">Max</th>
          </tr>
        </thead>
        <tbody>
          {profile.columns.map((c, i) => (
            <tr key={i}>
              <td className="text-mono">{c.name}</td>
              <td className="text-mono">{c.nulls.toLocaleString()}</td>
              <td className="text-mono">{c.min ?? "—"}</td>
              <td className="text-mono">{c.max ?? "—"}</td>
            </tr>
          ))}
        </tbody>
      </table>
      {profile.textColumn && (
        <p>
          Distinct values in <span className="text-mono">{profile.textColumn.name}</span>:{" "}
          <strong className="text-mono">{profile.textColumn.distinct.toLocaleString()}</strong>
        </p>
      )}
    </div>
  );
}

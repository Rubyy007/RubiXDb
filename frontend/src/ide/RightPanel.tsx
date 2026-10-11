import { useMemo, useState } from "react";
import { useAuthzQuery, useIndexesQuery, useObservedQueriesQuery, useObservedSessionsQuery, useTableDescriptionQuery } from "../api/queries";
import { Icon, type IconName } from "../components/Icon";
import { formatAge, formatMs } from "./format";
import { guessTable } from "./exports";
import { useIde, type RightTab } from "./ideContext";
import { PlanTree } from "./PlanTree";

/** Time of day, with the date when it is not today, so an entry is never ambiguous. */
function stamp(ms: number): string {
  const d = new Date(ms);
  const today = new Date();
  const sameDay = d.toDateString() === today.toDateString();
  return sameDay ? d.toLocaleTimeString() : d.toLocaleString();
}

/** A statement exactly as it was sent: whitespace and line breaks kept, long text folded behind a toggle. */
function SqlBlock({ sql }: { sql: string }) {
  const [open, setOpen] = useState(false);
  const long = sql.split("\n").length > 3 || sql.length > 200;
  return (
    <div className="ide-sqlblock">
      <pre className="text-mono" data-open={open || !long} aria-label="Statement text">
        {sql}
      </pre>
      {long && (
        <button type="button" className="ide-link ide-small" aria-expanded={open} onClick={() => setOpen((v) => !v)}>
          {open ? "Show less" : "Show full statement"}
        </button>
      )}
    </div>
  );
}

const TABS: { id: RightTab; label: string; icon: IconName }[] = [
  { id: "sessions", label: "Sessions", icon: "link" },
  { id: "running", label: "Running", icon: "monitoring" },
  { id: "history", label: "History", icon: "clock" },
  { id: "schema", label: "Schema", icon: "table" },
  { id: "plan", label: "Plan", icon: "plan" },
];

/** Right-hand context panel; collapses to a 40px icon rail. */
export function RightPanel() {
  const ide = useIde();
  if (!ide.rightOpen) {
    return (
      <aside className="ide-right is-rail" aria-label="Context panel (collapsed)">
        <button type="button" className="ide-iconbtn" aria-label="Expand context panel (Ctrl+J)" onClick={() => ide.setRightOpen(true)}>
          <Icon name="panelRight" />
        </button>
        {TABS.map((t) => (
          <button
            key={t.id}
            type="button"
            className="ide-iconbtn"
            aria-label={t.label}
            title={t.label}
            onClick={() => {
              ide.setRightTab(t.id);
              ide.setRightOpen(true);
            }}
          >
            <Icon name={t.icon} />
          </button>
        ))}
      </aside>
    );
  }
  return (
    <aside className="ide-right" aria-label="Context panel">
      <div className="ide-right-tabs" role="tablist" aria-label="Context panel views">
        {TABS.map((t) => (
          <button key={t.id} type="button" role="tab" aria-selected={ide.rightTab === t.id} className="ide-rtab" title={t.label} onClick={() => ide.setRightTab(t.id)}>
            <Icon name={t.icon} />
            <span>{t.label}</span>
          </button>
        ))}
        <button type="button" className="ide-iconbtn" aria-label="Collapse context panel (Ctrl+J)" onClick={() => ide.setRightOpen(false)}>
          <Icon name="chevronRight" />
        </button>
      </div>
      <div className="ide-right-body" role="tabpanel" aria-label={`${ide.rightTab} panel`}>
        {ide.rightTab === "sessions" && <SessionsTab />}
        {ide.rightTab === "running" && <RunningTab />}
        {ide.rightTab === "history" && <HistoryTab />}
        {ide.rightTab === "schema" && <SchemaTab />}
        {ide.rightTab === "plan" && <PlanTab />}
      </div>
    </aside>
  );
}

function SessionsTab() {
  const ide = useIde();
  const server = useObservedSessionsQuery(true);
  const owned = Object.entries(ide.runs.sessions);
  const ownerLabel = (key: string) => {
    if (key.startsWith("sess-")) return "this worksheet (autocommit off)";
    const nums = ide.flat.filter((f) => f.txnId === key).map((f) => f.num);
    return nums.length ? `transaction block · cells #${nums[0]}${nums.length > 1 ? `–#${nums[nums.length - 1]}` : ""}` : "transaction block";
  };
  const ownedIds = new Set(owned.map(([, s]) => s.sessionId));
  const others = (server.data?.sessions ?? []).filter((s) => !ownedIds.has(s.session_id));

  return (
    <div>
      <label className="ide-switch">
        <input type="checkbox" checked={ide.autocommit} onChange={(e) => ide.setAutocommit(e.target.checked)} />
        <span>Autocommit</span>
      </label>
      <p className="text-muted ide-small">
        {ide.autocommit
          ? "Each cell outside a transaction container runs on its own."
          : "Cells outside a container run in one worksheet transaction. Commit or roll it back below."}
      </p>

      <h3 className="ide-panel-h">Open transactions</h3>
      {owned.length === 0 && others.length === 0 && <p className="text-muted">No open transactions.</p>}
      {owned.map(([key, s]) => (
        <div key={key} className="ide-card" data-testid="session-row">
          <div className="ide-card-row">
            <span className="text-mono ide-wrap" title={s.sessionId}>
              {s.sessionId}
            </span>
            <span className="ide-badge is-open">open</span>
          </div>
          <dl className="ide-kv">
            <dt>Owner</dt>
            <dd>{ownerLabel(key)}</dd>
            <dt>Age</dt>
            <dd>{formatAge((ide.now - s.startedAt) / 1000)}</dd>
            <dt>State</dt>
            <dd>{s.busyCell ? "executing" : "idle"}</dd>
          </dl>
          <div className="ide-card-actions">
            <button type="button" className="btn btn-primary" disabled={!!s.busyCell} onClick={() => ide.finishTxn(key, "COMMIT")}>
              Commit
            </button>
            <button type="button" className="btn btn-danger" disabled={!!s.busyCell} onClick={() => ide.finishTxn(key, "ROLLBACK")}>
              Rollback
            </button>
          </div>
        </div>
      ))}
      {others.map((s) => (
        <div key={s.session_id} className="ide-card">
          <div className="ide-card-row">
            <span className="text-mono ide-wrap" title={s.session_id}>
              {s.session_id}
            </span>
            <span className="ide-badge">{s.state}</span>
          </div>
          <dl className="ide-kv">
            <dt>Owner</dt>
            <dd>not this browser tab</dd>
            <dt>Age</dt>
            <dd>{formatAge(s.age_seconds)}</dd>
            <dt>Idle</dt>
            <dd>{formatAge(s.idle_seconds)}</dd>
            <dt>Timeout</dt>
            <dd>{s.timeout_state}</dd>
          </dl>
          <p className="text-muted ide-small">Only the tab that opened a session can commit or roll it back.</p>
        </div>
      ))}
      {server.isError && (
        <p className="field-error" role="alert">
          Could not read /v1/observability/sessions.
        </p>
      )}
      <p className="text-muted ide-small">Autocommit cells never create a row here.</p>
    </div>
  );
}

function RunningTab() {
  const ide = useIde();
  const server = useObservedQueriesQuery(true);
  const rows = ide.runs.inFlight;
  const label = (cellId: string, sheetId: string) => {
    const f = sheetId === ide.sheet.id ? ide.flat.find((x) => x.cell.id === cellId) : undefined;
    return f ? `cell #${f.num}` : (ide.state.sheets.find((s) => s.id === sheetId)?.name ?? "other worksheet");
  };
  return (
    <div>
      <div className="ide-panel-row">
        <h3 className="ide-panel-h">{rows.length} in flight</h3>
        <button type="button" className="btn btn-danger" disabled={rows.length === 0 && ide.runs.queued === 0} onClick={ide.stopAll}>
          Cancel all
        </button>
      </div>
      {ide.runs.queued > 0 && <p className="text-muted ide-small">{ide.runs.queued} more waiting in this browser's queue.</p>}
      {rows.length === 0 && <p className="text-muted">Nothing is running.</p>}
      {rows.map((r) => (
        <div key={r.cellId} className="ide-card" data-testid="running-row">
          <div className="ide-card-row">
            <span className="ide-badge is-open">
              {label(r.cellId, r.sheetId)}
              {r.count > 1 ? ` · statement ${r.index + 1}/${r.count}` : ""}
            </span>
            <span className="ide-spinner" aria-hidden="true" />
          </div>
          <SqlBlock sql={r.sql} />
          <div className="ide-card-row">
            <span className="text-mono ide-small">elapsed {formatMs(Math.max(0, ide.now - r.startedAt))}</span>
            <button type="button" className="btn btn-danger" onClick={() => ide.cancel(r.cellId)} aria-label={`Cancel ${label(r.cellId, r.sheetId)}`}>
              Cancel
            </button>
          </div>
        </div>
      ))}
      <p className="text-muted ide-small">
        This list comes from this tab. The server reports {server.data ? `${server.data.active} active statement${server.data.active === 1 ? "" : "s"}` : "its own count when reachable"}
        {server.data && server.data.untracked_active > 0 ? ` (${server.data.untracked_active} untracked)` : ""}, but never the SQL text.
      </p>
    </div>
  );
}

function HistoryTab() {
  const ide = useIde();
  const server = useObservedQueriesQuery(true);
  const [scope, setScope] = useState<"all" | "sheet">("all");
  const [outcome, setOutcome] = useState<"all" | "ok" | "fail">("all");
  const [q, setQ] = useState("");
  const local = useMemo(
    () =>
      ide.history.filter(
        (h) =>
          (scope === "all" || h.sheetId === ide.sheet.id) &&
          (outcome === "all" || (outcome === "ok") === h.ok) &&
          (!q || h.sql.toLowerCase().includes(q.toLowerCase())),
      ),
    [ide.history, ide.sheet.id, scope, outcome, q],
  );
  const remote = (server.data?.queries ?? []).filter((r) => outcome === "all" || (outcome === "ok") === (r.error_class === null));
  const cellNum = (cellId: string, sheetId: string) =>
    sheetId === ide.sheet.id ? (ide.flat.find((f) => f.cell.id === cellId)?.num ?? null) : null;

  return (
    <div>
      <div className="ide-filters">
        <label>
          <span className="visually-hidden">Worksheet filter</span>
          <select value={scope} onChange={(e) => setScope(e.target.value === "sheet" ? "sheet" : "all")}>
            <option value="all">All worksheets</option>
            <option value="sheet">This worksheet</option>
          </select>
        </label>
        <label>
          <span className="visually-hidden">Outcome filter</span>
          <select value={outcome} onChange={(e) => setOutcome(e.target.value === "ok" ? "ok" : e.target.value === "fail" ? "fail" : "all")}>
            <option value="all">Any outcome</option>
            <option value="ok">Success</option>
            <option value="fail">Failure</option>
          </select>
        </label>
      </div>
      <input className="ide-search" type="search" placeholder="Search SQL" aria-label="Search history" value={q} onChange={(e) => setQ(e.target.value)} />

      <h3 className="ide-panel-h">This browser session ({local.length})</h3>
      {local.length === 0 && <p className="text-muted">No query history yet.</p>}
      <ul className="ide-histlist">
        {local.map((h) => {
          const n = cellNum(h.cellId, h.sheetId);
          const where = n ? `cell #${n}` : h.cellId.startsWith("c-") ? "other worksheet" : "transaction";
          return (
            <li key={h.seq} className="ide-card" data-testid="history-row">
              <div className="ide-card-row">
                <span className="text-mono ide-small" title={new Date(h.at).toLocaleString()}>
                  {stamp(h.at)}
                </span>
                <span className="ide-small">
                  {where}
                  {h.count > 1 ? ` · statement ${h.index}/${h.count}` : ""}
                </span>
                <span className={`ide-badge ${h.ok ? "is-ok" : "is-bad"}`}>{h.ok ? "success" : (h.code ?? "failed")}</span>
              </div>
              <SqlBlock sql={h.sql} />
              <div className="ide-card-row">
                <span className="text-mono ide-small">
                  {formatMs(h.ms)}
                  {h.rows !== null ? ` · ${h.rows.toLocaleString()} row${h.rows === 1 ? "" : "s"}` : ""}
                </span>
                <button type="button" className="ide-link ide-small" onClick={() => ide.loadHistorySql(h.sql)}>
                  Load into cell
                </button>
              </div>
            </li>
          );
        })}
      </ul>

      <h3 className="ide-panel-h">Server, last {server.data?.max_records ?? 200}</h3>
      <p className="text-muted ide-small">The server records class, timing and counts only, never the SQL text.</p>
      {server.isError && (
        <p className="field-error" role="alert">
          Could not read /v1/observability/queries.
        </p>
      )}
      <ul className="ide-histlist">
        {remote.slice(0, 50).map((r) => (
          <li key={r.query_id} className="ide-card" data-testid="server-history-row">
            <div className="ide-card-row">
              <span className="text-mono ide-small">{new Date(r.start_unix_ms).toLocaleTimeString()}</span>
              <span className="ide-small">{r.statement_class}</span>
              <span className={`ide-badge ${r.error_class ? "is-bad" : "is-ok"}`}>{r.error_class ?? r.state}</span>
            </div>
            <div className="text-mono ide-small">
              {formatMs(r.duration_ms)}
              {r.rows_returned !== null ? ` · ${r.rows_returned.toLocaleString()} returned` : r.rows_affected !== null ? ` · ${r.rows_affected.toLocaleString()} affected` : ""}
            </div>
          </li>
        ))}
      </ul>
    </div>
  );
}

function SchemaTab() {
  const ide = useIde();
  const focused = ide.flat.find((f) => f.cell.id === ide.focusedId);
  const fromCell = focused && focused.cell.kind === "sql" ? guessTable(focused.cell.text) : null;
  const guess = fromCell && fromCell !== "table_name" ? (fromCell.split(".").pop() ?? null) : null;
  const name = ide.selectedTable ?? (guess && ide.catalog.tables.some((t) => t.name.toLowerCase() === guess.toLowerCase()) ? guess : null);
  const desc = useTableDescriptionQuery(name);
  const indexes = useIndexesQuery();
  const authz = useAuthzQuery(name !== null);

  if (!name) return <p className="text-muted">Select a table in the tree, or put the cursor in a cell that reads from one.</p>;
  if (desc.isLoading) return <p className="text-muted">Loading {name}…</p>;
  if (desc.isError || !desc.data)
    return (
      <p className="field-error" role="alert">
        Could not read the table description for {name}.
      </p>
    );
  const cols = desc.data.columns;
  const pk = cols.filter((c) => c.primary_key).map((c) => c.name);
  const idx = (indexes.data ?? []).filter((i) => i.table_id === desc.data!.table_id);
  return (
    <div>
      <h3 className="ide-panel-h text-mono">{name}</h3>
      <h4 className="ide-panel-h2">Columns</h4>
      <ul className="ide-cols">
        {cols.map((c) => (
          <li key={c.ordinal}>
            <span className="text-mono">{c.name}</span>
            {c.primary_key && <span className="ide-badge is-open">PK</span>}
            <span className="text-mono ide-type">{c.data_type}</span>
          </li>
        ))}
      </ul>
      <h4 className="ide-panel-h2">Indexes</h4>
      {idx.length === 0 ? (
        <p className="text-muted ide-small">No indexes.</p>
      ) : (
        <ul className="ide-cols">
          {idx.map((i) => (
            <li key={i.index_id}>
              <span className="text-mono">{i.name}</span>
              <span className="text-mono ide-type">
                {i.kind}
                {i.unique ? " · unique" : ""} · {i.state}
              </span>
            </li>
          ))}
        </ul>
      )}
      <h4 className="ide-panel-h2">Constraints</h4>
      <ul className="ide-cols">
        {pk.length > 0 && (
          <li>
            <span className="text-mono">PRIMARY KEY</span>
            <span className="text-mono ide-type">({pk.join(", ")})</span>
          </li>
        )}
        {cols.filter((c) => !c.nullable).map((c) => (
          <li key={c.ordinal}>
            <span className="text-mono">NOT NULL</span>
            <span className="text-mono ide-type">{c.name}</span>
          </li>
        ))}
      </ul>
      <h4 className="ide-panel-h2">Your grants</h4>
      {authz.data ? (
        <>
          <p className="ide-small">
            {authz.data.principal} · role <strong>{authz.data.role}</strong>
          </p>
          {authz.data.explicit_grants.length === 0 ? (
            <p className="text-muted ide-small">No explicit grants; access comes from the role.</p>
          ) : (
            <ul className="ide-cols">
              {authz.data.explicit_grants.map((g, i) => (
                <li key={i}>
                  <span className="text-mono">{g.privilege}</span>
                  <span className="text-mono ide-type">
                    {g.object_kind} #{g.object_id}
                  </span>
                </li>
              ))}
            </ul>
          )}
        </>
      ) : (
        <p className="text-muted ide-small">{authz.isError ? "Could not read /v1/catalog/authz." : "Loading…"}</p>
      )}
      <p className="text-muted ide-small">Read-only. The API lists grants for the connected principal only.</p>
    </div>
  );
}

function PlanTab() {
  const ide = useIde();
  const focused = ide.flat.find((f) => f.cell.id === ide.focusedId);
  if (!focused || focused.cell.kind !== "sql") return <p className="text-muted">Focus a SQL cell to see its plan.</p>;
  const plan = ide.plans[focused.cell.id];
  return (
    <div>
      <div className="ide-panel-row">
        <h3 className="ide-panel-h">EXPLAIN · cell #{focused.num}</h3>
        <button type="button" className="btn btn-secondary" disabled={focused.cell.text.trim() === "" || plan?.status === "loading"} onClick={() => ide.requestPlan(focused.cell.id)}>
          {plan ? "Explain again" : "Explain"}
        </button>
      </div>
      {!plan && <p className="text-muted">Not explained yet. EXPLAIN never runs the statement.</p>}
      {plan?.status === "loading" && <p className="text-muted">Running EXPLAIN…</p>}
      {plan?.status === "error" && (
        <p className="field-error" role="alert">
          {plan.error?.code}: {plan.error?.message}
          {plan.error?.detail ? ` (${plan.error.detail})` : ""}
        </p>
      )}
      {plan?.status === "ready" && (
        <>
          {plan.sql !== focused.cell.text && <p className="text-muted ide-small">The cell text changed after this plan was made.</p>}
          <PlanTree text={plan.text ?? ""} />
          <p className="text-muted ide-small">The API returns the plan as text; the tree is built in this browser.</p>
        </>
      )}
    </div>
  );
}

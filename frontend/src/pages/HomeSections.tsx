import { useMemo, useState, type ReactNode } from "react";
import { Link, useNavigate } from "react-router-dom";
import { useSession } from "../context/SessionContext";
import {
  useAdminStatusQuery,
  useBackupsQuery,
  useCreateSnapshotMutation,
  useSnapshotsQuery,
} from "../api/queries";
import { Icon, type IconName } from "../components/Icon";
import { Tabs } from "../components/Tabs";
import { Table, type Column } from "../components/Table";
import { setPendingSql, queryTitle, useRecentQueries } from "../utils/sessionActivity";

// ---------------------------------------------------------------------
// Quick actions -- only actions rubiXDb really has. There is no import
// path (no bulk loader, no external storage), so there is no "Import
// data" card.
// ---------------------------------------------------------------------

function ActionCard({ icon, title, subtitle }: { icon: IconName; title: string; subtitle: string }) {
  return (
    <>
      <span className="qa-icon">
        <Icon name={icon} />
      </span>
      <span className="qa-title">{title}</span>
      <span className="qa-sub">{subtitle}</span>
    </>
  );
}

export function QuickActions() {
  const { session } = useSession();
  const isAdmin = session?.role === "admin";
  const create = useCreateSnapshotMutation();
  const [result, setResult] = useState<{ ok: boolean; text: string } | null>(null);

  return (
    <section aria-labelledby="home-quick-actions" className="home-section">
      <h2 id="home-quick-actions" className="home-heading">
        Quick actions
      </h2>
      <div className="qa-grid">
        <Link to="/sql" className="qa-card">
          <ActionCard icon="sql" title="New SQL query" subtitle="Open an empty editor" />
        </Link>
        {isAdmin && (
          <button
            type="button"
            className="qa-card"
            disabled={create.isPending}
            onClick={() =>
              create.mutate(undefined, {
                onSuccess: (s) => setResult({ ok: true, text: `Snapshot created at seq ${s.seq}.` }),
                onError: () => setResult({ ok: false, text: "Could not create the snapshot." }),
              })
            }
          >
            <ActionCard
              icon="snapshots"
              title="Create snapshot"
              subtitle={create.isPending ? "Creating…" : "Pin the current data version"}
            />
          </button>
        )}
        {isAdmin && (
          <Link to="/operations" className="qa-card">
            <ActionCard icon="admin" title="View backups" subtitle="List and verify backup files" />
          </Link>
        )}
      </div>
      <p className={result?.ok === false ? "field-error" : "text-muted"} role="status" aria-live="polite">
        {result?.text ?? ""}
      </p>
    </section>
  );
}

// ---------------------------------------------------------------------
// Recent items -- real state only: this session's query history (in
// memory), the held snapshots, and (admin) the backup files.
// ---------------------------------------------------------------------

type ItemType = "Query" | "Snapshot" | "Backup";

interface RecentItem {
  key: string;
  title: string;
  type: ItemType;
  viewedMs: number | null;
  updatedMs: number | null;
  sortMs: number;
  failed?: boolean;
}

const TABS = [
  { id: "all", label: "All" },
  { id: "queries", label: "Queries" },
  { id: "snapshots", label: "Snapshots" },
  { id: "backups", label: "Backups" },
];

const TAB_TYPE: Record<string, ItemType | null> = {
  all: null,
  queries: "Query",
  snapshots: "Snapshot",
  backups: "Backup",
};

function when(ms: number | null): string {
  return ms === null ? "—" : new Date(ms).toLocaleString();
}

const COLUMNS: Column<RecentItem>[] = [
  { key: "title", header: "Title", render: (r): ReactNode => r.title },
  { key: "type", header: "Type", render: (r) => (r.failed ? `${r.type} (failed)` : r.type) },
  { key: "viewed", header: "Viewed", render: (r) => when(r.viewedMs) },
  { key: "updated", header: "Updated", render: (r) => when(r.updatedMs) },
];

export function RecentItems() {
  const { session } = useSession();
  const isAdmin = session?.role === "admin";
  const [tab, setTab] = useState("all");
  const queries = useRecentQueries();
  const snapshots = useSnapshotsQuery();
  // Ask for the backup list only when the server says backups are configured
  // (admin status, the same query the Admin page already polls) -- otherwise
  // the endpoint answers 501 NOT_CONFIGURED and Home would raise a failed request.
  const adminStatus = useAdminStatusQuery(isAdmin);
  const backupsConfigured = adminStatus.data?.backups.configured === true;
  const backups = useBackupsQuery(isAdmin && backupsConfigured);

  const items = useMemo<RecentItem[]>(() => {
    const out: RecentItem[] = [];
    queries.forEach((q, i) =>
      out.push({
        key: `q-${q.ranAt}-${i}`,
        title: queryTitle(q.sql),
        type: "Query",
        viewedMs: q.ranAt,
        updatedMs: null,
        sortMs: q.ranAt,
        failed: !q.ok,
      }),
    );
    for (const s of snapshots.data ?? []) {
      const ms = s.created_at_unix_secs * 1000;
      out.push({ key: `s-${s.id}`, title: `Snapshot ${s.id}`, type: "Snapshot", viewedMs: null, updatedMs: ms, sortMs: ms });
    }
    for (const b of backups.data?.backups ?? []) {
      out.push({
        key: `b-${b.name}`,
        title: b.name,
        type: "Backup",
        viewedMs: null,
        updatedMs: b.modified_unix_ms,
        sortMs: b.modified_unix_ms ?? 0,
      });
    }
    return out.sort((a, b) => b.sortMs - a.sortMs);
  }, [queries, snapshots.data, backups.data]);

  const wanted = TAB_TYPE[tab];
  const rows = (wanted ? items.filter((i) => i.type === wanted) : items).slice(0, 25);

  let emptyTitle = "Nothing here yet";
  let emptyMessage: string | undefined = "Run a query or create a snapshot and it will appear here.";
  if (tab === "queries") {
    emptyTitle = "No queries yet";
    emptyMessage = "Query history is kept in memory for this browser session only.";
  } else if (tab === "snapshots") {
    emptyTitle = "No snapshots held";
    emptyMessage = "Snapshots you create are listed here.";
  } else if (tab === "backups") {
    if (!isAdmin) {
      emptyTitle = "Backups are listed for administrators";
      emptyMessage = "Sign in with an admin key to see backup files.";
    } else if (adminStatus.data && !backupsConfigured) {
      emptyTitle = "Backups are not configured";
      emptyMessage = "This server has no backup directory.";
    } else {
      emptyTitle = backups.isError ? "Backups could not be loaded" : "No backups";
      emptyMessage = undefined;
    }
  }

  return (
    <section aria-labelledby="home-recent" className="home-section">
      <h2 id="home-recent" className="home-heading">
        Recent items
      </h2>
      <Tabs tabs={TABS} active={tab} onChange={setTab} />
      <div role="tabpanel" aria-label="Recent items list">
        <Table
          columns={COLUMNS}
          rows={rows}
          rowKey={(r) => r.key}
          caption="Recent items"
          isLoading={tab === "snapshots" && snapshots.isLoading}
          emptyTitle={emptyTitle}
          emptyMessage={emptyMessage}
        />
      </div>
    </section>
  );
}

// ---------------------------------------------------------------------
// Start with a template -- static SQL snippets. Choosing one loads it into
// the SQL console editor; it is NOT executed. rubiXDb has no template
// mechanism of its own, so these are plain strings in this file, in the
// order that makes a runnable walk-through (verified against the real
// binary by e2e-gui/home.spec.ts).
// ---------------------------------------------------------------------

const TEMPLATES: { id: string; title: string; subtitle: string; sql: string }[] = [
  {
    id: "create",
    title: "Create a table",
    subtitle: "A customers table with a primary key",
    sql: "CREATE TABLE customers (id INTEGER PRIMARY KEY, name TEXT, city TEXT)",
  },
  {
    id: "insert",
    title: "Insert rows",
    subtitle: "Add two sample customers",
    sql: "INSERT INTO customers (id, name, city) VALUES (1, 'Ada', 'London'), (2, 'Grace', 'New York')",
  },
  {
    id: "select",
    title: "Filter and sort",
    subtitle: "Read rows with WHERE and ORDER BY",
    sql: "SELECT id, name FROM customers WHERE city = 'London' ORDER BY id",
  },
  {
    id: "index",
    title: "Add an index",
    subtitle: "Index the city column",
    sql: "CREATE INDEX customers_city_idx ON customers (city)",
  },
  {
    id: "group",
    title: "Group and count",
    subtitle: "Count customers per city",
    sql: "SELECT city, COUNT(*) AS n FROM customers GROUP BY city ORDER BY COUNT(*) DESC",
  },
  {
    id: "explain",
    title: "Explain a query",
    subtitle: "Show the plan for a lookup",
    sql: "EXPLAIN SELECT * FROM customers WHERE city = 'London'",
  },
];

export function Templates() {
  const navigate = useNavigate();
  return (
    <section aria-labelledby="home-templates" className="home-section">
      <h2 id="home-templates" className="home-heading">
        Start with a template
      </h2>
      <ul className="tpl-grid">
        {TEMPLATES.map((t) => (
          <li key={t.id} className="tpl-card">
            <div className="tpl-text">
              <span className="tpl-title">{t.title}</span>
              <span className="tpl-sub">{t.subtitle}</span>
            </div>
            <button
              type="button"
              className="shell-icon-btn tpl-add"
              aria-label={`Load template: ${t.title}`}
              onClick={() => {
                setPendingSql(t.sql);
                navigate("/sql");
              }}
            >
              <Icon name="plus" />
            </button>
          </li>
        ))}
      </ul>
    </section>
  );
}

import { useState } from "react";
import { useSession } from "../context/SessionContext";
import {
  useAdminStatusQuery,
  useBackupsQuery,
  useCheckMutation,
  useCreateBackupMutation,
  useDeleteBackupMutation,
  usePurgeOrphansMutation,
  useVerifyBackupMutation,
} from "../api/queries";
import { Card, CardGrid, Stat } from "../components/Card";
import { Badge } from "../components/Badge";
import { Button } from "../components/Button";
import { Dialog } from "../components/Dialog";
import { Input } from "../components/Field";
import { Spinner } from "../components/Spinner";
import { ErrorState } from "../components/EmptyState";
import { Table, type Column } from "../components/Table";
import { useToast } from "../components/Toast";
import { formatBytes, formatDurationSecs, formatMs, formatNumber } from "../utils/format";
import type { BackupEntry, CheckFinding, CheckResult, PurgeResult } from "../api/types";

/** Operator console: instance / WAL / compaction / query / resource status,
 * backups, integrity check and maintenance. Every action says which class it
 * is (INSPECTION, SAFE, DESTRUCTIVE); destructive ones need the exact name or
 * count typed back, and the server validates it again. */
export function OperationsPage() {
  const { session } = useSession();
  const isAdmin = session?.role === "admin";
  if (!isAdmin) {
    return (
      <div className="stack">
        <h1 style={{ fontSize: "var(--font-size-2xl)" }}>Operations</h1>
        <p className="text-muted" role="status">
          The operator console requires an administrator API key.
        </p>
      </div>
    );
  }
  return <OperationsAdmin />;
}

const BACKUP_NAME = /^[A-Za-z0-9_-][A-Za-z0-9._-]{0,63}$/;

function OperationsAdmin() {
  const { notify } = useToast();
  const status = useAdminStatusQuery();
  const backups = useBackupsQuery();
  const createBackup = useCreateBackupMutation();
  const verifyBackup = useVerifyBackupMutation();
  const deleteBackup = useDeleteBackupMutation();
  const check = useCheckMutation();
  const purge = usePurgeOrphansMutation();

  const [newName, setNewName] = useState("");
  const [pendingDelete, setPendingDelete] = useState<BackupEntry | null>(null);
  const [deleteConfirm, setDeleteConfirm] = useState("");
  const [checkResult, setCheckResult] = useState<CheckResult | null>(null);
  const [purgePlan, setPurgePlan] = useState<PurgeResult | null>(null);
  const [purgeConfirm, setPurgeConfirm] = useState("");
  const [purgeOpen, setPurgeOpen] = useState(false);
  const [verified, setVerified] = useState<string | null>(null);

  const s = status.data;
  const nameInvalid = newName.length > 0 && !BACKUP_NAME.test(newName);

  const backupColumns: Column<BackupEntry>[] = [
    { key: "name", header: "Name", mono: true, render: (b) => b.name },
    { key: "size", header: "Size", render: (b) => formatBytes(b.bytes) },
    {
      key: "actions",
      header: "",
      render: (b) => (
        <div className="row">
          <Button
            variant="ghost"
            disabled={verifyBackup.isPending}
            onClick={() =>
              verifyBackup.mutate(b.name, {
                onSuccess: (v) => {
                  setVerified(
                    `${b.name}: verified OK — format v${v.format_version}, ${formatNumber(v.entries)} entries, ${v.catalog.tables} table(s), snapshot seq ${v.snapshot_seq}`,
                  );
                  notify(`Backup ${b.name} verified`, "success");
                },
                onError: (err) => {
                  setVerified(null);
                  notify(err instanceof Error ? err.message : "Verification failed", "error");
                },
              })
            }
          >
            Verify
          </Button>
          <Button
            variant="danger"
            onClick={() => {
              setDeleteConfirm("");
              setPendingDelete(b);
            }}
          >
            Delete
          </Button>
        </div>
      ),
    },
  ];

  const findingColumns: Column<CheckFinding>[] = [
    {
      key: "sev",
      header: "Severity",
      render: (f) => (
        <Badge tone={f.severity === "error" ? "danger" : f.severity === "warning" ? "pressure" : "neutral"}>
          {f.severity}
        </Badge>
      ),
    },
    { key: "code", header: "Code", mono: true, render: (f) => f.code },
    { key: "obj", header: "Object", mono: true, render: (f) => f.object },
    { key: "detail", header: "Detail", render: (f) => f.detail },
  ];

  return (
    <div className="stack">
      <h1 style={{ fontSize: "var(--font-size-2xl)" }}>Operations</h1>
      <p className="text-muted">
        Operator view of this instance. Each action is labelled INSPECTION (read only), SAFE
        (creates a new file only) or DESTRUCTIVE (needs an exact confirmation).
      </p>

      {status.isLoading && <Spinner label="Loading operator status" />}
      {status.isError && <ErrorState message="Could not load operator status." />}

      {s && (
        <>
          <CardGrid>
            <Card title="Instance">
              <Stat label="Uptime" value={formatDurationSecs(s.instance.uptime_secs)} />
              <Stat label="Version" value={s.instance.version} />
            </Card>
            <Card title="Storage">
              <Badge tone={s.storage.state === "Healthy" ? "healthy" : "pressure"}>
                {s.storage.state}
              </Badge>
              <Stat label="SSTables" value={formatNumber(s.storage.sstable_count)} />
              <Stat label="Active memtable" value={formatBytes(s.storage.memtable_active_bytes)} />
            </Card>
            <Card title="Write-ahead log">
              <Badge tone={s.wal.poisoned ? "danger" : "healthy"}>
                {s.wal.poisoned ? "Poisoned" : s.wal.pool_state}
              </Badge>
              <Stat label="Durable through" value={formatNumber(s.wal.durable_through)} />
              <Stat label="Pending waiters" value={s.wal.pending_waiters} />
              <Stat label="Avg batch (records)" value={s.wal.avg_batch_records.toFixed(1)} />
              <Stat label="Avg flush" value={formatMs(s.wal.avg_batch_processing_ms)} />
              <Stat label="Sync failures" value={s.wal.sync_failures ?? "-"} />
            </Card>
            <Card title="Compaction">
              <Badge tone={s.compaction.auto_trigger_enabled ? "healthy" : "neutral"}>
                {s.compaction.auto_trigger_enabled ? "Auto" : "Manual only"}
              </Badge>
              <Stat label="Cycles" value={formatNumber(s.compaction.cycles_completed)} />
              <Stat
                label="Last cycle"
                value={s.compaction.last_cycle_ms === null ? "—" : formatMs(s.compaction.last_cycle_ms)}
              />
            </Card>
            <Card title="Queries">
              <Stat label="Total" value={formatNumber(s.queries.requests)} />
              <Stat label="Errors" value={formatNumber(s.queries.errors)} />
              <Stat label="Timeouts" value={formatNumber(s.queries.timeouts)} />
              <Stat label="Cancelled" value={formatNumber(s.queries.cancellations)} />
              <Stat label="Open transactions" value={s.sessions.active_transactions} />
              <Stat
                label="p50 / p99 / max"
                value={
                  s.queries.latency_ms
                    ? `${formatMs(s.queries.latency_ms.p50)} / ${formatMs(s.queries.latency_ms.p99)} / ${formatMs(s.queries.latency_ms.max)}`
                    : "—"
                }
              />
            </Card>
            <Card title="Resources">
              <Stat label="Memory (RSS)" value={formatBytes(s.resources.rss_bytes)} />
              <Stat label="Threads" value={s.resources.threads} />
              <Stat label="Handles" value={s.resources.handles} />
              <Stat label="Database size" value={formatBytes(s.disk.data_dir_bytes)} />
              <Stat
                label="Free on volume"
                value={s.disk.volume_free_bytes === null ? "—" : formatBytes(s.disk.volume_free_bytes)}
              />
            </Card>
          </CardGrid>
          <p className="text-muted" style={{ fontSize: "var(--font-size-sm)" }}>
            Last recovery: {formatMs(s.recovery.duration_ms)} (
            {formatNumber(s.recovery.wal_records_applied)} WAL records applied).
          </p>
        </>
      )}

      <Card title="Backups — SAFE to create, DESTRUCTIVE to delete">
        {s && !s.backups.configured && (
          <p className="text-muted" role="status">
            No backup directory is configured for this server.
          </p>
        )}
        <div className="row" style={{ alignItems: "flex-end" }}>
          <Input
            label="New backup name"
            value={newName}
            onChange={(e) => setNewName(e.target.value)}
            placeholder="nightly-2026-10-03"
            hint="1–64 letters, digits, '.', '_' or '-'. Never overwrites an existing backup."
            error={nameInvalid ? "Not a valid backup name." : undefined}
          />
          <Button
            variant="primary"
            disabled={createBackup.isPending || newName.length === 0 || nameInvalid}
            onClick={() =>
              createBackup.mutate(newName, {
                onSuccess: (r) => {
                  notify(
                    `Backup ${r.name} created: ${formatNumber(r.entries)} entries, ${formatBytes(r.file_bytes)} in ${formatMs(r.duration_ms)}`,
                    "success",
                  );
                  setNewName("");
                },
                onError: (err) =>
                  notify(err instanceof Error ? err.message : "Backup failed", "error"),
              })
            }
          >
            {createBackup.isPending ? "Backing up…" : "Create backup"}
          </Button>
        </div>
        {verified && (
          <p role="status" className="text-muted">
            {verified}
          </p>
        )}
        <Table
          caption="Backups"
          columns={backupColumns}
          rows={backups.data?.backups ?? []}
          rowKey={(b) => b.name}
          isLoading={backups.isLoading}
          isError={backups.isError}
          errorMessage="Could not list backups."
          emptyTitle="No backups"
          emptyMessage="Create a backup above."
        />
      </Card>

      <Card title="Integrity check — INSPECTION">
        <p className="text-muted">
          Scans the catalog, every table row and every index entry at one consistent snapshot,
          while the database keeps serving. It does not repair anything.
        </p>
        <Button
          variant="primary"
          disabled={check.isPending}
          onClick={() =>
            check.mutate(undefined, {
              onSuccess: (r) => setCheckResult(r),
              onError: (err) =>
                notify(err instanceof Error ? err.message : "Check failed to run", "error"),
            })
          }
        >
          {check.isPending ? "Checking…" : "Run integrity check"}
        </Button>
        {checkResult && (
          <div className="stack" role="status" aria-live="polite">
            <Badge tone={!checkResult.complete ? "danger" : checkResult.errors > 0 ? "danger" : checkResult.warnings > 0 ? "pressure" : "healthy"}>
              {!checkResult.complete
                ? "Incomplete"
                : checkResult.errors > 0
                  ? `${checkResult.errors} error(s)`
                  : checkResult.warnings > 0
                    ? `Clean, ${checkResult.warnings} warning(s)`
                    : "Clean"}
            </Badge>
            <span className="text-muted">
              {formatNumber(checkResult.stats.rows_checked)} rows and{" "}
              {formatNumber(checkResult.stats.index_entries_checked)} index entries in{" "}
              {formatMs(checkResult.stats.duration_ms)}
            </span>
            {checkResult.findings.length > 0 && (
              <Table
                caption="Integrity findings"
                columns={findingColumns}
                rows={checkResult.findings}
                rowKey={(f) => `${f.code}|${f.object}|${f.detail}`}
              />
            )}
          </div>
        )}
      </Card>

      <Card title="Maintenance — reclaim data left by DROP TABLE">
        <p className="text-muted">
          DROP TABLE removes the catalog entry but leaves the table's rows in storage. The dry run
          below (INSPECTION) lists what would be removed; deleting is DESTRUCTIVE and needs the
          exact entry count typed back.
        </p>
        <Button
          variant="ghost"
          disabled={purge.isPending}
          onClick={() =>
            purge.mutate(
              { apply: false },
              {
                onSuccess: (r) => {
                  setPurgePlan(r);
                  setPurgeConfirm("");
                },
                onError: (err) =>
                  notify(err instanceof Error ? err.message : "Dry run failed", "error"),
              },
            )
          }
        >
          Show what can be reclaimed
        </Button>
        {purgePlan && !purgePlan.applied && (
          <div className="stack" role="status">
            <span>
              {formatNumber(purgePlan.plan.entries)} entries belong to{" "}
              {purgePlan.plan.orphan_table_ids.length} dropped table(s).
            </span>
            {purgePlan.plan.entries > 0 && (
              <Button variant="danger" onClick={() => setPurgeOpen(true)}>
                Delete these entries…
              </Button>
            )}
          </div>
        )}
        {purgePlan && purgePlan.applied && (
          <p role="status">
            Deleted {formatNumber(purgePlan.deleted)} entries; {purgePlan.remaining} remain.
          </p>
        )}
      </Card>

      <Dialog
        open={pendingDelete !== null}
        title="Delete backup"
        confirmLabel="Delete backup"
        confirmVariant="danger"
        confirmDisabled={pendingDelete === null || deleteConfirm !== pendingDelete.name}
        busy={deleteBackup.isPending}
        onClose={() => setPendingDelete(null)}
        onConfirm={() => {
          if (!pendingDelete) return;
          deleteBackup.mutate(
            { name: pendingDelete.name, confirm: deleteConfirm },
            {
              onSuccess: () => {
                notify(`Backup ${pendingDelete.name} deleted`, "success");
                setPendingDelete(null);
              },
              onError: (err) =>
                notify(err instanceof Error ? err.message : "Delete failed", "error"),
            },
          );
        }}
      >
        <p>
          This permanently deletes the backup file. Type <strong>{pendingDelete?.name}</strong> to
          confirm.
        </p>
        <Input
          label="Backup name"
          value={deleteConfirm}
          onChange={(e) => setDeleteConfirm(e.target.value)}
          autoComplete="off"
        />
      </Dialog>

      <Dialog
        open={purgeOpen}
        title="Delete reclaimable data"
        confirmLabel="Delete entries"
        confirmVariant="danger"
        confirmDisabled={!purgePlan || purgeConfirm !== String(purgePlan.plan.entries)}
        busy={purge.isPending}
        onClose={() => setPurgeOpen(false)}
        onConfirm={() => {
          if (!purgePlan) return;
          purge.mutate(
            { apply: true, expected: purgePlan.plan.entries },
            {
              onSuccess: (r) => {
                setPurgePlan(r);
                setPurgeOpen(false);
                notify(`Deleted ${formatNumber(r.deleted)} entries`, "success");
              },
              onError: (err) => {
                setPurgeOpen(false);
                notify(err instanceof Error ? err.message : "Delete failed", "error");
              },
            },
          );
        }}
      >
        <p>
          This permanently deletes {purgePlan ? formatNumber(purgePlan.plan.entries) : 0} stored
          entries of dropped tables. Type the number <strong>{purgePlan?.plan.entries}</strong> to
          confirm.
        </p>
        <Input
          label="Entry count"
          value={purgeConfirm}
          onChange={(e) => setPurgeConfirm(e.target.value)}
          autoComplete="off"
        />
      </Dialog>
    </div>
  );
}

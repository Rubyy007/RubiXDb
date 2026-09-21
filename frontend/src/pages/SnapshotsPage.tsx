import { useState } from "react";
import { useSession } from "../context/SessionContext";
import {
  useCreateSnapshotMutation,
  useReleaseSnapshotMutation,
  useSnapshotsQuery,
} from "../api/queries";
import { Card } from "../components/Card";
import { Button } from "../components/Button";
import { Table, type Column } from "../components/Table";
import { Dialog } from "../components/Dialog";
import { useToast } from "../components/Toast";
import { formatTimestamp } from "../utils/format";
import type { SnapshotBody } from "../api/types";

export function SnapshotsPage() {
  const { session } = useSession();
  const { notify } = useToast();
  const snapshots = useSnapshotsQuery();
  const createMutation = useCreateSnapshotMutation();
  const releaseMutation = useReleaseSnapshotMutation();
  const [pendingRelease, setPendingRelease] = useState<SnapshotBody | null>(null);
  const canWrite = session?.role === "admin";

  const columns: Column<SnapshotBody>[] = [
    { key: "id", header: "ID", mono: true, render: (s) => s.id },
    { key: "seq", header: "Sequence", mono: true, render: (s) => String(s.seq) },
    { key: "created", header: "Created", render: (s) => formatTimestamp(s.created_at_unix_secs) },
    {
      key: "actions",
      header: "",
      render: (s) =>
        canWrite ? (
          <Button variant="danger" onClick={() => setPendingRelease(s)}>
            Release
          </Button>
        ) : null,
    },
  ];

  return (
    <div className="stack">
      <div className="spread">
        <h1 style={{ fontSize: "var(--font-size-2xl)" }}>Snapshots</h1>
        {canWrite && (
          <Button
            variant="primary"
            disabled={createMutation.isPending}
            onClick={() =>
              createMutation.mutate(undefined, {
                onSuccess: (s) => notify(`Snapshot created at seq ${s.seq}`, "success"),
                onError: (err) =>
                  notify(err instanceof Error ? err.message : "Failed to create snapshot", "error"),
              })
            }
          >
            {createMutation.isPending ? "Creating…" : "New snapshot"}
          </Button>
        )}
      </div>

      <p className="text-muted">
        A snapshot pins the engine's live-version retention so Compaction never removes data a
        held snapshot could still read historically. Snapshots live only in this connected
        service instance — restarting the API clears the list, but historical reads at a past
        sequence number remain correct regardless of whether a snapshot was ever taken for it.
      </p>

      <Card title={`Held snapshots (${snapshots.data?.length ?? 0})`}>
        <Table
          columns={columns}
          rows={snapshots.data ?? []}
          rowKey={(s) => s.id}
          caption="Held snapshots"
          isLoading={snapshots.isLoading}
          isError={snapshots.isError}
          emptyTitle="No snapshots held"
          emptyMessage={canWrite ? "Create one to pin the current state." : undefined}
        />
      </Card>

      <Dialog
        open={pendingRelease !== null}
        title="Release snapshot?"
        confirmLabel="Release"
        confirmVariant="danger"
        busy={releaseMutation.isPending}
        onClose={() => setPendingRelease(null)}
        onConfirm={() => {
          if (!pendingRelease) return;
          releaseMutation.mutate(pendingRelease.id, {
            onSuccess: () => {
              notify("Snapshot released", "success");
              setPendingRelease(null);
            },
            onError: (err) => {
              notify(err instanceof Error ? err.message : "Failed to release snapshot", "error");
              setPendingRelease(null);
            },
          });
        }}
      >
        Once released, data superseded before seq {pendingRelease?.seq} becomes eligible for
        Compaction to reclaim.
      </Dialog>
    </div>
  );
}

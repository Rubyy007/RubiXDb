import { useMetricsQuery, useStatusQuery } from "../api/queries";
import { Card, CardGrid, Stat } from "../components/Card";
import { Badge, storageStateTone } from "../components/Badge";
import { Table, type Column } from "../components/Table";
import { ErrorState } from "../components/EmptyState";
import { formatBytes, formatDurationSecs, formatMs, formatNumber } from "../utils/format";
import type { RouteMetricsSnapshot } from "../api/types";

const routeColumns: Column<RouteMetricsSnapshot>[] = [
  { key: "route", header: "Route", mono: true, render: (r) => r.route },
  { key: "count", header: "Requests", render: (r) => formatNumber(r.count) },
  {
    key: "errors",
    header: "Errors",
    render: (r) => (
      <span style={r.error_count > 0 ? { color: "var(--color-danger)", fontWeight: 600 } : undefined}>
        {formatNumber(r.error_count)}
      </span>
    ),
  },
  { key: "p50", header: "p50", render: (r) => formatMs(r.p50_ms) },
  { key: "p95", header: "p95", render: (r) => formatMs(r.p95_ms) },
  { key: "p99", header: "p99", render: (r) => formatMs(r.p99_ms) },
];

export function HealthPage() {
  const status = useStatusQuery();
  const metrics = useMetricsQuery();

  return (
    <div className="stack">
      <h1 style={{ fontSize: "var(--font-size-2xl)" }}>Health / Storage</h1>

      {status.isError && <ErrorState message="Could not load engine status." />}
      {status.data && (
        <Card title="Engine status">
          <CardGrid>
            <div>
              <div className="stat-label">Storage state</div>
              <div style={{ marginTop: "var(--space-1)" }}>
                <Badge tone={storageStateTone(status.data.storage_state)}>
                  {status.data.storage_state}
                </Badge>
              </div>
              <p className="text-faint" style={{ marginTop: "var(--space-1)", fontSize: "var(--font-size-xs)" }}>
                {status.data.storage_state === "Healthy" &&
                  "Normal operation. Writes and Compaction proceed as usual."}
                {status.data.storage_state === "StoragePressure" &&
                  "Flush is retrying an ENOSPC-classified failure at a slower cadence. Writes are still accepted; Compaction defers."}
                {status.data.storage_state === "StorageFull" &&
                  "Writes are being rejected before reaching the WAL. Reads still work. Resolves automatically once a flush succeeds."}
              </p>
            </div>
            <Stat
              label="Storage-pressure events"
              value={formatNumber(status.data.storage_pressure_events)}
            />
            <Stat label="SSTable count" value={formatNumber(status.data.sstable_count)} />
            <Stat
              label="Manifest size"
              value={status.data.manifest_size_bytes != null ? formatBytes(status.data.manifest_size_bytes) : "—"}
            />
            <Stat label="Manifest records" value={formatNumber(status.data.manifest_record_count)} />
            <Stat
              label="Capacity-pressure events"
              value={formatNumber(status.data.capacity_pressure_events)}
            />
            <Stat label="Service uptime" value={formatDurationSecs(status.data.uptime_secs)} />
          </CardGrid>
        </Card>
      )}

      {metrics.data && (
        <>
          <CardGrid>
            <Card title="Read metrics">
              <div className="stack">
                <div className="spread">
                  <span>Requests</span>
                  <span className="text-mono">{formatNumber(metrics.data.read.read_requests)}</span>
                </div>
                <div className="spread">
                  <span>Hits</span>
                  <span className="text-mono">{formatNumber(metrics.data.read.read_hits)}</span>
                </div>
                <div className="spread">
                  <span>Misses</span>
                  <span className="text-mono">{formatNumber(metrics.data.read.read_misses)}</span>
                </div>
                <div className="spread">
                  <span>Bloom negatives</span>
                  <span className="text-mono">{formatNumber(metrics.data.read.bloom_negatives)}</span>
                </div>
                <div className="spread">
                  <span>Blocks read</span>
                  <span className="text-mono">{formatNumber(metrics.data.read.blocks_read)}</span>
                </div>
              </div>
            </Card>
            <Card title="Write metrics">
              <div className="stack">
                <div className="spread">
                  <span>Pool state</span>
                  <span className="text-mono">{metrics.data.write.state}</span>
                </div>
                <div className="spread">
                  <span>Submitted</span>
                  <span className="text-mono">{formatNumber(metrics.data.write.submitted)}</span>
                </div>
                <div className="spread">
                  <span>Completed OK</span>
                  <span className="text-mono">{formatNumber(metrics.data.write.completed_ok)}</span>
                </div>
                <div className="spread">
                  <span>Completed with error</span>
                  <span className="text-mono">{formatNumber(metrics.data.write.completed_err)}</span>
                </div>
                <div className="spread">
                  <span>Queue depth</span>
                  <span className="text-mono">
                    {metrics.data.write.queue_depth} / {metrics.data.write.queue_capacity}
                  </span>
                </div>
              </div>
            </Card>
          </CardGrid>

          <Card title={`API request activity (${metrics.data.service.active_requests} in flight)`}>
            <Table
              columns={routeColumns}
              rows={metrics.data.service.routes}
              rowKey={(r) => r.route}
              caption="Per-route request metrics"
              emptyTitle="No requests recorded yet"
            />
          </Card>
        </>
      )}
    </div>
  );
}

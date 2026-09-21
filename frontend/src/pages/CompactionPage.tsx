import { useCompactionMetricsQuery, useCompactionStatusQuery } from "../api/queries";
import { Card, CardGrid, Stat } from "../components/Card";
import { Badge } from "../components/Badge";
import { Spinner } from "../components/Spinner";
import { ErrorState } from "../components/EmptyState";
import { formatBytes, formatMs, formatNumber } from "../utils/format";

export function CompactionPage() {
  const status = useCompactionStatusQuery();
  const metrics = useCompactionMetricsQuery();

  return (
    <div className="stack">
      <h1 style={{ fontSize: "var(--font-size-2xl)" }}>Compaction</h1>
      <p className="text-muted">
        Read-only: RubiXDB's Compaction runs size-tiered, full-merge, automatically. There is no
        manual "force compaction" control here because the engine does not expose one (its
        automatic trigger is the only certified entry point).
      </p>

      {status.isLoading && <Spinner label="Loading compaction status" />}
      {status.isError && <ErrorState message="Could not load compaction status." />}
      {status.data && (
        <CardGrid>
          <Card title="Automatic trigger">
            <Badge tone={status.data.auto_trigger_enabled ? "healthy" : "neutral"}>
              {status.data.auto_trigger_enabled ? "Enabled" : "Disabled"}
            </Badge>
          </Card>
          <Card title="Trigger threshold">
            <Stat label="Live SSTables" value={status.data.trigger_count} />
          </Card>
          <Card title="Live SSTable count">
            <Stat label="Right now" value={formatNumber(status.data.live_sstable_count)} />
          </Card>
          <Card title="Cycles completed">
            <Stat label="Since connect" value={formatNumber(status.data.cycles_completed)} />
          </Card>
        </CardGrid>
      )}

      {metrics.data && (
        <>
          <Card title="Cumulative totals">
            <CardGrid>
              <Stat label="Input SSTables" value={formatNumber(metrics.data.input_sstables_total)} />
              <Stat label="Input bytes" value={formatBytes(metrics.data.input_bytes_total)} />
              <Stat label="Output bytes" value={formatBytes(metrics.data.output_bytes_total)} />
              <Stat label="Records read" value={formatNumber(metrics.data.records_read_total)} />
              <Stat
                label="Records retained"
                value={formatNumber(metrics.data.records_retained_total)}
              />
              <Stat label="Records dropped" value={formatNumber(metrics.data.records_dropped_total)} />
              <Stat
                label="Tombstones dropped"
                value={formatNumber(metrics.data.tombstones_dropped_total)}
              />
              <Stat label="Max cycle duration" value={formatMs(metrics.data.duration_max_ms)} />
            </CardGrid>
          </Card>

          <Card title="Most recent cycle">
            {metrics.data.last_cycle ? (
              <CardGrid>
                <Stat
                  label="Input tables"
                  value={metrics.data.last_cycle.input_sstable_count}
                />
                <Stat label="Duration" value={formatMs(metrics.data.last_cycle.duration_ms)} />
                <Stat
                  label="Input bytes"
                  value={formatBytes(metrics.data.last_cycle.input_bytes)}
                />
                <Stat
                  label="Output bytes"
                  value={formatBytes(metrics.data.last_cycle.output_bytes)}
                />
                <Stat
                  label="Records dropped"
                  value={formatNumber(metrics.data.last_cycle.records_dropped)}
                />
                <Stat
                  label="Peak temp disk"
                  value={formatBytes(metrics.data.last_cycle.peak_temp_disk_bytes)}
                />
              </CardGrid>
            ) : (
              <p className="text-muted">No compaction cycle has run yet.</p>
            )}
          </Card>
        </>
      )}
    </div>
  );
}

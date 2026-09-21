import { Link } from "react-router-dom";
import { useCompactionStatusQuery, useMetricsQuery, useStatusQuery } from "../api/queries";
import { Card, CardGrid, Stat } from "../components/Card";
import { Badge, storageStateTone } from "../components/Badge";
import { Spinner } from "../components/Spinner";
import { ErrorState } from "../components/EmptyState";
import { formatBytes, formatDurationSecs, formatNumber } from "../utils/format";

export function DashboardPage() {
  const status = useStatusQuery();
  const compaction = useCompactionStatusQuery();
  const metrics = useMetricsQuery();

  return (
    <div className="stack">
      <h1 style={{ fontSize: "var(--font-size-2xl)" }}>Dashboard</h1>

      {status.isLoading && <Spinner label="Loading status" />}
      {status.isError && <ErrorState message="Could not load engine status." />}
      {status.data && (
        <CardGrid>
          <Card title="Storage state">
            <Badge tone={storageStateTone(status.data.storage_state)}>
              {status.data.storage_state}
            </Badge>
          </Card>
          <Card title="Live SSTables">
            <Stat label="Tables" value={formatNumber(status.data.live_sstable_count)} />
          </Card>
          <Card title="Checkpoint seq">
            <Stat label="Sequence" value={formatNumber(status.data.checkpoint_seq)} />
          </Card>
          <Card title="Uptime">
            <Stat label="Since connect" value={formatDurationSecs(status.data.uptime_secs)} />
          </Card>
        </CardGrid>
      )}

      <CardGrid>
        <Card title="Compaction">
          {compaction.data ? (
            <div className="stack">
              <div className="spread">
                <span>Automatic trigger</span>
                <Badge tone={compaction.data.auto_trigger_enabled ? "healthy" : "neutral"}>
                  {compaction.data.auto_trigger_enabled ? "Enabled" : "Disabled"}
                </Badge>
              </div>
              <div className="spread">
                <span>Trigger threshold</span>
                <span className="text-mono">{compaction.data.trigger_count} tables</span>
              </div>
              <div className="spread">
                <span>Cycles completed</span>
                <span className="text-mono">{formatNumber(compaction.data.cycles_completed)}</span>
              </div>
              <Link to="/compaction">View compaction details &rarr;</Link>
            </div>
          ) : (
            <Spinner label="Loading compaction status" />
          )}
        </Card>

        <Card title="Read / write activity">
          {metrics.data ? (
            <div className="stack">
              <div className="spread">
                <span>Read requests</span>
                <span className="text-mono">{formatNumber(metrics.data.read.read_requests)}</span>
              </div>
              <div className="spread">
                <span>Writes submitted</span>
                <span className="text-mono">{formatNumber(metrics.data.write.submitted)}</span>
              </div>
              <div className="spread">
                <span>Manifest size</span>
                <span className="text-mono">
                  {status.data?.manifest_size_bytes != null
                    ? formatBytes(status.data.manifest_size_bytes)
                    : "—"}
                </span>
              </div>
              <Link to="/health">View full metrics &rarr;</Link>
            </div>
          ) : (
            <Spinner label="Loading metrics" />
          )}
        </Card>
      </CardGrid>
    </div>
  );
}

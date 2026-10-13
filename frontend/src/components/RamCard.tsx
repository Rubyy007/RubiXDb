import { useEffect, useState } from "react";
import { useAdminStatusQuery, useStatusQuery } from "../api/queries";
import { useSession } from "../context/SessionContext";
import { Badge } from "./Badge";
import { Sparkline } from "./Sparkline";
import { deriveProcessMemory, deriveRam, ramLevel } from "../utils/ram";
import { formatBytes } from "../utils/format";

const SAMPLES = 30;
const LABELS = { healthy: "Healthy", warning: "Warning", critical: "Critical" } as const;
const TONES = { healthy: "healthy", warning: "pressure", critical: "danger" } as const;

/** Rail card. Two honest sources, in this order:
 *  1. `/v1/status` carrying a machine total (percentage + Healthy/Warning/Critical);
 *  2. admin `/v1/admin/status` `resources.rss_bytes` -- the memory the rubiXDb
 *     process holds right now, in bytes, with a live trend (no percentage: the
 *     API reports no machine total, so none is invented).
 * With neither it says so; it never shows a made-up figure. */
export function RamCard() {
  const status = useStatusQuery();
  const { session } = useSession();
  const isAdmin = session?.role === "admin";
  const admin = useAdminStatusQuery(isAdmin);
  const ram = deriveRam(status.data);
  const rss = deriveProcessMemory(admin.data);
  const [samples, setSamples] = useState<number[]>([]);

  // The figure the trend follows: percent when a total exists, else RSS bytes.
  const tracked = ram ? ram.percent : rss ?? undefined;
  const updatedAt = ram ? status.dataUpdatedAt : admin.dataUpdatedAt;

  useEffect(() => {
    if (tracked === undefined) {
      setSamples([]);
      return;
    }
    setSamples((s) => [...s, tracked].slice(-SAMPLES));
  }, [tracked, updatedAt]);

  const level = ram ? ramLevel(ram.percent) : null;
  const loading = status.isLoading || (isAdmin && admin.isLoading);

  return (
    <div className="ram-card">
      <div className="ram-card-head">
        <span className="ram-card-label">RAM usage</span>
        {ram && level ? (
          <span className="ram-card-value">
            <Badge tone={TONES[level]}>{LABELS[level]}</Badge>
            <span>{Math.round(ram.percent)}%</span>
          </span>
        ) : rss !== null ? (
          <span className="ram-card-value">{formatBytes(rss)}</span>
        ) : (
          <span className="ram-card-value">{loading ? "…" : "—"}</span>
        )}
      </div>
      {ram ? (
        <>
          <Sparkline values={samples} />
          <p className="ram-card-detail">
            {formatBytes(ram.usedBytes)} / {formatBytes(ram.totalBytes)} total
          </p>
        </>
      ) : rss !== null ? (
        <>
          <Sparkline values={samples} autoScale />
          <p className="ram-card-detail">rubiXDb process memory (RSS)</p>
        </>
      ) : (
        <p className="ram-card-detail">
          {loading ? "Loading" : isAdmin ? "Not available" : "Process memory needs an administrator key"}
        </p>
      )}
    </div>
  );
}

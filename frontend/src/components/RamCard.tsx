import { useEffect, useState } from "react";
import { useStatusQuery } from "../api/queries";
import { Badge } from "./Badge";
import { Sparkline } from "./Sparkline";
import { deriveRam, ramLevel } from "../utils/ram";
import { formatBytes } from "../utils/format";

const SAMPLES = 30;
const LABELS = { healthy: "Healthy", warning: "Warning", critical: "Critical" } as const;
const TONES = { healthy: "healthy", warning: "pressure", critical: "danger" } as const;

/** Rail card. Reads the same `/v1/status` query the rest of the console
 * already polls (no new request, no new endpoint). When the server does not
 * report memory it says so; it never shows a made-up figure. */
export function RamCard() {
  const status = useStatusQuery();
  const ram = deriveRam(status.data);
  const [samples, setSamples] = useState<number[]>([]);
  const percent = ram?.percent;
  const updatedAt = status.dataUpdatedAt;

  useEffect(() => {
    if (percent === undefined) {
      setSamples([]);
      return;
    }
    setSamples((s) => [...s, percent].slice(-SAMPLES));
  }, [percent, updatedAt]);

  const level = ram ? ramLevel(ram.percent) : null;

  return (
    <div className="ram-card">
      <div className="ram-card-head">
        <span className="ram-card-label">RAM usage</span>
        {ram && level ? (
          <span className="ram-card-value">
            <Badge tone={TONES[level]}>{LABELS[level]}</Badge>
            <span>{Math.round(ram.percent)}%</span>
          </span>
        ) : (
          <span className="ram-card-value">{status.isLoading ? "…" : "—"}</span>
        )}
      </div>
      {ram ? (
        <>
          <Sparkline values={samples} />
          <p className="ram-card-detail">
            {formatBytes(ram.usedBytes)} / {formatBytes(ram.totalBytes)} total
          </p>
        </>
      ) : (
        <p className="ram-card-detail">{status.isLoading ? "Loading" : "Not available"}</p>
      )}
    </div>
  );
}

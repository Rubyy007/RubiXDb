/** RAM figures for the rail card.
 *
 * Today `/v1/status` and `/v1/metrics` carry NO memory fields (the only
 * RSS figure the API exposes is admin-only `/v1/admin/status`
 * `resources.rss_bytes`, which has no total/capacity, so no percentage can
 * honestly be derived from it). This reader therefore returns `null`
 * ("not available") unless the status body actually carries BOTH
 * `memory_used_bytes` and `memory_total_bytes`. Those two names are
 * provisional -- a future API change must confirm them (OPEN_ITEMS).
 * Nothing is ever defaulted or invented. */

export interface RamUsage {
  usedBytes: number;
  totalBytes: number;
  percent: number;
}

export type RamLevel = "healthy" | "warning" | "critical";

export function deriveRam(status: unknown): RamUsage | null {
  if (typeof status !== "object" || status === null) return null;
  const s = status as Record<string, unknown>;
  const used = s.memory_used_bytes;
  const total = s.memory_total_bytes;
  if (typeof used !== "number" || typeof total !== "number") return null;
  if (!Number.isFinite(used) || !Number.isFinite(total) || total <= 0 || used < 0) return null;
  return { usedBytes: used, totalBytes: total, percent: Math.min(100, (used / total) * 100) };
}

// Presentation thresholds for the pill only (not an engine contract).
export function ramLevel(percent: number): RamLevel {
  if (percent >= 90) return "critical";
  if (percent >= 75) return "warning";
  return "healthy";
}

import { useSyncExternalStore } from "react";
import { clearWorksheets } from "./worksheets";

/** In-memory, per-tab record of what the operator did in this console
 * session. Deliberately NOT persisted anywhere (no sessionStorage, no
 * localStorage): SQL text can contain sensitive literals, and a reload
 * starts a fresh session exactly like the SQL console itself does. It is
 * cleared when the shell unmounts (log out, 401, closing the tab). */

export interface RecentQuery {
  sql: string;
  ranAt: number;
  ok: boolean;
  /** Browser-measured request round trip, when known. */
  ms?: number;
  /** Rows returned or affected, when the statement reported a count. */
  rows?: number | null;
}

const LIMIT = 50;
let queries: RecentQuery[] = [];
const listeners = new Set<() => void>();

function emit() {
  for (const l of listeners) l();
}

export function recordQuery(sql: string, ok: boolean, meta?: { ms?: number; rows?: number | null }): void {
  queries = [{ sql, ranAt: Date.now(), ok, ...meta }, ...queries].slice(0, LIMIT);
  emit();
}

export function clearSessionActivity(): void {
  clearWorksheets();
  queries = [];
  pendingSql = null;
  emit();
}

export function useRecentQueries(): readonly RecentQuery[] {
  return useSyncExternalStore(
    (cb) => {
      listeners.add(cb);
      return () => listeners.delete(cb);
    },
    () => queries,
  );
}

/** Title for a history row: the first non-empty line of the statement,
 * whitespace-collapsed and clipped. Plain text only -- callers render it
 * as a React text node. */
export function queryTitle(sql: string, max = 80): string {
  const line = sql.split(/\r?\n/).find((l) => l.trim().length > 0) ?? "";
  const flat = line.replace(/\s+/g, " ").trim();
  return flat.length > max ? `${flat.slice(0, max - 1)}…` : flat;
}

// --- hand-off of a static template into the SQL console editor ---

const MAX_PENDING = 4000;
let pendingSql: string | null = null;

/** Called by Home with a STATIC snippet from its own source. */
export function setPendingSql(sql: string): void {
  pendingSql = sql.length <= MAX_PENDING ? sql : null;
}

/** Called once by the SQL console when it mounts; consumes the snippet. */
export function takePendingSql(): string {
  const s = pendingSql ?? "";
  pendingSql = null;
  return s;
}

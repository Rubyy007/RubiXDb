import { useMemo } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { ApiClient, type RangeQuery } from "./client";
import { useSession } from "../context/SessionContext";

/** A client bound to the current session, auto-clearing the session on
 * a 401 (PHASE_FRONTEND_ARCHITECTURE.md §5). Returns `null` when not
 * connected -- callers gate their queries on this via `enabled`. */
export function useApiClient(): ApiClient | null {
  const { session, clearSession } = useSession();
  return useMemo(() => {
    if (!session) return null;
    return new ApiClient(session, clearSession);
  }, [session, clearSession]);
}

const REFRESH_INTERVAL_MS = 10_000;

export function useStatusQuery() {
  const client = useApiClient();
  return useQuery({
    queryKey: ["status"],
    queryFn: () => client!.status(),
    enabled: client !== null,
    refetchInterval: REFRESH_INTERVAL_MS,
  });
}

export function useMetadataQuery() {
  const client = useApiClient();
  return useQuery({
    queryKey: ["metadata"],
    queryFn: () => client!.metadata(),
    enabled: client !== null,
  });
}

export function useMetricsQuery() {
  const client = useApiClient();
  return useQuery({
    queryKey: ["metrics"],
    queryFn: () => client!.metrics(),
    enabled: client !== null,
    refetchInterval: REFRESH_INTERVAL_MS,
  });
}

export function useCompactionStatusQuery() {
  const client = useApiClient();
  return useQuery({
    queryKey: ["compaction", "status"],
    queryFn: () => client!.compactionStatus(),
    enabled: client !== null,
    refetchInterval: REFRESH_INTERVAL_MS,
  });
}

export function useCompactionMetricsQuery() {
  const client = useApiClient();
  return useQuery({
    queryKey: ["compaction", "metrics"],
    queryFn: () => client!.compactionMetrics(),
    enabled: client !== null,
    refetchInterval: REFRESH_INTERVAL_MS,
  });
}

export function useSnapshotsQuery() {
  const client = useApiClient();
  return useQuery({
    queryKey: ["snapshots"],
    queryFn: () => client!.listSnapshots(),
    enabled: client !== null,
  });
}

export function useCreateSnapshotMutation() {
  const client = useApiClient();
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: () => client!.createSnapshot(),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["snapshots"] }),
  });
}

export function useReleaseSnapshotMutation() {
  const client = useApiClient();
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => client!.releaseSnapshot(id),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["snapshots"] }),
  });
}

/** Invalidates every cached point-read query so a subsequent lookup
 * refetches rather than showing a stale pre-write value -- a real bug
 * `e2e/workflow.spec.ts` caught (looking up a key right after
 * overwriting it showed the previous value, since react-query does
 * not know a `PUT`/`DELETE` invalidates a `["kv", "get", key, ...]`
 * query it already has cached under an unchanged key). Invalidating
 * the whole `["kv"]` prefix, not just the specific key just written,
 * is deliberately broad but simple and correct for this console's own
 * traffic pattern (a human operator, not a high-frequency client). */
function invalidateKvQueries(queryClient: ReturnType<typeof useQueryClient>) {
  queryClient.invalidateQueries({ queryKey: ["kv"] });
}

export function usePutMutation() {
  const client = useApiClient();
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({ keyB64, valueB64 }: { keyB64: string; valueB64: string }) =>
      client!.put(keyB64, valueB64),
    onSuccess: () => invalidateKvQueries(queryClient),
  });
}

export function useDeleteMutation() {
  const client = useApiClient();
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (keyB64: string) => client!.delete(keyB64),
    onSuccess: () => invalidateKvQueries(queryClient),
  });
}

export function useGetQuery(keyB64: string, asOfSeq: number | undefined, enabled: boolean) {
  const client = useApiClient();
  return useQuery({
    queryKey: ["kv", "get", keyB64, asOfSeq ?? "now"],
    queryFn: () => client!.get(keyB64, asOfSeq),
    enabled: client !== null && enabled && keyB64.length > 0,
    retry: false,
  });
}

export function useExistsQuery(keyB64: string, asOfSeq: number | undefined, enabled: boolean) {
  const client = useApiClient();
  return useQuery({
    queryKey: ["kv", "exists", keyB64, asOfSeq ?? "now"],
    queryFn: () => client!.exists(keyB64, asOfSeq),
    enabled: client !== null && enabled && keyB64.length > 0,
    retry: false,
  });
}

export function useRangeMutation() {
  const client = useApiClient();
  return useMutation({
    mutationFn: (query: RangeQuery) => client!.range(query),
  });
}

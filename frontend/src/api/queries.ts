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

// -----------------------------------------------------------------
// SQL console catalog metadata -- read-only, cacheable exactly like
// every other status/metadata query above. The SQL *execution* call
// itself (`POST /v1/sql`) is deliberately *not* a react-query mutation
// -- it carries stateful session-id tracking and cancellation the
// generic mutation shape does not fit well, so `SqlConsolePage` calls
// `ApiClient.sql` directly instead (`PHASE_RELATIONAL_FRONTEND_SQL_
// ARCHITECTURE.md` §2).
// -----------------------------------------------------------------

export function useDatabasesQuery() {
  const client = useApiClient();
  return useQuery({
    queryKey: ["sql", "databases"],
    queryFn: () => client!.listDatabases(),
    enabled: client !== null,
  });
}

export function useSchemasQuery() {
  const client = useApiClient();
  return useQuery({
    queryKey: ["sql", "schemas"],
    queryFn: () => client!.listSchemas(),
    enabled: client !== null,
  });
}

export function useTablesQuery() {
  const client = useApiClient();
  return useQuery({
    queryKey: ["sql", "tables"],
    queryFn: () => client!.listTables(),
    enabled: client !== null,
  });
}

export function useIndexesQuery() {
  const client = useApiClient();
  return useQuery({
    queryKey: ["sql", "indexes"],
    queryFn: () => client!.listIndexes(),
    enabled: client !== null,
  });
}

/** Invalidates the catalog-metadata queries above -- called after any
 * SQL statement that might have changed the schema (DDL) or after any
 * statement at all, kept simple (this console is a human operator's
 * tool, the same "invalidate broadly, simply" tradeoff `invalidateKv
 * Queries` above already documents). */
export function invalidateCatalogQueries(queryClient: ReturnType<typeof useQueryClient>) {
  queryClient.invalidateQueries({ queryKey: ["sql"] });
}

// -----------------------------------------------------------------
// Delete safety (Increment 14, Blocker 12) -- Data Explorer's Objects
// tab. Every mutation below invalidates the catalog listing queries
// on success, so a stale row can never keep showing after this
// client's own delete succeeds; a *different* client's stale cached
// row is instead caught server-side (`api_delete_safety.rs`'s own
// stale-UI tests) when this client tries to act on it.
// -----------------------------------------------------------------

export function useDeleteSchemaMutation() {
  const client = useApiClient();
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({ schemaId, confirmName }: { schemaId: number; confirmName: string }) =>
      client!.deleteSchema(schemaId, confirmName),
    onSuccess: () => invalidateCatalogQueries(queryClient),
  });
}

export function useDeleteTableMutation() {
  const client = useApiClient();
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({
      tableId,
      schemaName,
      tableName,
    }: { tableId: number; schemaName: string; tableName: string }) =>
      client!.deleteTable(tableId, schemaName, tableName),
    onSuccess: () => invalidateCatalogQueries(queryClient),
  });
}

export function useDeleteIndexMutation() {
  const client = useApiClient();
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({
      indexId,
      schemaName,
      tableName,
      indexName,
    }: { indexId: number; schemaName: string; tableName: string; indexName: string }) =>
      client!.deleteIndex(indexId, schemaName, tableName, indexName),
    onSuccess: () => invalidateCatalogQueries(queryClient),
  });
}

// ---- operator endpoints (admin role) ----

export function useAdminStatusQuery(enabled = true) {
  const client = useApiClient();
  return useQuery({
    queryKey: ["admin", "status"],
    queryFn: () => client!.adminStatus(),
    enabled: client !== null && enabled,
    refetchInterval: 5_000,
    retry: false,
  });
}

export function useBackupsQuery(enabled = true) {
  const client = useApiClient();
  return useQuery({
    queryKey: ["admin", "backups"],
    queryFn: () => client!.adminBackups(),
    enabled: client !== null && enabled,
    retry: false,
  });
}

export function useCreateBackupMutation() {
  const client = useApiClient();
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (name: string) => client!.adminCreateBackup(name),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["admin"] }),
  });
}

export function useVerifyBackupMutation() {
  const client = useApiClient();
  return useMutation({ mutationFn: (name: string) => client!.adminVerifyBackup(name) });
}

export function useDeleteBackupMutation() {
  const client = useApiClient();
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({ name, confirm }: { name: string; confirm: string }) =>
      client!.adminDeleteBackup(name, confirm),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["admin", "backups"] }),
  });
}

export function useCheckMutation() {
  const client = useApiClient();
  return useMutation({ mutationFn: () => client!.adminCheck() });
}

export function usePurgeOrphansMutation() {
  const client = useApiClient();
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({ apply, expected }: { apply: boolean; expected?: number }) =>
      client!.adminPurgeOrphans(apply, expected),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["admin"] }),
  });
}

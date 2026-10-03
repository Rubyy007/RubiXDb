import re

base = r'E:\RubiXDb\frontend\src\api' + '\\'


def rw(path, fn):
    s = open(path, encoding='utf-8', newline='').read()
    nl = '\r\n' if '\r\n' in s else '\n'
    s2 = fn(s, nl)
    open(path, 'w', encoding='utf-8', newline='').write(s2)


TYPES_ADD = '''
// ---------------------------------------------------------------------
// Operator endpoints (/v1/admin/*) -- PHASE_RUBIXDB_PRODUCTION_OPERATIONS_
// ARCHITECTURE.md. Every route requires the admin role.
// ---------------------------------------------------------------------

export interface AdminStatusBody {
  instance: { id: string | null; name: string | null; uptime_secs: number; version: string };
  storage: {
    state: string;
    storage_pressure_events: number;
    capacity_pressure_events: number;
    sstable_count: number;
    checkpoint_seq: number;
    memtable_active_bytes: number;
    memtable_active_entries: number;
    memtable_immutable_count: number;
    memtable_immutable_bytes: number;
    manifest_records: number;
    manifest_bytes: number | null;
    oldest_live_snapshot_seq: number | null;
  };
  recovery: {
    duration_ms: number;
    wal_records_visited: number;
    wal_records_applied: number;
    manifest_edits_replayed: number;
  };
  wal: {
    pool_state: string;
    coordinator_alive: boolean;
    durable_through: number;
    highest_sequence: number;
    pending_waiters: number;
    queue_depth: number;
    queue_capacity: number;
    queued_bytes: number;
    submitted: number;
    completed_ok: number;
    completed_err: number;
    writes_timed_out: number;
    rejected_backpressure: number;
    sync_attempts: number;
    sync_failures: number;
    avg_batch_records: number;
    max_batch_records: number;
    avg_batch_bytes: number;
    avg_batch_processing_ms: number;
    segment_rotations: number;
    poisoned: boolean;
  };
  compaction: {
    auto_trigger_enabled: boolean;
    trigger_count: number;
    live_sstable_count: number;
    cycles_completed: number;
    input_bytes_total: number;
    output_bytes_total: number;
    tombstones_dropped_total: number;
    duration_max_ms: number;
    last_cycle_ms: number | null;
  };
  reads: { requests: number; hits: number; misses: number; bloom_negatives: number; blocks_read: number };
  queries: {
    requests: number;
    success: number;
    errors: number;
    cancellations: number;
    timeouts: number;
    rows_returned: number;
    rows_affected: number;
    parse_errors: number;
    bind_errors: number;
    authorization_denials: number;
    latency_ms: { p50: number; p95: number; p99: number; max: number } | null;
    active_http_requests: number;
  };
  sessions: { active_transactions: number };
  resources: { rss_bytes: number; threads: number; handles: number; cpu_seconds: number };
  disk: {
    data_dir_bytes: number;
    wal_bytes: number;
    sstable_bytes: number;
    manifest_bytes: number;
    volume_free_bytes: number | null;
  };
  backups: {
    configured: boolean;
    running: boolean;
    ok_total: number;
    failed_total: number;
    last: Record<string, unknown> | null;
  };
  background: {
    backup_running: boolean;
    check_running: boolean;
    maintenance_running: boolean;
    last_check: Record<string, unknown> | null;
  };
}

export interface BackupEntry {
  name: string;
  bytes: number;
  modified_unix_ms: number | null;
}

export interface BackupCreated {
  name: string;
  backup_id: string;
  snapshot_seq: number;
  entries: number;
  chunks: number;
  file_bytes: number;
  content_digest: string;
  duration_ms: number;
}

export interface BackupVerified {
  name: string;
  ok: boolean;
  format_version: number;
  backup_id: string;
  snapshot_seq: number;
  entries: number;
  chunks: number;
  file_bytes: number;
  catalog: { tables: number; indexes: number; schemas: number };
  orphan_table_entries: number;
  duration_ms: number;
}

export interface CheckFinding {
  severity: "info" | "warning" | "error";
  code: string;
  object: string;
  detail: string;
}

export interface CheckResult {
  complete: boolean;
  clean: boolean;
  errors: number;
  warnings: number;
  findings: CheckFinding[];
  stats: { rows_checked: number; index_entries_checked: number; duration_ms: number };
}

export interface PurgeResult {
  applied: boolean;
  class: "INSPECTION" | "DESTRUCTIVE";
  plan: { entries: number; orphan_table_ids: number[] };
  deleted: number;
  remaining: number;
}
'''

CLIENT_TAIL = '''
  // ---- operator endpoints (admin role) ----

  adminStatus() {
    return this.request<AdminStatusBody>("GET", "/v1/admin/status");
  }

  adminBackups() {
    return this.request<{ backups: BackupEntry[] }>("GET", "/v1/admin/backups");
  }

  adminCreateBackup(name: string) {
    return this.request<BackupCreated>("POST", "/v1/admin/backups", { name });
  }

  adminVerifyBackup(name: string) {
    return this.request<BackupVerified>(
      "POST",
      `/v1/admin/backups/${encodeURIComponent(name)}/verify`,
    );
  }

  /** DESTRUCTIVE. The server requires `confirm` to equal the backup's exact name. */
  adminDeleteBackup(name: string, confirm: string) {
    return this.request<{ deleted: string }>(
      "DELETE",
      `/v1/admin/backups/${encodeURIComponent(name)}?confirm=${encodeURIComponent(confirm)}`,
    );
  }

  adminCheck() {
    return this.request<CheckResult>("POST", "/v1/admin/check");
  }

  /** Dry run unless `apply`; `apply` needs the entry count the dry run reported. */
  adminPurgeOrphans(apply: boolean, expectedEntries?: number) {
    return this.request<PurgeResult>("POST", "/v1/admin/maintenance/purge-orphans", {
      apply,
      expected_entries: expectedEntries ?? null,
    });
  }
}
'''

QUERIES_ADD = '''
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
'''


def types_fn(s, nl):
    return s + TYPES_ADD.replace('\n', nl)


def client_fn(s, nl):
    s = s.replace(
        "import type {" + nl + "  ApiErrorBody,",
        "import type {" + nl + "  AdminStatusBody," + nl + "  ApiErrorBody," + nl +
        "  BackupCreated," + nl + "  BackupEntry," + nl + "  BackupVerified," + nl +
        "  CheckResult," + nl + "  PurgeResult,", 1)
    idx = s.rstrip().rfind("}")
    return s[:idx].rstrip() + nl + CLIENT_TAIL.replace('\n', nl)


def queries_fn(s, nl):
    return s + QUERIES_ADD.replace('\n', nl)


rw(base + 'types.ts', types_fn)
rw(base + 'client.ts', client_fn)
rw(base + 'queries.ts', queries_fn)
print("patched")

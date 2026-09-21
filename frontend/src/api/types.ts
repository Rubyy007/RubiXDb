// Mirrors the JSON shapes rubixdb-api actually returns
// (PHASE_API_ARCHITECTURE.md §2/§3). Field names match exactly.
//
// Note on `seq`/count fields typed `number`: the backend serializes
// Rust `u64` as a JSON number. Values beyond 2^53 would lose
// precision -- not a realistic concern for the sequence/byte counts
// this console displays in practice, but stated here rather than
// silently assumed safe.

export type Role = "admin" | "reader";

export interface WhoAmIBody {
  principal_name: string;
  role: Role;
}

export interface ReadyBody {
  ready: boolean;
  storage_state: string;
}

export interface StatusBody {
  storage_state: string;
  storage_pressure_events: number;
  sstable_count: number;
  checkpoint_seq: number;
  manifest_record_count: number;
  manifest_size_bytes: number | null;
  live_sstable_count: number;
  capacity_pressure_events: number;
  uptime_secs: number;
}

export interface MetadataBody {
  keyspace_model: string;
  data_dir: string;
  memtable_max_size_bytes: number;
  max_immutable_memtables: number;
  sstable_target_block_size: number;
  bloom_bits_per_key: number;
  compaction_trigger_count: number;
  compaction_auto_trigger: boolean;
}

export interface SeqResponse {
  seq: number;
}

export interface GetResponse {
  key_b64: string;
  value_b64: string;
  seq_queried: number;
}

export interface ExistsResponse {
  exists: boolean;
  seq_queried: number;
}

export interface RangeRow {
  key_b64: string;
  value_b64: string;
}

export interface RangeResponse {
  rows: RangeRow[];
  truncated: boolean;
  seq_queried: number | null;
}

export interface SnapshotBody {
  id: string;
  seq: number;
  created_at_unix_secs: number;
}

export interface CompactionStatusBody {
  auto_trigger_enabled: boolean;
  trigger_count: number;
  live_sstable_count: number;
  cycles_completed: number;
}

export interface CompactionCycleBody {
  input_sstable_count: number;
  output_sstable_count: number;
  input_bytes: number;
  output_bytes: number;
  records_read: number;
  records_retained: number;
  records_dropped: number;
  tombstones_dropped: number;
  versions_dropped: number;
  duration_ms: number;
  peak_temp_disk_bytes: number;
}

export interface CompactionMetricsBody {
  cycles_completed: number;
  input_sstables_total: number;
  input_bytes_total: number;
  output_bytes_total: number;
  records_read_total: number;
  records_retained_total: number;
  records_dropped_total: number;
  tombstones_dropped_total: number;
  versions_dropped_total: number;
  duration_total_ms: number;
  duration_max_ms: number;
  peak_temp_disk_bytes_max: number;
  last_cycle: CompactionCycleBody | null;
}

export interface ReadMetricsBody {
  read_requests: number;
  read_hits: number;
  read_misses: number;
  bloom_negatives: number;
  blocks_read: number;
  sstables_consulted: number;
}

export interface WriteMetricsBody {
  state: string;
  submitted: number;
  completed_ok: number;
  completed_err: number;
  rejected_backpressure: number;
  queue_depth: number;
  queue_capacity: number;
}

export interface RouteMetricsSnapshot {
  route: string;
  count: number;
  error_count: number;
  p50_ms: number;
  p95_ms: number;
  p99_ms: number;
}

export interface ServiceMetricsBody {
  active_requests: number;
  uptime_secs: number;
  routes: RouteMetricsSnapshot[];
}

export interface MetricsBody {
  storage_state: string;
  sstable_count: number;
  compaction_cycles_completed: number;
  read: ReadMetricsBody;
  write: WriteMetricsBody;
  service: ServiceMetricsBody;
}

export interface ApiErrorBody {
  error: {
    code: string;
    message: string;
    detail?: string;
  };
}

/** Thrown by the API client for any non-2xx response. */
export class ApiRequestError extends Error {
  status: number;
  code: string;
  detail?: string;

  constructor(status: number, body: ApiErrorBody) {
    super(body.error.message);
    this.status = status;
    this.code = body.error.code;
    this.detail = body.error.detail;
  }
}

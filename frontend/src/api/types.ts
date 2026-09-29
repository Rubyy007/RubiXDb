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

// -----------------------------------------------------------------
// SQL console (`POST /v1/sql`, `/v1/catalog/*`) --
// `PHASE_RELATIONAL_FRONTEND_SQL_ARCHITECTURE.md`. Mirrors the API's
// own wire contract exactly (`api/src/routes/sql.rs`,
// `api/src/sql_params.rs`) -- typed values are kept intact end to end
// (item 96), never collapsed to a plain string before this layer.
// -----------------------------------------------------------------

/** One typed SQL value, on the wire -- the exact tagged shape
 * `api/src/sql_params.rs::SqlValueJson`/`SqlParam` serializes. 64-bit-
 * or-wider numeric types travel as strings to avoid `JSON.parse`'s
 * silent `Number` precision loss beyond 2^53 (that module's own doc
 * comment has the full reasoning) -- this type follows the same rule
 * on the way back into a request. */
export type SqlValue =
  | { type: "null" }
  | { type: "boolean"; value: boolean }
  | { type: "integer"; value: number }
  | { type: "bigint"; value: string }
  | { type: "real"; value: number }
  | { type: "double"; value: number }
  | { type: "decimal"; unscaled: string; scale: number }
  | { type: "text"; value: string }
  | { type: "blob"; value_b64: string }
  | { type: "date"; value: string }
  | { type: "time"; value: string }
  | { type: "timestamp"; value: string };

export interface SqlColumnMeta {
  name: string;
  type: string | null;
  nullable: boolean;
}

export type SqlResultBody =
  | { kind: "rows"; columns: SqlColumnMeta[]; rows: SqlValue[][]; row_count: number }
  | { kind: "write"; statement: string; rows_affected: number }
  | { kind: "ddl" }
  | { kind: "explain"; plan_text: string }
  | { kind: "begin" }
  | { kind: "commit" }
  | { kind: "rollback" };

export interface SqlResponseBody {
  session_id: string | null;
  result: SqlResultBody;
}

export interface SqlRequestBody {
  sql: string;
  params?: SqlValue[];
  session_id?: string | null;
}

export interface DatabaseInfo {
  database_id: number;
  name: string;
}

export interface SchemaInfo {
  schema_id: number;
  database_id: number;
  name: string;
}

export interface TableInfo {
  table_id: number;
  schema_id: number;
  name: string;
}

export interface ColumnInfo {
  ordinal: number;
  name: string;
  data_type: string;
  nullable: boolean;
  primary_key: boolean;
}

export interface TableDescription {
  table_id: number;
  name: string;
  columns: ColumnInfo[];
}

export interface IndexInfo {
  index_id: number;
  table_id: number;
  table_name: string | null;
  name: string;
  kind: string;
  unique: boolean;
  state: string;
  columns: string[];
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

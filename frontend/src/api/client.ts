import type {
  ApiErrorBody,
  CompactionMetricsBody,
  CompactionStatusBody,
  ExistsResponse,
  GetResponse,
  MetadataBody,
  MetricsBody,
  RangeResponse,
  ReadyBody,
  SeqResponse,
  SnapshotBody,
  StatusBody,
  WhoAmIBody,
} from "./types";
import { ApiRequestError } from "./types";
import type { Session } from "../context/SessionContext";

export interface RangeQuery {
  startB64?: string;
  endB64?: string;
  startInclusive?: boolean;
  endInclusive?: boolean;
  asOfSeq?: number;
  limit?: number;
}

/** Thin, typed wrapper over fetch -- PHASE_API_ARCHITECTURE.md §2/§3's
 * contract, nothing more. `onUnauthorized` centralizes the 401 ->
 * clear-session-and-redirect behavior (PHASE_FRONTEND_ARCHITECTURE.md
 * §5) instead of duplicating it in every call site. */
export class ApiClient {
  constructor(
    private readonly session: Session,
    private readonly onUnauthorized: () => void,
  ) {}

  private async request<T>(
    method: string,
    path: string,
    body?: unknown,
  ): Promise<T> {
    const response = await fetch(`${this.session.baseUrl}${path}`, {
      method,
      headers: {
        Authorization: `Bearer ${this.session.apiKey}`,
        ...(body !== undefined ? { "Content-Type": "application/json" } : {}),
      },
      body: body !== undefined ? JSON.stringify(body) : undefined,
    });

    if (response.status === 401) {
      this.onUnauthorized();
    }

    if (!response.ok) {
      let errorBody: ApiErrorBody;
      try {
        errorBody = (await response.json()) as ApiErrorBody;
      } catch {
        errorBody = { error: { code: "UNKNOWN", message: response.statusText } };
      }
      throw new ApiRequestError(response.status, errorBody);
    }

    if (response.status === 204) {
      return undefined as T;
    }
    return (await response.json()) as T;
  }

  whoami() {
    return this.request<WhoAmIBody>("GET", "/v1/whoami");
  }

  ready() {
    return this.request<ReadyBody>("GET", "/readyz");
  }

  status() {
    return this.request<StatusBody>("GET", "/v1/status");
  }

  metadata() {
    return this.request<MetadataBody>("GET", "/v1/metadata");
  }

  metrics() {
    return this.request<MetricsBody>("GET", "/v1/metrics");
  }

  compactionStatus() {
    return this.request<CompactionStatusBody>("GET", "/v1/compaction/status");
  }

  compactionMetrics() {
    return this.request<CompactionMetricsBody>("GET", "/v1/compaction/metrics");
  }

  put(keyB64: string, valueB64: string) {
    return this.request<SeqResponse>("PUT", "/v1/kv", {
      key_b64: keyB64,
      value_b64: valueB64,
    });
  }

  delete(keyB64: string) {
    return this.request<SeqResponse>("DELETE", `/v1/kv/${encodeURIComponent(keyB64)}`);
  }

  get(keyB64: string, asOfSeq?: number) {
    const q = asOfSeq !== undefined ? `?as_of_seq=${asOfSeq}` : "";
    return this.request<GetResponse>("GET", `/v1/kv/${encodeURIComponent(keyB64)}${q}`);
  }

  exists(keyB64: string, asOfSeq?: number) {
    const q = asOfSeq !== undefined ? `?as_of_seq=${asOfSeq}` : "";
    return this.request<ExistsResponse>(
      "GET",
      `/v1/kv/${encodeURIComponent(keyB64)}/exists${q}`,
    );
  }

  range(query: RangeQuery) {
    const params = new URLSearchParams();
    if (query.startB64) params.set("start_b64", query.startB64);
    if (query.endB64) params.set("end_b64", query.endB64);
    if (query.startInclusive !== undefined) {
      params.set("start_inclusive", String(query.startInclusive));
    }
    if (query.endInclusive !== undefined) {
      params.set("end_inclusive", String(query.endInclusive));
    }
    if (query.asOfSeq !== undefined) params.set("as_of_seq", String(query.asOfSeq));
    if (query.limit !== undefined) params.set("limit", String(query.limit));
    const qs = params.toString();
    return this.request<RangeResponse>("GET", `/v1/range${qs ? `?${qs}` : ""}`);
  }

  createSnapshot() {
    return this.request<SnapshotBody>("POST", "/v1/snapshots");
  }

  listSnapshots() {
    return this.request<SnapshotBody[]>("GET", "/v1/snapshots");
  }

  getSnapshot(id: string) {
    return this.request<SnapshotBody>("GET", `/v1/snapshots/${encodeURIComponent(id)}`);
  }

  releaseSnapshot(id: string) {
    return this.request<void>("DELETE", `/v1/snapshots/${encodeURIComponent(id)}`);
  }
}

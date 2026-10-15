import type { SqlRequestBody, SqlResponseBody } from "../api/types";
import { ApiRequestError } from "../api/types";
import { splitStatements, type SqlStatement } from "./sqlSplit";

/** Runs the IDE's cells against `POST /v1/sql`.
 *
 * - Autocommit cells are stateless requests. Several can be in flight at once; up to
 *   `maxConcurrent` run and the rest wait in this browser's own queue.
 * - Cells that belong to a transaction (an explicit container, or the worksheet's implicit one when
 *   autocommit is off) carry a server `session_id` and run one at a time per session.
 * - Cancel aborts the HTTP request; the API cancels the statement when the connection drops.
 *
 * No SQL is parsed here and nothing is retried. Sessions are held only in memory: a reload starts
 * without any, exactly like a fresh CLI process. */

export type RunStatus = "idle" | "queued" | "running" | "success" | "error" | "cancelled";

export interface RunError {
  code: string;
  message: string;
  detail?: string;
  /** The server's "what to do next" line for this error, when it sent one. */
  hint?: string;
  /** HTTP status when the API answered; absent for a client-side failure. */
  http?: number;
  /** Which statement of a multi-statement cell failed (set by the runner). */
  statement?: { index: number; count: number; text: string; startLine: number; startCol: number };
}

/** The outcome of one statement of a cell. A cell with several `;`-separated statements runs them in
 * order, one request each, and stops at the first failure. */
export interface StmtRun {
  index: number;
  text: string;
  startLine: number;
  startCol: number;
  status: "success" | "error" | "cancelled";
  response?: SqlResponseBody;
  error?: RunError;
  /** Browser-measured round trip of this statement. */
  ms: number;
}

export interface CellRun {
  status: RunStatus;
  /** The statement this state belongs to. */
  sql: string;
  response?: SqlResponseBody;
  error?: RunError;
  startedAt?: number;
  /** Browser-measured round trip. */
  ms?: number;
  /** Set when another cell holds the session this cell wanted. */
  blockedBy?: { sessionId: string; cellId: string };
  /** The session a cell ran in, for the details tab. */
  sessionId?: string | null;
  /** One entry per statement that has finished so far. */
  parts?: StmtRun[];
  /** Number of statements in the cell text when it was run. */
  statementCount?: number;
  /** Index of the statement being executed while status is running. */
  current?: number;
}

export interface SessionInfo {
  sessionId: string;
  startedAt: number;
  /** Cell currently running on this session, if any. */
  busyCell: string | null;
}

export interface InFlight {
  cellId: string;
  sheetId: string;
  /** The statement being executed right now (exactly as sent), not the whole cell. */
  sql: string;
  startedAt: number;
  sessionKey: string | null;
  /** Position of that statement in the cell (0-based) and how many the cell has. */
  index: number;
  count: number;
}

export interface HistoryEntry {
  /** Increasing sequence number, unique within this tab. */
  seq: number;
  /** Position of the statement in its cell (1-based) and how many the cell had. */
  index: number;
  count: number;
  at: number;
  sheetId: string;
  cellId: string;
  sql: string;
  ok: boolean;
  code: string | null;
  ms: number;
  rows: number | null;
}

export interface RunnerState {
  runs: Record<string, CellRun>;
  sessions: Record<string, SessionInfo>;
  /** Short messages about a session or container (begin/commit failures, adoption). */
  notices: Record<string, string>;
  /** The schema a `USE SCHEMA` selected (sent with every statement); null: the server's default schema. */
  schema: string | null;
  inFlight: InFlight[];
  queued: number;
}

export type Send = (req: SqlRequestBody, signal: AbortSignal) => Promise<SqlResponseBody>;

export interface RunnerOptions {
  send: Send;
  maxConcurrent?: number;
  now?: () => number;
  perf?: () => number;
  onHistory?: (e: HistoryEntry) => void;
  /** The schema to start in (remembered by the page between visits). */
  initialSchema?: string | null;
  /** Called whenever `USE SCHEMA` (or a reset) changes the schema. */
  onSchemaChange?: (schema: string | null) => void;
}

interface Job {
  cellId: string;
  sheetId: string;
  sql: string;
  resolve: (r: CellRun) => void;
}

export const DEFAULT_MAX_CONCURRENT = 4;

export function implicitSessionKey(sheetId: string): string {
  return `sess-${sheetId}`;
}

export function toRunError(err: unknown): RunError {
  if (err instanceof ApiRequestError) {
    return { code: err.code, message: err.message, detail: err.detail, hint: err.hint, http: err.status };
  }
  if (err && typeof err === "object" && (err as { name?: string }).name === "AbortError") {
    return { code: "CANCELLED", message: "the request was cancelled" };
  }
  return { code: "NETWORK_ERROR", message: err instanceof Error ? err.message : "the request failed" };
}

const EMPTY: RunnerState = { runs: {}, sessions: {}, notices: {}, schema: null, inFlight: [], queued: 0 };

export class Runner {
  private st: RunnerState = EMPTY;
  private readonly listeners = new Set<() => void>();
  private readonly controllers = new Map<string, AbortController>();
  private readonly queue: Job[] = [];
  private activeAuto = 0;
  private seq = 0;
  private readonly send: Send;
  private readonly max: number;
  private readonly now: () => number;
  private readonly perf: () => number;
  private readonly onHistory?: (e: HistoryEntry) => void;
  private readonly onSchemaChange?: (schema: string | null) => void;

  constructor(opts: RunnerOptions) {
    if (opts.initialSchema) this.st = { ...EMPTY, schema: opts.initialSchema };
    this.onSchemaChange = opts.onSchemaChange;
    this.send = opts.send;
    this.max = Math.max(1, opts.maxConcurrent ?? DEFAULT_MAX_CONCURRENT);
    this.now = opts.now ?? (() => Date.now());
    this.perf = opts.perf ?? (() => performance.now());
    this.onHistory = opts.onHistory;
  }

  subscribe = (l: () => void): (() => void) => {
    this.listeners.add(l);
    return () => this.listeners.delete(l);
  };

  getState = (): RunnerState => this.st;

  private set(patch: Partial<RunnerState>): void {
    this.st = { ...this.st, ...patch };
    for (const l of this.listeners) l();
  }

  private setRun(cellId: string, run: CellRun): void {
    this.set({ runs: { ...this.st.runs, [cellId]: run } });
  }

  /** Choose the schema unqualified names resolve in (`null`: back to the server's default). */
  setSchema(schema: string | null): void {
    if (this.st.schema === schema) return;
    this.set({ schema });
    this.onSchemaChange?.(schema);
  }

  /** A request body: the schema is part of it only when one was chosen, so requests stay as small as before. */
  private request(sql: string, sessionId: string | null): SqlRequestBody {
    const req: SqlRequestBody = sessionId ? { sql, session_id: sessionId } : { sql };
    if (this.st.schema) req.schema = this.st.schema;
    return req;
  }

  private setNotice(key: string, text: string | null): void {
    const notices = { ...this.st.notices };
    if (text === null) delete notices[key];
    else notices[key] = text;
    this.set({ notices });
  }

  private setSession(key: string, info: SessionInfo | null): void {
    const sessions = { ...this.st.sessions };
    if (info === null) delete sessions[key];
    else sessions[key] = info;
    this.set({ sessions });
  }

  isActive(cellId: string): boolean {
    const s = this.st.runs[cellId]?.status;
    return s === "queued" || s === "running";
  }

  // ---- autocommit ----------------------------------------------------------------------------

  runAutocommit(req: { cellId: string; sheetId: string; sql: string }): Promise<CellRun | null> {
    if (splitStatements(req.sql).length === 0 || this.isActive(req.cellId)) return Promise.resolve(null);
    return new Promise((resolve) => {
      this.queue.push({ ...req, resolve });
      this.setRun(req.cellId, { status: "queued", sql: req.sql });
      this.pump();
    });
  }

  private pump(): void {
    while (this.activeAuto < this.max && this.queue.length > 0) {
      const job = this.queue.shift() as Job;
      this.activeAuto++;
      void this.execute(job, null).then((run) => {
        this.activeAuto--;
        job.resolve(run);
        this.pump();
      });
    }
    if (this.st.queued !== this.queue.length) this.set({ queued: this.queue.length });
  }

  // ---- transactions --------------------------------------------------------------------------

  /** Opens a transaction for `key` (a container id, or `implicitSessionKey`). One open transaction
   * per tab: a second `BEGIN` is refused here with a clear message. */
  async begin(key: string, sheetId: string): Promise<{ ok: boolean }> {
    if (this.st.sessions[key]) return { ok: true };
    const other = Object.values(this.st.sessions)[0];
    if (other) {
      this.setNotice(key, `Another transaction is open (session ${other.sessionId}). Commit or roll it back first.`);
      return { ok: false };
    }
    this.setNotice(key, null);
    const res = await this.control(key, sheetId, "BEGIN", null);
    if (res && res.session_id) {
      this.setSession(key, { sessionId: res.session_id, startedAt: this.now(), busyCell: null });
      return { ok: true };
    }
    if (res) this.setNotice(key, "BEGIN did not return a session.");
    return { ok: false };
  }

  async finish(key: string, sheetId: string, verb: "COMMIT" | "ROLLBACK"): Promise<{ ok: boolean }> {
    const s = this.st.sessions[key];
    if (!s) return { ok: false };
    if (s.busyCell) {
      this.setNotice(key, `Wait for the running cell before ${verb === "COMMIT" ? "committing" : "rolling back"}.`);
      return { ok: false };
    }
    this.setNotice(key, null);
    const res = await this.control(key, sheetId, verb, s.sessionId);
    if (res) {
      this.setSession(key, null);
      return { ok: true };
    }
    return { ok: false };
  }

  /** BEGIN / COMMIT / ROLLBACK. Returns the response, or null after recording the failure notice. */
  private async control(key: string, sheetId: string, sql: string, sessionId: string | null): Promise<SqlResponseBody | null> {
    const controller = new AbortController();
    const t0 = this.perf();
    try {
      const res = await this.send(this.request(sql, sessionId), controller.signal);
      this.history(sheetId, key, sql, true, null, this.perf() - t0, null, 1, 1);
      return res;
    } catch (err) {
      const e = toRunError(err);
      this.history(sheetId, key, sql, false, e.code, this.perf() - t0, null, 1, 1);
      if (e.code === "SESSION_NOT_FOUND") this.setSession(key, null);
      this.setNotice(key, `${e.code}: ${e.message}${e.detail ? ` (${e.detail})` : ""}${e.hint ? ` — ${e.hint}` : ""}`);
      return null;
    }
  }

  async runInSession(req: {
    cellId: string;
    sheetId: string;
    sql: string;
    sessionKey: string;
  }): Promise<{ outcome: "done" | "blocked" | "no-session" | "ignored"; run?: CellRun }> {
    const s = this.st.sessions[req.sessionKey];
    if (!s) return { outcome: "no-session" };
    if (splitStatements(req.sql).length === 0 || this.isActive(req.cellId)) return { outcome: "ignored" };
    if (s.busyCell && s.busyCell !== req.cellId) {
      const prev = this.st.runs[req.cellId];
      this.setRun(req.cellId, {
        status: prev?.status ?? "idle",
        sql: prev?.sql ?? req.sql,
        response: prev?.response,
        error: prev?.error,
        ms: prev?.ms,
        blockedBy: { sessionId: s.sessionId, cellId: s.busyCell },
      });
      return { outcome: "blocked" };
    }
    this.setSession(req.sessionKey, { ...s, busyCell: req.cellId });
    const run = await this.execute({ cellId: req.cellId, sheetId: req.sheetId, sql: req.sql, resolve: () => {} }, req.sessionKey);
    return { outcome: "done", run };
  }

  /** Runs the cells in order on one session; stops at the first failure. */
  async runSequence(
    items: { cellId: string; sheetId: string; sql: string }[],
    sessionKey: string,
  ): Promise<void> {
    for (const it of items) {
      if (splitStatements(it.sql).length === 0) continue;
      const r = await this.runInSession({ ...it, sessionKey });
      if (r.outcome !== "done" || r.run?.status !== "success") return;
    }
  }

  // ---- execution -----------------------------------------------------------------------------

  private async execute(job: Job, sessionKey: string | null): Promise<CellRun> {
    const controller = new AbortController();
    this.controllers.set(job.cellId, controller);
    const session = sessionKey ? this.st.sessions[sessionKey] : undefined;
    const sessionId = session ? session.sessionId : null;
    const statements = splitStatements(job.sql);
    const startedAt = this.now();
    const t0 = this.perf();
    const parts: StmtRun[] = [];
    // A script such as BEGIN; ...; COMMIT; carries the session from one statement to the next.
    let sid: string | null = sessionId;
    let failure: RunError | undefined;
    let cancelled = false;

    for (let i = 0; i < statements.length; i++) {
      if (controller.signal.aborted) {
        cancelled = true;
        break;
      }
      const stmt = statements[i];
      this.markCurrent(job, sessionKey, stmt, i, statements.length, startedAt, sessionId, parts);
      const t1 = this.perf();
      try {
        const response = await this.send(this.request(stmt.text, sid), controller.signal);
        const ms = this.perf() - t1;
        sid = response.session_id;
        if (response.result.kind === "use_schema") this.setSchema(response.result.schema);
        parts.push({ index: i, text: stmt.text, startLine: stmt.startLine, startCol: stmt.startCol, status: "success", response, ms });
        const r = response.result;
        const rows = r.kind === "rows" ? r.row_count : r.kind === "write" ? r.rows_affected : null;
        this.history(job.sheetId, job.cellId, stmt.text, true, null, ms, rows, i + 1, statements.length);
      } catch (err) {
        const ms = this.perf() - t1;
        const aborted = controller.signal.aborted;
        const base = aborted ? { code: "CANCELLED", message: "the request was cancelled" } : toRunError(err);
        const error: RunError = {
          ...base,
          statement: { index: i, count: statements.length, text: stmt.text, startLine: stmt.startLine, startCol: stmt.startCol },
        };
        parts.push({ index: i, text: stmt.text, startLine: stmt.startLine, startCol: stmt.startCol, status: aborted ? "cancelled" : "error", error, ms });
        this.history(job.sheetId, job.cellId, stmt.text, false, error.code, ms, null, i + 1, statements.length);
        if (aborted) cancelled = true;
        else failure = error;
        if (sessionKey && error.code === "SESSION_NOT_FOUND") {
          this.setSession(sessionKey, null);
          this.setNotice(sessionKey, "The server no longer knows this session (it expired or ended).");
          sid = null;
        }
        break;
      }
    }
    this.controllers.delete(job.cellId);

    const last = parts[parts.length - 1];
    const run: CellRun = {
      status: cancelled ? "cancelled" : failure ? "error" : "success",
      sql: job.sql,
      response: !cancelled && !failure ? last?.response : undefined,
      error: cancelled || failure ? last?.error : undefined,
      startedAt,
      ms: this.perf() - t0,
      sessionId: sid ?? sessionId,
      parts,
      statementCount: statements.length,
    };
    this.afterRun(job, sessionKey, sid);
    this.releaseCell(job.cellId, sessionKey);
    this.setRun(job.cellId, run);
    return run;
  }

  private markCurrent(
    job: Job,
    sessionKey: string | null,
    stmt: SqlStatement,
    index: number,
    count: number,
    startedAt: number,
    sessionId: string | null,
    parts: StmtRun[],
  ): void {
    const entry: InFlight = { cellId: job.cellId, sheetId: job.sheetId, sql: stmt.text, startedAt, sessionKey, index, count };
    this.set({ inFlight: [...this.st.inFlight.filter((f) => f.cellId !== job.cellId), entry] });
    this.setRun(job.cellId, {
      status: "running",
      sql: job.sql,
      startedAt,
      sessionId,
      parts: parts.slice(),
      statementCount: count,
      current: index,
    });
  }

  /** What the session looks like once a cell's statements are done. */
  private afterRun(job: Job, sessionKey: string | null, sid: string | null): void {
    if (sessionKey) {
      // A COMMIT or ROLLBACK in the cell text ended the session on the server.
      if (sid === null && this.st.sessions[sessionKey]) {
        this.setSession(sessionKey, null);
        this.setNotice(sessionKey, "The cell's COMMIT or ROLLBACK ended the session.");
      }
      return;
    }
    if (sid) {
      // A BEGIN in an autocommit cell opened a session. Adopt it when this tab has none.
      const key = implicitSessionKey(job.sheetId);
      if (Object.keys(this.st.sessions).length === 0) {
        this.setSession(key, { sessionId: sid, startedAt: this.now(), busyCell: null });
        this.setNotice(key, "A transaction opened by a cell is now this worksheet's open transaction.");
      } else {
        this.setNotice(key, `That statement opened session ${sid}, which this tab cannot manage; it expires on the server's idle timeout.`);
      }
    }
  }

  private releaseCell(cellId: string, sessionKey: string | null): void {
    const inFlight = this.st.inFlight.filter((f) => f.cellId !== cellId);
    const patch: Partial<RunnerState> = { inFlight };
    if (sessionKey) {
      const s = this.st.sessions[sessionKey];
      if (s && s.busyCell === cellId) patch.sessions = { ...this.st.sessions, [sessionKey]: { ...s, busyCell: null } };
    }
    // Cells that were blocked by this one are free to run again.
    const runs: Record<string, CellRun> = {};
    let changed = false;
    for (const [id, r] of Object.entries(this.st.runs)) {
      if (r.blockedBy && r.blockedBy.cellId === cellId) {
        runs[id] = { ...r, blockedBy: undefined };
        changed = true;
      } else {
        runs[id] = r;
      }
    }
    if (changed) patch.runs = runs;
    this.set(patch);
  }

  private history(
    sheetId: string,
    cellId: string,
    sql: string,
    ok: boolean,
    code: string | null,
    ms: number,
    rows: number | null,
    index: number,
    count: number,
  ): void {
    this.onHistory?.({ seq: ++this.seq, index, count, at: this.now(), sheetId, cellId, sql, ok, code, ms, rows });
  }

  // ---- cancel / reset ------------------------------------------------------------------------

  cancel(cellId: string): void {
    const qi = this.queue.findIndex((j) => j.cellId === cellId);
    if (qi >= 0) {
      const [job] = this.queue.splice(qi, 1);
      const run: CellRun = { status: "cancelled", sql: job.sql, error: { code: "CANCELLED", message: "the request was cancelled" } };
      this.setRun(cellId, run);
      this.set({ queued: this.queue.length });
      job.resolve(run);
      return;
    }
    this.controllers.get(cellId)?.abort();
  }

  /** Cancels every running and queued cell. */
  stopAll(): void {
    for (const job of [...this.queue]) this.cancel(job.cellId);
    for (const id of [...this.controllers.keys()]) this.cancel(id);
  }

  /** Clears outputs of cells that are not running (all, or the given cells). */
  reset(cellIds?: string[]): void {
    const keep: Record<string, CellRun> = {};
    for (const [id, r] of Object.entries(this.st.runs)) {
      const target = !cellIds || cellIds.includes(id);
      if (!target || r.status === "running" || r.status === "queued") keep[id] = r;
    }
    this.set({ runs: keep });
  }

  /** Forget a cell that was deleted. */
  forget(cellId: string): void {
    this.cancel(cellId);
    if (!(cellId in this.st.runs)) return;
    const runs = { ...this.st.runs };
    delete runs[cellId];
    this.set({ runs });
  }

  clearNotice(key: string): void {
    if (key in this.st.notices) this.setNotice(key, null);
  }
}

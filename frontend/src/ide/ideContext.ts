import { createContext, useContext } from "react";
import type { CatalogLists } from "./completion";
import type { ColumnInfo } from "./suggest";
import type { CellKind, FlatCell, IdeState, IdeWorksheet } from "./model";
import type { HistoryEntry, RunError, RunnerState } from "./runner";

export interface PlanEntry {
  sql: string;
  status: "loading" | "ready" | "error";
  text?: string;
  error?: RunError;
}

export interface TableRef {
  db: string;
  schema: string;
  table: string;
}

export type RightTab = "sessions" | "running" | "history" | "schema" | "plan";

/** Everything the IDE's components share. Built once per render by `IdePage`. */
export interface IdeApi {
  state: IdeState;
  sheet: IdeWorksheet;
  flat: FlatCell[];
  runs: RunnerState;
  /** Wall clock, refreshed while something is running so elapsed timers tick. */
  now: number;
  focusedId: string | null;
  setFocused: (cellId: string) => void;
  history: HistoryEntry[];
  catalog: CatalogLists;
  /** The schema unqualified names resolve in: the one `USE SCHEMA` chose, else the instance's first schema. */
  activeSchema: string | null;
  /** True once a `USE SCHEMA` chose it (false: it is only the server's default). */
  schemaChosen: boolean;
  catalogStatus: "loading" | "ready" | "error";
  /** Re-read databases, schemas, tables (and drop cached column lists) from the server. */
  refreshCatalog: () => void;
  /** True while the catalog is being re-read. */
  catalogRefreshing: boolean;
  /** When the catalog was last read successfully (ms since epoch), or null. */
  catalogUpdatedAt: number | null;
  /** `table` is bare for a table of the active schema, `schema.table` for any other. */
  fetchColumns: (table: string) => Promise<string[]>;
  /** Columns with their types (cached); an empty list when the table cannot be described. */
  fetchColumnInfo: (table: string) => Promise<ColumnInfo[]>;
  /** Turn keywords typed in lower case into UPPER CASE as each word is finished. */
  autoUpper: boolean;
  setAutoUpper: (on: boolean) => void;
  plans: Record<string, PlanEntry>;
  requestPlan: (cellId: string) => void;
  selectedTable: string | null;
  setSelectedTable: (name: string) => void;

  setText: (cellId: string, text: string) => void;
  /** Remember where the caret is in a cell's editor, so Run can pick the statement under it. */
  setCursor: (cellId: string, offset: number, text: string) => void;
  setNotes: (text: string) => void;
  runCell: (cellId: string) => void;
  runAll: () => void;
  runAbove: (cellId: string) => void;
  cancel: (cellId: string) => void;
  stopAll: () => void;
  beginTxn: (txnId: string) => void;
  finishTxn: (key: string, verb: "COMMIT" | "ROLLBACK") => void;
  runSteps: (txnId: string) => void;
  addAfter: (afterId: string | null, kind: CellKind) => void;
  addTxnAfter: (afterId: string | null) => void;
  deleteBlock: (id: string) => void;
  move: (id: string, dir: -1 | 1) => void;
  duplicate: (cellId: string) => void;
  setKind: (cellId: string, kind: CellKind) => void;
  toggleCollapse: (cellId: string) => void;
  jump: (cellId: string, line: number, col: number) => void;
  focusHeader: (cellId: string, dir: -1 | 1) => void;

  openSheet: (id: string) => void;
  newSheet: () => void;
  deleteSheet: (id: string) => void;
  /** Status dot and unsaved-run marker for a worksheet in the list. */
  sheetBadge: (sheetId: string) => { dot: "idle" | "running" | "error"; dirty: boolean };
  tableAction: (kind: "insert" | "preview" | "copy", t: TableRef) => void;

  autocommit: boolean;
  setAutocommit: (on: boolean) => void;
  rightTab: RightTab;
  setRightTab: (t: RightTab) => void;
  rightOpen: boolean;
  setRightOpen: (open: boolean) => void;
  loadHistorySql: (sql: string) => void;
}

export const IdeContext = createContext<IdeApi | null>(null);

export function useIde(): IdeApi {
  const v = useContext(IdeContext);
  if (!v) throw new Error("useIde must be used inside the IDE page");
  return v;
}

/** `ed-<cellId>`: DOM id of a cell's editor textarea. */
export function editorId(cellId: string): string {
  return `ed-${cellId}`;
}

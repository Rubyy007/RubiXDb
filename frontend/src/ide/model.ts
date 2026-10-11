/** Worksheet document model for the SQL IDE: ordered blocks of SQL cells, markdown cells and
 * transaction containers (which hold SQL cells that share one server session). Client-side only,
 * kept in sessionStorage so a reload restores the text. Query text, never results or credentials.
 * Everything read back from storage is untrusted and re-validated. */

export type CellKind = "sql" | "md";

export interface IdeCell {
  id: string;
  kind: CellKind;
  text: string;
  collapsed: boolean;
}

export interface TxnBlock {
  id: string;
  kind: "txn";
  /** SQL cells only. */
  cells: IdeCell[];
}

export type Block = IdeCell | TxnBlock;

export interface IdeWorksheet {
  id: string;
  name: string;
  blocks: Block[];
  /** The always-present markdown scratchpad at the end of the worksheet. */
  notes: string;
  createdAt: number;
  updatedAt: number;
  /** Next cell/block id counter for this worksheet. */
  counter: number;
}

export interface IdeState {
  sheets: IdeWorksheet[];
  /** Worksheets open as tabs, in tab order. */
  openIds: string[];
  activeId: string;
  /** Next worksheet id counter. */
  counter: number;
  autocommit: boolean;
}

const KEY = "rubixdb-ide-worksheets";
export const MAX_WORKSHEETS = 20;
export const MAX_BLOCKS = 100;
export const MAX_CELLS_IN_TXN = 50;
const MAX_NAME = 60;
const MAX_TEXT = 1_048_576;
const MAX_NOTES = 65_536;

export function isTxn(b: Block): b is TxnBlock {
  return b.kind === "txn";
}

export function newCell(sheet: IdeWorksheet, kind: CellKind, text = ""): [IdeCell, IdeWorksheet] {
  const counter = sheet.counter + 1;
  return [{ id: `c-${counter}`, kind, text, collapsed: false }, { ...sheet, counter }];
}

export function newSheet(counter: number, now: number, withCell = true): IdeWorksheet {
  const base: IdeWorksheet = {
    id: `w-${counter}`,
    name: `Untitled ${counter}`,
    blocks: [],
    notes: "",
    createdAt: now,
    updatedAt: now,
    counter: 0,
  };
  if (!withCell) return base;
  const [cell, sheet] = newCell(base, "sql");
  return { ...sheet, blocks: [cell] };
}

export function freshState(now = Date.now()): IdeState {
  const sheet = newSheet(1, now);
  return { sheets: [sheet], openIds: [sheet.id], activeId: sheet.id, counter: 1, autocommit: true };
}

// ---- flat view -------------------------------------------------------------------------------

export interface FlatCell {
  cell: IdeCell;
  /** 1-based position among all cells in the worksheet (markdown cells included). */
  num: number;
  /** Container the cell belongs to, or null for a top-level cell. */
  txnId: string | null;
}

export function flatCells(sheet: IdeWorksheet): FlatCell[] {
  const out: FlatCell[] = [];
  let n = 0;
  for (const b of sheet.blocks) {
    if (isTxn(b)) {
      for (const c of b.cells) out.push({ cell: c, num: ++n, txnId: b.id });
    } else {
      out.push({ cell: b, num: ++n, txnId: null });
    }
  }
  return out;
}

export function findCell(sheet: IdeWorksheet, cellId: string): FlatCell | null {
  return flatCells(sheet).find((f) => f.cell.id === cellId) ?? null;
}

export function cellCount(sheet: IdeWorksheet): number {
  return flatCells(sheet).length;
}

// ---- immutable edits ---------------------------------------------------------------------------

function touch(sheet: IdeWorksheet, now: number): IdeWorksheet {
  return { ...sheet, updatedAt: now };
}

export function setCellText(sheet: IdeWorksheet, cellId: string, text: string, now: number): IdeWorksheet {
  const t = text.slice(0, MAX_TEXT);
  const mapCell = (c: IdeCell) => (c.id === cellId ? { ...c, text: t } : c);
  return touch(
    { ...sheet, blocks: sheet.blocks.map((b) => (isTxn(b) ? { ...b, cells: b.cells.map(mapCell) } : mapCell(b))) },
    now,
  );
}

export function setCellKind(sheet: IdeWorksheet, cellId: string, kind: CellKind, now: number): IdeWorksheet {
  // Cells inside a container are SQL only.
  const mapTop = (b: Block): Block => (!isTxn(b) && b.id === cellId ? { ...b, kind } : b);
  return touch({ ...sheet, blocks: sheet.blocks.map(mapTop) }, now);
}

export function setCollapsed(sheet: IdeWorksheet, cellId: string, collapsed: boolean): IdeWorksheet {
  const mapCell = (c: IdeCell) => (c.id === cellId ? { ...c, collapsed } : c);
  return { ...sheet, blocks: sheet.blocks.map((b) => (isTxn(b) ? { ...b, cells: b.cells.map(mapCell) } : mapCell(b))) };
}

export function setNotes(sheet: IdeWorksheet, notes: string, now: number): IdeWorksheet {
  return touch({ ...sheet, notes: notes.slice(0, MAX_NOTES) }, now);
}

export function renameSheet(sheet: IdeWorksheet, name: string, now: number): IdeWorksheet {
  const n = name.trim().slice(0, MAX_NAME);
  return n ? touch({ ...sheet, name: n }, now) : sheet;
}

/** Insert a new cell after `afterId` (a top-level block id or a cell id inside a container), or at
 * the end when `afterId` is null. Returns the new cell id (null when a limit was hit). */
export function insertCell(
  sheet: IdeWorksheet,
  afterId: string | null,
  kind: CellKind,
  text: string,
  now: number,
): { sheet: IdeWorksheet; cellId: string | null } {
  if (cellCount(sheet) >= MAX_BLOCKS) return { sheet, cellId: null };
  const [cell, s1] = newCell(sheet, kind, text);
  if (afterId === null) return { sheet: touch({ ...s1, blocks: [...s1.blocks, cell] }, now), cellId: cell.id };
  const inTxn = s1.blocks.find((b): b is TxnBlock => isTxn(b) && b.cells.some((c) => c.id === afterId));
  if (inTxn) {
    if (inTxn.cells.length >= MAX_CELLS_IN_TXN) return { sheet, cellId: null };
    const sqlCell: IdeCell = { ...cell, kind: "sql" };
    const blocks = s1.blocks.map((b) => {
      if (b.id !== inTxn.id || !isTxn(b)) return b;
      const i = b.cells.findIndex((c) => c.id === afterId);
      return { ...b, cells: [...b.cells.slice(0, i + 1), sqlCell, ...b.cells.slice(i + 1)] };
    });
    return { sheet: touch({ ...s1, blocks }, now), cellId: sqlCell.id };
  }
  const i = s1.blocks.findIndex((b) => b.id === afterId);
  const at = i < 0 ? s1.blocks.length : i + 1;
  return {
    sheet: touch({ ...s1, blocks: [...s1.blocks.slice(0, at), cell, ...s1.blocks.slice(at)] }, now),
    cellId: cell.id,
  };
}

export function addTxn(sheet: IdeWorksheet, afterId: string | null, now: number): { sheet: IdeWorksheet; txnId: string | null } {
  if (cellCount(sheet) >= MAX_BLOCKS) return { sheet, txnId: null };
  const counter = sheet.counter + 2;
  const child: IdeCell = { id: `c-${counter}`, kind: "sql", text: "", collapsed: false };
  const txn: TxnBlock = { id: `t-${counter - 1}`, kind: "txn", cells: [child] };
  const i = afterId === null ? -1 : sheet.blocks.findIndex((b) => b.id === afterId);
  const at = i < 0 ? sheet.blocks.length : i + 1;
  return {
    sheet: touch({ ...sheet, counter, blocks: [...sheet.blocks.slice(0, at), txn, ...sheet.blocks.slice(at)] }, now),
    txnId: txn.id,
  };
}

export function deleteBlock(sheet: IdeWorksheet, id: string, now: number): IdeWorksheet {
  const blocks: Block[] = [];
  for (const b of sheet.blocks) {
    if (b.id === id) continue;
    if (isTxn(b)) {
      const cells = b.cells.filter((c) => c.id !== id);
      // A container that lost its last cell is removed with it.
      if (cells.length === 0) continue;
      blocks.push({ ...b, cells });
    } else {
      blocks.push(b);
    }
  }
  return touch({ ...sheet, blocks }, now);
}

export function moveBlock(sheet: IdeWorksheet, id: string, dir: -1 | 1, now: number): IdeWorksheet {
  const swap = <T,>(arr: T[], i: number): T[] => {
    const j = i + dir;
    if (j < 0 || j >= arr.length) return arr;
    const next = arr.slice();
    [next[i], next[j]] = [next[j], next[i]];
    return next;
  };
  const top = sheet.blocks.findIndex((b) => b.id === id);
  if (top >= 0) return touch({ ...sheet, blocks: swap(sheet.blocks, top) }, now);
  return touch(
    {
      ...sheet,
      blocks: sheet.blocks.map((b) => {
        if (!isTxn(b)) return b;
        const i = b.cells.findIndex((c) => c.id === id);
        return i < 0 ? b : { ...b, cells: swap(b.cells, i) };
      }),
    },
    now,
  );
}

export function duplicateCell(sheet: IdeWorksheet, cellId: string, now: number): { sheet: IdeWorksheet; cellId: string | null } {
  const f = findCell(sheet, cellId);
  if (!f) return { sheet, cellId: null };
  return insertCell(sheet, cellId, f.cell.kind, f.cell.text, now);
}

// ---- state-level edits ---------------------------------------------------------------------------

export function updateSheet(state: IdeState, id: string, fn: (s: IdeWorksheet) => IdeWorksheet): IdeState {
  return { ...state, sheets: state.sheets.map((s) => (s.id === id ? fn(s) : s)) };
}

export function addSheet(state: IdeState, now: number, withCell = true): IdeState {
  if (state.sheets.length >= MAX_WORKSHEETS) return state;
  const counter = state.counter + 1;
  const sheet = newSheet(counter, now, withCell);
  return { ...state, sheets: [...state.sheets, sheet], openIds: [...state.openIds, sheet.id], activeId: sheet.id, counter };
}

export function openSheet(state: IdeState, id: string): IdeState {
  if (!state.sheets.some((s) => s.id === id)) return state;
  return { ...state, openIds: state.openIds.includes(id) ? state.openIds : [...state.openIds, id], activeId: id };
}

export function closeTab(state: IdeState, id: string, now: number): IdeState {
  const idx = state.openIds.indexOf(id);
  if (idx < 0) return state;
  if (state.openIds.length === 1) return addSheetAfterClose(state, id, now);
  const openIds = state.openIds.filter((x) => x !== id);
  const activeId = state.activeId === id ? openIds[Math.min(idx, openIds.length - 1)] : state.activeId;
  return { ...state, openIds, activeId };
}

function addSheetAfterClose(state: IdeState, closing: string, now: number): IdeState {
  // Closing the last open tab leaves a fresh worksheet open (or reopens another when at the limit).
  if (state.sheets.length >= MAX_WORKSHEETS) {
    const other = state.sheets.find((s) => s.id !== closing);
    return other ? { ...state, openIds: [other.id], activeId: other.id } : state;
  }
  const counter = state.counter + 1;
  const sheet = newSheet(counter, now);
  return { ...state, sheets: [...state.sheets, sheet], openIds: [sheet.id], activeId: sheet.id, counter };
}

export function deleteSheet(state: IdeState, id: string, now: number): IdeState {
  if (!state.sheets.some((s) => s.id === id)) return state;
  let next: IdeState = { ...state, sheets: state.sheets.filter((s) => s.id !== id) };
  next = { ...next, openIds: next.openIds.filter((x) => x !== id) };
  if (next.sheets.length === 0) return freshStateFrom(next, now);
  if (next.openIds.length === 0) next = { ...next, openIds: [next.sheets[0].id] };
  if (!next.openIds.includes(next.activeId)) next = { ...next, activeId: next.openIds[0] };
  return next;
}

function freshStateFrom(prev: IdeState, now: number): IdeState {
  const counter = prev.counter + 1;
  const sheet = newSheet(counter, now);
  return { ...prev, sheets: [sheet], openIds: [sheet.id], activeId: sheet.id, counter };
}

export function reorderTabs(state: IdeState, fromId: string, toId: string): IdeState {
  const from = state.openIds.indexOf(fromId);
  const to = state.openIds.indexOf(toId);
  if (from < 0 || to < 0 || from === to) return state;
  const next = state.openIds.slice();
  next.splice(from, 1);
  next.splice(to, 0, fromId);
  return { ...state, openIds: next };
}

// ---- grouping for the worksheet list -------------------------------------------------------------

export type GroupLabel = "Today" | "Yesterday" | "This week" | "Older";

function startOfDay(ms: number): number {
  const d = new Date(ms);
  d.setHours(0, 0, 0, 0);
  return d.getTime();
}

export function groupLabel(updatedAt: number, now: number): GroupLabel {
  const today = startOfDay(now);
  if (updatedAt >= today) return "Today";
  const yesterday = new Date(today);
  yesterday.setDate(yesterday.getDate() - 1);
  if (updatedAt >= yesterday.getTime()) return "Yesterday";
  const week = new Date(today);
  week.setDate(week.getDate() - 7);
  if (updatedAt >= week.getTime()) return "This week";
  return "Older";
}

export function groupSheets(
  sheets: IdeWorksheet[],
  now: number,
  filter = "",
): { label: GroupLabel; sheets: IdeWorksheet[] }[] {
  const q = filter.trim().toLowerCase();
  const order: GroupLabel[] = ["Today", "Yesterday", "This week", "Older"];
  const sorted = sheets
    .filter((s) => !q || s.name.toLowerCase().includes(q))
    .slice()
    .sort((a, b) => b.updatedAt - a.updatedAt);
  return order
    .map((label) => ({ label, sheets: sorted.filter((s) => groupLabel(s.updatedAt, now) === label) }))
    .filter((g) => g.sheets.length > 0);
}

// ---- persistence (validated) ---------------------------------------------------------------------

const ID_RE = /^[a-z]-\d{1,6}$/;

function str(v: unknown, max: number): string | null {
  return typeof v === "string" ? v.slice(0, max) : null;
}

function readCell(v: unknown): IdeCell | null {
  if (!v || typeof v !== "object") return null;
  const o = v as Record<string, unknown>;
  const text = str(o.text, MAX_TEXT);
  if (typeof o.id !== "string" || !/^c-\d{1,6}$/.test(o.id) || text === null) return null;
  if (o.kind !== "sql" && o.kind !== "md") return null;
  return { id: o.id, kind: o.kind, text, collapsed: o.collapsed === true };
}

function readSheet(v: unknown): IdeWorksheet | null {
  if (!v || typeof v !== "object") return null;
  const o = v as Record<string, unknown>;
  const name = str(o.name, MAX_NAME);
  if (typeof o.id !== "string" || !/^w-\d{1,6}$/.test(o.id) || !name || !Array.isArray(o.blocks)) return null;
  const seen = new Set<string>();
  const blocks: Block[] = [];
  let maxId = 0;
  const note = (id: string) => {
    const n = Number(id.slice(2));
    if (n > maxId) maxId = n;
  };
  for (const raw of o.blocks.slice(0, MAX_BLOCKS)) {
    if (!raw || typeof raw !== "object") continue;
    const b = raw as Record<string, unknown>;
    if (b.kind === "txn") {
      if (typeof b.id !== "string" || !/^t-\d{1,6}$/.test(b.id) || seen.has(b.id) || !Array.isArray(b.cells)) continue;
      const cells: IdeCell[] = [];
      for (const rc of b.cells.slice(0, MAX_CELLS_IN_TXN)) {
        const c = readCell(rc);
        // Containers hold SQL cells only.
        if (!c || c.kind !== "sql" || seen.has(c.id)) continue;
        seen.add(c.id);
        note(c.id);
        cells.push(c);
      }
      if (cells.length === 0) continue;
      seen.add(b.id);
      note(b.id);
      blocks.push({ id: b.id, kind: "txn", cells });
    } else {
      const c = readCell(raw);
      if (!c || seen.has(c.id)) continue;
      seen.add(c.id);
      note(c.id);
      blocks.push(c);
    }
  }
  const counter = Math.max(maxId, typeof o.counter === "number" && Number.isFinite(o.counter) ? Math.floor(o.counter) : 0);
  const time = (x: unknown) => (typeof x === "number" && Number.isFinite(x) && x > 0 ? x : Date.now());
  return {
    id: o.id,
    name,
    blocks,
    notes: str(o.notes, MAX_NOTES) ?? "",
    createdAt: time(o.createdAt),
    updatedAt: time(o.updatedAt),
    counter,
  };
}

export function loadIde(now = Date.now()): IdeState {
  try {
    const raw = window.sessionStorage.getItem(KEY);
    if (!raw) return freshState(now);
    const v = JSON.parse(raw) as Record<string, unknown> | null;
    if (!v || !Array.isArray(v.sheets)) return freshState(now);
    const sheets: IdeWorksheet[] = [];
    const seen = new Set<string>();
    for (const rs of v.sheets.slice(0, MAX_WORKSHEETS)) {
      const s = readSheet(rs);
      if (!s || seen.has(s.id) || !ID_RE.test(s.id)) continue;
      seen.add(s.id);
      sheets.push(s);
    }
    if (sheets.length === 0) return freshState(now);
    let openIds = Array.isArray(v.openIds)
      ? (v.openIds as unknown[]).filter((x): x is string => typeof x === "string" && seen.has(x))
      : [];
    openIds = openIds.filter((x, i) => openIds.indexOf(x) === i);
    if (openIds.length === 0) openIds = [sheets[0].id];
    const activeId = typeof v.activeId === "string" && openIds.includes(v.activeId) ? v.activeId : openIds[0];
    const maxW = Math.max(...sheets.map((s) => Number(s.id.slice(2))));
    const counter = Math.max(maxW, typeof v.counter === "number" && Number.isFinite(v.counter) ? Math.floor(v.counter) : 0);
    return { sheets, openIds, activeId, counter, autocommit: v.autocommit !== false };
  } catch {
    return freshState(now);
  }
}

/** Returns false when storage is unavailable or full (the IDE keeps working in memory). */
export function saveIde(state: IdeState): boolean {
  try {
    window.sessionStorage.setItem(KEY, JSON.stringify(state));
    return true;
  } catch {
    return false;
  }
}

export function clearIde(): void {
  try {
    window.sessionStorage.removeItem(KEY);
  } catch {
    /* storage unavailable: nothing to clear */
  }
}

/** A worksheet counts as "dirty" while it holds SQL text that has never been run in this tab. The
 * caller supplies the run record; this module stays free of runtime state. */
export function sqlOf(cell: IdeCell): string {
  return cell.kind === "sql" ? cell.text : "";
}

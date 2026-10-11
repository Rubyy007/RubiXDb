import { useCallback, useEffect, useMemo, useRef, useState, useSyncExternalStore, type DragEvent } from "react";
import { useQueryClient } from "@tanstack/react-query";
import {
  invalidateCatalogQueries,
  useApiClient,
  useDatabasesQuery,
  useInstanceQuery,
  useSchemasQuery,
  useTablesQuery,
} from "../api/queries";
import { OverflowMenu } from "../components/console/OverflowMenu";
import { Icon } from "../components/Icon";
import { useSession } from "../context/SessionContext";
import { toCsv } from "../utils/csv";
import { recordQuery, takePendingSql } from "../utils/sessionActivity";
import { toggleTheme } from "../utils/theme";
import { CellList } from "./CellView";
import { CommandPalette } from "./CommandPalette";
import type { CatalogLists } from "./completion";
import { IdeContext, editorId, type IdeApi, type PlanEntry, type RightTab, type TableRef } from "./ideContext";
import { LeftRail } from "./LeftRail";
import {
  MAX_WORKSHEETS,
  addSheet,
  addTxn,
  closeTab,
  deleteBlock,
  deleteSheet,
  duplicateCell,
  findCell,
  flatCells,
  insertCell,
  isTxn,
  loadIde,
  moveBlock,
  openSheet,
  renameSheet,
  reorderTabs,
  saveIde,
  setCellKind,
  setCellText,
  setCollapsed,
  setNotes,
  updateSheet,
  type IdeState,
  type IdeWorksheet,
} from "./model";
import type { PaletteAction } from "./palette";
import { RightPanel } from "./RightPanel";
import { hasSql, splitStatements } from "./sqlSplit";
import { Runner, implicitSessionKey, toRunError, type HistoryEntry } from "./runner";
import "../styles/console.css";
import "../styles/ide.css";

const SAVE_DELAY_MS = 400;
const HISTORY_LIMIT = 200;

function initialState(): IdeState {
  const loaded = loadIde();
  // A template picked on Home goes into the active worksheet's empty last cell, else a new cell.
  const pending = takePendingSql();
  if (!pending) return loaded;
  const now = Date.now();
  return updateSheet(loaded, loaded.activeId, (s) => {
    const flat = flatCells(s).filter((f) => f.cell.kind === "sql");
    const last = flat[flat.length - 1];
    if (last && last.cell.text.trim() === "") return setCellText(s, last.cell.id, pending, now);
    return insertCell(s, null, "sql", pending, now).sheet;
  });
}

export function IdePage() {
  const client = useApiClient();
  const clientRef = useRef(client);
  clientRef.current = client;
  const queryClient = useQueryClient();
  const { session } = useSession();
  const databases = useDatabasesQuery();
  const schemas = useSchemasQuery();
  const tables = useTablesQuery();
  const instance = useInstanceQuery();

  const [state, setState] = useState<IdeState>(initialState);
  const [history, setHistory] = useState<HistoryEntry[]>([]);
  const [plans, setPlans] = useState<Record<string, PlanEntry>>({});
  const [focusedId, setFocusedId] = useState<string | null>(null);
  const [selectedTable, setSelectedTable] = useState<string | null>(null);
  const [rightTab, setRightTab] = useState<RightTab>("history");
  const [rightOpen, setRightOpen] = useState(() => window.innerWidth > 1100);
  const [leftOpen, setLeftOpen] = useState(() => window.innerWidth > 800);
  const [palette, setPalette] = useState<null | "commands" | "sheets">(null);
  const [help, setHelp] = useState(false);
  const [flashMsg, setFlashMsg] = useState("");
  const [now, setNow] = useState(() => Date.now());
  const [savedAt, setSavedAt] = useState<number | null>(null);
  const firstSave = useRef(true);
  const pendingFocus = useRef<string | null>(null);
  const dragTab = useRef<string | null>(null);
  const flashTimer = useRef<number | undefined>(undefined);

  // ---- runner ------------------------------------------------------------------------------------
  const runnerRef = useRef<Runner | null>(null);
  if (runnerRef.current === null) {
    runnerRef.current = new Runner({
      send: (req, signal) => {
        const c = clientRef.current;
        return c ? c.sql(req, signal) : Promise.reject(new Error("Not connected"));
      },
      onHistory: (e) => {
        setHistory((h) => [e, ...h].slice(0, HISTORY_LIMIT));
        recordQuery(e.sql, e.ok, { ms: e.ms, rows: e.rows });
        if (e.ok && !/^\s*(select|explain|begin|commit|rollback)\b/i.test(e.sql)) invalidateCatalogQueries(queryClient);
      },
    });
  }
  const runner = runnerRef.current;
  const runs = useSyncExternalStore(runner.subscribe, runner.getState);

  const sheet: IdeWorksheet = state.sheets.find((s) => s.id === state.activeId) ?? state.sheets[0];
  const flat = useMemo(() => flatCells(sheet), [sheet]);
  const ticking = runs.inFlight.length > 0 || Object.keys(runs.sessions).length > 0;

  useEffect(() => {
    if (!ticking) return;
    setNow(Date.now());
    const t = window.setInterval(() => setNow(Date.now()), 250);
    return () => window.clearInterval(t);
  }, [ticking]);

  // Stop everything if the page goes away; the API cancels statements whose connection drops.
  useEffect(() => () => runner.stopAll(), [runner]);

  useEffect(() => {
    if (firstSave.current) {
      firstSave.current = false;
      return;
    }
    const t = window.setTimeout(() => {
      if (saveIde(state)) setSavedAt(Date.now());
    }, SAVE_DELAY_MS);
    return () => window.clearTimeout(t);
  }, [state]);

  useEffect(() => {
    if (pendingFocus.current) {
      const el = document.getElementById(editorId(pendingFocus.current));
      if (el) {
        (el as HTMLTextAreaElement).focus();
        pendingFocus.current = null;
      }
    }
  });

  const flash = useCallback((msg: string) => {
    setFlashMsg(msg);
    window.clearTimeout(flashTimer.current);
    flashTimer.current = window.setTimeout(() => setFlashMsg(""), 5000);
  }, []);

  // ---- catalog -----------------------------------------------------------------------------------
  const catalog: CatalogLists = useMemo(
    () => ({
      databases: (databases.data ?? []).map((d) => ({ id: d.database_id, name: d.name })),
      schemas: (schemas.data ?? []).map((s) => ({ id: s.schema_id, databaseId: s.database_id, name: s.name })),
      tables: (tables.data ?? []).map((t) => ({ id: t.table_id, schemaId: t.schema_id, name: t.name })),
    }),
    [databases.data, schemas.data, tables.data],
  );
  const catalogStatus: "loading" | "ready" | "error" =
    databases.isError || schemas.isError || tables.isError ? "error" : databases.isLoading || schemas.isLoading || tables.isLoading ? "loading" : "ready";

  const fetchColumns = useCallback(
    async (table: string): Promise<string[]> => {
      const c = clientRef.current;
      if (!c) return [];
      try {
        const d = await queryClient.fetchQuery({ queryKey: ["sql", "table", table], queryFn: () => c.describeTable(table), staleTime: 30_000 });
        return d.columns.map((x) => x.name);
      } catch {
        return [];
      }
    },
    [queryClient],
  );

  const tableRefOf = useCallback(
    (name: string): TableRef | null => {
      const t = catalog.tables.find((x) => x.name === name);
      const s = t && catalog.schemas.find((x) => x.id === t.schemaId);
      const d = s && catalog.databases.find((x) => x.id === s.databaseId);
      return t && s && d ? { db: d.name, schema: s.name, table: t.name } : null;
    },
    [catalog],
  );

  // ---- edits -------------------------------------------------------------------------------------
  const edit = useCallback((fn: (s: IdeWorksheet) => IdeWorksheet) => setState((st) => updateSheet(st, st.activeId, fn)), []);
  const focusEditor = (cellId: string | null) => {
    if (cellId) pendingFocus.current = cellId;
  };

  const setText = (cellId: string, text: string) => edit((s) => setCellText(s, cellId, text, Date.now()));

  function addAfter(afterId: string | null, kind: "sql" | "md") {
    const r = insertCell(sheet, afterId, kind, "", Date.now());
    if (!r.cellId) {
      flash("This worksheet has reached its cell limit.");
      return;
    }
    setState((st) => updateSheet(st, st.activeId, () => r.sheet));
    if (kind === "sql") {
      focusEditor(r.cellId);
      setFocusedId(r.cellId);
    }
  }

  function addTxnAfter(afterId: string | null) {
    const r = addTxn(sheet, afterId, Date.now());
    if (!r.txnId) {
      flash("This worksheet has reached its cell limit.");
      return;
    }
    setState((st) => updateSheet(st, st.activeId, () => r.sheet));
  }

  function removeBlock(id: string) {
    const block = sheet.blocks.find((b) => b.id === id);
    const ids = block && isTxn(block) ? block.cells.map((c) => c.id) : [id];
    const txnKey = block && isTxn(block) ? block.id : null;
    if (txnKey && runs.sessions[txnKey]) {
      flash("Commit or roll back the open transaction before deleting its container.");
      return;
    }
    for (const cid of ids) {
      runner.forget(cid);
      setPlans((p) => {
        const n = { ...p };
        delete n[cid];
        return n;
      });
    }
    // A cell inside a container: removing it must not leave the container's session without cells.
    edit((s) => deleteBlock(s, id, Date.now()));
  }

  // ---- running -----------------------------------------------------------------------------------
  const topKey = implicitSessionKey(sheet.id);

  async function runTop(items: { cellId: string; sheetId: string; sql: string }[]) {
    const todo = items.filter((i) => hasSql(i.sql));
    if (todo.length === 0) return;
    if (state.autocommit) {
      for (const it of todo) void runner.runAutocommit(it);
      return;
    }
    const b = await runner.begin(topKey, sheet.id);
    if (!b.ok) return;
    await runner.runSequence(todo, topKey);
  }

  function runCell(cellId: string) {
    const f = findCell(sheet, cellId);
    if (!f || f.cell.kind !== "sql") return;
    const item = { cellId, sheetId: sheet.id, sql: f.cell.text };
    if (f.txnId) {
      if (!hasSql(item.sql)) return;
      void runner.runInSession({ ...item, sessionKey: f.txnId }).then((r) => {
        if (r.outcome === "no-session") flash("Begin the transaction before running its cells.");
      });
      return;
    }
    void runTop([item]);
  }

  const topCells = () =>
    sheet.blocks.filter((b): b is Extract<typeof b, { kind: "sql" | "md" }> => !isTxn(b) && b.kind === "sql").map((b) => ({ cellId: b.id, sheetId: sheet.id, sql: b.text }));

  function runAll() {
    const items = topCells();
    if (items.every((i) => !hasSql(i.sql))) {
      flash("No SQL to run outside transaction containers.");
      return;
    }
    void runTop(items);
  }

  function runAbove(cellId: string) {
    const f = findCell(sheet, cellId);
    if (!f) return;
    const stopAt = f.txnId ?? cellId;
    const idx = sheet.blocks.findIndex((b) => b.id === stopAt);
    const items = topCells().filter((c) => sheet.blocks.findIndex((b) => b.id === c.cellId) < idx);
    if (items.length === 0) {
      flash("There are no SQL cells above this one.");
      return;
    }
    void runTop(items);
  }

  function runSteps(txnId: string) {
    const block = sheet.blocks.find((b) => b.id === txnId);
    if (!block || !isTxn(block)) return;
    void runner.runSequence(
      block.cells.map((c) => ({ cellId: c.id, sheetId: sheet.id, sql: c.text })),
      txnId,
    );
  }

  const requestPlan = (cellId: string) => {
    const f = findCell(sheet, cellId);
    const c = clientRef.current;
    if (!f || f.cell.kind !== "sql" || !c) return;
    const text = f.cell.text;
    const stmts = splitStatements(text);
    if (stmts.length === 0) return;
    // EXPLAIN takes one statement; for a script it explains the last one.
    const body = stmts[stmts.length - 1].text;
    const stmt = /^explain\b/i.test(body) ? body : `EXPLAIN ${body}`;
    setPlans((p) => ({ ...p, [cellId]: { sql: text, status: "loading" } }));
    c.sql({ sql: stmt })
      .then((res) => {
        const r = res.result;
        setPlans((p) =>
          r.kind === "explain"
            ? { ...p, [cellId]: { sql: text, status: "ready", text: r.plan_text } }
            : { ...p, [cellId]: { sql: text, status: "error", error: { code: "UNSUPPORTED", message: "EXPLAIN did not return a plan" } } },
        );
      })
      .catch((err) => setPlans((p) => ({ ...p, [cellId]: { sql: text, status: "error", error: toRunError(err) } })));
  };

  function jump(cellId: string, line: number, col: number) {
    const ta = document.getElementById(editorId(cellId)) as HTMLTextAreaElement | null;
    if (!ta) return;
    const lines = ta.value.split("\n");
    let off = 0;
    for (let i = 0; i < Math.min(line - 1, lines.length - 1); i++) off += lines[i].length + 1;
    off += Math.max(0, Math.min(col - 1, (lines[line - 1] ?? "").length));
    ta.focus();
    ta.setSelectionRange(off, off);
  }

  function focusHeader(cellId: string, dir: -1 | 1) {
    const sql = flat.filter((f) => f.cell.kind === "sql");
    const i = sql.findIndex((f) => f.cell.id === cellId);
    const next = sql[i + dir];
    if (next) document.getElementById(`hd-${next.cell.id}`)?.focus();
  }

  // ---- tables ------------------------------------------------------------------------------------
  async function copyText(text: string): Promise<boolean> {
    try {
      await navigator.clipboard.writeText(text);
      return true;
    } catch {
      return false;
    }
  }

  function tableAction(kind: "insert" | "preview" | "copy", t: TableRef) {
    const qn = `${t.db}.${t.schema}.${t.table}`;
    if (kind === "copy") {
      void copyText(qn).then((ok) => flash(ok ? `Copied ${qn}` : "Copy failed: the browser denied clipboard access."));
      return;
    }
    const sql = `SELECT *\nFROM   ${qn}\nLIMIT  100;`;
    const focused = focusedId ? findCell(sheet, focusedId) : null;
    if (kind === "insert" && focused && focused.cell.kind === "sql" && focused.cell.text.trim() === "") {
      setText(focused.cell.id, sql);
      focusEditor(focused.cell.id);
      return;
    }
    const after = focused ? (focused.txnId ?? focused.cell.id) : null;
    const r = insertCell(sheet, after, "sql", sql, Date.now());
    if (!r.cellId) {
      flash("This worksheet has reached its cell limit.");
      return;
    }
    setState((st) => updateSheet(st, st.activeId, () => r.sheet));
    setSelectedTable(t.table);
    if (kind === "preview") void runTop([{ cellId: r.cellId, sheetId: sheet.id, sql }]);
    else focusEditor(r.cellId);
  }

  // ---- worksheets --------------------------------------------------------------------------------
  function newSheet() {
    if (state.sheets.length >= MAX_WORKSHEETS) {
      flash(`At most ${MAX_WORKSHEETS} worksheets.`);
      return;
    }
    setState((st) => addSheet(st, Date.now()));
  }

  function sheetBadge(sheetId: string) {
    const s = state.sheets.find((x) => x.id === sheetId);
    if (!s) return { dot: "idle" as const, dirty: false };
    let dot: "idle" | "running" | "error" = "idle";
    let dirty = false;
    for (const f of flatCells(s)) {
      if (f.cell.kind !== "sql") continue;
      const r = runs.runs[f.cell.id];
      if (r && (r.status === "running" || r.status === "queued")) dot = "running";
      else if (r && r.status === "error" && dot !== "running") dot = "error";
      if (f.cell.text.trim() !== "" && (!r || r.sql !== f.cell.text)) dirty = true;
    }
    return { dot, dirty };
  }

  function loadHistorySql(sql: string) {
    const f = focusedId ? findCell(sheet, focusedId) : null;
    if (f && f.cell.kind === "sql") {
      setText(f.cell.id, sql);
      focusEditor(f.cell.id);
    } else {
      const r = insertCell(sheet, null, "sql", sql, Date.now());
      if (r.cellId) setState((st) => updateSheet(st, st.activeId, () => r.sheet));
    }
  }

  function downloadFocusedCsv() {
    const run = focusedId ? runs.runs[focusedId] : undefined;
    const r = run?.response?.result;
    if (!r || r.kind !== "rows") {
      flash("The focused cell has no row results to export.");
      return;
    }
    const url = URL.createObjectURL(new Blob([toCsv(r.columns, r.rows)], { type: "text/csv;charset=utf-8" }));
    const a = document.createElement("a");
    a.href = url;
    a.download = `rubixdb-results-${Date.now()}.csv`;
    document.body.appendChild(a);
    a.click();
    a.remove();
    window.setTimeout(() => URL.revokeObjectURL(url), 1000);
  }

  function toggleMarkdown() {
    const f = focusedId ? findCell(sheet, focusedId) : null;
    if (!f || f.txnId) {
      flash("Focus a cell outside a transaction container to switch its type.");
      return;
    }
    runner.forget(f.cell.id);
    edit((s) => setCellKind(s, f.cell.id, f.cell.kind === "sql" ? "md" : "sql", Date.now()));
  }

  function clearAll() {
    if (!window.confirm("Clear the text and outputs of every cell in this worksheet?")) return;
    const ids = flat.map((f) => f.cell.id);
    runner.reset(ids);
    edit((s) => ids.reduce((acc, id) => setCellText(acc, id, "", Date.now()), s));
  }

  // ---- finish / begin wrappers --------------------------------------------------------------------
  const sheetIdForKey = (key: string) => (key.startsWith("sess-") ? key.slice(5) : sheet.id);

  // ---- keyboard ----------------------------------------------------------------------------------
  const act = useRef<{ [k: string]: () => void }>({});
  act.current = {
    commands: () => setPalette("commands"),
    sheets: () => setPalette("sheets"),
    runAll,
    runFocused: () => {
      const f = focusedId ?? flat.find((x) => x.cell.kind === "sql")?.cell.id ?? null;
      if (f) runCell(f);
    },
    stop: () => runner.stopAll(),
    newSheet,
    left: () => setLeftOpen((v) => !v),
    right: () => setRightOpen((v) => !v),
    md: toggleMarkdown,
  };
  useEffect(() => {
    function onKey(e: KeyboardEvent) {
      if (e.defaultPrevented) return;
      const mod = e.ctrlKey || e.metaKey;
      if (!mod) return;
      const k = e.key.toLowerCase();
      const a = act.current;
      let fn: (() => void) | null = null;
      if (k === "k" || (e.shiftKey && k === "p")) fn = a.commands;
      else if (k === "p") fn = a.sheets;
      else if (k === "enter") fn = e.shiftKey ? a.runAll : a.runFocused;
      else if (k === ".") fn = a.stop;
      else if (k === "t") fn = a.newSheet;
      else if (k === "b") fn = a.left;
      else if (k === "j") fn = a.right;
      else if (k === "/" && !(e.target instanceof HTMLTextAreaElement)) fn = a.md;
      if (!fn) return;
      e.preventDefault();
      fn();
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  // ---- context value -----------------------------------------------------------------------------
  const api: IdeApi = {
    state,
    sheet,
    flat,
    runs,
    now,
    focusedId,
    setFocused: setFocusedId,
    history,
    catalog,
    catalogStatus,
    fetchColumns,
    plans,
    requestPlan,
    selectedTable,
    setSelectedTable,
    setText,
    setNotes: (t) => edit((s) => setNotes(s, t, Date.now())),
    runCell,
    runAll,
    runAbove,
    cancel: (id) => runner.cancel(id),
    stopAll: () => runner.stopAll(),
    beginTxn: (id) => void runner.begin(id, sheet.id),
    finishTxn: (key, verb) => void runner.finish(key, sheetIdForKey(key), verb),
    runSteps,
    addAfter,
    addTxnAfter,
    deleteBlock: removeBlock,
    move: (id, dir) => edit((s) => moveBlock(s, id, dir, Date.now())),
    duplicate: (id) => {
      const r = duplicateCell(sheet, id, Date.now());
      if (r.cellId) {
        setState((st) => updateSheet(st, st.activeId, () => r.sheet));
        focusEditor(r.cellId);
      } else flash("This worksheet has reached its cell limit.");
    },
    setKind: (id, kind) => {
      runner.forget(id);
      edit((s) => setCellKind(s, id, kind, Date.now()));
    },
    toggleCollapse: (id) => {
      const f = findCell(sheet, id);
      if (f) edit((s) => setCollapsed(s, id, !f.cell.collapsed));
    },
    jump,
    focusHeader,
    openSheet: (id) => setState((st) => openSheet(st, id)),
    newSheet,
    deleteSheet: (id) => {
      const s = state.sheets.find((x) => x.id === id);
      if (!s || !window.confirm(`Delete worksheet "${s.name}"? Its cells cannot be recovered.`)) return;
      for (const f of flatCells(s)) runner.forget(f.cell.id);
      setState((st) => deleteSheet(st, id, Date.now()));
    },
    sheetBadge,
    tableAction,
    autocommit: state.autocommit,
    setAutocommit: (on) => setState((st) => ({ ...st, autocommit: on })),
    rightTab,
    setRightTab,
    rightOpen,
    setRightOpen,
    loadHistorySql,
  };

  // ---- palette actions ---------------------------------------------------------------------------
  const selectedRef = selectedTable ? tableRefOf(selectedTable) : null;
  const focusedRun = focusedId ? runs.runs[focusedId] : undefined;
  const hasRows = focusedRun?.response?.result.kind === "rows";
  const focusedCell = focusedId ? findCell(sheet, focusedId) : null;
  const openTab = (t: RightTab) => () => {
    setRightTab(t);
    setRightOpen(true);
  };
  const commands: PaletteAction[] = [
    { id: "new-ws", label: "New worksheet", group: "Worksheet", shortcut: "Ctrl T", run: newSheet },
    { id: "open-ws", label: "Open worksheet…", group: "Worksheet", shortcut: "Ctrl P", run: () => setPalette("sheets") },
    { id: "rename", label: "Rename worksheet", group: "Worksheet", run: () => document.getElementById("ide-title")?.focus() },
    { id: "clear", label: "Clear all cells", group: "Worksheet", run: clearAll },
    { id: "run", label: "Run cell", group: "Run", shortcut: "Ctrl ↵", run: () => act.current.runFocused() },
    { id: "run-all", label: "Run all", group: "Run", shortcut: "Ctrl ⇧ ↵", run: runAll },
    { id: "run-above", label: "Run above", group: "Run", disabled: !focusedId, run: () => focusedId && runAbove(focusedId) },
    { id: "stop", label: "Stop all running cells", group: "Run", shortcut: "Ctrl .", run: () => runner.stopAll() },
    { id: "reset", label: "Reset outputs", group: "Run", run: () => runner.reset(flat.map((f) => f.cell.id)) },
    { id: "add-sql", label: "Add SQL cell", group: "Edit", run: () => addAfter(focusedCell ? (focusedCell.txnId ?? focusedCell.cell.id) : null, "sql") },
    { id: "add-md", label: "Add markdown cell", group: "Edit", run: () => addAfter(focusedCell ? (focusedCell.txnId ?? focusedCell.cell.id) : null, "md") },
    { id: "add-txn", label: "Add transaction container", group: "Edit", run: () => addTxnAfter(focusedCell ? (focusedCell.txnId ?? focusedCell.cell.id) : null) },
    { id: "toggle-md", label: "Toggle markdown cell", group: "Edit", shortcut: "Ctrl /", run: toggleMarkdown },
    {
      id: "insert-name",
      label: "Insert table name",
      group: "Edit",
      disabled: !selectedRef || !focusedCell || focusedCell.cell.kind !== "sql",
      run: () => {
        if (selectedRef && focusedCell && focusedCell.cell.kind === "sql") {
          setText(focusedCell.cell.id, `${focusedCell.cell.text}${selectedRef.db}.${selectedRef.schema}.${selectedRef.table}`);
          focusEditor(focusedCell.cell.id);
        }
      },
    },
    { id: "insert-select", label: "Insert SELECT for selected table", group: "Edit", disabled: !selectedRef, run: () => selectedRef && tableAction("insert", selectedRef) },
    { id: "history", label: "Open history", group: "View", run: openTab("history") },
    { id: "sessions", label: "Open sessions", group: "View", run: openTab("sessions") },
    { id: "running", label: "Open running queries", group: "View", run: openTab("running") },
    { id: "schema", label: "Open schema", group: "View", run: openTab("schema") },
    { id: "plan", label: "Open plan", group: "View", run: openTab("plan") },
    { id: "csv", label: "Export results CSV", group: "Data", disabled: !hasRows, run: downloadFocusedCsv },
    { id: "left", label: "Toggle left rail", group: "View", shortcut: "Ctrl B", run: () => setLeftOpen((v) => !v) },
    { id: "right", label: "Toggle right panel", group: "View", shortcut: "Ctrl J", run: () => setRightOpen((v) => !v) },
    { id: "autocommit", label: state.autocommit ? "Turn autocommit off" : "Turn autocommit on", group: "Run", run: () => setState((st) => ({ ...st, autocommit: !st.autocommit })) },
    { id: "dark", label: "Toggle dark mode", group: "View", run: () => void toggleTheme() },
    { id: "help", label: "Keyboard shortcuts and what is not yet developed", group: "Help", run: () => setHelp(true) },
  ];
  const sheetActions: PaletteAction[] = state.sheets.map((s) => ({
    id: `ws-${s.id}`,
    label: s.name,
    group: s.id === state.activeId ? "Current" : "Worksheet",
    run: () => setState((st) => openSheet(st, s.id)),
  }));

  // ---- derived header values -----------------------------------------------------------------------
  const active = flat.filter((f) => runs.runs[f.cell.id] && (runs.runs[f.cell.id].status === "running" || runs.runs[f.cell.id].status === "queued"));
  const runningCount = runs.inFlight.length;
  const dbName = catalog.databases[0]?.name ?? "—";
  const schemaName = catalog.schemas.find((s) => s.name === "public")?.name ?? catalog.schemas[0]?.name ?? "—";
  const port = (() => {
    try {
      return new URL(session?.baseUrl ?? "").port || "";
    } catch {
      return "";
    }
  })();
  const instName = instance.data?.name ?? "instance";
  const implicitNotice = runs.notices[topKey];
  const openTxns = Object.keys(runs.sessions).length;
  const anyActive = active.length > 0 || runs.queued > 0 || runningCount > 0;

  const onTabDrop = (e: DragEvent, toId: string) => {
    e.preventDefault();
    if (dragTab.current) setState((st) => reorderTabs(st, dragTab.current as string, toId));
    dragTab.current = null;
  };

  return (
    <IdeContext.Provider value={api}>
      <div className="ide" data-left={leftOpen} data-right={rightOpen}>
        <h1 className="visually-hidden">SQL Console</h1>
        {leftOpen && <LeftRail />}
        <div className="ide-center">
          <section className="ide-panel ide-wshead" aria-label="Worksheet">
            <div className="ide-crumb text-muted">
              <button type="button" className="ide-iconbtn" aria-label={leftOpen ? "Collapse left rail (Ctrl+B)" : "Expand left rail (Ctrl+B)"} onClick={() => setLeftOpen((v) => !v)}>
                <Icon name="panelLeft" />
              </button>
              <span>{dbName}</span>
              <span aria-hidden="true">/</span>
              <span>{schemaName}</span>
              <span aria-hidden="true">/</span>
              <span>worksheet</span>
              <span className="ide-spacer" />
              <span className="ide-small" role="status">
                {savedAt ? `Autosaved ${new Date(savedAt).toLocaleTimeString()} · browser session` : "Autosaved to this browser session"}
              </span>
            </div>
            <TitleInput key={`${sheet.id}-${sheet.name}`} name={sheet.name} onCommit={(n) => edit((s) => renameSheet(s, n, Date.now()))} />

            <div className="ide-tabs" role="tablist" aria-label="Open worksheets">
              {state.openIds.map((id) => {
                const s = state.sheets.find((x) => x.id === id);
                if (!s) return null;
                const badge = sheetBadge(id);
                return (
                  <div
                    key={id}
                    className="ide-tab"
                    data-active={id === state.activeId}
                    draggable
                    onDragStart={() => (dragTab.current = id)}
                    onDragOver={(e) => e.preventDefault()}
                    onDrop={(e) => onTabDrop(e, id)}
                  >
                    <button type="button" role="tab" aria-selected={id === state.activeId} className="ide-tab-btn" onClick={() => setState((st) => openSheet(st, id))}>
                      <i className={`ide-dot is-${badge.dot}`} aria-hidden="true" />
                      <span className="ide-tab-name">{s.name}</span>
                      <span className="visually-hidden">{`, ${badge.dot}`}</span>
                      {badge.dirty && <i className="ide-dirty" title="Has SQL that has not been run" aria-hidden="true" />}
                    </button>
                    <button type="button" className="ide-iconbtn" aria-label={`Close worksheet ${s.name}`} onClick={() => setState((st) => closeTab(st, id, Date.now()))}>
                      <Icon name="close" />
                    </button>
                  </div>
                );
              })}
              <button type="button" className="ide-iconbtn" aria-label="New worksheet (Ctrl+T)" title="New worksheet (Ctrl+T)" onClick={newSheet}>
                <Icon name="plus" />
              </button>
            </div>

            <div className="ide-toolbar" role="toolbar" aria-label="Worksheet actions">
              <button type="button" className="btn btn-primary" title="Run current cell (Ctrl+Enter)" onClick={() => act.current.runFocused()}>
                <Icon name="play" /> <span className="ide-lbl">Run cell</span> <kbd>Ctrl ↵</kbd>
              </button>
              <button type="button" className="btn btn-secondary" title="Run all (Ctrl+Shift+Enter)" onClick={runAll}>
                <Icon name="runAll" /> <span className="ide-lbl">Run all</span>
              </button>
              <button type="button" className="btn btn-secondary" title="Run every autocommit cell above the focused cell" disabled={!focusedId} onClick={() => focusedId && runAbove(focusedId)}>
                <Icon name="runAbove" /> <span className="ide-lbl">Run above</span>
              </button>
              <button type="button" className="btn btn-danger" title="Stop all running cells (Ctrl+.)" disabled={!anyActive} onClick={() => runner.stopAll()}>
                <Icon name="stop" /> <span className="ide-lbl">Stop</span>
              </button>
              <button type="button" className="btn btn-secondary" title="Clear all outputs" onClick={() => runner.reset(flat.map((f) => f.cell.id))}>
                <Icon name="reset" /> <span className="ide-lbl">Reset outputs</span>
              </button>
              <span className="ide-spacer" />
              <span className="ide-chip" title="Role comes from the connected API key. Switching roles inside the IDE is not yet developed.">
                Role <strong>{session?.role ?? "—"}</strong>
              </span>
              <span className="ide-chip text-mono" title="Instance name and port">
                <i className="ide-dot is-ok" aria-hidden="true" /> {instName}
                {port ? `:${port}` : ""}
              </span>
              <label className="ide-switch">
                <input type="checkbox" checked={state.autocommit} onChange={(e) => setState((st) => ({ ...st, autocommit: e.target.checked }))} />
                <span>Autocommit</span>
              </label>
              <button type="button" className="ide-iconbtn" aria-label="Toggle dark mode" title="Toggle dark mode" onClick={() => void toggleTheme()}>
                <Icon name="moon" />
              </button>
              <button type="button" className="ide-iconbtn" aria-label="Command palette (Ctrl+K)" title="Command palette (Ctrl+K)" onClick={() => setPalette("commands")}>
                <Icon name="search" />
              </button>
              <OverflowMenu
                items={[
                  { id: "fmt", label: "Format SQL (not yet developed)", disabled: true, onSelect: () => {} },
                  { id: "csv", label: "Download CSV", disabled: !hasRows, onSelect: downloadFocusedCsv },
                  { id: "clear", label: "Clear all", onSelect: clearAll },
                  { id: "rename", label: "Rename worksheet", onSelect: () => document.getElementById("ide-title")?.focus() },
                  { id: "help", label: "Keyboard shortcuts and status", onSelect: () => setHelp(true) },
                ]}
              />
            </div>

            <div className="ide-strip" role="status" aria-live="polite" data-active={anyActive}>
              <span>
                {runningCount} running, {runs.queued} queued
              </span>
              {openTxns > 0 && <span>· {openTxns} open transaction{openTxns === 1 ? "" : "s"}</span>}
              {(flashMsg || implicitNotice) && <span className="ide-strip-msg">· {flashMsg || implicitNotice}</span>}
              {anyActive && <span className="ide-strip-bar" aria-hidden="true" />}
            </div>
          </section>

          <div className="ide-scroll">
            <CellList />
          </div>
        </div>
        <RightPanel />
      </div>

      {palette && (
        <CommandPalette
          title={palette === "commands" ? "Command palette" : "Open worksheet"}
          placeholder={palette === "commands" ? "Type a command…" : "Find a worksheet…"}
          actions={palette === "commands" ? commands : sheetActions}
          onClose={() => setPalette(null)}
        />
      )}
      {help && <HelpDialog onClose={() => setHelp(false)} />}
    </IdeContext.Provider>
  );
}

function TitleInput({ name, onCommit }: { name: string; onCommit: (n: string) => void }) {
  const [draft, setDraft] = useState(name);
  return (
    <input
      id="ide-title"
      className="ide-title"
      aria-label="Worksheet title"
      value={draft}
      maxLength={60}
      onChange={(e) => setDraft(e.target.value)}
      onBlur={() => (draft.trim() ? onCommit(draft) : setDraft(name))}
      onKeyDown={(e) => {
        if (e.key === "Enter") e.currentTarget.blur();
        else if (e.key === "Escape") {
          setDraft(name);
          e.currentTarget.blur();
        }
      }}
    />
  );
}

const SHORTCUTS: [string, string][] = [
  ["Ctrl+Enter", "Run the current cell"],
  ["Ctrl+Shift+Enter", "Run all autocommit cells"],
  ["Ctrl+.", "Stop all running cells"],
  ["Ctrl+T", "New worksheet (some browsers keep this key for a new browser tab)"],
  ["Ctrl+/", "Toggle markdown cell (inside an editor it comments the line)"],
  ["Ctrl+K or Ctrl+Shift+P", "Command palette"],
  ["Ctrl+P", "Open a worksheet by name"],
  ["Ctrl+B", "Toggle the left rail"],
  ["Ctrl+J", "Toggle the right panel"],
  ["Shift+Enter", "Insert a new cell below (in an editor)"],
  ["Esc", "Leave the editor"],
  ["↑ / ↓ on a cell header", "Move between cells"],
];

const NOT_DEVELOPED = [
  "Reading as of a pinned snapshot (POST /v1/sql has no snapshot parameter).",
  "Switching role inside the IDE (the role is the connected API key's).",
  "Format SQL.",
  "Server-side query text in History and Running (the API never returns it).",
  "Colored badges for tier-like values.",
];

function HelpDialog({ onClose }: { onClose: () => void }) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    ref.current?.focus();
  }, []);
  return (
    <div className="ide-pal-scrim" onMouseDown={onClose}>
      <div
        ref={ref}
        tabIndex={-1}
        className="ide-pal ide-help"
        role="dialog"
        aria-modal="true"
        aria-label="Keyboard shortcuts and status"
        onMouseDown={(e) => e.stopPropagation()}
        onKeyDown={(e) => {
          if (e.key === "Escape") onClose();
        }}
      >
        <h2>Keyboard shortcuts</h2>
        <dl className="ide-keys">
          {SHORTCUTS.map(([k, d]) => (
            <div key={k}>
              <dt>
                <kbd>{k}</kbd>
              </dt>
              <dd>{d}</dd>
            </div>
          ))}
        </dl>
        <h2>Not yet developed</h2>
        <ul>
          {NOT_DEVELOPED.map((x) => (
            <li key={x}>{x}</li>
          ))}
        </ul>
        <button type="button" className="btn btn-secondary" onClick={onClose}>
          Close
        </button>
      </div>
    </div>
  );
}

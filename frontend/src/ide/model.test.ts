import { beforeEach, describe, expect, it } from "vitest";
import {
  MAX_WORKSHEETS,
  addSheet,
  addTxn,
  closeTab,
  deleteBlock,
  deleteSheet,
  duplicateCell,
  flatCells,
  freshState,
  groupLabel,
  groupSheets,
  insertCell,
  loadIde,
  moveBlock,
  newSheet,
  reorderTabs,
  saveIde,
  setCellText,
  type IdeState,
} from "./model";

const NOW = new Date(2026, 9, 11, 14, 0, 0).getTime();

beforeEach(() => window.sessionStorage.clear());

describe("worksheet model", () => {
  it("numbers cells across top-level blocks and container children", () => {
    let s = newSheet(1, NOW);
    s = insertCell(s, null, "sql", "SELECT 1", NOW).sheet;
    const t = addTxn(s, null, NOW);
    const flat = flatCells(t.sheet);
    expect(flat.map((f) => f.num)).toEqual([1, 2, 3]);
    expect(flat[2].txnId).toBe(t.txnId);
    expect(flat[0].txnId).toBeNull();
  });

  it("inserts after a cell inside a container as a SQL cell in that container", () => {
    const base = addTxn(newSheet(1, NOW, false), null, NOW);
    const first = flatCells(base.sheet)[0].cell.id;
    const r = insertCell(base.sheet, first, "md", "x", NOW);
    const flat = flatCells(r.sheet);
    expect(flat).toHaveLength(2);
    expect(flat[1].txnId).toBe(base.txnId);
    expect(flat[1].cell.kind).toBe("sql");
  });

  it("removes a container when its last cell is deleted", () => {
    const base = addTxn(newSheet(1, NOW, false), null, NOW);
    const only = flatCells(base.sheet)[0].cell.id;
    expect(deleteBlock(base.sheet, only, NOW).blocks).toHaveLength(0);
  });

  it("moves and duplicates cells", () => {
    let s = newSheet(1, NOW, false);
    const a = insertCell(s, null, "sql", "a", NOW);
    const b = insertCell(a.sheet, null, "sql", "b", NOW);
    s = moveBlock(b.sheet, b.cellId as string, -1, NOW);
    expect(flatCells(s).map((f) => f.cell.text)).toEqual(["b", "a"]);
    const d = duplicateCell(s, a.cellId as string, NOW);
    expect(flatCells(d.sheet).map((f) => f.cell.text)).toEqual(["b", "a", "a"]);
  });

  it("edits cell text", () => {
    const s = newSheet(1, NOW);
    const id = flatCells(s)[0].cell.id;
    expect(flatCells(setCellText(s, id, "SELECT 2", NOW + 5))[0].cell.text).toBe("SELECT 2");
  });

  it("caps worksheets at 20 and keeps a tab open when the last one closes", () => {
    let st: IdeState = freshState(NOW);
    for (let i = 0; i < 40; i++) st = addSheet(st, NOW);
    expect(st.sheets).toHaveLength(MAX_WORKSHEETS);
    const one = freshState(NOW);
    const closed = closeTab(one, one.activeId, NOW);
    expect(closed.openIds).toHaveLength(1);
    expect(closed.activeId).not.toBe(one.activeId);
  });

  it("deleting every worksheet leaves one fresh worksheet", () => {
    const st = freshState(NOW);
    const next = deleteSheet(st, st.activeId, NOW);
    expect(next.sheets).toHaveLength(1);
    expect(next.sheets[0].id).not.toBe(st.sheets[0].id);
  });

  it("reorders open tabs", () => {
    let st = addSheet(addSheet(freshState(NOW), NOW), NOW);
    const [a, , c] = st.openIds;
    st = reorderTabs(st, c, a);
    expect(st.openIds[0]).toBe(c);
  });

  it("groups worksheets by recency", () => {
    const day = 86_400_000;
    expect(groupLabel(NOW - 1000, NOW)).toBe("Today");
    expect(groupLabel(NOW - day, NOW)).toBe("Yesterday");
    expect(groupLabel(NOW - 4 * day, NOW)).toBe("This week");
    expect(groupLabel(NOW - 30 * day, NOW)).toBe("Older");
    const sheets = [newSheet(1, NOW), { ...newSheet(2, NOW), updatedAt: NOW - 30 * day, name: "Old one" }];
    expect(groupSheets(sheets, NOW).map((g) => g.label)).toEqual(["Today", "Older"]);
    expect(groupSheets(sheets, NOW, "old")[0].sheets[0].name).toBe("Old one");
  });
});

describe("persistence", () => {
  it("round-trips through sessionStorage", () => {
    const st = freshState(NOW);
    st.sheets[0] = setCellText(st.sheets[0], flatCells(st.sheets[0])[0].cell.id, "SELECT 1", NOW);
    expect(saveIde(st)).toBe(true);
    const back = loadIde(NOW);
    expect(flatCells(back.sheets[0])[0].cell.text).toBe("SELECT 1");
  });

  it("falls back to a fresh state for corrupt or hostile storage", () => {
    window.sessionStorage.setItem("rubixdb-ide-worksheets", "{not json");
    expect(loadIde(NOW).sheets).toHaveLength(1);
    window.sessionStorage.setItem(
      "rubixdb-ide-worksheets",
      JSON.stringify({
        sheets: [
          { id: "../../x", name: "bad", blocks: [] },
          { id: "w-3", name: "ok", blocks: [{ id: "c-1", kind: "script", text: "x" }, { id: "c-2", kind: "sql", text: "SELECT 1" }, { id: "c-2", kind: "sql", text: "dup" }] },
        ],
        openIds: ["w-3", "w-3", "nope"],
        activeId: "nope",
      }),
    );
    const st = loadIde(NOW);
    expect(st.sheets.map((s) => s.id)).toEqual(["w-3"]);
    expect(st.openIds).toEqual(["w-3"]);
    expect(st.activeId).toBe("w-3");
    expect(flatCells(st.sheets[0]).map((f) => f.cell.text)).toEqual(["SELECT 1"]);
  });

  it("drops markdown cells found inside a container", () => {
    window.sessionStorage.setItem(
      "rubixdb-ide-worksheets",
      JSON.stringify({
        sheets: [{ id: "w-1", name: "t", blocks: [{ id: "t-1", kind: "txn", cells: [{ id: "c-2", kind: "md", text: "x" }, { id: "c-3", kind: "sql", text: "y" }] }] }],
      }),
    );
    expect(flatCells(loadIde(NOW).sheets[0]).map((f) => f.cell.kind)).toEqual(["sql"]);
  });
});

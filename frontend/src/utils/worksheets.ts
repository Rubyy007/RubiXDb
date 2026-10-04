/** Worksheet tabs for the SQL console: client-side only, kept in sessionStorage
 * (this tab's session) so a reload restores them. This is query text, never
 * credentials, and never localStorage. Every read is validated: stored data
 * is untrusted input. */

export interface Worksheet {
  id: string;
  name: string;
  sql: string;
}

export interface WorksheetState {
  tabs: Worksheet[];
  activeId: string;
  counter: number;
}

const KEY = "rubixdb-console-worksheets";
export const MAX_TABS = 20;
const MAX_NAME = 40;
const MAX_SQL = 1_048_576;

export function newWorksheet(counter: number, sql = ""): Worksheet {
  return { id: `ws-${counter}`, name: `Untitled ${counter}`, sql };
}

export function freshState(): WorksheetState {
  return { tabs: [newWorksheet(1)], activeId: "ws-1", counter: 1 };
}

export function loadWorksheets(): WorksheetState {
  try {
    const raw = window.sessionStorage.getItem(KEY);
    if (!raw) return freshState();
    const v = JSON.parse(raw) as Partial<WorksheetState> | null;
    if (!v || !Array.isArray(v.tabs) || v.tabs.length === 0) return freshState();
    const tabs: Worksheet[] = [];
    const seen = new Set<string>();
    for (const t of v.tabs.slice(0, MAX_TABS)) {
      if (
        !t ||
        typeof t.id !== "string" ||
        typeof t.name !== "string" ||
        typeof t.sql !== "string" ||
        !/^ws-\d{1,6}$/.test(t.id) ||
        seen.has(t.id)
      ) {
        continue;
      }
      seen.add(t.id);
      tabs.push({ id: t.id, name: t.name.slice(0, MAX_NAME), sql: t.sql.slice(0, MAX_SQL) });
    }
    if (tabs.length === 0) return freshState();
    const maxId = Math.max(...tabs.map((t) => Number(t.id.slice(3))));
    const counter = Math.max(maxId, typeof v.counter === "number" && Number.isFinite(v.counter) ? v.counter : 0);
    const activeId = tabs.some((t) => t.id === v.activeId) ? (v.activeId as string) : tabs[0].id;
    return { tabs, activeId, counter };
  } catch {
    return freshState();
  }
}

/** Returns false when storage is unavailable or full (the console keeps working in memory). */
export function saveWorksheets(state: WorksheetState): boolean {
  try {
    window.sessionStorage.setItem(KEY, JSON.stringify(state));
    return true;
  } catch {
    return false;
  }
}

export function clearWorksheets(): void {
  try {
    window.sessionStorage.removeItem(KEY);
  } catch {
    /* storage unavailable: nothing to clear */
  }
}

import { useEffect, useMemo, useRef, useState, type KeyboardEvent, type MouseEvent } from "react";
import { Icon } from "../components/Icon";
import { useIde, type TableRef } from "./ideContext";
import { groupSheets } from "./model";

interface MenuState {
  x: number;
  y: number;
  table: TableRef;
}

/** Left rail: worksheet list (grouped by recency) and the database tree (mirror of the catalog). */
export function LeftRail() {
  const ide = useIde();
  const [filter, setFilter] = useState("");
  const [treeFilter, setTreeFilter] = useState("");
  const [open, setOpen] = useState<ReadonlySet<string>>(new Set());
  const [menu, setMenu] = useState<MenuState | null>(null);
  const [now] = useState(() => Date.now());
  const groups = useMemo(() => groupSheets(ide.state.sheets, now, filter), [ide.state.sheets, now, filter]);

  const tree = useMemo(() => {
    const q = treeFilter.trim().toLowerCase();
    return ide.catalog.databases.map((d) => ({
      db: d,
      schemas: ide.catalog.schemas
        .filter((s) => s.databaseId === d.id)
        .map((s) => ({
          schema: s,
          tables: ide.catalog.tables.filter((t) => t.schemaId === s.id && (!q || t.name.toLowerCase().includes(q))),
        }))
        .filter((s) => !q || s.tables.length > 0),
    }));
  }, [ide.catalog, treeFilter]);

  const toggle = (key: string) =>
    setOpen((o) => {
      const n = new Set(o);
      if (n.has(key)) n.delete(key);
      else n.add(key);
      return n;
    });
  // Everything starts expanded; a user toggle records the exceptions.
  const isOpen = (key: string) => (treeFilter.trim() ? true : !open.has(key));

  function showMenu(e: MouseEvent, table: TableRef) {
    e.preventDefault();
    setMenu({ x: e.clientX, y: e.clientY, table });
  }
  function menuFromKey(e: KeyboardEvent<HTMLButtonElement>, table: TableRef) {
    if (e.key === "ContextMenu" || (e.shiftKey && e.key === "F10")) {
      e.preventDefault();
      const r = e.currentTarget.getBoundingClientRect();
      setMenu({ x: r.left + 24, y: r.bottom, table });
    }
  }

  return (
    <aside className="ide-left" aria-label="Worksheets and databases">
      <section className="ide-side-sec" aria-labelledby="ide-ws-title">
        <h2 id="ide-ws-title" className="ide-side-title">
          Worksheets
        </h2>
        <button type="button" className="btn btn-secondary ide-wide" onClick={ide.newSheet}>
          <Icon name="plus" /> New worksheet
        </button>
        <label className="visually-hidden" htmlFor="ide-ws-search">
          Search worksheets
        </label>
        <input id="ide-ws-search" className="ide-search" type="search" placeholder="Search worksheets" value={filter} onChange={(e) => setFilter(e.target.value)} />
        {groups.length === 0 && <p className="text-muted ide-small">No worksheet matches.</p>}
        {groups.map((g) => (
          <div key={g.label}>
            <h3 className="ide-group">{g.label}</h3>
            <ul className="ide-wslist">
              {g.sheets.map((s) => {
                const badge = ide.sheetBadge(s.id);
                return (
                  <li key={s.id} data-active={s.id === ide.state.activeId}>
                    <button type="button" className="ide-ws-item" aria-current={s.id === ide.state.activeId ? "true" : undefined} onClick={() => ide.openSheet(s.id)}>
                      <i className={`ide-dot is-${badge.dot}`} title={badge.dot} aria-hidden="true" />
                      <span className="ide-ws-name">{s.name}</span>
                      <span className="visually-hidden">{`, ${badge.dot}${badge.dirty ? ", has unrun changes" : ""}`}</span>
                      {badge.dirty && <i className="ide-dirty" title="Has SQL that has not been run" aria-hidden="true" />}
                    </button>
                    <button type="button" className="ide-iconbtn ide-ws-del" aria-label={`Delete worksheet ${s.name}`} onClick={() => ide.deleteSheet(s.id)}>
                      <Icon name="close" />
                    </button>
                  </li>
                );
              })}
            </ul>
          </div>
        ))}
      </section>

      <section className="ide-side-sec" aria-labelledby="ide-db-title">
        <h2 id="ide-db-title" className="ide-side-title">
          Databases
        </h2>
        <label className="visually-hidden" htmlFor="ide-tree-filter">
          Filter databases, schemas, tables
        </label>
        <input id="ide-tree-filter" className="ide-search" type="search" placeholder="Filter databases, schemas, tables…" value={treeFilter} onChange={(e) => setTreeFilter(e.target.value)} />
        {ide.catalogStatus === "loading" && <p className="text-muted ide-small">Loading catalog…</p>}
        {ide.catalogStatus === "error" && (
          <p className="field-error ide-small" role="alert">
            Could not load the catalog.
          </p>
        )}
        {ide.catalogStatus === "ready" && tree.length === 0 && <p className="text-muted ide-small">No databases.</p>}
        <ul className="ide-tree" aria-label="Database catalog">
          {tree.map(({ db, schemas }) => (
            <li key={db.id}>
              <button type="button" className="ide-node" aria-expanded={isOpen(`d${db.id}`)} onClick={() => toggle(`d${db.id}`)}>
                <span aria-hidden="true">{isOpen(`d${db.id}`) ? "▾" : "▸"}</span> {db.name}
              </button>
              {isOpen(`d${db.id}`) && (
                <ul>
                  {schemas.map(({ schema, tables }) => (
                    <li key={schema.id}>
                      <button type="button" className="ide-node is-l1" aria-expanded={isOpen(`s${schema.id}`)} onClick={() => toggle(`s${schema.id}`)}>
                        <span aria-hidden="true">{isOpen(`s${schema.id}`) ? "▾" : "▸"}</span> {schema.name}
                      </button>
                      {isOpen(`s${schema.id}`) && (
                        <ul>
                          {tables.length === 0 && <li className="ide-small text-muted ide-empty-node">No tables</li>}
                          {tables.map((t) => {
                            const ref: TableRef = { db: db.name, schema: schema.name, table: t.name };
                            return (
                              <li key={t.id}>
                                <button
                                  type="button"
                                  className="ide-node is-l2 is-table"
                                  data-selected={ide.selectedTable === t.name}
                                  aria-haspopup="menu"
                                  onClick={() => ide.setSelectedTable(t.name)}
                                  onDoubleClick={() => ide.tableAction("insert", ref)}
                                  onContextMenu={(e) => showMenu(e, ref)}
                                  onKeyDown={(e) => menuFromKey(e, ref)}
                                >
                                  <Icon name="catalog" /> {t.name}
                                </button>
                              </li>
                            );
                          })}
                        </ul>
                      )}
                    </li>
                  ))}
                </ul>
              )}
            </li>
          ))}
        </ul>
        <p className="text-muted ide-small">Right-click a table for actions. Double-click inserts a SELECT.</p>
      </section>

      {menu && (
        <TableMenu
          menu={menu}
          onClose={() => setMenu(null)}
          onPick={(kind) => {
            ide.tableAction(kind, menu.table);
            setMenu(null);
          }}
        />
      )}
    </aside>
  );
}

function TableMenu({ menu, onClose, onPick }: { menu: MenuState; onClose: () => void; onPick: (k: "insert" | "preview" | "copy") => void }) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    ref.current?.querySelector<HTMLButtonElement>("button")?.focus();
    const down = (e: globalThis.MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) onClose();
    };
    document.addEventListener("mousedown", down);
    return () => document.removeEventListener("mousedown", down);
  }, [onClose]);

  function onKey(e: KeyboardEvent<HTMLDivElement>) {
    if (e.key === "Escape") {
      e.preventDefault();
      onClose();
      return;
    }
    if (e.key !== "ArrowDown" && e.key !== "ArrowUp") return;
    e.preventDefault();
    const items = Array.from(ref.current?.querySelectorAll<HTMLButtonElement>("button") ?? []);
    const i = items.indexOf(document.activeElement as HTMLButtonElement);
    items[(i + (e.key === "ArrowDown" ? 1 : items.length - 1)) % items.length]?.focus();
  }

  const left = Math.min(menu.x, window.innerWidth - 270);
  const top = Math.min(menu.y, window.innerHeight - 140);
  return (
    <div ref={ref} className="ide-ctx" role="menu" aria-label={`Actions for ${menu.table.table}`} style={{ left, top }} onKeyDown={onKey}>
      <button type="button" role="menuitem" onClick={() => onPick("insert")}>
        Insert SELECT in current cell
      </button>
      <button type="button" role="menuitem" onClick={() => onPick("preview")}>
        Preview 100 rows
      </button>
      <button type="button" role="menuitem" onClick={() => onPick("copy")}>
        Copy fully-qualified name
      </button>
      <div className="ide-ctx-name text-mono">{`${menu.table.db}.${menu.table.schema}.${menu.table.table}`}</div>
    </div>
  );
}

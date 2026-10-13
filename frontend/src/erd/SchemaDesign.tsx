import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type KeyboardEvent, type PointerEvent } from "react";
import { useQueries } from "@tanstack/react-query";
import { useApiClient, useSchemasQuery, useTablesQuery } from "../api/queries";
import { useSession } from "../context/SessionContext";
import { Icon } from "../components/Icon";
import {
  CARD_W,
  HEAD_H,
  MORE_H,
  ROW_H,
  cardHeight,
  columnY,
  groupFrames,
  inferLinks,
  layoutTables,
  linkPath,
  visibleColumns,
  type Box,
  type ErdTable,
} from "./schemaGraph";
import "../styles/erd.css";

/** Catalog > Schema design: every table as a card (columns, types, keys) on a pan / zoom canvas, with the
 * relationships between them drawn as lines. rubiXDB has no foreign keys, so the lines are INFERRED from
 * column and table names (see `schemaGraph.ts`) and are drawn dashed and labelled as inferred -- never as
 * a constraint. Read-only: nothing here writes to the database. */

const MAX_TABLES = 120;
const MIN_ZOOM = 0.2;
const MAX_ZOOM = 1.6;
const STORAGE_PREFIX = "rubixdb-erd-layout-v1:";

type Offsets = Record<string, { x: number; y: number }>;

function loadOffsets(storageKey: string): Offsets {
  try {
    const raw = window.localStorage.getItem(storageKey);
    const parsed: unknown = raw ? JSON.parse(raw) : {};
    if (typeof parsed !== "object" || parsed === null) return {};
    const out: Offsets = {};
    for (const [k, v] of Object.entries(parsed as Record<string, unknown>)) {
      const o = v as { x?: unknown; y?: unknown };
      if (typeof o?.x === "number" && typeof o?.y === "number" && Number.isFinite(o.x) && Number.isFinite(o.y)) out[k] = { x: o.x, y: o.y };
    }
    return out;
  } catch {
    return {};
  }
}

function saveOffsets(storageKey: string, offsets: Offsets) {
  try {
    if (Object.keys(offsets).length === 0) window.localStorage.removeItem(storageKey);
    else window.localStorage.setItem(storageKey, JSON.stringify(offsets));
  } catch {
    /* storage unavailable: the layout still works for this page view */
  }
}

export function SchemaDesign() {
  const client = useApiClient();
  const { session } = useSession();
  const schemasQuery = useSchemasQuery();
  const tablesQuery = useTablesQuery();
  const listed = useMemo(() => (tablesQuery.data ?? []).slice(0, MAX_TABLES), [tablesQuery.data]);
  const capped = (tablesQuery.data?.length ?? 0) > MAX_TABLES;

  // One describe call per table. Same query key as the SQL IDE's column lookup, so the cache is shared.
  const described = useQueries({
    queries: listed.map((t) => ({
      queryKey: ["sql", "table", t.name],
      queryFn: () => client!.describeTable(t.name),
      enabled: client !== null,
      staleTime: 30_000,
    })),
  });

  const schemaName = useCallback(
    (id: number) => schemasQuery.data?.find((s) => s.schema_id === id)?.name ?? `schema ${id}`,
    [schemasQuery.data],
  );

  const tables: ErdTable[] = useMemo(
    () =>
      listed.map((t, i) => {
        const d = described[i]?.data;
        const schema = schemaName(t.schema_id);
        return {
          key: `${schema}.${t.name}`,
          schema,
          name: t.name,
          columns: d ? d.columns.map((c) => ({ name: c.name, dataType: c.data_type, nullable: c.nullable, primaryKey: c.primary_key })) : [],
        };
      }),
    // `described` is a new array every render; its data identities are what matter.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [listed, schemaName, ...described.map((q) => q.data)],
  );

  const loadingColumns = described.some((q) => q.isLoading);
  const failedKeys = new Set(tables.filter((_, i) => described[i]?.isError).map((t) => t.key));
  const failedColumns = failedKeys.size;

  const links = useMemo(() => inferLinks(tables), [tables]);
  const auto = useMemo(() => layoutTables(tables, links), [tables, links]);

  const storageKey = `${STORAGE_PREFIX}${session?.baseUrl ?? ""}`;
  const [offsets, setOffsets] = useState<Offsets>(() => loadOffsets(storageKey));
  const [showLinks, setShowLinks] = useState(true);
  const [active, setActive] = useState<string | null>(null); // hovered / focused table key
  const [pickedLink, setPickedLink] = useState<string | null>(null);

  const boxes: Record<string, Box> = useMemo(() => {
    const out: Record<string, Box> = {};
    for (const t of tables) {
      const a = auto[t.key];
      if (!a) continue;
      const o = offsets[t.key];
      out[t.key] = o ? { ...a, x: o.x, y: o.y } : a;
    }
    return out;
  }, [tables, auto, offsets]);

  const frames = useMemo(() => groupFrames(tables, boxes), [tables, boxes]);
  const byKey = useMemo(() => new Map(tables.map((t) => [t.key, t])), [tables]);

  // ---- pan / zoom ------------------------------------------------------------------------------------
  const viewportRef = useRef<HTMLDivElement>(null);
  const [view, setView] = useState({ x: 24, y: 24, k: 1 });

  const fit = useCallback(() => {
    const vp = viewportRef.current;
    const all = Object.values(boxes);
    if (!vp || all.length === 0) return;
    const pad = 40;
    const x0 = Math.min(...frames.map((f) => f.x), ...all.map((b) => b.x));
    const y0 = Math.min(...frames.map((f) => f.y), ...all.map((b) => b.y));
    const x1 = Math.max(...frames.map((f) => f.x + f.w), ...all.map((b) => b.x + b.w));
    const y1 = Math.max(...frames.map((f) => f.y + f.h), ...all.map((b) => b.y + b.h));
    const k = Math.max(MIN_ZOOM, Math.min(1, (vp.clientWidth - pad * 2) / (x1 - x0), (vp.clientHeight - pad * 2) / (y1 - y0)));
    setView({ k, x: pad - x0 * k + (vp.clientWidth - pad * 2 - (x1 - x0) * k) / 2, y: pad - y0 * k });
  }, [boxes, frames]);

  // Fit once, when the first complete layout is there.
  const fitted = useRef(false);
  useLayoutEffect(() => {
    if (!fitted.current && Object.keys(boxes).length > 0 && !loadingColumns) {
      fitted.current = true;
      fit();
    }
  }, [boxes, loadingColumns, fit]);

  const zoomAt = useCallback((factor: number, cx?: number, cy?: number) => {
    const vp = viewportRef.current;
    setView((v) => {
      const k = Math.max(MIN_ZOOM, Math.min(MAX_ZOOM, v.k * factor));
      const px = cx ?? (vp ? vp.clientWidth / 2 : 0);
      const py = cy ?? (vp ? vp.clientHeight / 2 : 0);
      return { k, x: px - ((px - v.x) / v.k) * k, y: py - ((py - v.y) / v.k) * k };
    });
  }, []);

  // Ctrl/Cmd + wheel zooms; a plain wheel scrolls the page as usual. Needs a non-passive listener.
  useEffect(() => {
    const vp = viewportRef.current;
    if (!vp) return;
    const onWheel = (e: WheelEvent) => {
      if (!(e.ctrlKey || e.metaKey)) return;
      e.preventDefault();
      const r = vp.getBoundingClientRect();
      zoomAt(Math.exp(-e.deltaY * 0.0015), e.clientX - r.left, e.clientY - r.top);
    };
    vp.addEventListener("wheel", onWheel, { passive: false });
    return () => vp.removeEventListener("wheel", onWheel);
  }, [zoomAt, tables.length]);

  const pan = useRef<{ px: number; py: number; vx: number; vy: number } | null>(null);
  const onBgDown = (e: PointerEvent<HTMLDivElement>) => {
    if (e.button !== 0 || (e.target as HTMLElement).closest(".erd-card, .erd-tools")) return;
    pan.current = { px: e.clientX, py: e.clientY, vx: view.x, vy: view.y };
    e.currentTarget.setPointerCapture(e.pointerId);
    setPickedLink(null);
  };
  const onBgMove = (e: PointerEvent<HTMLDivElement>) => {
    const p = pan.current;
    if (p) setView((v) => ({ ...v, x: p.vx + e.clientX - p.px, y: p.vy + e.clientY - p.py }));
  };
  const onBgUp = () => {
    pan.current = null;
  };

  // ---- dragging a card -------------------------------------------------------------------------------
  const drag = useRef<{ key: string; px: number; py: number; bx: number; by: number } | null>(null);
  const offsetsRef = useRef(offsets);
  offsetsRef.current = offsets;
  const moveCard = (key: string, x: number, y: number) => setOffsets((o) => ({ ...o, [key]: { x: Math.round(x), y: Math.round(y) } }));

  const onCardDown = (key: string) => (e: PointerEvent<HTMLElement>) => {
    if (e.button !== 0) return;
    const b = boxes[key];
    if (!b) return;
    drag.current = { key, px: e.clientX, py: e.clientY, bx: b.x, by: b.y };
    e.currentTarget.setPointerCapture(e.pointerId);
    e.stopPropagation();
  };
  const onCardMove = (e: PointerEvent<HTMLElement>) => {
    const d = drag.current;
    if (d) moveCard(d.key, d.bx + (e.clientX - d.px) / view.k, d.by + (e.clientY - d.py) / view.k);
  };
  const onCardUp = () => {
    if (drag.current) saveOffsets(storageKey, offsetsRef.current);
    drag.current = null;
  };
  const onCardKey = (key: string) => (e: KeyboardEvent<HTMLElement>) => {
    const step = e.shiftKey ? 40 : 10;
    const d: Record<string, [number, number]> = { ArrowLeft: [-step, 0], ArrowRight: [step, 0], ArrowUp: [0, -step], ArrowDown: [0, step] };
    const m = d[e.key];
    const b = boxes[key];
    if (!m || !b) return;
    e.preventDefault();
    const next = { ...offsetsRef.current, [key]: { x: b.x + m[0], y: b.y + m[1] } };
    setOffsets(next);
    saveOffsets(storageKey, next);
  };

  const resetLayout = () => {
    setOffsets({});
    saveOffsets(storageKey, {});
    fitted.current = false;
    setTimeout(() => {
      fitted.current = true;
      fit();
    }, 0);
  };

  // ---- derived drawing data --------------------------------------------------------------------------
  const lines = useMemo(() => {
    return links.flatMap((l) => {
      const a = byKey.get(l.from.table);
      const b = byKey.get(l.to.table);
      const ba = boxes[l.from.table];
      const bb = boxes[l.to.table];
      if (!a || !b || !ba || !bb) return [];
      const p = linkPath(ba, columnY(a, links, ba.y, l.from.column), bb, columnY(b, links, bb.y, l.to.column));
      return [{ link: l, ...p }];
    });
  }, [links, byKey, boxes]);

  const touching = (key: string) => links.filter((l) => l.from.table === key || l.to.table === key);
  const linked = useMemo(() => {
    const s = new Set<string>();
    if (active) {
      s.add(active);
      for (const l of touching(active)) {
        s.add(l.from.table);
        s.add(l.to.table);
      }
    }
    return s;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [active, links]);

  const stage = useMemo(() => {
    const all = [...Object.values(boxes), ...frames];
    return { w: Math.max(0, ...all.map((b) => b.x + b.w)) + 80, h: Math.max(0, ...all.map((b) => b.y + b.h)) + 80 };
  }, [boxes, frames]);

  // ---- render ----------------------------------------------------------------------------------------
  const noTablesSchemas = (schemasQuery.data ?? []).map((s) => s.name).filter((n) => !tables.some((t) => t.schema === n));
  const picked = links.find((l) => l.id === pickedLink) ?? null;

  if (tablesQuery.isLoading || schemasQuery.isLoading) return <p className="text-muted">Loading the catalog&hellip;</p>;
  if (tablesQuery.isError) return <p className="field-error">The table list could not be loaded.</p>;

  return (
    <div className="erd" data-testid="schema-design">
      <div className="erd-bar">
        <div className="erd-summary" role="status">
          <strong>{tables.length}</strong> table{tables.length === 1 ? "" : "s"} · <strong>{links.length}</strong> inferred relationship{links.length === 1 ? "" : "s"}
          {loadingColumns && " · reading columns…"}
        </div>
        <label className="erd-toggle">
          <input type="checkbox" checked={showLinks} onChange={(e) => setShowLinks(e.target.checked)} /> Show relationships
        </label>
        <div className="erd-legend" aria-hidden="true">
          <svg width="46" height="10" viewBox="0 0 46 10"><path d="M2 5 H42" className="erd-legend-line" /><path d="M42 1 L46 5 L42 9 z" className="erd-legend-head" /></svg>
          inferred from names, not a constraint
        </div>
      </div>

      {tables.length === 0 ? (
        <div className="erd-empty">
          <Icon name="table" />
          <p>No tables to draw yet. Create one in the SQL Console and it will appear here.</p>
        </div>
      ) : (
        <div
          className="erd-viewport"
          ref={viewportRef}
          onPointerDown={onBgDown}
          onPointerMove={onBgMove}
          onPointerUp={onBgUp}
          onPointerCancel={onBgUp}
        >
          <div className="erd-tools" role="toolbar" aria-label="Diagram view">
            <button type="button" aria-label="Zoom in" title="Zoom in" onClick={() => zoomAt(1.2)}>+</button>
            <button type="button" aria-label="Zoom out" title="Zoom out" onClick={() => zoomAt(1 / 1.2)}>−</button>
            <button type="button" aria-label="Fit to view" title="Fit all tables in view" onClick={fit}>Fit</button>
            <button type="button" aria-label="Reset layout" title="Put every table back where it was placed automatically" onClick={resetLayout}>
              Reset
            </button>
            <span className="erd-zoom" aria-live="off">{Math.round(view.k * 100)}%</span>
          </div>
          <div className="erd-stage" style={{ width: stage.w, height: stage.h, transform: `translate(${view.x}px, ${view.y}px) scale(${view.k})` }}>
            {frames.map((f) => (
              <div key={f.schema} className="erd-frame" style={{ left: f.x, top: f.y, width: f.w, height: f.h }}>
                <span className="erd-frame-name">{f.schema}</span>
              </div>
            ))}

            <svg className="erd-lines" width={stage.w} height={stage.h} aria-hidden="true">
              {showLinks &&
                lines.map(({ link, d, start, end }) => {
                  const on = active ? link.from.table === active || link.to.table === active : false;
                  const dim = active !== null && !on;
                  const sel = pickedLink === link.id;
                  return (
                    <g key={link.id} className="erd-link" data-on={on || sel} data-dim={dim && !sel} onClick={() => setPickedLink(sel ? null : link.id)}>
                      <path d={d} className="erd-hit" />
                      <path d={d} className="erd-line" />
                      <circle cx={start.x} cy={start.y} r={3.5} className="erd-dot" />
                      <path d={`M ${end.x} ${end.y} l ${end.x >= start.x ? -9 : 9} -5 v 10 z`} className="erd-head" />
                      <title>{link.reason}</title>
                    </g>
                  );
                })}
            </svg>

            {tables.map((t) => {
              const b = boxes[t.key];
              if (!b) return null;
              const { shown, hidden } = visibleColumns(t, links);
              const rel = touching(t.key).length;
              const dim = active !== null && !linked.has(t.key);
              return (
                <article
                  key={t.key}
                  className="erd-card"
                  data-dim={dim}
                  data-active={active === t.key}
                  style={{ left: b.x, top: b.y, width: CARD_W, height: cardHeight(t, links) }}
                  tabIndex={0}
                  aria-label={`Table ${t.schema}.${t.name}, ${t.columns.length} columns, ${rel} inferred relationship${rel === 1 ? "" : "s"}. Arrow keys move it.`}
                  onKeyDown={onCardKey(t.key)}
                  onPointerEnter={() => setActive(t.key)}
                  onPointerLeave={() => setActive((a) => (a === t.key ? null : a))}
                  onFocus={() => setActive(t.key)}
                  onBlur={() => setActive((a) => (a === t.key ? null : a))}
                >
                  <header className="erd-card-head" style={{ height: HEAD_H }} onPointerDown={onCardDown(t.key)} onPointerMove={onCardMove} onPointerUp={onCardUp} onPointerCancel={onCardUp}>
                    <Icon name="table" />
                    <span className="erd-card-name">{t.name}</span>
                    <span className="erd-card-schema">{t.schema}</span>
                  </header>
                  <ul className="erd-cols">
                    {shown.length === 0 && (
                      <li className="erd-col is-note" style={{ height: ROW_H }}>
                        {failedKeys.has(t.key) ? "columns unavailable" : "reading columns…"}
                      </li>
                    )}
                    {shown.map((c) => (
                      <li key={c.name} className="erd-col" style={{ height: ROW_H }} data-pk={c.primaryKey} title={`${c.name} ${c.dataType}${c.nullable ? "" : " not null"}${c.primaryKey ? " primary key" : ""}`}>
                        <span className="erd-key" aria-label={c.primaryKey ? "primary key" : undefined}>{c.primaryKey ? "PK" : ""}</span>
                        <span className="erd-col-name">{c.name}</span>
                        <span className="erd-col-type">{c.dataType}{c.nullable ? "" : "*"}</span>
                      </li>
                    ))}
                    {hidden > 0 && (
                      <li className="erd-col is-note" style={{ height: MORE_H }}>
                        +{hidden} more column{hidden === 1 ? "" : "s"}
                      </li>
                    )}
                  </ul>
                </article>
              );
            })}
          </div>
        </div>
      )}

      {picked && (
        <p className="erd-picked" role="status">
          <strong>{picked.from.table}.{picked.from.column}</strong> → <strong>{picked.to.table}.{picked.to.column}</strong>. {picked.reason}.
        </p>
      )}

      <section className="erd-list" aria-label="Inferred relationships">
        <h2>Inferred relationships</h2>
        {links.length === 0 ? (
          <p className="text-muted">
            None found. A relationship is inferred only when a column is named like another table or its key (for example <code>customer_id</code> next to a
            <code> customers</code> table) and has the same type as that table&apos;s single-column primary key.
          </p>
        ) : (
          <ul>
            {links.map((l) => (
              <li key={l.id} data-testid="erd-link-row" onPointerEnter={() => setActive(l.from.table)} onPointerLeave={() => setActive(null)}>
                <code>{l.from.table}.{l.from.column}</code> → <code>{l.to.table}.{l.to.column}</code>
                <span className="erd-rule">{l.rule === "id-suffix" ? "id column" : l.rule === "table-name" ? "table name" : "same key name"}</span>
              </li>
            ))}
          </ul>
        )}
        <p className="text-muted erd-note">
          rubiXDB has no foreign keys, so these lines are guesses from names and types, never constraints. Nothing here changes your data.
        </p>
      </section>

      {noTablesSchemas.length > 0 && (
        <p className="text-muted erd-note" role="note">
          No tables are listed for: {noTablesSchemas.join(", ")}. (The catalog API currently lists the tables of the default schema only, so tables in other schemas cannot be drawn.)
        </p>
      )}
      {capped && <p className="text-muted erd-note">Showing the first {MAX_TABLES} of {tablesQuery.data?.length} tables.</p>}
      {failedColumns > 0 && <p className="field-error erd-note">Columns could not be read for {failedColumns} table{failedColumns === 1 ? "" : "s"}.</p>}
    </div>
  );
}

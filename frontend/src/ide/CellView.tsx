import { Fragment, useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent } from "react";
import { Icon } from "../components/Icon";
import { SqlEditor } from "../components/console/SqlEditor";
import { OverflowMenu } from "../components/console/OverflowMenu";
import { renderMarkdown } from "./markdown";
import { cellPosition, hasSql } from "./sqlSplit";
import { neededTables, suggest, type ColumnInfo, type SuggestKind, type SuggestResult, type Suggestion } from "./suggest";
import { editorId, useIde } from "./ideContext";
import { formatAge, formatMs } from "./format";
import { isTxn, type IdeCell, type TxnBlock } from "./model";
import { OutputPanel } from "./OutputPanel";
import type { CellRun, RunError } from "./runner";

const STATUS_LABEL: Record<CellRun["status"], string> = {
  idle: "idle",
  queued: "queued",
  running: "running",
  success: "success",
  error: "error",
  cancelled: "cancelled",
};

/** All blocks of the active worksheet, with a "+" row between them and the scratchpad at the end. */
export function CellList() {
  const ide = useIde();
  const { sheet } = ide;
  return (
    <div className="ide-cells">
      {sheet.blocks.length === 0 && (
        <div className="ide-empty">
          <Icon name="sql" />
          <h2>Add a SQL cell to begin.</h2>
          <p className="text-muted">Press ⌘/Ctrl+Enter to run the statement at the cursor, ⌘/Ctrl+Shift+Enter to run all.</p>
        </div>
      )}
      {sheet.blocks.map((b) => (
        <Fragment key={b.id}>
          {isTxn(b) ? <TxnView txn={b} /> : b.kind === "md" ? <MarkdownCell cell={b} /> : <SqlCell cell={b} txnId={null} />}
          <AddRow afterId={b.id} />
        </Fragment>
      ))}
      <AddRow afterId={null} prominent />
      <MarkdownBox label="Scratchpad · markdown" text={sheet.notes} onChange={ide.setNotes} />
    </div>
  );
}

function AddRow({ afterId, prominent = false, inTxn = false }: { afterId: string | null; prominent?: boolean; inTxn?: boolean }) {
  const ide = useIde();
  return (
    <div className="ide-add" data-prominent={prominent} role="group" aria-label={afterId ? "Add a cell here" : "Add a cell at the end"}>
      <button type="button" className="ide-add-btn" onClick={() => ide.addAfter(afterId, "sql")}>
        + SQL cell
      </button>
      {!inTxn && (
        <>
          <button type="button" className="ide-add-btn" onClick={() => ide.addAfter(afterId, "md")}>
            + Markdown
          </button>
          <button type="button" className="ide-add-btn" onClick={() => ide.addTxnAfter(afterId)}>
            + Transaction
          </button>
        </>
      )}
    </div>
  );
}

// ---- SQL cell ---------------------------------------------------------------------------------------

function jumpTarget(detail: string | undefined): { line: number; col: number } | null {
  if (!detail) return null;
  // The server writes "at Line: 1, Column: 8"; other messages say "line 1 col 8".
  const m = /line\W+(\d+)(?:\D+col(?:umn)?\W+(\d+))?/i.exec(detail);
  return m ? { line: Number(m[1]), col: m[2] ? Number(m[2]) : 1 } : null;
}

function ErrorPane({ cellId, error, ms }: { cellId: string; error: RunError; ms?: number }) {
  const ide = useIde();
  const rel = jumpTarget(error.detail);
  // The server reports the position inside the one statement it received; map it back to the cell.
  const target = rel ? (error.statement ? cellPosition(error.statement, rel.line, rel.col) : rel) : null;
  const st = error.statement;
  return (
    <div className="ide-error" role="alert" data-code={error.code}>
      <div className="ide-error-head">
        <span className={`ide-code${error.code === "CANCELLED" ? " is-grey" : ""}`}>{error.code}</span>
        <strong>{error.message}</strong>
        {error.code === "CANCELLED" && <span className="ide-stamp">cancelled</span>}
      </div>
      {st && st.count > 1 && (
        <div className="ide-error-detail">
          Statement {st.index + 1} of {st.count} failed
          {st.index + 1 < st.count ? `; the ${st.count - st.index - 1} after it did not run` : ""}.
        </div>
      )}
      {error.detail && <div className="ide-error-detail">{error.detail}</div>}
      <div className="ide-error-meta text-mono">
        {error.http !== undefined ? `HTTP ${error.http}` : "no HTTP response"}
        {ms !== undefined ? ` · ${formatMs(ms)}` : ""}
        {target && (
          <>
            {" · "}
            <button type="button" className="ide-link" onClick={() => ide.jump(cellId, target.line, target.col)}>
              Jump to line {target.line}
            </button>
          </>
        )}
      </div>
    </div>
  );
}

function SqlCell({ cell, txnId }: { cell: IdeCell; txnId: string | null }) {
  const ide = useIde();
  const run = ide.runs.runs[cell.id];
  const status = run?.status ?? "idle";
  const num = ide.flat.find((f) => f.cell.id === cell.id)?.num ?? 0;
  const session = txnId ? ide.runs.sessions[txnId] : undefined;
  const active = status === "queued" || status === "running";
  const [height, setHeight] = useState<number | null>(null);
  const autoHeight = Math.max(96, Math.min(360, cell.text.split("\n").length * 20 + 28));
  const [comp, setComp] = useState<{ res: SuggestResult; index: number } | null>(null);
  const seq = useRef(0);
  const lastValue = useRef<string | null>(null);
  // Columns of the tables this cell has needed so far (with their types), dropped whenever the catalog is re-read.
  const colsRef = useRef<Record<string, ColumnInfo[]>>({});

  const needsSession = txnId !== null && !session;
  const canRun = hasSql(cell.text) && !active && !needsSession;
  const runTitle = needsSession ? "Begin the transaction first" : "Run the statement at the cursor (Ctrl+Enter). Run all: Ctrl+Shift+Enter";

  const { catalog, fetchColumnInfo, setCursor, autoUpper, catalogUpdatedAt } = ide;
  const cellId = cell.id;
  const catalogSig = catalog.tables.map((t) => `${t.id}:${t.name}`).join(",");
  useEffect(() => {
    colsRef.current = {};
  }, [catalogSig, catalogUpdatedAt]);

  /** Work out the suggestions for the caret; if some table's columns are still unknown, load them and look again. */
  const runSuggest = useCallback(
    (offset: number, value: string, manual = false) => {
      const compute = () => suggest({ text: value, caret: offset, catalog, columns: (t) => colsRef.current[t], manual });
      const show = (res: SuggestResult | null) =>
        setComp((cur) => {
          if (!res) return null;
          const same = (a: SuggestResult, b: SuggestResult) =>
            a.start === b.start && a.end === b.end && a.partial === b.partial && a.items.map((i) => i.label).join("\n") === b.items.map((i) => i.label).join("\n");
          // The same list again (a key-up, a click): keep the row the user moved to.
          if (cur && same(cur.res, res)) return cur;
          return { res, index: res.auto ? -1 : 0 };
        });
      show(compute());
      const missing = neededTables(value, offset, catalog).filter((t) => !colsRef.current[t]);
      const my = ++seq.current;
      if (missing.length) {
        void Promise.all(
          missing.map(async (t) => {
            colsRef.current[t] = await fetchColumnInfo(t);
          }),
        ).then(() => {
          if (my === seq.current) show(compute());
        });
      }
    },
    [catalog, fetchColumnInfo],
  );

  const onCursor = useCallback(
    (offset: number, value: string) => {
      setCursor(cellId, offset, value);
      // Only typing (or an inserted suggestion) opens a list. Moving the caret through unchanged text does not,
      // and closes the list once the caret leaves the word it was for.
      if (lastValue.current === value) {
        setComp((cur) => (cur && offset >= cur.res.start && offset <= cur.res.end ? cur : null));
        return;
      }
      lastValue.current = value;
      runSuggest(offset, value);
    },
    [setCursor, cellId, runSuggest],
  );

  function accept(item: Suggestion) {
    if (!comp) return;
    const { start, end } = comp.res;
    const next = cell.text.slice(0, start) + item.insert + cell.text.slice(end);
    const caret = start + item.insert.length;
    ide.setText(cell.id, next);
    setComp(null);
    window.requestAnimationFrame(() => {
      const ta = document.getElementById(editorId(cell.id)) as HTMLTextAreaElement | null;
      ta?.focus();
      ta?.setSelectionRange(caret, caret);
    });
  }

  /** Words that must keep the case they were typed in: tables, schemas, databases and the columns seen so far. */
  const keepCase = useCallback(
    (w: string) => {
      if (catalog.tables.some((t) => t.name.toLowerCase() === w) || catalog.schemas.some((x) => x.name.toLowerCase() === w) || catalog.databases.some((d) => d.name.toLowerCase() === w)) return true;
      return Object.values(colsRef.current).some((cols) => cols.some((c) => c.name.toLowerCase() === w));
    },
    [catalog],
  );

  function onKeyExtra(e: KeyboardEvent<HTMLTextAreaElement>): boolean {
    const mod = e.ctrlKey || e.metaKey;
    if (mod && e.key === " ") {
      // Ctrl+Space: suggestions on demand, even with nothing typed.
      e.preventDefault();
      const ta = e.currentTarget;
      lastValue.current = ta.value;
      runSuggest(ta.selectionStart, ta.value, true);
      return true;
    }
    if (comp && comp.res.items.length > 0) {
      const n = comp.res.items.length;
      if (e.key === "ArrowDown" || e.key === "ArrowUp") {
        e.preventDefault();
        const down = e.key === "ArrowDown";
        setComp({ ...comp, index: comp.index < 0 ? (down ? 0 : n - 1) : (comp.index + (down ? 1 : n - 1)) % n });
        return true;
      }
      if (e.key === "Tab" && !e.shiftKey) {
        e.preventDefault();
        accept(comp.res.items[Math.max(0, comp.index)]);
        return true;
      }
      if (e.key === "Enter" && !mod && !e.shiftKey) {
        // A list that opened by itself (after FROM, WHERE ...) needs a deliberate choice; Enter then just
        // starts a new line, so typing a query never inserts a name nobody picked.
        if (comp.index >= 0) {
          e.preventDefault();
          accept(comp.res.items[comp.index]);
          return true;
        }
        setComp(null);
        return false;
      }
      if (e.key === "Escape") {
        e.preventDefault();
        setComp(null);
        return true;
      }
    }
    if (e.key === "Escape") {
      e.preventDefault();
      e.currentTarget.blur();
      document.getElementById(`hd-${cell.id}`)?.focus();
      return true;
    }
    if (mod && e.shiftKey && e.key === "Enter") {
      e.preventDefault();
      ide.runAll();
      return true;
    }
    if (!mod && e.shiftKey && e.key === "Enter") {
      e.preventDefault();
      ide.addAfter(cell.id, "sql");
      return true;
    }
    return false;
  }

  // Where the statement that failed went wrong, drawn in the editor. Only while the text is still what ran.
  const errorMark = useMemo(() => {
    const err = run?.error;
    const st = err?.statement;
    if (status !== "error" || !err || !st) return null;
    const lines = cell.text.split("\n");
    let offset = 0;
    for (let i = 0; i < st.startLine - 1; i++) offset += (lines[i]?.length ?? 0) + 1;
    offset += st.startCol - 1;
    if (cell.text.slice(offset, offset + st.text.length) !== st.text) return null; // edited since: no stale marker
    const rel = jumpTarget(err.detail);
    const pos = rel ? cellPosition(st, rel.line, rel.col) : { line: st.startLine, col: st.startCol };
    const lineText = lines[pos.line - 1];
    if (lineText === undefined) return null;
    const word = /^[A-Za-z0-9_]+|^\S/.exec(lineText.slice(pos.col - 1));
    return { line: pos.line, col: pos.col, len: word ? word[0].length : 1, message: `${err.code}: ${err.message}` };
  }, [run?.error, status, cell.text]);

  function onHeaderKey(e: KeyboardEvent<HTMLElement>) {
    if (e.target !== e.currentTarget) return;
    if (e.key === "ArrowUp" || e.key === "ArrowDown") {
      e.preventDefault();
      ide.focusHeader(cell.id, e.key === "ArrowUp" ? -1 : 1);
    } else if (e.key === "Enter" && !(e.ctrlKey || e.metaKey)) {
      e.preventDefault();
      document.getElementById(editorId(cell.id))?.focus();
    }
  }

  const blockedBy = run?.blockedBy;
  const blockedNum = blockedBy ? ide.flat.find((f) => f.cell.id === blockedBy.cellId)?.num : undefined;
  const elapsed = run?.startedAt ? Math.max(0, ide.now - run.startedAt) : 0;

  return (
    <section
      className="ide-cell"
      data-status={status}
      data-focused={ide.focusedId === cell.id}
      aria-label={`Cell ${num}`}
      onFocusCapture={() => ide.setFocused(cell.id)}
    >
      <header className="ide-cell-head" id={`hd-${cell.id}`} tabIndex={0} onKeyDown={onHeaderKey} aria-label={`Cell ${num} header, ${STATUS_LABEL[status]}`}>
        {txnId === null ? (
          <select
            className="ide-kind"
            aria-label={`Cell ${num} type`}
            value="sql"
            onChange={(e) => ide.setKind(cell.id, e.target.value === "md" ? "md" : "sql")}
          >
            <option value="sql">SQL</option>
            <option value="md">Markdown</option>
          </select>
        ) : (
          <span className="ide-kind-static">SQL</span>
        )}
        <span className="ide-cellnum text-mono">cell #{num}</span>
        <span className="ide-status" data-status={status}>
          <i className="ide-dot" aria-hidden="true" />
          {STATUS_LABEL[status]}
        </span>
        <span className="ide-spacer" />
        {status === "running" ? (
          <>
            <span className="ide-spinner" aria-hidden="true" />
            <span className="ide-elapsed text-mono">{formatMs(elapsed)}</span>
            <button type="button" className="ide-run is-stop" onClick={() => ide.cancel(cell.id)} aria-label={`Cancel cell ${num}`} title="Cancel this cell">
              <Icon name="stop" /> Cancel
            </button>
          </>
        ) : (
          <>
            <button type="button" className="ide-run" disabled={!canRun} onClick={() => ide.runCell(cell.id)} aria-label={`Run cell ${num}`} title={runTitle}>
              <Icon name="play" /> Run
            </button>
            {run?.ms !== undefined && status !== "queued" && <span className="ide-elapsed text-mono">{formatMs(run.ms)}</span>}
          </>
        )}
        <OverflowMenu
          items={[
            { id: "dup", label: "Duplicate cell", onSelect: () => ide.duplicate(cell.id) },
            { id: "collapse", label: cell.collapsed ? "Expand editor" : "Collapse editor", onSelect: () => ide.toggleCollapse(cell.id) },
            { id: "above", label: "Run all cells above", disabled: txnId !== null, onSelect: () => ide.runAbove(cell.id) },
          ]}
        />
        <button type="button" className="ide-iconbtn" aria-label={`Move cell ${num} up`} onClick={() => ide.move(cell.id, -1)}>
          ▲
        </button>
        <button type="button" className="ide-iconbtn" aria-label={`Move cell ${num} down`} onClick={() => ide.move(cell.id, 1)}>
          ▼
        </button>
        <button type="button" className="ide-iconbtn" aria-label={`Delete cell ${num}`} onClick={() => ide.deleteBlock(cell.id)}>
          <Icon name="close" />
        </button>
      </header>
      {status === "running" && <div className="ide-progress" aria-hidden="true" />}

      {!cell.collapsed && (
        <div className="ide-editor">
          <SqlEditor
            id={editorId(cell.id)}
            ariaLabel={`Cell ${num} SQL`}
            value={cell.text}
            onChange={(v) => ide.setText(cell.id, v)}
            onRun={() => ide.runCell(cell.id)}
            height={height ?? autoHeight}
            onHeightChange={setHeight}
            onKeyExtra={onKeyExtra}
            onCursor={onCursor}
            autoUppercase={autoUpper}
            keepCase={keepCase}
            errorMark={errorMark}
            onBlur={() => setComp(null)}
            popup={comp && comp.res.items.length > 0 ? <SuggestList comp={comp} onPick={accept} /> : null}
            footerLeft={
              <span className="ide-mode" data-mode={txnId ? "txn" : ide.state.autocommit ? "auto" : "implicit"}>
                {txnId ? "in transaction" : ide.state.autocommit ? "autocommit" : "worksheet transaction"}
              </span>
            }
          />
        </div>
      )}
      {needsSession && status === "idle" && <p className="ide-hint">Begin the transaction above to run this cell.</p>}

      {status === "queued" && (
        <div className="ide-notice" role="status">
          <i className="ide-dot" aria-hidden="true" /> Queued · waiting for a free request slot
        </div>
      )}
      {status === "running" && (
        <div className="ide-notice" role="status">
          <span className="ide-spinner" aria-hidden="true" /> Request in flight
          {run?.statementCount && run.statementCount > 1 ? ` · statement ${(run.current ?? 0) + 1} of ${run.statementCount}` : ""} · {formatMs(elapsed)} · no response yet
        </div>
      )}
      {blockedBy && (
        <div className="ide-notice is-warn" role="status">
          Blocked: session {blockedBy.sessionId} is running in cell #{blockedNum ?? "?"}.
        </div>
      )}
      {(status === "error" || status === "cancelled") && run?.error && <ErrorPane cellId={cell.id} error={run.error} ms={run.ms} />}
      {status === "success" && run && <OutputPanel cell={cell} run={run} />}
    </section>
  );
}

const KIND_MARK: Record<SuggestKind, string> = { table: "T", column: "C", schema: "S", database: "D", keyword: "K", function: "ƒ", snippet: "›" };
const KIND_NAME: Record<SuggestKind, string> = { table: "table", column: "column", schema: "schema", database: "database", keyword: "keyword", function: "function", snippet: "statement" };

function SuggestList({ comp, onPick }: { comp: { res: SuggestResult; index: number }; onPick: (item: Suggestion) => void }) {
  const { items, partial } = comp.res;
  return (
    <div className="ide-comp">
      <ul role="listbox" aria-label="Suggestions">
        {items.map((it, i) => {
          const typed = it.label.toLowerCase().startsWith(partial.toLowerCase()) ? partial.length : 0;
          return (
            <li
              key={`${it.kind}:${it.label}`}
              role="option"
              aria-label={`${it.label}, ${KIND_NAME[it.kind]}${it.detail ? `, ${it.detail}` : ""}`}
              aria-selected={i === comp.index}
              data-active={i === comp.index}
              onMouseDown={(e) => {
                e.preventDefault();
                onPick(it);
              }}
            >
              <span className="ide-comp-kind" data-kind={it.kind} title={KIND_NAME[it.kind]} aria-hidden="true">
                {KIND_MARK[it.kind]}
              </span>
              <span className="ide-comp-label">
                <b>{it.label.slice(0, typed)}</b>
                {it.label.slice(typed)}
              </span>
              {it.detail && <span className="ide-comp-detail">{it.detail}</span>}
            </li>
          );
        })}
      </ul>
      <div className="ide-comp-hint" aria-hidden="true">
        ↑↓ choose · Tab insert · Esc close
      </div>
    </div>
  );
}

// ---- markdown --------------------------------------------------------------------------------------

function MarkdownBox({
  label,
  text,
  onChange,
  onClose,
  onKind,
}: {
  label: string;
  text: string;
  onChange: (t: string) => void;
  onClose?: () => void;
  onKind?: (k: "sql" | "md") => void;
}) {
  const [editing, setEditing] = useState(text === "" && onClose !== undefined);
  return (
    <section className="ide-cell ide-md-cell" data-status="idle" aria-label={label}>
      <header className="ide-cell-head">
        {onKind ? (
          <select className="ide-kind" aria-label={`${label} type`} value="md" onChange={(e) => onKind(e.target.value === "sql" ? "sql" : "md")}>
            <option value="sql">SQL</option>
            <option value="md">Markdown</option>
          </select>
        ) : (
          <span className="ide-kind-static">MD</span>
        )}
        <span className="ide-cellnum">{label}</span>
        <span className="ide-spacer" />
        <button type="button" className="ide-run" onClick={() => setEditing((v) => !v)}>
          {editing ? "Done" : "Edit"}
        </button>
        {onClose && (
          <button type="button" className="ide-iconbtn" aria-label={`Delete ${label}`} onClick={onClose}>
            <Icon name="close" />
          </button>
        )}
      </header>
      {editing ? (
        <textarea
          className="ide-md-edit text-mono"
          aria-label={label}
          value={text}
          autoFocus
          rows={Math.max(3, text.split("\n").length + 1)}
          placeholder="Write notes in Markdown"
          onChange={(e) => onChange(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Escape" || ((e.ctrlKey || e.metaKey) && e.key === "Enter")) {
              e.preventDefault();
              setEditing(false);
            }
          }}
        />
      ) : (
        <div className="ide-md" onDoubleClick={() => setEditing(true)} title="Double-click to edit">
          {text.trim() === "" ? <p className="text-muted">Empty. Double-click to edit.</p> : renderMarkdown(text)}
        </div>
      )}
    </section>
  );
}

function MarkdownCell({ cell }: { cell: IdeCell }) {
  const ide = useIde();
  const num = ide.flat.find((f) => f.cell.id === cell.id)?.num ?? 0;
  return <MarkdownBox label={`Markdown cell #${num}`} text={cell.text} onChange={(t) => ide.setText(cell.id, t)} onClose={() => ide.deleteBlock(cell.id)} onKind={(k) => ide.setKind(cell.id, k)} />;
}

// ---- transaction container --------------------------------------------------------------------------

function TxnView({ txn }: { txn: TxnBlock }) {
  const ide = useIde();
  const session = ide.runs.sessions[txn.id];
  const notice = ide.runs.notices[txn.id];
  const otherOpen = Object.keys(ide.runs.sessions).some((k) => k !== txn.id);
  const busy = !!session?.busyCell;
  const anySql = txn.cells.some((c) => hasSql(c.text));
  return (
    <section className="ide-txn" data-open={!!session} aria-label="Transaction">
      <header className="ide-txn-head">
        <Icon name="snapshots" />
        <strong>Transaction</strong>
        {session ? (
          <>
            <span className="ide-session text-mono" title="Server-side session id">
              session {session.sessionId}
            </span>
            <span className="ide-badge is-open">open · {formatAge((ide.now - session.startedAt) / 1000)}</span>
          </>
        ) : (
          <span className="ide-badge">not started</span>
        )}
        <span className="ide-spacer" />
        <button type="button" className="btn btn-secondary" disabled={!!session || otherOpen} title={otherOpen ? "Another transaction is open in this tab" : "Start the transaction (BEGIN)"} onClick={() => ide.beginTxn(txn.id)}>
          Begin
        </button>
        <button type="button" className="btn btn-secondary" disabled={!session || busy || !anySql} onClick={() => ide.runSteps(txn.id)} title="Run the cells below in order, stopping at the first failure">
          Run steps
        </button>
        <button type="button" className="btn btn-primary" disabled={!session || busy} onClick={() => ide.finishTxn(txn.id, "COMMIT")}>
          Commit
        </button>
        <button type="button" className="btn btn-danger" disabled={!session || busy} onClick={() => ide.finishTxn(txn.id, "ROLLBACK")}>
          Rollback
        </button>
        <button type="button" className="ide-iconbtn" aria-label="Delete transaction container" onClick={() => ide.deleteBlock(txn.id)}>
          <Icon name="close" />
        </button>
      </header>
      <p className="ide-txn-note">Cells in this container run one at a time on one session.</p>
      {notice && (
        <div className="ide-notice is-warn" role="status">
          {notice}
        </div>
      )}
      {txn.cells.map((c) => (
        <Fragment key={c.id}>
          <SqlCell cell={c} txnId={txn.id} />
          <AddRow afterId={c.id} inTxn />
        </Fragment>
      ))}
    </section>
  );
}

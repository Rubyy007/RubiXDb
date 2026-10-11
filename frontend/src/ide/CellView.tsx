import { Fragment, useCallback, useRef, useState, type KeyboardEvent } from "react";
import { Icon } from "../components/Icon";
import { SqlEditor } from "../components/console/SqlEditor";
import { OverflowMenu } from "../components/console/OverflowMenu";
import { renderMarkdown } from "./markdown";
import { applyCompletion, completionContext, filterItems, resolveBase, type CompletionContext } from "./completion";
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
          <p className="text-muted">Press ⌘/Ctrl+Enter to run.</p>
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
  const m = /line\s+(\d+)(?:\D+col(?:umn)?\s+(\d+))?/i.exec(detail);
  return m ? { line: Number(m[1]), col: m[2] ? Number(m[2]) : 1 } : null;
}

function ErrorPane({ cellId, error, ms }: { cellId: string; error: RunError; ms?: number }) {
  const ide = useIde();
  const target = jumpTarget(error.detail);
  return (
    <div className="ide-error" role="alert" data-code={error.code}>
      <div className="ide-error-head">
        <span className={`ide-code${error.code === "CANCELLED" ? " is-grey" : ""}`}>{error.code}</span>
        <strong>{error.message}</strong>
        {error.code === "CANCELLED" && <span className="ide-stamp">cancelled</span>}
      </div>
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
  const [comp, setComp] = useState<{ ctx: CompletionContext; items: string[]; index: number } | null>(null);
  const seq = useRef(0);

  const needsSession = txnId !== null && !session;
  const canRun = cell.text.trim() !== "" && !active && !needsSession;
  const runTitle = needsSession ? "Begin the transaction first" : "Run cell (Ctrl+Enter)";

  const { catalog, fetchColumns } = ide;
  const onCursor = useCallback(
    (offset: number, value: string) => {
      const ctx = completionContext(value, offset);
      const resolved = ctx ? resolveBase(ctx.base, catalog) : null;
      if (!ctx || !resolved) {
        setComp(null);
        return;
      }
      const show = (items: string[]) => setComp(items.length ? { ctx, items: filterItems(items, ctx.partial), index: 0 } : null);
      const my = ++seq.current;
      if (resolved.kind === "columns") {
        void fetchColumns(resolved.table).then((cols) => {
          if (my === seq.current) show(cols);
        });
      } else {
        show(resolved.items);
      }
    },
    [catalog, fetchColumns],
  );

  function applyChoice(choice: string) {
    if (!comp) return;
    const r = applyCompletion(cell.text, comp.ctx, choice);
    ide.setText(cell.id, r.text);
    setComp(null);
    window.requestAnimationFrame(() => {
      const ta = document.getElementById(editorId(cell.id)) as HTMLTextAreaElement | null;
      ta?.focus();
      ta?.setSelectionRange(r.caret, r.caret);
    });
  }

  function onKeyExtra(e: KeyboardEvent<HTMLTextAreaElement>): boolean {
    const mod = e.ctrlKey || e.metaKey;
    if (comp && comp.items.length > 0) {
      if (e.key === "ArrowDown" || e.key === "ArrowUp") {
        e.preventDefault();
        const n = comp.items.length;
        setComp({ ...comp, index: (comp.index + (e.key === "ArrowDown" ? 1 : n - 1)) % n });
        return true;
      }
      if ((e.key === "Enter" && !mod && !e.shiftKey) || e.key === "Tab") {
        e.preventDefault();
        applyChoice(comp.items[comp.index]);
        return true;
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
            footerLeft={
              <span className="ide-mode" data-mode={txnId ? "txn" : ide.state.autocommit ? "auto" : "implicit"}>
                {txnId ? "in transaction" : ide.state.autocommit ? "autocommit" : "worksheet transaction"}
              </span>
            }
          />
          {comp && comp.items.length > 0 && (
            <ul className="ide-comp" role="listbox" aria-label="Completions">
              {comp.items.map((it, i) => (
                <li key={it} role="option" aria-selected={i === comp.index} data-active={i === comp.index} onMouseDown={(e) => { e.preventDefault(); applyChoice(it); }}>
                  {it}
                </li>
              ))}
            </ul>
          )}
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
          <span className="ide-spinner" aria-hidden="true" /> Request in flight · {formatMs(elapsed)} · no response yet
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
  const hasSql = txn.cells.some((c) => c.text.trim() !== "");
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
        <button type="button" className="btn btn-secondary" disabled={!session || busy || !hasSql} onClick={() => ide.runSteps(txn.id)} title="Run the cells below in order, stopping at the first failure">
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

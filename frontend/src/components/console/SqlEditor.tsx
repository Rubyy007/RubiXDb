import {
  useCallback,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type KeyboardEvent,
  type PointerEvent as ReactPointerEvent,
  type ReactNode,
} from "react";
import { caretPosition, toggleLineComment, tokenize } from "../../utils/sqlHighlight";

const LINE_HEIGHT = 20;
const MIN_HEIGHT = 96;

interface SqlEditorProps {
  value: string;
  onChange: (value: string) => void;
  onRun: () => void;
  height: number;
  onHeightChange: (h: number) => void;
  /** Rendered at the left of the footer (the autocommit / transaction pill). */
  footerLeft: ReactNode;
}

/** Plain <textarea> (the real, accessible input, labelled "SQL") on top of a
 * highlight layer. The layer draws tokens as React text nodes -- never HTML.
 * No editor dependency. */
export function SqlEditor({ value, onChange, onRun, height, onHeightChange, footerLeft }: SqlEditorProps) {
  const taRef = useRef<HTMLTextAreaElement>(null);
  const hlRef = useRef<HTMLPreElement>(null);
  const gutterRef = useRef<HTMLDivElement>(null);
  const pendingSel = useRef<{ start: number; end: number } | null>(null);
  const [caret, setCaret] = useState({ line: 1, col: 1 });

  const tokens = useMemo(() => tokenize(value), [value]);
  const lineCount = useMemo(() => value.split("\n").length, [value]);
  const gutterText = useMemo(() => Array.from({ length: lineCount }, (_, i) => i + 1).join("\n"), [lineCount]);

  const syncCaret = useCallback(() => {
    const ta = taRef.current;
    if (!ta) return;
    setCaret(caretPosition(ta.value, ta.selectionStart));
  }, []);

  const syncScroll = useCallback(() => {
    const ta = taRef.current;
    if (!ta) return;
    if (hlRef.current) {
      hlRef.current.scrollTop = ta.scrollTop;
      hlRef.current.scrollLeft = ta.scrollLeft;
    }
    if (gutterRef.current) gutterRef.current.scrollTop = ta.scrollTop;
  }, []);

  // Restore the selection after a programmatic edit (e.g. comment toggle).
  useLayoutEffect(() => {
    const sel = pendingSel.current;
    const ta = taRef.current;
    if (sel && ta) {
      ta.setSelectionRange(sel.start, sel.end);
      pendingSel.current = null;
      syncCaret();
    }
  }, [value, syncCaret]);

  function onKeyDown(e: KeyboardEvent<HTMLTextAreaElement>) {
    const mod = e.ctrlKey || e.metaKey;
    if (mod && e.key === "Enter") {
      e.preventDefault();
      onRun();
    } else if (mod && e.key === "/") {
      e.preventDefault();
      const ta = e.currentTarget;
      const r = toggleLineComment(ta.value, ta.selectionStart, ta.selectionEnd);
      pendingSel.current = { start: r.start, end: r.end };
      onChange(r.text);
    }
  }

  // --- resize handle (pointer drag + arrow keys) ---
  const drag = useRef<{ y: number; h: number } | null>(null);
  const maxHeight = () => Math.max(MIN_HEIGHT + 40, Math.floor(window.innerHeight * 0.6));
  const clamp = (h: number) => Math.max(MIN_HEIGHT, Math.min(maxHeight(), Math.round(h)));

  function onHandleDown(e: ReactPointerEvent<HTMLDivElement>) {
    drag.current = { y: e.clientY, h: height };
    e.currentTarget.setPointerCapture(e.pointerId);
  }
  function onHandleMove(e: ReactPointerEvent<HTMLDivElement>) {
    if (drag.current) onHeightChange(clamp(drag.current.h + e.clientY - drag.current.y));
  }
  function onHandleUp() {
    drag.current = null;
  }
  function onHandleKey(e: KeyboardEvent<HTMLDivElement>) {
    if (e.key === "ArrowUp") {
      e.preventDefault();
      onHeightChange(clamp(height - 24));
    } else if (e.key === "ArrowDown") {
      e.preventDefault();
      onHeightChange(clamp(height + 24));
    }
  }

  return (
    <div className="sqled-wrap">
      <div className="sqled" style={{ height }}>
        <div className="sqled-gutter" ref={gutterRef} aria-hidden="true">
          <pre>{gutterText}</pre>
        </div>
        <div className="sqled-body">
          <pre className="sqled-hl" ref={hlRef} aria-hidden="true">
            <span className="sqled-band" style={{ top: 12 + (caret.line - 1) * LINE_HEIGHT }} />
            <code>
              {tokens.map((t, i) =>
                t.kind === "plain" ? t.text : (
                  <span key={i} className={`tok-${t.kind}`}>
                    {t.text}
                  </span>
                ),
              )}
              {"\n"}
            </code>
          </pre>
          <label htmlFor="sql-editor" className="visually-hidden">
            SQL
          </label>
          <textarea
            id="sql-editor"
            ref={taRef}
            className="sqled-input"
            value={value}
            spellCheck={false}
            autoCapitalize="off"
            autoCorrect="off"
            wrap="off"
            placeholder="SELECT * FROM t WHERE id = 1"
            onChange={(e) => {
              onChange(e.target.value);
              setCaret(caretPosition(e.target.value, e.target.selectionStart));
            }}
            onKeyDown={onKeyDown}
            onKeyUp={syncCaret}
            onClick={syncCaret}
            onSelect={syncCaret}
            onScroll={syncScroll}
          />
        </div>
      </div>
      <div
        className="sqled-resize"
        role="separator"
        aria-orientation="horizontal"
        aria-label="Resize editor"
        aria-valuenow={height}
        aria-valuemin={MIN_HEIGHT}
        aria-valuemax={maxHeight()}
        tabIndex={0}
        onPointerDown={onHandleDown}
        onPointerMove={onHandleMove}
        onPointerUp={onHandleUp}
        onPointerCancel={onHandleUp}
        onKeyDown={onHandleKey}
      />
      <div className="sqled-footer">
        <div className="sqled-footer-left">{footerLeft}</div>
        <div className="sqled-footer-right">
          <span>
            Ln {caret.line}, Col {caret.col}
          </span>
          <span>{value.length.toLocaleString()} chars</span>
        </div>
      </div>
    </div>
  );
}

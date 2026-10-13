import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type KeyboardEvent,
  type PointerEvent as ReactPointerEvent,
  type ReactNode,
} from "react";
import { caretPosition, toggleLineComment, tokenize } from "../../utils/sqlHighlight";
import { upperEdit } from "../../ide/autoUpper";

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
  /** DOM id of the textarea (default "sql-editor"). The SQL IDE renders many editors, so each needs its own. */
  id?: string;
  /** Accessible name of the textarea (default "SQL"). */
  ariaLabel?: string;
  readOnly?: boolean;
  /** Runs before the built-in keys; return true to mark the event handled. */
  onKeyExtra?: (e: KeyboardEvent<HTMLTextAreaElement>) => boolean;
  /** Called with the caret offset whenever it may have moved. */
  onCursor?: (offset: number, value: string) => void;
  /** Turn a keyword typed in lower case into UPPER CASE when the word is finished (the IDE turns this on). */
  autoUppercase?: boolean;
  /** Words (lower case) that must never be uppercased, e.g. the names of tables and columns. */
  keepCase?: (word: string) => boolean;
  /** Marks where the statement that failed went wrong: 1-based line and column, and how many characters. */
  errorMark?: { line: number; col: number; len: number; message?: string } | null;
  /** A list drawn under the caret (the IDE's suggestions). */
  popup?: ReactNode;
  /** Called when the editor loses focus. */
  onBlur?: () => void;
}

/** Plain <textarea> (the real, accessible input, labelled "SQL") on top of a
 * highlight layer. The layer draws tokens as React text nodes -- never HTML.
 * No editor dependency. */
export function SqlEditor({
  value,
  onChange,
  onRun,
  height,
  onHeightChange,
  footerLeft,
  id = "sql-editor",
  ariaLabel = "SQL",
  readOnly = false,
  onKeyExtra,
  onCursor,
  autoUppercase = false,
  keepCase,
  errorMark = null,
  popup,
  onBlur,
}: SqlEditorProps) {
  const taRef = useRef<HTMLTextAreaElement>(null);
  const hlRef = useRef<HTMLPreElement>(null);
  const gutterRef = useRef<HTMLDivElement>(null);
  const pendingSel = useRef<{ start: number; end: number } | null>(null);
  const [caret, setCaret] = useState({ line: 1, col: 1 });
  const [charW, setCharW] = useState(7.8);
  const measureRef = useRef<HTMLSpanElement>(null);
  const popupRef = useRef<HTMLDivElement>(null);
  const [anchor, setAnchor] = useState<{ left: number; top: number } | null>(null);

  const tokens = useMemo(() => tokenize(value), [value]);
  const lineCount = useMemo(() => value.split("\n").length, [value]);
  const gutterText = useMemo(() => Array.from({ length: lineCount }, (_, i) => i + 1).join("\n"), [lineCount]);

  // Width of one character of the editor font, for placing the error underline and the suggestion list.
  useLayoutEffect(() => {
    const w = measureRef.current?.getBoundingClientRect().width;
    if (w && w > 0) setCharW(w / 10);
  }, []);

  const syncCaret = useCallback(() => {
    const ta = taRef.current;
    if (!ta) return;
    setCaret(caretPosition(ta.value, ta.selectionStart));
    onCursor?.(ta.selectionStart, ta.value);
  }, [onCursor]);

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

  // The suggestion list is placed in WINDOW coordinates from the caret's real screen position, so a cell that
  // clips its content cannot cut it off. It stays inside the window and flips above the line when there is no
  // room below. It is placed again when the caret moves, when the text area scrolls, when the window changes
  // and when the list changes size.
  const hasPopup = Boolean(popup);
  useLayoutEffect(() => {
    if (!hasPopup) {
      setAnchor((a) => (a === null ? a : null));
      return;
    }
    const place = () => {
      const ta = taRef.current;
      if (!ta) return;
      const r = ta.getBoundingClientRect();
      const lineTop = r.top + 12 + (caret.line - 1) * LINE_HEIGHT - ta.scrollTop;
      const x = r.left + 16 + (caret.col - 1) * charW - ta.scrollLeft;
      const w = popupRef.current?.offsetWidth ?? 300;
      const h = popupRef.current?.offsetHeight ?? 220;
      const left = Math.max(8, Math.min(x, window.innerWidth - w - 8));
      let top = lineTop + LINE_HEIGHT + 2;
      if (top + h > window.innerHeight - 8 && lineTop - h - 2 > 8) top = lineTop - h - 2;
      setAnchor((a) => (a && Math.abs(a.left - left) < 0.5 && Math.abs(a.top - top) < 0.5 ? a : { left, top }));
    };
    place();
    const ta = taRef.current;
    const ro = typeof ResizeObserver === "undefined" ? null : new ResizeObserver(place);
    if (ro && popupRef.current) ro.observe(popupRef.current);
    ta?.addEventListener("scroll", place);
    window.addEventListener("scroll", place, true);
    window.addEventListener("resize", place);
    return () => {
      ro?.disconnect();
      ta?.removeEventListener("scroll", place);
      window.removeEventListener("scroll", place, true);
      window.removeEventListener("resize", place);
    };
  }, [hasPopup, caret.line, caret.col, charW]);

  // Auto-uppercase. A native `beforeinput` listener sees the separator (space, newline, "(" ...) BEFORE it is
  // inserted, so the finished word is replaced first and the caret never moves. The replacement goes through
  // `execCommand("insertText")` so the browser's own undo still works; where that is unavailable the same
  // edit is made through `onChange` instead.
  const autoRef = useRef({ on: autoUppercase, keepCase, onChange });
  autoRef.current = { on: autoUppercase, keepCase, onChange };
  useEffect(() => {
    const ta = taRef.current;
    if (!ta) return;
    const onBefore = (ev: Event) => {
      const e = ev as InputEvent;
      const { on, keepCase: keep, onChange: change } = autoRef.current;
      if (!on || e.isComposing || ta.readOnly || ta.selectionStart !== ta.selectionEnd) return;
      let typed: string | null = null;
      if (e.inputType === "insertText" && e.data && e.data.length === 1) typed = e.data;
      else if (e.inputType === "insertLineBreak" || e.inputType === "insertParagraph") typed = "\n";
      if (typed === null) return;
      const caretAt = ta.selectionStart;
      const edit = upperEdit(ta.value, caretAt, typed, keep);
      if (!edit) return;
      ta.setSelectionRange(edit.start, edit.end);
      let done = false;
      try {
        done = typeof document.execCommand === "function" && document.execCommand("insertText", false, edit.upper);
      } catch {
        done = false;
      }
      if (done) {
        ta.setSelectionRange(caretAt, caretAt); // same length: the caret is where it was
        return;
      }
      // Fallback: make the whole edit (the word and the typed character) ourselves.
      e.preventDefault();
      const next = ta.value.slice(0, edit.start) + edit.upper + ta.value.slice(edit.end, caretAt) + typed + ta.value.slice(caretAt);
      pendingSel.current = { start: caretAt + typed.length, end: caretAt + typed.length };
      change(next);
    };
    ta.addEventListener("beforeinput", onBefore);
    return () => ta.removeEventListener("beforeinput", onBefore);
  }, []);

  function onKeyDown(e: KeyboardEvent<HTMLTextAreaElement>) {
    if (onKeyExtra?.(e)) return;
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
          {errorMark && errorMark.line <= lineCount ? (
            <pre>
              {Array.from({ length: lineCount }, (_, i) => (
                <span key={i} className={i + 1 === errorMark.line ? "sqled-ln-err" : undefined}>
                  {i + 1}
                  {i + 1 < lineCount ? "\n" : ""}
                </span>
              ))}
            </pre>
          ) : (
            <pre>{gutterText}</pre>
          )}
        </div>
        <div className="sqled-body">
          <pre className="sqled-hl" ref={hlRef} aria-hidden="true">
            <span className="sqled-band" style={{ top: 12 + (caret.line - 1) * LINE_HEIGHT }} />
            {errorMark && errorMark.line <= lineCount && (
              <>
                <span className="sqled-errband" style={{ top: 12 + (errorMark.line - 1) * LINE_HEIGHT }} />
                <span
                  className="sqled-squiggle"
                  title={errorMark.message}
                  style={{ top: 12 + (errorMark.line - 1) * LINE_HEIGHT + 16, left: `calc(16px + ${errorMark.col - 1}ch)`, width: `${Math.max(1, errorMark.len)}ch` }}
                />
              </>
            )}
            <span className="sqled-measure" ref={measureRef} aria-hidden="true">
              0000000000
            </span>
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
          <label htmlFor={id} className="visually-hidden">
            {ariaLabel}
          </label>
          <textarea
            id={id}
            readOnly={readOnly}
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
              onCursor?.(e.target.selectionStart, e.target.value);
            }}
            onKeyDown={onKeyDown}
            onKeyUp={syncCaret}
            onClick={syncCaret}
            onSelect={syncCaret}
            onScroll={syncScroll}
            onBlur={onBlur}
          />
          {popup && (
            <div ref={popupRef} className="sqled-popup" style={{ left: anchor?.left ?? -9999, top: anchor?.top ?? -9999 }}>
              {popup}
            </div>
          )}
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

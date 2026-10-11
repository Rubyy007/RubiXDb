import { useEffect, useMemo, useRef, useState, type KeyboardEvent } from "react";
import { filterActions, type PaletteAction } from "./palette";

interface Props {
  title: string;
  placeholder: string;
  actions: PaletteAction[];
  onClose: () => void;
}

/** Fuzzy action list opened with Ctrl+K / Ctrl+Shift+P (commands) or Ctrl+P (worksheets). Every
 * entry is a local action; opening the palette makes no request. */
export function CommandPalette({ title, placeholder, actions, onClose }: Props) {
  const [query, setQuery] = useState("");
  const [index, setIndex] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);
  const shown = useMemo(() => filterActions(actions, query), [actions, query]);
  const active = Math.min(index, Math.max(0, shown.length - 1));

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  function choose(a: PaletteAction | undefined) {
    if (!a || a.disabled) return;
    onClose();
    a.run();
  }

  function onKeyDown(e: KeyboardEvent<HTMLDivElement>) {
    if (e.key === "Escape") {
      e.preventDefault();
      onClose();
    } else if (e.key === "ArrowDown") {
      e.preventDefault();
      setIndex(Math.min(active + 1, shown.length - 1));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setIndex(Math.max(active - 1, 0));
    } else if (e.key === "Enter") {
      e.preventDefault();
      choose(shown[active]);
    }
  }

  return (
    <div className="ide-pal-scrim" onMouseDown={onClose}>
      <div
        className="ide-pal"
        role="dialog"
        aria-modal="true"
        aria-label={title}
        onMouseDown={(e) => e.stopPropagation()}
        onKeyDown={onKeyDown}
      >
        <div className="ide-pal-input">
          <input
            ref={inputRef}
            type="text"
            value={query}
            placeholder={placeholder}
            aria-label={title}
            role="combobox"
            aria-expanded="true"
            aria-controls="ide-pal-list"
            aria-activedescendant={shown[active] ? `ide-pal-${shown[active].id}` : undefined}
            onChange={(e) => {
              setQuery(e.target.value);
              setIndex(0);
            }}
          />
          <kbd>Esc</kbd>
        </div>
        <ul id="ide-pal-list" role="listbox" aria-label="Actions">
          {shown.length === 0 && <li className="ide-pal-empty">No matching action</li>}
          {shown.map((a, i) => (
            <li
              key={a.id}
              id={`ide-pal-${a.id}`}
              role="option"
              aria-selected={i === active}
              aria-disabled={a.disabled || undefined}
              className="ide-pal-row"
              data-active={i === active}
              onMouseEnter={() => setIndex(i)}
              onClick={() => choose(a)}
            >
              <span>{a.label}</span>
              <span className="ide-pal-group">{a.group}</span>
              {a.shortcut && <kbd>{a.shortcut}</kbd>}
            </li>
          ))}
        </ul>
        <div className="ide-pal-foot">
          <span>↑ ↓ navigate</span>
          <span>↵ run</span>
          <span>Esc close</span>
        </div>
      </div>
    </div>
  );
}

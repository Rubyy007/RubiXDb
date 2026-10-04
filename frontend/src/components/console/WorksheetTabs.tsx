import type { KeyboardEvent } from "react";
import { Icon } from "../Icon";
import type { Worksheet } from "../../utils/worksheets";

interface Props {
  tabs: Worksheet[];
  activeId: string;
  dirty: ReadonlySet<string>;
  canAdd: boolean;
  onSelect: (id: string) => void;
  onClose: (id: string) => void;
  onAdd: () => void;
}

/** Worksheet tab strip. A tablist may contain only tabs, so the "x" is a decorative
 * mouse target inside the tab; the keyboard/AT ways to close are the Delete key on a
 * focused tab and "Close worksheet" in the overflow menu. Names render as text nodes. */
export function WorksheetTabs({ tabs, activeId, dirty, canAdd, onSelect, onClose, onAdd }: Props) {
  function onKeyDown(e: KeyboardEvent<HTMLButtonElement>, id: string) {
    const i = tabs.findIndex((t) => t.id === id);
    if (e.key === "Delete") {
      e.preventDefault();
      onClose(id);
    } else if (e.key === "ArrowRight" || e.key === "ArrowLeft") {
      e.preventDefault();
      const next = tabs[(i + (e.key === "ArrowRight" ? 1 : tabs.length - 1)) % tabs.length];
      onSelect(next.id);
      requestAnimationFrame(() => document.getElementById(`tab-${next.id}`)?.focus());
    }
  }
  return (
    <div className="ws-strip">
      <div className="ws-tabs" role="tablist" aria-label="Worksheets">
        {tabs.map((t) => {
          const active = t.id === activeId;
          return (
            <div key={t.id} role="presentation" className="ws-tab-wrap" data-active={active}>
              <button
                type="button"
                role="tab"
                id={`tab-${t.id}`}
                className="ws-tab"
                aria-selected={active}
                aria-keyshortcuts="Delete"
                tabIndex={active ? 0 : -1}
                onClick={() => onSelect(t.id)}
                onKeyDown={(e) => onKeyDown(e, t.id)}
              >
                <span className="ws-tab-name">{t.name}</span>
                {dirty.has(t.id) && (
                  <span className="ws-dirty" title="Edited since last run">
                    <span className="visually-hidden">(edited since last run)</span>
                  </span>
                )}
                <span
                  className="ws-close"
                  aria-hidden="true"
                  data-close={t.id}
                  onClick={(e) => {
                    e.stopPropagation();
                    onClose(t.id);
                  }}
                >
                  <Icon name="close" />
                </span>
              </button>
            </div>
          );
        })}
      </div>
      <button type="button" className="ws-add" aria-label="Add worksheet" disabled={!canAdd} onClick={onAdd}>
        <Icon name="plus" />
      </button>
    </div>
  );
}

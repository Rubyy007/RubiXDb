import { useMemo, useState } from "react";
import { hasChildren, hiddenByCollapse, parsePlan } from "./plan";

/** EXPLAIN output as an indented, collapsible tree with the node name as a chip. Built from the
 * API's `plan_text`; every part is rendered as a React text node. */
export function PlanTree({ text }: { text: string }) {
  const nodes = useMemo(() => parsePlan(text), [text]);
  const [collapsed, setCollapsed] = useState<ReadonlySet<number>>(new Set());

  function toggle(i: number) {
    setCollapsed((c) => {
      const n = new Set(c);
      if (n.has(i)) n.delete(i);
      else n.add(i);
      return n;
    });
  }

  if (nodes.length === 0) return <p className="text-muted">The plan is empty.</p>;
  return (
    <ul className="ide-plan" aria-label="Query plan">
      {nodes.map((n, i) =>
        hiddenByCollapse(nodes, i, collapsed) ? null : (
          <li key={i} style={{ paddingLeft: n.depth * 16 }}>
            {hasChildren(nodes, i) ? (
              <button
                type="button"
                className="ide-plan-toggle"
                aria-expanded={!collapsed.has(i)}
                aria-label={`${collapsed.has(i) ? "Expand" : "Collapse"} ${n.name}`}
                onClick={() => toggle(i)}
              >
                {collapsed.has(i) ? "▸" : "▾"}
              </button>
            ) : (
              <span className="ide-plan-toggle" aria-hidden="true" />
            )}
            <span className="ide-chip">{n.name}</span>
            {n.detail && <span className="ide-plan-detail text-mono">{n.detail}</span>}
          </li>
        ),
      )}
    </ul>
  );
}

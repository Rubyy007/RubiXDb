/** Parses the text returned by `EXPLAIN` (`result.kind === "explain"`, `plan_text`) into a tree.
 * The API returns indented text, two spaces per level, one node per line, for example:
 *
 *   Query (max_parameter=0)
 *     Projection [id, name]
 *       Filter ...
 *
 * The first word of each line is the node name; the rest is its detail. The parse is purely
 * structural, so an unfamiliar line is still shown (never dropped). */

export interface PlanNode {
  name: string;
  detail: string;
  depth: number;
}

const MAX_LINES = 500;

export function parsePlan(planText: string): PlanNode[] {
  const nodes: PlanNode[] = [];
  for (const line of planText.split(/\r?\n/).slice(0, MAX_LINES)) {
    if (line.trim() === "") continue;
    const indent = line.length - line.trimStart().length;
    const body = line.trim();
    const m = /^(\S+(?: Join)?)\s*(.*)$/.exec(body);
    const name = m ? m[1] : body;
    const detail = m ? m[2] : "";
    nodes.push({ name, detail, depth: Math.floor(indent / 2) });
  }
  return nodes;
}

/** Index of the node's parent (nearest earlier node one level up), or -1 for a root. */
export function parentIndex(nodes: PlanNode[], i: number): number {
  for (let j = i - 1; j >= 0; j--) if (nodes[j].depth < nodes[i].depth) return j;
  return -1;
}

/** Whether node `i` is hidden because an ancestor is collapsed. */
export function hiddenByCollapse(nodes: PlanNode[], i: number, collapsed: ReadonlySet<number>): boolean {
  let p = parentIndex(nodes, i);
  while (p >= 0) {
    if (collapsed.has(p)) return true;
    p = parentIndex(nodes, p);
  }
  return false;
}

export function hasChildren(nodes: PlanNode[], i: number): boolean {
  return i + 1 < nodes.length && nodes[i + 1].depth > nodes[i].depth;
}

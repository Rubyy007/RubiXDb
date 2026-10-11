import type { ReactNode } from "react";

/** Minimal Markdown renderer for markdown cells and the scratchpad. It builds React elements only,
 * never HTML strings, so note text can never inject markup. Supported: # headings, paragraphs,
 * - / * bullet lists, 1. numbered lists, > quotes, ``` fenced code, and inline **bold**, *italic*,
 * `code`. Links are deliberately not rendered (plain text), so a note can never carry a clickable
 * `javascript:` URL. */

const MAX_LINES = 2000;

function inline(text: string, keyBase: string): ReactNode[] {
  const out: ReactNode[] = [];
  const re = /(`[^`]+`|\*\*[^*]+\*\*|\*[^*\s][^*]*\*)/g;
  let last = 0;
  let i = 0;
  for (let m = re.exec(text); m; m = re.exec(text)) {
    if (m.index > last) out.push(text.slice(last, m.index));
    const tok = m[0];
    const key = `${keyBase}-${i++}`;
    if (tok.startsWith("`")) out.push(<code key={key}>{tok.slice(1, -1)}</code>);
    else if (tok.startsWith("**")) out.push(<strong key={key}>{tok.slice(2, -2)}</strong>);
    else out.push(<em key={key}>{tok.slice(1, -1)}</em>);
    last = m.index + tok.length;
  }
  if (last < text.length) out.push(text.slice(last));
  return out;
}

export function renderMarkdown(src: string): ReactNode[] {
  const lines = src.split(/\r?\n/).slice(0, MAX_LINES);
  const out: ReactNode[] = [];
  let i = 0;
  let k = 0;
  while (i < lines.length) {
    const line = lines[i];
    if (line.trim() === "") {
      i++;
      continue;
    }
    if (line.startsWith("```")) {
      const code: string[] = [];
      i++;
      while (i < lines.length && !lines[i].startsWith("```")) code.push(lines[i++]);
      i++;
      out.push(
        <pre key={k++}>
          <code>{code.join("\n")}</code>
        </pre>,
      );
      continue;
    }
    const h = /^(#{1,3})\s+(.*)$/.exec(line);
    if (h) {
      const level = h[1].length;
      const content = inline(h[2], `h${k}`);
      out.push(level === 1 ? <h3 key={k++}>{content}</h3> : level === 2 ? <h4 key={k++}>{content}</h4> : <h5 key={k++}>{content}</h5>);
      i++;
      continue;
    }
    if (/^\s*[-*]\s+/.test(line)) {
      const items: ReactNode[] = [];
      while (i < lines.length && /^\s*[-*]\s+/.test(lines[i])) {
        items.push(<li key={items.length}>{inline(lines[i].replace(/^\s*[-*]\s+/, ""), `u${k}-${items.length}`)}</li>);
        i++;
      }
      out.push(<ul key={k++}>{items}</ul>);
      continue;
    }
    if (/^\s*\d+\.\s+/.test(line)) {
      const items: ReactNode[] = [];
      while (i < lines.length && /^\s*\d+\.\s+/.test(lines[i])) {
        items.push(<li key={items.length}>{inline(lines[i].replace(/^\s*\d+\.\s+/, ""), `o${k}-${items.length}`)}</li>);
        i++;
      }
      out.push(<ol key={k++}>{items}</ol>);
      continue;
    }
    if (line.startsWith(">")) {
      const q: string[] = [];
      while (i < lines.length && lines[i].startsWith(">")) q.push(lines[i++].replace(/^>\s?/, ""));
      out.push(<blockquote key={k++}>{inline(q.join(" "), `q${k}`)}</blockquote>);
      continue;
    }
    const para: string[] = [];
    while (
      i < lines.length &&
      lines[i].trim() !== "" &&
      !lines[i].startsWith("```") &&
      !/^(#{1,3}\s|\s*[-*]\s|\s*\d+\.\s|>)/.test(lines[i])
    ) {
      para.push(lines[i++]);
    }
    out.push(<p key={k++}>{inline(para.join(" "), `p${k}`)}</p>);
  }
  return out;
}

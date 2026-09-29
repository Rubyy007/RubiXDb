// Typed SQL value -> display text -- mirrors `cli/src/render.rs`'s own
// rules exactly (one shared contract across both clients, item 96):
// values stay typed until this, the very last step; 64-bit-or-wider
// numeric types are already strings on the wire (`api/src/sql_params.rs`'s
// own doc comment on why); NULL renders as the literal text "NULL",
// distinguishable from an empty string.

import type { SqlValue } from "../api/types";

export function formatSqlValue(v: SqlValue): string {
  switch (v.type) {
    case "null":
      return "NULL";
    case "boolean":
      return String(v.value);
    case "integer":
    case "real":
    case "double":
      return String(v.value);
    case "bigint":
      return v.value;
    case "decimal":
      return formatDecimal(v.unscaled, v.scale);
    case "text":
    case "date":
    case "time":
    case "timestamp":
      return v.value;
    case "blob":
      return `(${blobByteLength(v.value_b64)}-byte blob)`;
    default: {
      // Exhaustiveness guard -- a server-side response-shape change
      // shows up as visible, honest text here rather than a silently
      // blank cell.
      const unknown = v as { type: string };
      return `(unrecognized value type ${unknown.type})`;
    }
  }
}

export function isNullValue(v: SqlValue): boolean {
  return v.type === "null";
}

function formatDecimal(unscaled: string, scale: number): string {
  if (scale === 0) return unscaled;
  const negative = unscaled.startsWith("-");
  const digits = negative ? unscaled.slice(1) : unscaled;
  const padded = digits.padStart(scale + 1, "0");
  const intPart = padded.slice(0, padded.length - scale);
  const fracPart = padded.slice(padded.length - scale);
  return `${negative ? "-" : ""}${intPart}.${fracPart}`;
}

function blobByteLength(b64: string): number {
  const stripped = b64.replace(/=+$/, "");
  const pad = b64.length - stripped.length;
  return Math.floor((b64.length / 4) * 3) - Math.min(pad, 2);
}

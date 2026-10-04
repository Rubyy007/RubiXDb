/** Hand-rolled inline-SVG sparkline: numbers in, one <polyline> out. No
 * chart library, no user content, no inline script or style. Decorative:
 * the value is always also shown as text next to it. */
export function Sparkline({ values, width = 120, height = 24 }: { values: readonly number[]; width?: number; height?: number }) {
  if (values.length < 2) return null;
  const max = Math.max(...values, 100);
  const step = width / (values.length - 1);
  const points = values
    .map((v, i) => `${(i * step).toFixed(1)},${(height - 1 - (Math.max(0, v) / max) * (height - 2)).toFixed(1)}`)
    .join(" ");
  return (
    <svg
      className="sparkline"
      viewBox={`0 0 ${width} ${height}`}
      preserveAspectRatio="none"
      aria-hidden="true"
      focusable="false"
    >
      <polyline points={points} fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinejoin="round" />
    </svg>
  );
}

/** Hand-rolled inline-SVG sparkline: numbers in, one <polyline> out. No
 * chart library, no user content, no inline script or style. Decorative:
 * the value is always also shown as text next to it. */
export function Sparkline({
  values,
  width = 120,
  height = 24,
  autoScale = false,
}: {
  values: readonly number[];
  width?: number;
  height?: number;
  /** Fit the line to the values' own range instead of a 0..100 percent scale. */
  autoScale?: boolean;
}) {
  if (values.length < 2) return null;
  const lo = autoScale ? Math.min(...values) : 0;
  const hi = autoScale ? Math.max(...values) : Math.max(...values, 100);
  const span = hi - lo;
  const step = width / (values.length - 1);
  const points = values
    .map((v, i) => {
      // A flat series (span 0) is drawn mid-height rather than dividing by zero.
      const frac = span > 0 ? (Math.max(lo, v) - lo) / span : 0.5;
      return `${(i * step).toFixed(1)},${(height - 1 - frac * (height - 2)).toFixed(1)}`;
    })
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

import type { ChartData } from "./exports";

/** A plain bar chart of one numeric result column. SVG built from React elements; labels are text
 * nodes. Deliberately minimal: no legend (one series), no animation. */
export function BarChart({ data }: { data: ChartData }) {
  const W = 560;
  const H = 160;
  const pad = { l: 44, r: 8, t: 8, b: 22 };
  const max = Math.max(...data.points.map((p) => p.value), 0);
  const min = Math.min(...data.points.map((p) => p.value), 0);
  const span = max - min || 1;
  const bw = (W - pad.l - pad.r) / data.points.length;
  const y = (v: number) => pad.t + (1 - (v - min) / span) * (H - pad.t - pad.b);
  const zero = y(0);
  const fmt = (n: number) => (Math.abs(n) >= 1000 ? n.toLocaleString(undefined, { maximumFractionDigits: 0 }) : String(+n.toFixed(2)));
  const step = Math.ceil(data.points.length / 8);
  return (
    <figure className="ide-chart">
      <figcaption>{data.label}</figcaption>
      <svg viewBox={`0 0 ${W} ${H}`} role="img" aria-label={`Bar chart of ${data.label}, ${data.points.length} values`}>
        <line x1={pad.l} x2={W - pad.r} y1={zero} y2={zero} className="ide-chart-axis" />
        <text x={pad.l - 6} y={y(max) + 4} textAnchor="end" className="ide-chart-tick">
          {fmt(max)}
        </text>
        {min < 0 && (
          <text x={pad.l - 6} y={y(min) + 4} textAnchor="end" className="ide-chart-tick">
            {fmt(min)}
          </text>
        )}
        {data.points.map((p, i) => {
          const top = Math.min(y(p.value), zero);
          const h = Math.max(1, Math.abs(zero - y(p.value)));
          return (
            <g key={i}>
              <rect x={pad.l + i * bw + 1} y={top} width={Math.max(1, bw - 2)} height={h} rx={2} className="ide-chart-bar">
                <title>{`${p.label}: ${fmt(p.value)}`}</title>
              </rect>
              {i % step === 0 && (
                <text x={pad.l + i * bw + bw / 2} y={H - 6} textAnchor="middle" className="ide-chart-tick">
                  {p.label.length > 8 ? `${p.label.slice(0, 7)}…` : p.label}
                </text>
              )}
            </g>
          );
        })}
      </svg>
    </figure>
  );
}

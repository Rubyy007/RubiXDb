import type { ReactNode } from "react";

export function Card({ title, children }: { title?: string; children: ReactNode }) {
  return (
    <section className="card">
      {/* h2, not h3: every page's own title is an h1, and axe-core's
          real audit (e2e/a11y.spec.ts) caught the original h3 as an
          invalid skipped heading level -- Cards are always the next
          section level directly under a page's h1. */}
      {title && <h2 className="card-title">{title}</h2>}
      {children}
    </section>
  );
}

export function CardGrid({ children }: { children: ReactNode }) {
  return <div className="card-grid">{children}</div>;
}

export function Stat({ label, value }: { label: string; value: ReactNode }) {
  return (
    <div>
      <div className="stat-value">{value}</div>
      <div className="stat-label">{label}</div>
    </div>
  );
}

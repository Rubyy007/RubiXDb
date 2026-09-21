import type { ReactNode } from "react";

export function EmptyState({
  icon = "□",
  title,
  message,
  action,
}: {
  icon?: string;
  title: string;
  message?: string;
  action?: ReactNode;
}) {
  return (
    <div className="empty-state">
      <div className="empty-state-icon" aria-hidden="true">
        {icon}
      </div>
      <p style={{ fontWeight: 600 }}>{title}</p>
      {message && <p className="text-muted">{message}</p>}
      {action}
    </div>
  );
}

export function ErrorState({ message }: { message: string }) {
  return (
    <div className="empty-state" role="alert">
      <div className="empty-state-icon" aria-hidden="true">
        !
      </div>
      <p style={{ fontWeight: 600, color: "var(--color-danger)" }}>Something went wrong</p>
      <p className="text-muted">{message}</p>
    </div>
  );
}

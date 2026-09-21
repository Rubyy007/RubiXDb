import type { ReactNode } from "react";
import { Spinner } from "./Spinner";
import { EmptyState, ErrorState } from "./EmptyState";

export interface Column<T> {
  key: string;
  header: string;
  render: (row: T) => ReactNode;
  mono?: boolean;
}

interface TableProps<T> {
  columns: Column<T>[];
  rows: T[];
  rowKey: (row: T) => string;
  caption: string;
  isLoading?: boolean;
  isError?: boolean;
  errorMessage?: string;
  emptyTitle?: string;
  emptyMessage?: string;
}

/** One `Table` implementation, reused for every list in the console --
 * built-in loading/error/empty states so no screen has to invent its
 * own (PHASE_FRONTEND_ARCHITECTURE.md §4). */
export function Table<T>({
  columns,
  rows,
  rowKey,
  caption,
  isLoading,
  isError,
  errorMessage,
  emptyTitle,
  emptyMessage,
}: TableProps<T>) {
  if (isLoading) {
    return (
      <div className="table-wrap" style={{ padding: "var(--space-6)", textAlign: "center" }}>
        <Spinner label={`Loading ${caption}`} />
      </div>
    );
  }
  if (isError) {
    return (
      <div className="table-wrap">
        <ErrorState message={errorMessage ?? "Failed to load data."} />
      </div>
    );
  }
  if (rows.length === 0) {
    return (
      <div className="table-wrap">
        <EmptyState title={emptyTitle ?? "No results"} message={emptyMessage} />
      </div>
    );
  }
  return (
    <div className="table-wrap">
      <table className="table">
        <caption className="visually-hidden">{caption}</caption>
        <thead>
          <tr>
            {columns.map((col) => (
              <th key={col.key} scope="col">
                {col.header}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((row) => (
            <tr key={rowKey(row)} tabIndex={0}>
              {columns.map((col) => (
                <td key={col.key} className={col.mono ? "text-mono" : undefined}>
                  {col.render(row)}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

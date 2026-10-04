import { useMemo } from "react";
import type { SqlColumnMeta, SqlValue } from "../../api/types";
import { formatSqlValue, isNullValue } from "../../utils/sqlValue";
import { Button } from "../Button";
import { EmptyState } from "../EmptyState";

export const PAGE_SIZES = [100, 200, 500] as const;

interface Props {
  columns: SqlColumnMeta[];
  rows: SqlValue[][];
  rowCount: number;
  page: number;
  pageSize: number;
  onPageChange: (p: number) => void;
  onPageSizeChange: (n: number) => void;
}

/** Result grid: sticky header, row-number column, type tags, 32px rows, and
 * client-side pagination over the rows the API already returned (no new
 * request). Every cell is a React text node -- result data is untrusted. */
export function ResultsGrid({ columns, rows, rowCount, page, pageSize, onPageChange, onPageSizeChange }: Props) {
  const totalPages = Math.max(1, Math.ceil(rows.length / pageSize));
  const current = Math.min(page, totalPages - 1);
  const first = current * pageSize;
  const pageRows = useMemo(() => rows.slice(first, first + pageSize), [rows, first, pageSize]);

  if (rowCount === 0) {
    return (
      <div className="grid-panel">
        <div className="grid-toolbar">
          <span className="grid-count">Result (0 rows)</span>
        </div>
        <div className="grid-empty">
          <EmptyState title="No rows returned" message="The statement ran successfully but matched nothing." />
        </div>
      </div>
    );
  }

  const shownFrom = first + 1;
  const shownTo = first + pageRows.length;

  return (
    <div className="grid-panel">
      <div className="grid-toolbar">
        <span className="grid-count">{`Result (${rowCount} row${rowCount === 1 ? "" : "s"})`}</span>
      </div>
      <div className="table-wrap grid-scroll">
        <table className="grid">
          <caption className="visually-hidden">Query result</caption>
          <thead>
            <tr>
              <th scope="col" className="grid-rownum">
                #
              </th>
              {columns.map((c, i) => (
                <th key={`${i}-${c.name}`} scope="col">
                  <span className="grid-colname">{c.name}</span>
                  {c.type && <span className="grid-type">{c.type}</span>}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {pageRows.map((row, i) => (
              <tr key={first + i}>
                <td className="grid-rownum">{first + i + 1}</td>
                {row.map((cell, j) => (
                  <td key={j} className="grid-cell text-mono">
                    {isNullValue(cell) ? <span className="grid-null">NULL</span> : formatSqlValue(cell)}
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <div className="grid-footer">
        <span>
          {`Showing ${shownFrom.toLocaleString()}–${shownTo.toLocaleString()} of ${rows.length.toLocaleString()} rows`}
          {totalPages > 1 && ` · Page ${current + 1} of ${totalPages.toLocaleString()}`}
        </span>
        <span className="grid-pager">
          <label className="grid-pagesize">
            Rows per page
            <select value={pageSize} onChange={(e) => onPageSizeChange(Number(e.target.value))}>
              {PAGE_SIZES.map((n) => (
                <option key={n} value={n}>
                  {n}
                </option>
              ))}
            </select>
          </label>
          <Button variant="secondary" onClick={() => onPageChange(current - 1)} disabled={current === 0}>
            Prev
          </Button>
          <Button variant="secondary" onClick={() => onPageChange(current + 1)} disabled={current >= totalPages - 1}>
            Next
          </Button>
        </span>
      </div>
    </div>
  );
}

import { useState } from "react";
import { useSession } from "../context/SessionContext";
import {
  useDeleteMutation,
  useExistsQuery,
  useGetQuery,
  usePutMutation,
  useRangeMutation,
} from "../api/queries";
import { ApiRequestError } from "../api/types";
import { b64ToUtf8, utf8ToB64 } from "../utils/base64";
import { Card } from "../components/Card";
import { Tabs } from "../components/Tabs";
import { Input, TextArea } from "../components/Field";
import { Button } from "../components/Button";
import { Table, type Column } from "../components/Table";
import { Dialog } from "../components/Dialog";
import { useToast } from "../components/Toast";
import type { RangeRow } from "../api/types";

function displayValue(b64: string): string {
  try {
    return b64ToUtf8(b64);
  } catch {
    return `(binary, base64) ${b64}`;
  }
}

function PointLookup() {
  const { session } = useSession();
  const { notify } = useToast();
  const [keyText, setKeyText] = useState("");
  const [asOfSeq, setAsOfSeq] = useState("");
  const [valueText, setValueText] = useState("");
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [lookupEnabled, setLookupEnabled] = useState(false);

  const keyB64 = utf8ToB64(keyText);
  const seq = asOfSeq.trim() === "" ? undefined : Number(asOfSeq);
  const getQuery = useGetQuery(keyB64, seq, lookupEnabled);
  const existsQuery = useExistsQuery(keyB64, seq, lookupEnabled);
  const putMutation = usePutMutation();
  const deleteMutation = useDeleteMutation();

  const canWrite = session?.role === "admin";

  return (
    <div className="stack">
      <Card title="Look up a key">
        <div className="stack">
          <Input
            label="Key"
            value={keyText}
            onChange={(e) => setKeyText(e.target.value)}
            placeholder="e.g. user:1234"
            mono
          />
          <Input
            label="As-of sequence (optional)"
            value={asOfSeq}
            onChange={(e) => setAsOfSeq(e.target.value)}
            placeholder="leave blank for the current value"
            hint="A historical read at a specific sequence number, exactly like range_scan's own seq parameter."
            mono
          />
          <div className="row">
            <Button
              variant="primary"
              disabled={!keyText}
              onClick={() => setLookupEnabled(true)}
            >
              Look up
            </Button>
          </div>
        </div>
      </Card>

      {lookupEnabled && keyText && (
        <Card title="Result">
          {getQuery.isLoading || existsQuery.isLoading ? (
            <p className="text-muted">Loading&hellip;</p>
          ) : getQuery.isError && getQuery.error instanceof ApiRequestError && getQuery.error.status === 404 ? (
            <p>
              <strong>{keyText}</strong> does not exist{seq !== undefined ? ` as of seq ${seq}` : ""}.
            </p>
          ) : getQuery.data ? (
            <div className="stack">
              <div>
                <span className="field-label">Value</span>
                <pre className="text-mono" style={{ whiteSpace: "pre-wrap", wordBreak: "break-all" }}>
                  {displayValue(getQuery.data.value_b64)}
                </pre>
              </div>
              <div className="spread">
                <span className="text-muted">exists: {String(existsQuery.data?.exists ?? true)}</span>
                <span className="text-muted">seq queried: {getQuery.data.seq_queried}</span>
              </div>
              {canWrite && (
                <Button variant="danger" onClick={() => setConfirmDelete(true)}>
                  Delete this key
                </Button>
              )}
            </div>
          ) : (
            <p className="text-muted">No result.</p>
          )}
        </Card>
      )}

      {canWrite && (
        <Card title="Write">
          <div className="stack">
            <Input
              label="Key"
              value={keyText}
              onChange={(e) => setKeyText(e.target.value)}
              mono
            />
            <TextArea
              label="Value"
              value={valueText}
              onChange={(e) => setValueText(e.target.value)}
              mono
              rows={4}
            />
            <div className="row">
              <Button
                variant="primary"
                disabled={!keyText || putMutation.isPending}
                onClick={() => {
                  putMutation.mutate(
                    { keyB64: utf8ToB64(keyText), valueB64: utf8ToB64(valueText) },
                    {
                      onSuccess: (res) => {
                        notify(`Wrote ${keyText} at seq ${res.seq}`, "success");
                        setLookupEnabled(true);
                      },
                      onError: (err) =>
                        notify(err instanceof Error ? err.message : "Write failed", "error"),
                    },
                  );
                }}
              >
                {putMutation.isPending ? "Writing…" : "Put"}
              </Button>
            </div>
          </div>
        </Card>
      )}

      <Dialog
        open={confirmDelete}
        title="Delete key?"
        confirmLabel="Delete"
        confirmVariant="danger"
        busy={deleteMutation.isPending}
        onClose={() => setConfirmDelete(false)}
        onConfirm={() => {
          deleteMutation.mutate(keyB64, {
            onSuccess: (res) => {
              notify(`Deleted ${keyText} at seq ${res.seq}`, "success");
              setConfirmDelete(false);
            },
            onError: (err) => {
              notify(err instanceof Error ? err.message : "Delete failed", "error");
              setConfirmDelete(false);
            },
          });
        }}
      >
        This deletes <strong>{keyText}</strong>. Historical reads at earlier sequence numbers are
        unaffected.
      </Dialog>
    </div>
  );
}

const rangeColumns: Column<RangeRow>[] = [
  { key: "key", header: "Key", mono: true, render: (r) => displayValue(r.key_b64) },
  { key: "value", header: "Value", mono: true, render: (r) => displayValue(r.value_b64) },
];

function RangeQueryPanel() {
  const [startText, setStartText] = useState("");
  const [endText, setEndText] = useState("");
  const [asOfSeq, setAsOfSeq] = useState("");
  const [limit, setLimit] = useState("100");
  const rangeMutation = useRangeMutation();

  function runQuery() {
    rangeMutation.mutate({
      startB64: startText ? utf8ToB64(startText) : undefined,
      endB64: endText ? utf8ToB64(endText) : undefined,
      asOfSeq: asOfSeq.trim() === "" ? undefined : Number(asOfSeq),
      limit: Number(limit),
    });
  }

  return (
    <div className="stack">
      <Card title="Range query">
        <div className="stack">
          <div className="row row-wrap">
            <Input
              label="Start key (inclusive, optional)"
              value={startText}
              onChange={(e) => setStartText(e.target.value)}
              mono
            />
            <Input
              label="End key (exclusive, optional)"
              value={endText}
              onChange={(e) => setEndText(e.target.value)}
              mono
            />
          </div>
          <div className="row row-wrap">
            <Input
              label="As-of sequence (optional)"
              value={asOfSeq}
              onChange={(e) => setAsOfSeq(e.target.value)}
              mono
            />
            <Input
              label="Limit"
              type="number"
              value={limit}
              onChange={(e) => setLimit(e.target.value)}
              mono
            />
          </div>
          <div className="row">
            <Button variant="primary" onClick={runQuery} disabled={rangeMutation.isPending}>
              {rangeMutation.isPending ? "Querying…" : "Run range query"}
            </Button>
          </div>
        </div>
      </Card>

      {rangeMutation.data && (
        <Card title={`Results (${rangeMutation.data.rows.length}${rangeMutation.data.truncated ? "+, truncated" : ""})`}>
          <Table
            columns={rangeColumns}
            rows={rangeMutation.data.rows}
            rowKey={(r) => r.key_b64}
            caption="Range query results"
            emptyTitle="No keys in this range"
          />
          {rangeMutation.data.truncated && (
            <p className="text-muted" style={{ marginTop: "var(--space-3)" }}>
              More rows exist beyond the limit — results are returned in key order, so this page
              always reflects the lowest {limit} keys in range, never an arbitrary subset.
            </p>
          )}
        </Card>
      )}
      {rangeMutation.isError && (
        <Card title="Error">
          <p className="field-error">
            {rangeMutation.error instanceof Error ? rangeMutation.error.message : "Query failed"}
          </p>
        </Card>
      )}
    </div>
  );
}

export function ExplorerPage() {
  const [tab, setTab] = useState("lookup");

  return (
    <div className="stack">
      <h1 style={{ fontSize: "var(--font-size-2xl)" }}>Data Explorer</h1>
      <Tabs
        tabs={[
          { id: "lookup", label: "Point Lookup" },
          { id: "range", label: "Range Query" },
        ]}
        active={tab}
        onChange={setTab}
      />
      {tab === "lookup" ? <PointLookup /> : <RangeQueryPanel />}
    </div>
  );
}

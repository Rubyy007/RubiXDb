import { useEffect, useState, type ReactNode } from "react";
import { useSession } from "../context/SessionContext";
import {
  useDeleteIndexMutation,
  useDeleteMutation,
  useDeleteSchemaMutation,
  useDeleteTableMutation,
  useExistsQuery,
  useGetQuery,
  useIndexesQuery,
  usePutMutation,
  useRangeMutation,
  useSchemasQuery,
  useTablesQuery,
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
import type { IndexInfo, RangeRow, SchemaInfo, TableInfo } from "../api/types";

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
            onChange={(e) => {
              setKeyText(e.target.value);
              // Editing the key after a lookup re-arms it -- without
              // this, `getQuery`/`existsQuery` stay `enabled` from the
              // previous click and silently re-fire on every keystroke
              // (a live query against whatever partial/not-yet-written
              // key is currently in the box), which is both wasteful
              // and surprising for a button that reads as an explicit,
              // deliberate action. A real 404-per-keystroke instance of
              // this was found by `e2e/production_validation.spec.ts`'s
              // own repeated-workflow stability test.
              setLookupEnabled(false);
            }}
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

// =======================================================================
// Object browser + delete safety (Increment 14, Blocker 12). No
// delete-object UI existed anywhere in this console before this
// increment (`PHASE_RUBIXDB_GUI_ARCHITECTURE.md` §10's own named gap)
// -- this is the real, backend-authoritative implementation, not a
// client-side-only confirmation: every delete call the mutations below
// make is re-validated against the *live* catalog server-side
// (`api/src/routes/catalog.rs`), so a stale row this browser is still
// showing can never actually delete the wrong (or already-gone)
// object -- confirmed by `api_delete_safety.rs`'s stale-UI tests.
// =======================================================================

interface TypeToConfirmDialogProps {
  open: boolean;
  title: string;
  expected: string;
  danger: ReactNode;
  busy: boolean;
  errorMessage?: string;
  onCancel: () => void;
  onConfirm: () => void;
}

function TypeToConfirmDialog({
  open,
  title,
  expected,
  danger,
  busy,
  errorMessage,
  onCancel,
  onConfirm,
}: TypeToConfirmDialogProps) {
  const [typed, setTyped] = useState("");
  useEffect(() => {
    if (!open) setTyped("");
  }, [open]);
  const matches = expected.length > 0 && typed === expected;

  return (
    <Dialog
      open={open}
      title={title}
      confirmLabel="Delete permanently"
      confirmVariant="danger"
      busy={busy}
      confirmDisabled={!matches}
      onClose={onCancel}
      onConfirm={onConfirm}
    >
      <div className="stack">
        <p>{danger}</p>
        <p>
          Type <strong>{expected}</strong> below to enable permanent deletion. Wrong, partial, or
          empty text keeps the delete button disabled.
        </p>
        <Input
          label={`Type "${expected}" to confirm`}
          value={typed}
          onChange={(e) => setTyped(e.target.value)}
          mono
        />
        {errorMessage && <p className="field-error">{errorMessage}</p>}
      </div>
    </Dialog>
  );
}

function SchemasPanel() {
  const { session } = useSession();
  const canWrite = session?.role === "admin";
  const { notify } = useToast();
  const schemasQuery = useSchemasQuery();
  const deleteSchema = useDeleteSchemaMutation();
  const [target, setTarget] = useState<SchemaInfo | null>(null);

  const columns: Column<SchemaInfo>[] = [
    { key: "schema_id", header: "ID", render: (s) => String(s.schema_id) },
    { key: "name", header: "Name", mono: true, render: (s) => s.name },
    ...(canWrite
      ? [
          {
            key: "actions",
            header: "",
            render: (s: SchemaInfo) => (
              <Button variant="danger" onClick={() => setTarget(s)}>
                Delete
              </Button>
            ),
          } as Column<SchemaInfo>,
        ]
      : []),
  ];

  return (
    <Card title="Schemas">
      <Table
        columns={columns}
        rows={schemasQuery.data ?? []}
        rowKey={(s) => String(s.schema_id)}
        caption="Schemas in the current database"
        emptyTitle="No schemas"
      />
      <TypeToConfirmDialog
        open={target !== null}
        title={`Delete schema ${target?.name ?? ""}?`}
        expected={target?.name ?? ""}
        danger={
          <>
            This permanently deletes schema <strong>{target?.name}</strong>. It is refused if the
            schema still has tables.
          </>
        }
        busy={deleteSchema.isPending}
        errorMessage={
          deleteSchema.isError
            ? deleteSchema.error instanceof Error
              ? deleteSchema.error.message
              : "Delete failed"
            : undefined
        }
        onCancel={() => setTarget(null)}
        onConfirm={() => {
          if (!target) return;
          deleteSchema.mutate(
            { schemaId: target.schema_id, confirmName: target.name },
            {
              onSuccess: () => {
                notify(`Deleted schema ${target.name}`, "success");
                setTarget(null);
              },
            },
          );
        }}
      />
    </Card>
  );
}

function TablesPanel() {
  const { session } = useSession();
  const canWrite = session?.role === "admin";
  const { notify } = useToast();
  const schemasQuery = useSchemasQuery();
  const tablesQuery = useTablesQuery();
  const deleteTable = useDeleteTableMutation();
  const [target, setTarget] = useState<TableInfo | null>(null);

  const schemaName = (schemaId: number) =>
    schemasQuery.data?.find((s) => s.schema_id === schemaId)?.name ?? `schema ${schemaId}`;

  const columns: Column<TableInfo>[] = [
    { key: "table_id", header: "ID", render: (t) => String(t.table_id) },
    { key: "schema", header: "Schema", render: (t) => schemaName(t.schema_id) },
    { key: "name", header: "Table", mono: true, render: (t) => t.name },
    ...(canWrite
      ? [
          {
            key: "actions",
            header: "",
            render: (t: TableInfo) => (
              <Button variant="danger" onClick={() => setTarget(t)}>
                Delete
              </Button>
            ),
          } as Column<TableInfo>,
        ]
      : []),
  ];

  const targetSchemaName = target ? schemaName(target.schema_id) : "";

  return (
    <Card title="Tables">
      <Table
        columns={columns}
        rows={tablesQuery.data ?? []}
        rowKey={(t) => String(t.table_id)}
        caption="Tables visible to this principal"
        emptyTitle="No tables"
      />
      <TypeToConfirmDialog
        open={target !== null}
        title={`Delete table ${targetSchemaName}.${target?.name ?? ""}?`}
        expected={target?.name ?? ""}
        danger={
          <>
            This permanently deletes table{" "}
            <strong>
              {targetSchemaName}.{target?.name}
            </strong>
            , including its rows and indexes.
          </>
        }
        busy={deleteTable.isPending}
        errorMessage={
          deleteTable.isError
            ? deleteTable.error instanceof Error
              ? deleteTable.error.message
              : "Delete failed"
            : undefined
        }
        onCancel={() => setTarget(null)}
        onConfirm={() => {
          if (!target) return;
          deleteTable.mutate(
            { tableId: target.table_id, schemaName: targetSchemaName, tableName: target.name },
            {
              onSuccess: () => {
                notify(`Deleted table ${targetSchemaName}.${target.name}`, "success");
                setTarget(null);
              },
            },
          );
        }}
      />
    </Card>
  );
}

function IndexesPanel() {
  const { session } = useSession();
  const canWrite = session?.role === "admin";
  const { notify } = useToast();
  const schemasQuery = useSchemasQuery();
  const tablesQuery = useTablesQuery();
  const indexesQuery = useIndexesQuery();
  const deleteIndex = useDeleteIndexMutation();
  const [target, setTarget] = useState<IndexInfo | null>(null);

  const tableOf = (tableId: number) => tablesQuery.data?.find((t) => t.table_id === tableId);
  const schemaNameOf = (tableId: number) => {
    const t = tableOf(tableId);
    if (!t) return `table ${tableId}`;
    return schemasQuery.data?.find((s) => s.schema_id === t.schema_id)?.name ?? `schema ${t.schema_id}`;
  };

  const columns: Column<IndexInfo>[] = [
    { key: "index_id", header: "ID", render: (i) => String(i.index_id) },
    { key: "schema", header: "Schema", render: (i) => schemaNameOf(i.table_id) },
    { key: "table", header: "Table", render: (i) => i.table_name ?? String(i.table_id) },
    { key: "name", header: "Index", mono: true, render: (i) => i.name },
    { key: "kind", header: "Kind", render: (i) => i.kind },
    { key: "state", header: "State", render: (i) => i.state },
    ...(canWrite
      ? [
          {
            key: "actions",
            header: "",
            render: (i: IndexInfo) =>
              i.kind === "primary" ? (
                <span className="text-muted">n/a (primary)</span>
              ) : (
                <Button variant="danger" onClick={() => setTarget(i)}>
                  Delete
                </Button>
              ),
          } as Column<IndexInfo>,
        ]
      : []),
  ];

  const targetSchemaName = target ? schemaNameOf(target.table_id) : "";
  const targetTableName = target?.table_name ?? "";

  return (
    <Card title="Indexes">
      <Table
        columns={columns}
        rows={indexesQuery.data ?? []}
        rowKey={(i) => String(i.index_id)}
        caption="Indexes on tables visible to this principal"
        emptyTitle="No indexes"
      />
      <TypeToConfirmDialog
        open={target !== null}
        title={`Delete index ${target?.name ?? ""}?`}
        expected={target?.name ?? ""}
        danger={
          <>
            This permanently deletes index <strong>{target?.name}</strong> on{" "}
            <strong>
              {targetSchemaName}.{targetTableName}
            </strong>
            . Queries fall back to a full scan afterward.
          </>
        }
        busy={deleteIndex.isPending}
        errorMessage={
          deleteIndex.isError
            ? deleteIndex.error instanceof Error
              ? deleteIndex.error.message
              : "Delete failed"
            : undefined
        }
        onCancel={() => setTarget(null)}
        onConfirm={() => {
          if (!target) return;
          deleteIndex.mutate(
            {
              indexId: target.index_id,
              schemaName: targetSchemaName,
              tableName: targetTableName,
              indexName: target.name,
            },
            {
              onSuccess: () => {
                notify(`Deleted index ${target.name}`, "success");
                setTarget(null);
              },
            },
          );
        }}
      />
    </Card>
  );
}

function ObjectsBrowser() {
  return (
    <div className="stack">
      <SchemasPanel />
      <TablesPanel />
      <IndexesPanel />
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
          { id: "objects", label: "Objects" },
        ]}
        active={tab}
        onChange={setTab}
      />
      {tab === "lookup" ? (
        <PointLookup />
      ) : tab === "range" ? (
        <RangeQueryPanel />
      ) : (
        <ObjectsBrowser />
      )}
    </div>
  );
}

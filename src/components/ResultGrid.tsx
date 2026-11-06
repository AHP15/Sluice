import { useMemo, useRef, useState } from "react";
import {
  flexRender,
  getCoreRowModel,
  useReactTable,
  type ColumnDef,
} from "@tanstack/react-table";
import { useVirtualizer } from "@tanstack/react-virtual";
import { Download, Eye, Loader2, Lock, Undo2 } from "lucide-react";
import type { ColumnDesc, EditGate, WireValue } from "../lib/contract";
import { formatBytes, isAbsent, renderValue } from "../lib/contract";
import { fetchValue, saveValueToFile } from "../lib/ipc";
import { cellReadOnlyReason, pendingCount, staged, unprotectedColumns, type Pending } from "../lib/pendingEdits";
import { cn } from "../lib/utils";
import { CellEditor } from "./CellEditor";

type Row = WireValue[];

/**
 * T-056 / D-027: staged editing. Present only for a structured browse of one
 * relation; raw query results are never editable.
 */
export interface GridEditing {
  gate: EditGate;
  pending: Pending;
  onStage: (row: number, col: number, value: WireValue) => void;
  onUnstage: (row: number, col: number) => void;
  onApply: () => void;
  onDiscard: () => void;
  applying: boolean;
}

/**
 * The result grid (T-002 / D-023): TanStack Table for the table model, TanStack
 * Virtual for the window.
 *
 * Properties this file has to keep:
 *
 * - **Virtualized** (D-010): result sets are arbitrarily long, so the DOM holds
 *   a window of the table's row model, never the set.
 * - **Text nodes only** (T-021/D-007): every value below reaches the DOM through
 *   `{string}`. There is no markup path for database content, which is what
 *   bounds an XSS here to "someone read one table".
 * - **Headless model** (D-023): column definitions, sizing and (later) sort,
 *   selection and staged-edit state live in TanStack Table's model, not in
 *   hand-rolled arrays — the reason D-023 chose a headless table.
 */
export function ResultGrid({
  sessionId,
  columns,
  rows,
  truncated,
  editing,
}: {
  sessionId: string;
  columns: ColumnDesc[];
  rows: WireValue[][];
  truncated: boolean;
  editing?: GridEditing;
}) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const rowHeight = 28;

  // One definition per descriptor. Accessors index the row positionally — the
  // contract's rows are arrays aligned with `columns`, and column names are not
  // guaranteed unique (a script can select `a, a`), so ids are positional too.
  const columnDefs = useMemo<ColumnDef<Row, WireValue>[]>(
    () =>
      columns.map((c, i) => ({
        id: `c${i}`,
        accessorFn: (row: Row) => row[i],
        size: 180,
        minSize: 60,
        header: () => (
          <>
            <div className="truncate">{c.name}</div>
            <div className="truncate text-[10px] font-normal text-muted-foreground">
              {c.type_name}
              {/* Schemaless engines: the column's type varied row to row,
                  so the descriptor is wrong for some of them (T-073 F-2). */}
              {c.mixed && <span className="text-destructive"> · mixed</span>}
              {c.derived && <span> · inferred</span>}
            </div>
          </>
        ),
        cell: (ctx) => {
          // Read through table meta so staging does not rebuild the columns.
          const ed = (ctx.table.options.meta as { editing?: GridEditing } | undefined)?.editing;
          const read = ctx.getValue();
          const row = ctx.row.index;
          const pending = ed ? staged(ed.pending, row, i) : undefined;
          if (!ed || ed.gate.state !== "editable") {
            return <Cell sessionId={sessionId} value={read} column={c} />;
          }
          return (
            <EditableCell
              sessionId={sessionId}
              column={c}
              read={read}
              pending={pending}
              disabled={ed.applying}
              onStage={(v) => ed.onStage(row, i, v)}
              onUnstage={() => ed.onUnstage(row, i)}
            />
          );
        },
      })),
    [columns, sessionId],
  );

  const table = useReactTable({
    data: rows,
    columns: columnDefs,
    getCoreRowModel: getCoreRowModel(),
    columnResizeMode: "onChange",
    meta: { editing },
  });
  const changed = editing ? pendingCount(editing.pending) : 0;
  const unprotected = editing && changed > 0 ? unprotectedColumns(editing.pending, columns) : [];
  const model = table.getRowModel().rows;

  const virtualizer = useVirtualizer({
    count: model.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => rowHeight,
    overscan: 12,
  });
  const items = virtualizer.getVirtualItems();

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div ref={scrollRef} className="min-h-0 flex-1 overflow-auto">
        <table
          className="border-collapse text-sm"
          style={{ tableLayout: "fixed", width: table.getTotalSize() }}
        >
          <thead className="sticky top-0 z-10 bg-secondary">
            {table.getHeaderGroups().map((group) => (
              <tr key={group.id}>
                {group.headers.map((h) => (
                  <th
                    key={h.id}
                    style={{ width: h.getSize() }}
                    className="relative border-b border-r px-2 py-1 text-left align-top font-medium"
                  >
                    {flexRender(h.column.columnDef.header, h.getContext())}
                    <div
                      role="separator"
                      aria-orientation="vertical"
                      aria-label="Resize column"
                      onMouseDown={h.getResizeHandler()}
                      onTouchStart={h.getResizeHandler()}
                      className={cn(
                        "absolute right-0 top-0 h-full w-1 cursor-col-resize select-none hover:bg-primary",
                        h.column.getIsResizing() && "bg-primary",
                      )}
                    />
                  </th>
                ))}
              </tr>
            ))}
          </thead>
          <tbody>
            {items.length > 0 && <tr style={{ height: items[0].start }} aria-hidden="true" />}
            {items.map((vi) => {
              const row = model[vi.index];
              return (
                <tr key={row.id} style={{ height: rowHeight }} className="hover:bg-accent">
                  {row.getVisibleCells().map((cell) => (
                    <td
                      key={cell.id}
                      style={{ width: cell.column.getSize() }}
                      className="overflow-hidden border-b border-r px-2 align-middle font-mono"
                    >
                      {flexRender(cell.column.columnDef.cell, cell.getContext())}
                    </td>
                  ))}
                </tr>
              );
            })}
            {items.length > 0 && (
              <tr
                style={{ height: virtualizer.getTotalSize() - items[items.length - 1].end }}
                aria-hidden="true"
              />
            )}
          </tbody>
        </table>
        {rows.length === 0 && (
          <div className="p-6 text-center text-sm text-muted-foreground">No rows</div>
        )}
      </div>
      <div className="flex items-center gap-2 border-t px-2 py-1 text-xs text-muted-foreground">
        {editing?.gate.state === "read_only" && (
          <span className="flex items-center gap-1" title={editing.gate.reason}>
            <Lock className="h-3 w-3" />
            read-only: {editing.gate.reason}
          </span>
        )}
        {editing?.gate.state === "editable" && changed > 0 && (
          <span className="flex items-center gap-2 text-foreground">
            <span>
              {changed} pending change{changed === 1 ? "" : "s"}
            </span>
            {/* D-027: applied together, in one transaction, after review of
                the exact statements (T-057). Discard is the only undo; there
                is none once applied. */}
            <button
              type="button"
              disabled={editing.applying}
              className="rounded bg-primary px-2 py-0.5 text-primary-foreground disabled:opacity-50"
              onClick={editing.onApply}
            >
              {editing.applying ? "Working…" : "Review & apply…"}
            </button>
            <button
              type="button"
              disabled={editing.applying}
              className="rounded px-2 py-0.5 hover:bg-secondary disabled:opacity-50"
              onClick={editing.onDiscard}
            >
              Discard
            </button>
            <span className="text-muted-foreground">applied in one transaction · no undo after</span>
            {unprotected.length > 0 && (
              <span className="text-destructive" title="These types cannot be compared, so a concurrent change to them would be overwritten.">
                not checked for concurrent changes (protected by unique key only): {unprotected.join(", ")}
              </span>
            )}
          </span>
        )}
        <span>
          {rows.length} rows · {items.length} rendered
        </span>
        {truncated && (
          <span className="rounded bg-secondary px-1.5 py-0.5">
            truncated — more rows available
          </span>
        )}
      </div>
    </div>
  );
}

function Cell({
  sessionId,
  value,
  column,
}: {
  sessionId: string;
  value: WireValue;
  column: ColumnDesc;
}) {
  const [loaded, setLoaded] = useState<WireValue | null>(null);
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<string | null>(null);

  const shown = loaded ?? value;

  // T-018 / R-008: an oversized value never crossed the wire. Offer to fetch it
  // on demand instead of having inlined it for every row.
  if (shown.t === "oversized") {
    const handle = shown.v.handle;
    return (
      <span className="flex items-center gap-1 text-muted-foreground">
        <span className="truncate italic">
          {formatBytes(shown.v.byte_len)} {shown.v.type_name}
        </span>
        {busy && <Loader2 className="h-3 w-3 animate-spin" />}
        {handle && !busy && (
          <>
            <button
              type="button"
              title="Load this value"
              className="rounded px-1 hover:bg-secondary"
              onClick={async () => {
                setBusy(true);
                setNote(null);
                try {
                  setLoaded(await fetchValue(sessionId, handle));
                } catch (e) {
                  setNote(String((e as { message?: string })?.message ?? e));
                } finally {
                  setBusy(false);
                }
              }}
            >
              <Eye className="h-3 w-3" />
            </button>
            <button
              type="button"
              title="Save to file"
              className="rounded px-1 hover:bg-secondary"
              onClick={async () => {
                setBusy(true);
                setNote(null);
                try {
                  const saved = await saveValueToFile(sessionId, handle, column.name);
                  setNote(`saved ${formatBytes(saved.bytes)} to ${saved.path}`);
                } catch (e) {
                  setNote(String((e as { message?: string })?.message ?? e));
                } finally {
                  setBusy(false);
                }
              }}
            >
              <Download className="h-3 w-3" />
            </button>
          </>
        )}
        {note && <span className="truncate text-[10px]">{note}</span>}
      </span>
    );
  }

  const text = renderValue(shown);
  return (
    <span
      className={cn("block truncate", isAbsent(shown) && "italic text-muted-foreground")}
      title={text.length > 40 ? text : undefined}
    >
      {text}
    </span>
  );
}

/**
 * T-056: a cell in an editable relation. Double-click to edit; a staged value
 * is shown in place of the value as read, marked, with a per-cell revert.
 */
function EditableCell({
  sessionId,
  column,
  read,
  pending,
  disabled,
  onStage,
  onUnstage,
}: {
  sessionId: string;
  column: ColumnDesc;
  read: WireValue;
  pending: WireValue | undefined;
  disabled: boolean;
  onStage: (v: WireValue) => void;
  onUnstage: () => void;
}) {
  const [open, setOpen] = useState(false);
  const shown = pending ?? read;
  const reason = cellReadOnlyReason(column, read);

  if (open) {
    return (
      <CellEditor
        column={column}
        value={shown}
        onCommit={(v) => {
          setOpen(false);
          onStage(v);
        }}
        onCancel={() => setOpen(false)}
      />
    );
  }

  return (
    <div
      className={cn("flex items-center gap-1", pending !== undefined && "-mx-2 bg-primary/15 px-2")}
      title={reason ?? (pending !== undefined ? `was: ${renderValue(read)}` : "Double-click to edit")}
      onDoubleClick={() => {
        if (!reason && !disabled) setOpen(true);
      }}
    >
      <div className="min-w-0 flex-1">
        <Cell sessionId={sessionId} value={shown} column={column} />
      </div>
      {pending !== undefined && !disabled && (
        <button type="button" title="Revert this cell" className="shrink-0 rounded hover:bg-secondary" onClick={onUnstage}>
          <Undo2 className="h-3 w-3" />
        </button>
      )}
    </div>
  );
}

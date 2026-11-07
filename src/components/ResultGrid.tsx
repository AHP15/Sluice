import { useEffect, useMemo, useRef, useState } from "react";
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
import { cellReadOnlyReason, pendingCount, staged, type Pending } from "../lib/pendingEdits";
import { cn } from "../lib/utils";
import { useUiStore } from "../store/ui";
import { CellEditor } from "./CellEditor";
import { ExportActions, type GridExport } from "./ExportActions";

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
/** D-031 §3: numeric columns are right-aligned so digits line up. */
function isNumeric(c: ColumnDesc | undefined): boolean {
  return !!c && (c.type_class === "integer" || c.type_class === "exact_numeric" || c.type_class === "float");
}

export function ResultGrid({
  sessionId,
  columns,
  rows,
  truncated,
  editing,
  exporting,
}: {
  sessionId: string;
  columns: ColumnDesc[];
  rows: WireValue[][];
  truncated: boolean;
  editing?: GridEditing;
  /** T-074: offered on the truncation notice, D-010's escape hatch. */
  exporting?: GridExport;
}) {
  const scrollRef = useRef<HTMLDivElement>(null);
  // D-031 §5: compact 28px or comfortable 34px, matching `--row-h`.
  const density = useUiStore((st) => st.density);
  const rowHeight = density === "comfortable" ? 34 : 28;

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
        // D-031 §3: carried on the definition so cells read it directly.
        meta: { numeric: isNumeric(c) },
        header: () => (
          <>
            <div className={cn("flex min-w-0 items-baseline gap-1.5", isNumeric(c) && "justify-end")}>
              <span className="truncate text-foreground">{c.name}</span>
              <span className="truncate text-2xs font-normal text-muted-foreground/80">
                {c.type_name}
                {/* Schemaless engines: the column's type varied row to row,
                    so the descriptor is wrong for some of them (T-073 F-2).
                    D-032: a caution, not a failure. */}
                {c.mixed && <span className="text-caution"> · mixed</span>}
                {c.derived && <span> · inferred</span>}
              </span>
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
  const model = table.getRowModel().rows;

  const virtualizer = useVirtualizer({
    count: model.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => rowHeight,
    overscan: 12,
  });
  // A density switch changes every row's height at once.
  useEffect(() => virtualizer.measure(), [rowHeight, virtualizer]);
  const items = virtualizer.getVirtualItems();

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div ref={scrollRef} className="min-h-0 flex-1 overflow-auto">
        <table
          className="border-separate border-spacing-0 text-data"
          style={{ tableLayout: "fixed", width: table.getTotalSize() }}
        >
          <thead className="sticky top-0 z-10 bg-panel">
            {table.getHeaderGroups().map((group) => (
              <tr key={group.id}>
                {group.headers.map((h) => (
                  <th
                    key={h.id}
                    style={{ width: h.getSize() }}
                    className="relative h-row border-b border-r border-b-border-strong px-3 text-left align-middle font-mono text-xs font-medium"
                  >
                    {flexRender(h.column.columnDef.header, h.getContext())}
                    <div
                      role="separator"
                      aria-orientation="vertical"
                      aria-label="Resize column"
                      onMouseDown={h.getResizeHandler()}
                      onTouchStart={h.getResizeHandler()}
                      className={cn(
                        "absolute right-0 top-0 h-full w-1 cursor-col-resize select-none transition-colors duration-interact hover:bg-primary",
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
                <tr key={row.id} style={{ height: rowHeight }} className="transition-colors duration-interact hover:bg-accent/50">
                  {row.getVisibleCells().map((cell) => (
                    <td
                      key={cell.id}
                      style={{ width: cell.column.getSize() }}
                      className={cn(
                        "overflow-hidden border-b border-r border-border/70 px-3 align-middle font-mono",
                        // D-031 §3: numbers line up — right-aligned, tabular.
                        (cell.column.columnDef.meta as { numeric?: boolean } | undefined)?.numeric && "text-right",
                      )}
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
      <div className="flex min-h-8 flex-wrap items-center gap-x-3 gap-y-1 border-t bg-panel px-3 py-1 text-xs text-muted-foreground">
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
              className="focus-glow h-6 rounded bg-primary px-2.5 font-semibold text-primary-foreground transition-opacity duration-interact hover:opacity-90 disabled:opacity-50"
              onClick={editing.onApply}
            >
              {editing.applying ? "Working…" : "Review & apply…"}
            </button>
            <button
              type="button"
              disabled={editing.applying}
              className="focus-glow h-6 rounded border border-border-strong bg-raised px-2.5 transition-colors duration-interact hover:border-muted-foreground/50 disabled:opacity-50"
              onClick={editing.onDiscard}
            >
              Discard
            </button>
            <span className="text-muted-foreground">applied in one transaction · no undo after</span>
          </span>
        )}
        <span className="font-mono text-2xs">
          {rows.length} rows · {items.length} rendered
        </span>
        {/* D-032: truncation is a caution — there is more than is shown. */}
        {truncated && (
          <span className="inline-flex h-5 items-center rounded-sm border border-caution/60 bg-caution/10 px-1.5 text-2xs text-caution">
            truncated — more rows available
          </span>
        )}
        {truncated && exporting && <ExportActions exporting={exporting} />}
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
              className="focus-glow rounded border border-transparent px-1 hover:bg-accent"
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
              className="focus-glow rounded border border-transparent px-1 hover:bg-accent"
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
        {note && <span className="truncate text-2xs">{note}</span>}
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
      className={cn(
        "flex items-center gap-1",
        // D-032: a staged, unapplied change is a caution until applied.
        pending !== undefined && "-mx-3 bg-caution/10 px-3 shadow-inset-caution",
      )}
      title={reason ?? (pending !== undefined ? `was: ${renderValue(read)}` : "Double-click to edit")}
      onDoubleClick={() => {
        if (!reason && !disabled) setOpen(true);
      }}
    >
      <div className="min-w-0 flex-1">
        <Cell sessionId={sessionId} value={shown} column={column} />
      </div>
      {pending !== undefined && !disabled && (
        <button
          type="button"
          title="Revert this cell"
          className="focus-glow shrink-0 rounded border border-transparent p-0.5 text-muted-foreground hover:text-foreground"
          onClick={onUnstage}
        >
          <Undo2 className="h-3 w-3" />
        </button>
      )}
    </div>
  );
}

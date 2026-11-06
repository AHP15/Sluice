import { useMemo, useRef, useState } from "react";
import {
  flexRender,
  getCoreRowModel,
  useReactTable,
  type ColumnDef,
} from "@tanstack/react-table";
import { useVirtualizer } from "@tanstack/react-virtual";
import { Download, Eye, Loader2 } from "lucide-react";
import type { ColumnDesc, WireValue } from "../lib/contract";
import { formatBytes, isAbsent, renderValue } from "../lib/contract";
import { fetchValue, saveValueToFile } from "../lib/ipc";
import { cn } from "../lib/utils";

type Row = WireValue[];

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
}: {
  sessionId: string;
  columns: ColumnDesc[];
  rows: WireValue[][];
  truncated: boolean;
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
        cell: (ctx) => <Cell sessionId={sessionId} value={ctx.getValue()} column={c} />,
      })),
    [columns, sessionId],
  );

  const table = useReactTable({
    data: rows,
    columns: columnDefs,
    getCoreRowModel: getCoreRowModel(),
    columnResizeMode: "onChange",
  });
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

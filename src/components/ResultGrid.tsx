import { useMemo, useRef, useState } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { Download, Eye, Loader2 } from "lucide-react";
import type { ColumnDesc, WireValue } from "../lib/contract";
import { formatBytes, isAbsent, renderValue } from "../lib/contract";
import { fetchValue, saveValueToFile } from "../lib/ipc";
import { cn } from "../lib/utils";

/**
 * The result grid (T-002 grid shell, now rendering real contract values).
 *
 * Two properties this file has to keep:
 *
 * - **Virtualized** (D-010): result sets are arbitrarily long, so the DOM holds
 *   a window, never the set.
 * - **Text nodes only** (T-021/D-007): every value below reaches the DOM through
 *   `{string}`. There is no markup path for database content, which is what
 *   bounds an XSS here to "someone read one table".
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

  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => rowHeight,
    overscan: 12,
  });
  const items = virtualizer.getVirtualItems();
  const widths = useMemo(() => columns.map(() => 180), [columns]);

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div ref={scrollRef} className="min-h-0 flex-1 overflow-auto">
        <table className="border-collapse text-sm" style={{ tableLayout: "fixed" }}>
          <thead className="sticky top-0 z-10 bg-secondary">
            <tr>
              {columns.map((c, i) => (
                <th
                  key={c.name}
                  style={{ width: widths[i] }}
                  className="border-b border-r px-2 py-1 text-left align-top font-medium"
                >
                  <div className="truncate">{c.name}</div>
                  <div className="truncate text-[10px] font-normal text-muted-foreground">
                    {c.type_name}
                    {/* Schemaless engines: the column's type varied row to row,
                        so the descriptor is wrong for some of them (T-073 F-2). */}
                    {c.mixed && <span className="text-destructive"> · mixed</span>}
                    {c.derived && <span> · inferred</span>}
                  </div>
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {items.length > 0 && <tr style={{ height: items[0].start }} aria-hidden="true" />}
            {items.map((vi) => (
              <tr key={vi.index} style={{ height: rowHeight }} className="hover:bg-accent">
                {rows[vi.index].map((cell, ci) => (
                  <td
                    key={ci}
                    style={{ width: widths[ci] }}
                    className="overflow-hidden border-b border-r px-2 align-middle font-mono"
                  >
                    <Cell sessionId={sessionId} value={cell} column={columns[ci]} />
                  </td>
                ))}
              </tr>
            ))}
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

import { useEffect, useMemo, useRef, useState } from "react";
import {
  flexRender,
  getCoreRowModel,
  useReactTable,
  type ColumnDef,
} from "@tanstack/react-table";
import { useVirtualizer } from "@tanstack/react-virtual";
import {
  ChevronLeft,
  ChevronRight,
  Download,
  Eye,
  EyeOff,
  Loader2,
  Lock,
  Pin,
  PinOff,
  Plus,
  RefreshCw,
  Trash2,
  Undo2,
  X,
} from "lucide-react";
import type { ColumnDesc, ColumnInference, EditGate, WireValue } from "../lib/contract";
import * as columnLayout from "../lib/columnLayout";
import type { CachedLayout, ColumnLayout, LayoutColumn } from "../lib/columnLayout";
import { formatBytes, isAbsent, renderValue } from "../lib/contract";
import { fetchValue, saveValueToFile } from "../lib/ipc";
import {
  cellReadOnlyReason,
  newCellReadOnlyReason,
  pendingCount,
  staged,
  type NewRows,
  type Pending,
} from "../lib/pendingEdits";
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
  /** Rows added in the grid, shown after the rows as read until applied. */
  newRows: NewRows;
  onAddRow: () => void;
  onStageNew: (row: number, col: number, value: WireValue) => void;
  /** Put a new row's cell back to the column default. */
  onUnstageNew: (row: number, col: number) => void;
  onRemoveNew: (row: number) => void;
}

/**
 * T-063 / D-018: a schemaless relation's columns are derived, so the grid
 * offers the user control over them. `key` names the collection the layout
 * and inference are cached under; `infer` re-runs inference, and is called
 * only when nothing is cached for `key` or on the user's explicit refresh.
 */
export interface GridColumnLayout {
  key: string;
  infer: () => Promise<ColumnInference>;
}

/** One rendered column: where its value comes from and whether it edits positionally. */
interface ColumnSpec {
  id: string;
  desc: ColumnDesc;
  get: (row: Row) => WireValue;
  /** The page column index an edit stages against; null renders read-only. */
  edit: number | null;
  layout?: LayoutColumn;
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
  layout,
}: {
  sessionId: string;
  columns: ColumnDesc[];
  rows: WireValue[][];
  truncated: boolean;
  editing?: GridEditing;
  /** T-074: offered on the truncation notice, D-010's escape hatch. */
  exporting?: GridExport;
  /** T-063: set for a relation whose columns are derived (D-018). */
  layout?: GridColumnLayout;
}) {
  const scrollRef = useRef<HTMLDivElement>(null);
  // D-031 §5: compact 28px or comfortable 34px, matching `--row-h`.
  const density = useUiStore((st) => st.density);
  const rowHeight = density === "comfortable" ? 34 : 28;

  // T-063: the cached layout for this collection, and an inference in flight.
  const layoutKey = layout?.key;
  const [entry, setEntry] = useState<CachedLayout | null>(null);
  const [inferring, setInferring] = useState(false);
  const [inferError, setInferError] = useState<string | null>(null);
  const inferRef = useRef(layout?.infer);
  inferRef.current = layout?.infer;
  const layoutKeyRef = useRef(layoutKey);
  layoutKeyRef.current = layoutKey;

  const runInference = async (key: string) => {
    const infer = inferRef.current;
    if (!infer) return;
    setInferring(true);
    setInferError(null);
    try {
      const inference = await infer();
      const cur = columnLayout.layoutCache.get(key) ?? { inference: null, layout: columnLayout.EMPTY_LAYOUT };
      const next = { ...cur, inference };
      columnLayout.layoutCache.set(key, next);
      // A result for a collection no longer on screen is cached, not shown.
      if (layoutKeyRef.current === key) setEntry(next);
    } catch (e) {
      if (layoutKeyRef.current === key) setInferError(String((e as { message?: string })?.message ?? e));
    } finally {
      setInferring(false);
    }
  };

  useEffect(() => {
    setInferError(null);
    if (!layoutKey) {
      setEntry(null);
      return;
    }
    const cached = columnLayout.layoutCache.get(layoutKey);
    if (cached) {
      setEntry(cached);
      return;
    }
    const fresh = { inference: null, layout: columnLayout.EMPTY_LAYOUT };
    columnLayout.layoutCache.set(layoutKey, fresh);
    setEntry(fresh);
    void runInference(layoutKey);
    // runInference reads the latest `infer` through a ref; only the
    // collection changing should trigger it.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [layoutKey]);

  const setLayout = (f: (l: ColumnLayout) => ColumnLayout) => {
    if (!layoutKey) return;
    const cur = columnLayout.layoutCache.get(layoutKey) ?? { inference: null, layout: columnLayout.EMPTY_LAYOUT };
    const next = { ...cur, layout: f(cur.layout) };
    columnLayout.layoutCache.set(layoutKey, next);
    setEntry(next);
  };

  const resolved = useMemo(
    () => (entry ? columnLayout.resolveColumns(entry.inference, entry.layout, columns) : null),
    [entry, columns],
  );

  // Without a layout: one spec per descriptor, indexed positionally — the
  // contract's rows are arrays aligned with `columns`, and column names are not
  // guaranteed unique (a script can select `a, a`), so ids are positional too.
  // With one (T-063): one spec per visible path, looked up, never converted.
  const specs = useMemo<ColumnSpec[]>(
    () =>
      resolved
        ? resolved.visible.map((lc) => ({
            id: `p${lc.key}`,
            desc: columnLayout.layoutDesc(lc, columns),
            get: (row: Row) => columnLayout.pathValue(columns, row, lc.path),
            edit: lc.pageIndex,
            layout: lc,
          }))
        : columns.map((c, i) => ({ id: `c${i}`, desc: c, get: (row: Row) => row[i], edit: i })),
    [resolved, columns],
  );
  const visibleKeys = resolved ? resolved.visible.map((c) => c.key) : [];
  const controlsRef = useRef({ setLayout, visibleKeys });
  controlsRef.current = { setLayout, visibleKeys };
  const sampled = entry?.inference?.sampled ?? 0;

  const columnDefs = useMemo<ColumnDef<Row, WireValue>[]>(
    () =>
      specs.map(({ id, desc: c, get, edit: i, layout: lc }, specIndex) => ({
        id,
        accessorFn: get,
        size: 180,
        minSize: 60,
        // D-031 §3: carried on the definition so cells read it directly.
        meta: { numeric: isNumeric(c) },
        header: () => (
          <>
            <div className={cn("flex min-w-0 items-baseline gap-1.5", isNumeric(c) && "justify-end")}>
              {lc?.pinned && <Pin aria-label="pinned" className="h-3 w-3 shrink-0 self-center text-primary" />}
              <span className="truncate text-foreground">{c.name}</span>
              <span className="truncate text-2xs font-normal text-muted-foreground/80">
                {c.type_name}
                {/* Schemaless engines: the column's type varied row to row,
                    so the descriptor is wrong for some of them (T-073 F-2).
                    D-032: a caution, not a failure. */}
                {c.mixed && <span className="text-caution"> · mixed</span>}
                {c.derived && <span> · inferred</span>}
                {lc?.inferred && sampled > 0 && (
                  <span title="share of sampled documents carrying this path">
                    {" · "}
                    {columnLayout.frequencyLabel(lc.inferred, sampled)}
                  </span>
                )}
              </span>
              {lc && (
                <HeaderControls
                  column={lc}
                  onLayout={(f) => controlsRef.current.setLayout(f)}
                  order={() => controlsRef.current.visibleKeys}
                />
              )}
            </div>
          </>
        ),
        cell: (ctx) => {
          // Read through table meta so staging does not rebuild the columns.
          const ed = (ctx.table.options.meta as { editing?: GridEditing } | undefined)?.editing;
          const read = ctx.getValue();
          const row = ctx.row.index;
          // A row added in the grid: it sits after the rows as read.
          const added = ed && ed.gate.state === "editable" ? row - (ctx.table.options.meta as { readCount: number }).readCount : -1;
          if (ed && added >= 0) {
            return (
              <NewCell
                sessionId={sessionId}
                column={c}
                value={i === null ? undefined : ed.newRows[added]?.get(i)}
                editable={i !== null}
                disabled={ed.applying}
                onStage={(v) => i !== null && ed.onStageNew(added, i, v)}
                onUnstage={() => i !== null && ed.onUnstageNew(added, i)}
                onRemove={specIndex === 0 ? () => ed.onRemoveNew(added) : undefined}
              />
            );
          }
          // T-063: a nested or hand-added path is not a page column, so it has
          // no positional edit identity; it renders read-only (T-062 decides
          // which paths become editable).
          if (i === null) return <Cell sessionId={sessionId} value={read} column={c} />;
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
    [specs, sessionId, sampled],
  );

  // New rows join the table model after the rows as read, so they scroll,
  // size and virtualize like any other row. Their cells read the staged value
  // through table meta; this placeholder only gives each one a row shape.
  const insertable = editing?.gate.state === "editable";
  const newCount = insertable ? editing.newRows.length : 0;
  const data = useMemo(
    () => (newCount ? rows.concat(Array.from({ length: newCount }, () => columns.map((): WireValue => ({ t: "null" })))) : rows),
    [rows, columns, newCount],
  );
  const table = useReactTable({
    data,
    columns: columnDefs,
    getCoreRowModel: getCoreRowModel(),
    columnResizeMode: "onChange",
    meta: { editing, readCount: rows.length },
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

  // Bring a just-added row into view.
  const lastNew = useRef(newCount);
  useEffect(() => {
    if (newCount > lastNew.current) virtualizer.scrollToIndex(model.length - 1, { align: "end" });
    lastNew.current = newCount;
  }, [newCount, model.length, virtualizer]);

  return (
    <div className="flex h-full min-h-0 flex-col">
      {layoutKey && resolved && (
        <LayoutBar
          inference={entry?.inference ?? null}
          hidden={resolved.hidden}
          inferring={inferring}
          error={inferError}
          onRefresh={() => void runInference(layoutKey)}
          onLayout={setLayout}
        />
      )}
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
              const isNew = vi.index >= rows.length;
              return (
                <tr
                  key={row.id}
                  style={{ height: rowHeight }}
                  data-new-row={isNew || undefined}
                  className={cn(
                    "transition-colors duration-interact hover:bg-accent/50",
                    // D-032: a staged, unapplied row is a caution until applied.
                    isNew && "bg-caution/5",
                  )}
                >
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
        {rows.length === 0 && newCount === 0 && (
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
        {insertable && (
          <button
            type="button"
            disabled={editing.applying}
            title="Add a row; columns you leave alone take their default"
            className="focus-glow inline-flex h-6 items-center gap-1 rounded border border-border-strong bg-raised px-2 text-foreground transition-colors duration-interact hover:border-muted-foreground/50 disabled:opacity-50"
            onClick={editing.onAddRow}
          >
            <Plus className="h-3 w-3" />
            Add row
          </button>
        )}
        {editing?.gate.state === "editable" && changed + newCount > 0 && (
          <span className="flex items-center gap-2 text-foreground">
            <span>
              {[
                changed > 0 && `${changed} pending change${changed === 1 ? "" : "s"}`,
                newCount > 0 && `${newCount} new row${newCount === 1 ? "" : "s"}`,
              ]
                .filter(Boolean)
                .join(" · ")}
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

/**
 * A cell of a row added in the grid. With no value it shows the column will
 * take its default; double-click gives it one, and revert puts the default
 * back. The first cell of the row also carries the row's remove button.
 */
function NewCell({
  sessionId,
  column,
  value,
  editable,
  disabled,
  onStage,
  onUnstage,
  onRemove,
}: {
  sessionId: string;
  column: ColumnDesc;
  value: WireValue | undefined;
  editable: boolean;
  disabled: boolean;
  onStage: (v: WireValue) => void;
  onUnstage: () => void;
  onRemove?: () => void;
}) {
  const [open, setOpen] = useState(false);
  const reason = editable ? newCellReadOnlyReason(column) : "This path is not a column of the relation.";

  if (open) {
    return (
      <CellEditor
        column={column}
        value={value ?? { t: "null" }}
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
      className={cn("flex items-center gap-1", value !== undefined && "-mx-3 bg-caution/10 px-3 shadow-inset-caution")}
      title={reason ?? (value === undefined ? "Takes the column default · double-click to set a value" : "Double-click to edit")}
      onDoubleClick={() => {
        if (!reason && !disabled) setOpen(true);
      }}
    >
      {onRemove && (
        <button
          type="button"
          title="Remove this new row"
          aria-label="Remove new row"
          disabled={disabled}
          className="focus-glow shrink-0 rounded border border-transparent p-0.5 text-muted-foreground hover:text-destructive disabled:opacity-50"
          onClick={onRemove}
        >
          <Trash2 className="h-3 w-3" />
        </button>
      )}
      <div className="min-w-0 flex-1">
        {value === undefined ? (
          <span className="block truncate italic text-muted-foreground">DEFAULT</span>
        ) : (
          <Cell sessionId={sessionId} value={value} column={column} />
        )}
      </div>
      {value !== undefined && !disabled && (
        <button
          type="button"
          title="Use the column default"
          className="focus-glow shrink-0 rounded border border-transparent p-0.5 text-muted-foreground hover:text-foreground"
          onClick={onUnstage}
        >
          <Undo2 className="h-3 w-3" />
        </button>
      )}
    </div>
  );
}

/**
 * T-063: per-column layout controls — pin, move, hide, and remove for a path
 * added by hand. `order` is read at click time so the header definitions need
 * not rebuild on every layout change.
 */
function HeaderControls({
  column,
  onLayout,
  order,
}: {
  column: LayoutColumn;
  onLayout: (f: (l: ColumnLayout) => ColumnLayout) => void;
  order: () => string[];
}) {
  const btn =
    "focus-glow shrink-0 rounded border border-transparent p-0.5 text-muted-foreground hover:bg-accent hover:text-foreground";
  const k = column.key;
  return (
    <span className="ml-auto flex shrink-0 items-center gap-0.5 self-center">
      <button
        type="button"
        className={btn}
        title={column.pinned ? `Unpin ${column.label}` : `Pin ${column.label}`}
        onClick={() => onLayout((l) => columnLayout.togglePin(l, k))}
      >
        {column.pinned ? <PinOff className="h-3 w-3" /> : <Pin className="h-3 w-3" />}
      </button>
      <button
        type="button"
        className={btn}
        title={`Move ${column.label} left`}
        onClick={() => onLayout((l) => columnLayout.move(l, order(), k, -1))}
      >
        <ChevronLeft className="h-3 w-3" />
      </button>
      <button
        type="button"
        className={btn}
        title={`Move ${column.label} right`}
        onClick={() => onLayout((l) => columnLayout.move(l, order(), k, 1))}
      >
        <ChevronRight className="h-3 w-3" />
      </button>
      {column.added ? (
        <button
          type="button"
          className={btn}
          title={`Remove ${column.label}`}
          onClick={() => onLayout((l) => columnLayout.removeAdded(l, k))}
        >
          <X className="h-3 w-3" />
        </button>
      ) : (
        <button
          type="button"
          className={btn}
          title={`Hide ${column.label}`}
          onClick={() => onLayout((l) => columnLayout.hide(l, k))}
        >
          <EyeOff className="h-3 w-3" />
        </button>
      )}
    </span>
  );
}

/**
 * T-063 / D-018: what the columns were derived from, the explicit refresh, a
 * path added by hand, and every hidden column offered back — hiding is the
 * user's choice and is never silent.
 */
function LayoutBar({
  inference,
  hidden,
  inferring,
  error,
  onRefresh,
  onLayout,
}: {
  inference: ColumnInference | null;
  hidden: LayoutColumn[];
  inferring: boolean;
  error: string | null;
  onRefresh: () => void;
  onLayout: (f: (l: ColumnLayout) => ColumnLayout) => void;
}) {
  const [draft, setDraft] = useState("");
  const parsed = columnLayout.parsePath(draft);
  const source = !inference
    ? "columns not inferred yet"
    : inference.from_validator
      ? `columns from the validator · ${inference.sampled} documents sampled`
      : `columns inferred from ${inference.sampled} documents${inference.sample_truncated ? ` (first ${inference.sample_limit})` : ""}`;
  return (
    <div className="flex min-h-8 flex-wrap items-center gap-x-3 gap-y-1 border-b bg-panel px-3 py-1 text-xs text-muted-foreground">
      <span className="font-mono text-2xs">{source}</span>
      <button
        type="button"
        disabled={inferring}
        title="Re-run column inference"
        className="focus-glow inline-flex h-6 items-center gap-1 rounded border border-border-strong bg-raised px-2 text-foreground transition-colors duration-interact hover:border-muted-foreground/50 disabled:opacity-50"
        onClick={onRefresh}
      >
        {inferring ? <Loader2 className="h-3 w-3 animate-spin" /> : <RefreshCw className="h-3 w-3" />}
        Refresh columns
      </button>
      <form
        className="flex items-center gap-1"
        onSubmit={(e) => {
          e.preventDefault();
          if (!parsed) return;
          onLayout((l) => columnLayout.addPath(l, parsed));
          setDraft("");
        }}
      >
        <input
          aria-label="Add a column by path"
          placeholder="add path, e.g. address.city"
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          className="focus-glow h-6 w-48 rounded border border-input bg-transparent px-2 font-mono text-xs text-foreground"
        />
        <button
          type="submit"
          disabled={!parsed}
          title="Add column"
          className="focus-glow inline-flex h-6 items-center rounded border border-border-strong bg-raised px-1.5 text-foreground disabled:opacity-50"
        >
          <Plus className="h-3 w-3" />
        </button>
      </form>
      {hidden.length > 0 && (
        <span className="flex flex-wrap items-center gap-1">
          <span>hidden:</span>
          {hidden.map((h) => (
            <button
              key={h.key}
              type="button"
              title={`Show ${h.label}`}
              className="focus-glow inline-flex h-5 items-center gap-1 rounded-sm border border-border px-1.5 font-mono text-2xs hover:bg-accent hover:text-foreground"
              onClick={() => onLayout((l) => columnLayout.show(l, h.key))}
            >
              <Eye className="h-3 w-3" />
              {h.label}
            </button>
          ))}
        </span>
      )}
      {error && <span className="text-destructive">inference failed: {error}</span>}
    </div>
  );
}

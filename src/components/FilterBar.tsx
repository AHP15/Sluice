import { useState } from "react";
import { ListFilter, Plus, X } from "lucide-react";
import type { ColumnDesc, FilterOpDecl, FilterTree } from "../lib/contract";
import { arity, buildFilter, newRow, opsFor, type FilterRow } from "../lib/filter";
import { Button } from "./ui/button";

/**
 * The browse filter bar (D-005): a flat AND list of `column · operator ·
 * value` rows. Operators come from the driver's capability descriptor,
 * matched to each column's type class — the bar never knows engine type
 * names. A case-insensitive toggle appears only where the operator declares
 * one (T-015). Applying hands a wire `FilterTree` to the caller; the core
 * compiles and binds it (T-009).
 */
export function FilterBar({
  columns,
  ops,
  onApply,
}: {
  columns: ColumnDesc[];
  ops: FilterOpDecl[];
  onApply: (filter: FilterTree | null) => void;
}) {
  const [rows, setRows] = useState<FilterRow[]>([]);
  const [applied, setApplied] = useState(0);

  const update = (i: number, patch: Partial<FilterRow>) =>
    setRows((rs) => rs.map((r, j) => (j === i ? { ...r, ...patch } : r)));

  const apply = (next: FilterRow[]) => {
    const tree = buildFilter(next, columns, ops);
    setApplied(tree && tree.node === "and" ? tree[0].length : 0);
    onApply(tree);
  };

  if (columns.length === 0) return null;

  return (
    <form
      aria-label="Filter rows"
      className="flex flex-col gap-1 border-b px-2 py-1 text-xs"
      onSubmit={(e) => {
        e.preventDefault();
        apply(rows);
      }}
    >
      {rows.map((row, i) => {
        const available = opsFor(columns[row.column], ops);
        const decl = available.find((o) => o.id === row.op);
        const n = arity(row.op);
        return (
          <div key={i} className="flex items-center gap-1" data-testid="filter-row">
            <select
              aria-label="Column"
              className="rounded border border-input bg-transparent px-1 py-0.5"
              value={row.column}
              onChange={(e) => {
                const column = Number(e.target.value);
                const nextOps = opsFor(columns[column], ops);
                // Keep the operator if the new column still supports it.
                const op = nextOps.some((o) => o.id === row.op) ? row.op : (nextOps[0]?.id ?? row.op);
                update(i, { column, op });
              }}
            >
              {columns.map((c, ci) => (
                <option key={ci} value={ci}>
                  {c.name}
                </option>
              ))}
            </select>
            <select
              aria-label="Operator"
              className="rounded border border-input bg-transparent px-1 py-0.5"
              value={row.op}
              onChange={(e) => update(i, { op: e.target.value })}
            >
              {available.map((o) => (
                <option key={o.id} value={o.id}>
                  {o.label}
                </option>
              ))}
            </select>
            {n !== 0 && (
              <input
                aria-label="Value"
                className="w-40 rounded border border-input bg-transparent px-1 py-0.5"
                placeholder={n === "list" ? "a, b, c" : n === 2 ? "from" : "value"}
                value={row.value}
                onChange={(e) => update(i, { value: e.target.value })}
              />
            )}
            {n === 2 && (
              <input
                aria-label="Upper value"
                className="w-40 rounded border border-input bg-transparent px-1 py-0.5"
                placeholder="to"
                value={row.value2}
                onChange={(e) => update(i, { value2: e.target.value })}
              />
            )}
            {decl?.case_insensitive_toggle && (
              <label className="flex items-center gap-1 text-muted-foreground">
                <input
                  type="checkbox"
                  aria-label="Ignore case"
                  checked={row.caseInsensitive}
                  onChange={(e) => update(i, { caseInsensitive: e.target.checked })}
                />
                Aa
              </label>
            )}
            <button
              type="button"
              aria-label="Remove filter"
              className="rounded p-0.5 text-muted-foreground hover:text-destructive"
              onClick={() => {
                const next = rows.filter((_, j) => j !== i);
                setRows(next);
                apply(next);
              }}
            >
              <X className="h-3.5 w-3.5" />
            </button>
          </div>
        );
      })}
      <div className="flex items-center gap-1">
        <button
          type="button"
          className="flex items-center gap-1 rounded px-1 py-0.5 text-muted-foreground hover:text-foreground"
          onClick={() => setRows((rs) => [...rs, newRow(columns, ops)])}
        >
          {rows.length === 0 ? <ListFilter className="h-3.5 w-3.5" /> : <Plus className="h-3.5 w-3.5" />}
          {rows.length === 0 ? "Filter" : "And"}
        </button>
        {rows.length > 0 && (
          <>
            <Button type="submit" size="sm" variant="secondary" className="h-6">
              Apply
            </Button>
            <button
              type="button"
              className="rounded px-1 py-0.5 text-muted-foreground hover:text-foreground"
              onClick={() => {
                setRows([]);
                apply([]);
              }}
            >
              Clear
            </button>
          </>
        )}
        {applied > 0 && (
          <span className="text-muted-foreground" data-testid="filter-applied">
            {applied} filter{applied === 1 ? "" : "s"} applied
          </span>
        )}
      </div>
    </form>
  );
}

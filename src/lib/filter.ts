import type { ColumnDesc, FilterOpDecl, FilterTree, Predicate, WireValue } from "./contract";

/**
 * The filter bar's model (D-005): a flat AND list of predicates. The wire
 * format already carries a full AND/OR tree; v1's UI only produces the flat
 * AND case, which is the tree `{and: [pred, pred, …]}`.
 *
 * Every value is sent as text. The driver binds it as a parameter with a cast
 * to the column's own type (T-009/T-013), so an exact numeric typed here is
 * never parsed as a JavaScript number on the way (D-006/R-005), and nothing
 * the user types is ever interpolated into SQL.
 */
export interface FilterRow {
  /** Positional column index — names are not guaranteed unique. */
  column: number;
  op: string;
  value: string;
  /** Upper bound, for `between`. */
  value2: string;
  caseInsensitive: boolean;
}

/** Operators that take no value. */
const NULLARY = new Set(["is_null", "is_not_null"]);

export function arity(op: string): 0 | 1 | 2 | "list" {
  if (NULLARY.has(op)) return 0;
  if (op === "between") return 2;
  if (op === "in") return "list";
  return 1;
}

/**
 * The operators that apply to a column: the driver's declarations whose
 * type classes include the column's (D-005). The UI never decides this from
 * engine type names.
 */
export function opsFor(column: ColumnDesc | undefined, ops: FilterOpDecl[]): FilterOpDecl[] {
  if (!column) return [];
  return ops.filter((o) => o.type_classes.includes(column.type_class));
}

export function newRow(columns: ColumnDesc[], ops: FilterOpDecl[]): FilterRow {
  const first = opsFor(columns[0], ops)[0];
  return { column: 0, op: first?.id ?? "eq", value: "", value2: "", caseInsensitive: false };
}

const text = (v: string): WireValue => ({ t: "text", v });

/** One row as a predicate, or `null` while it is still incomplete. */
export function toPredicate(row: FilterRow, columns: ColumnDesc[], ops: FilterOpDecl[]): Predicate | null {
  const column = columns[row.column];
  const decl = opsFor(column, ops).find((o) => o.id === row.op);
  if (!column || !decl) return null;

  let value: WireValue;
  switch (arity(row.op)) {
    case 0:
      value = { t: "null" };
      break;
    case 2:
      if (row.value === "" || row.value2 === "") return null;
      value = { t: "array", v: [text(row.value), text(row.value2)] };
      break;
    case "list": {
      const items = row.value
        .split(",")
        .map((s) => s.trim())
        .filter((s) => s !== "");
      if (items.length === 0) return null;
      value = { t: "array", v: items.map(text) };
      break;
    }
    default:
      // An empty string is a legitimate value for text equality, but for a
      // half-typed row it is almost never what was meant; require something.
      if (row.value === "") return null;
      value = text(row.value);
  }

  const p: Predicate = { column: column.name, op: row.op, value };
  if (row.caseInsensitive && decl.case_insensitive_toggle) p.case_insensitive = true;
  return p;
}

/** The whole bar as a wire filter: `null` when nothing is complete. */
export function buildFilter(rows: FilterRow[], columns: ColumnDesc[], ops: FilterOpDecl[]): FilterTree | null {
  const preds = rows.map((r) => toPredicate(r, columns, ops)).filter((p): p is Predicate => p !== null);
  if (preds.length === 0) return null;
  return { node: "and", 0: preds.map((p) => ({ node: "pred", 0: p }) as FilterTree) };
}

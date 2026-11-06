/**
 * T-056 / D-027: staged cell edits, held client-side until applied.
 *
 * The rules this module carries:
 *
 * - Edits **accumulate** as pending changes; nothing is written until the user
 *   applies them, and then all of them go to the core together, where they run
 *   in ONE transaction.
 * - **Undo is discard-pending-changes only.** Once applied there is no undo;
 *   this module has no notion of a committed edit to reverse.
 * - Editing a cell back to the value it was read with un-stages it, so the
 *   pending count is the number of cells that would actually change.
 * - The edit carries values, never SQL (D-029), and identifies its row by the
 *   unique key the core declared (`EditGate.key`), with key values taken from
 *   the row **as read**, so editing a key column still targets the original row.
 *
 * Pure functions over immutable maps, so React state updates stay trivial and
 * every rule is unit-testable without rendering.
 */
import type { ColumnDesc, EditGate, EditRequest, NodeId, Original, WireValue } from "./contract";
import { checkExact } from "./exactNumeric";

/** row index → column index → staged value. */
export type Pending = ReadonlyMap<number, ReadonlyMap<number, WireValue>>;

export const EMPTY: Pending = new Map();

const SCALAR = new Set<WireValue["t"]>(["bool", "int", "float", "numeric", "text", "timestamp", "uuid"]);

/**
 * Whether two values are the same for staging purposes. Typed input arrives
 * as text (`5` → `numeric "5"`), so a scalar retyped verbatim compares by its
 * editor text rather than by tag; NULL only ever equals NULL.
 */
export function sameValue(a: WireValue, b: WireValue): boolean {
  if (JSON.stringify(a) === JSON.stringify(b)) return true;
  return SCALAR.has(a.t) && SCALAR.has(b.t) && editorText(a) === editorText(b);
}

/** Stage `value` for a cell, or un-stage it if it equals the value as read. */
export function stage(p: Pending, row: number, col: number, value: WireValue, asRead: WireValue): Pending {
  const next = new Map(p);
  const cells = new Map(p.get(row) ?? []);
  if (sameValue(value, asRead)) cells.delete(col);
  else cells.set(col, value);
  if (cells.size === 0) next.delete(row);
  else next.set(row, cells);
  return next;
}

/** Drop one cell's staged change. */
export function unstage(p: Pending, row: number, col: number): Pending {
  const cells = p.get(row);
  if (!cells?.has(col)) return p;
  const next = new Map(p);
  const rest = new Map(cells);
  rest.delete(col);
  if (rest.size === 0) next.delete(row);
  else next.set(row, rest);
  return next;
}

export function staged(p: Pending, row: number, col: number): WireValue | undefined {
  return p.get(row)?.get(col);
}

/** Changed cells across all rows. */
export function pendingCount(p: Pending): number {
  let n = 0;
  for (const cells of p.values()) n += cells.size;
  return n;
}

/**
 * Why a cell cannot be edited, or `null` if it can. Values the core never
 * round-trips (placeholders, undecodable types) and classes with no editor
 * here are refused up front rather than failing on apply.
 */
export function cellReadOnlyReason(column: ColumnDesc, value: WireValue): string | null {
  switch (value.t) {
    case "oversized":
      return "This value was too large to load, so it cannot be edited here.";
    case "unsupported":
      return "This type could not be decoded, so it cannot be written back.";
    case "missing":
    case "document":
    case "array":
    case "opaque":
      return "Structured values are not editable here.";
    case "bytes":
      return "Binary values are not editable here.";
  }
  switch (column.type_class) {
    case "binary":
    case "structural":
    case "opaque":
    case "oversized":
    case "unknown":
      return `${column.type_name} values are not editable here.`;
  }
  return null;
}

export type Parsed = { ok: true; value: WireValue } | { ok: false; reason: string };

/**
 * Turn editor text into a wire value for `column`. Exact numerics go through
 * `checkExact` and stay strings (T-019 / R-005). Everything else is validated
 * only as far as the client can know; the database is the final validator.
 */
export function parseCellInput(column: ColumnDesc, text: string): Parsed {
  if (column.exact) return checkExact(text, column.exact);
  switch (column.type_class) {
    case "boolean": {
      const t = text.trim().toLowerCase();
      if (t === "true") return { ok: true, value: { t: "bool", v: true } };
      if (t === "false") return { ok: true, value: { t: "bool", v: false } };
      return { ok: false, reason: "Enter true or false." };
    }
    // Inexact numbers too travel as the literal text typed, cast by the
    // database to the column's type: no JS number is ever in the path, so
    // what the user typed is what is bound (R-005), and range is the
    // database's to check.
    case "integer": {
      const t = text.trim();
      if (!/^[+-]?\d+$/.test(t)) return { ok: false, reason: "Enter a whole number." };
      return { ok: true, value: { t: "numeric", v: t } };
    }
    case "float": {
      const t = text.trim();
      if (!/^[+-]?(\d+\.?\d*|\.\d+)([eE][+-]?\d+)?$/.test(t) && !/^[+-]?(Infinity|NaN)$/i.test(t)) {
        return { ok: false, reason: "Enter a number." };
      }
      return { ok: true, value: { t: "numeric", v: t } };
    }
    case "timestamp":
      return { ok: true, value: { t: "timestamp", v: text } };
    case "uuid":
      return { ok: true, value: { t: "uuid", v: text.trim() } };
    case "text":
      return { ok: true, value: { t: "text", v: text } };
    default:
      return { ok: false, reason: `${column.type_name} values are not editable here.` };
  }
}

/** The text an editor opens with for a value. */
export function editorText(v: WireValue): string {
  switch (v.t) {
    case "null":
      return "";
    case "bool":
      return v.v ? "true" : "false";
    case "int":
    case "float":
      return String(v.v);
    case "numeric":
    case "text":
    case "timestamp":
    case "uuid":
      return v.v;
    default:
      return "";
  }
}

export type Built = { ok: true; edits: EditRequest[] } | { ok: false; reason: string };

/** One `EditRequest` per changed row, in row order. */
export function buildEditRequests(
  p: Pending,
  relation: NodeId,
  columns: ColumnDesc[],
  rows: WireValue[][],
  gate: EditGate,
): Built {
  if (gate.state !== "editable") return { ok: false, reason: gate.reason };
  const keyIdx: number[] = [];
  for (const k of gate.key) {
    const i = columns.findIndex((c) => c.name === k);
    if (i < 0) return { ok: false, reason: `The key column ${k} is not in this result.` };
    keyIdx.push(i);
  }
  const edits: EditRequest[] = [];
  for (const r of [...p.keys()].sort((a, b) => a - b)) {
    const cells = p.get(r)!;
    const row = rows[r];
    if (!row) return { ok: false, reason: "A staged row is no longer in the result." };
    const key: Original[] = keyIdx.map((i) => ({ column: columns[i].name, value: row[i] }));
    const cols = [...cells.keys()].sort((a, b) => a - b);
    const set: Original[] = cols.map((c) => ({ column: columns[c].name, value: cells.get(c)! }));
    const originals: Original[] = cols.map((c) => ({ column: columns[c].name, value: row[c] }));
    edits.push({ relation, key, set, originals });
  }
  return { ok: true, edits };
}

/** The rows as they stand after a committed apply. */
export function applyToRows(rows: WireValue[][], p: Pending): WireValue[][] {
  if (p.size === 0) return rows;
  return rows.map((row, r) => {
    const cells = p.get(r);
    if (!cells) return row;
    return row.map((v, c) => cells.get(c) ?? v);
  });
}

/**
 * T-058 / D-027: staged columns the core cannot check for concurrent changes,
 * so their edits are protected by the unique key only. Names, deduplicated,
 * in column order.
 */
export function unprotectedColumns(p: Pending, columns: ColumnDesc[]): string[] {
  const idx = new Set<number>();
  for (const cells of p.values()) for (const c of cells.keys()) if (!columns[c]?.concurrency_comparable) idx.add(c);
  return [...idx].sort((a, b) => a - b).map((c) => columns[c].name);
}

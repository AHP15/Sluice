/**
 * T-056 / D-027: staged edits accumulate, discard is the only undo, and the
 * requests sent to the core identify rows by the declared unique key.
 *
 * Run with `bun test tests/unit`.
 */
import { describe, expect, test } from "bun:test";

import type { ColumnDesc, EditGate, NodeId, WireValue } from "../../src/lib/contract";
import {
  EMPTY,
  applyToRows,
  buildEditRequests,
  cellReadOnlyReason,
  parseCellInput,
  pendingCount,
  stage,
  unprotectedColumns,
  unstage,
} from "../../src/lib/pendingEdits";

const col = (name: string, type_class: ColumnDesc["type_class"], extra: Partial<ColumnDesc> = {}): ColumnDesc => ({
  name,
  type_name: type_class,
  type_class,
  nullable: true,
  mixed: false,
  derived: false,
  exact: null,
  concurrency_comparable: true,
  ...extra,
});

const columns = [col("id", "integer", { nullable: false }), col("name", "text"), col("score", "float")];
const rows: WireValue[][] = [
  [{ t: "int", v: 1 }, { t: "text", v: "a" }, { t: "float", v: 1.5 }],
  [{ t: "int", v: 2 }, { t: "text", v: "b" }, { t: "null" }],
];
const rel: NodeId = { kind: "table", path: ["public", "t"] };
const gate: EditGate = { state: "editable", key: ["id"] };
const text = (v: string): WireValue => ({ t: "text", v });

describe("staging", () => {
  test("edits accumulate across cells and rows", () => {
    let p = stage(EMPTY, 0, 1, text("x"), rows[0][1]);
    p = stage(p, 1, 1, text("y"), rows[1][1]);
    p = stage(p, 1, 2, { t: "numeric", v: "2" }, rows[1][2]);
    expect(pendingCount(p)).toBe(3);
  });

  test("restaging a cell replaces its value rather than adding one", () => {
    let p = stage(EMPTY, 0, 1, text("x"), rows[0][1]);
    p = stage(p, 0, 1, text("z"), rows[0][1]);
    expect(pendingCount(p)).toBe(1);
  });

  test("editing back to the value as read un-stages the cell", () => {
    let p = stage(EMPTY, 0, 1, text("x"), rows[0][1]);
    p = stage(p, 0, 1, text("a"), rows[0][1]);
    expect(pendingCount(p)).toBe(0);
    // Typed input arrives as text: retyping 1.5 is not a change.
    expect(pendingCount(stage(EMPTY, 0, 2, { t: "numeric", v: "1.5" }, rows[0][2]))).toBe(0);
  });

  test("an empty string is a change from NULL, not the same value", () => {
    expect(pendingCount(stage(EMPTY, 1, 2, text(""), { t: "null" }))).toBe(1);
  });

  test("unstage reverts one cell; discarding is returning to EMPTY", () => {
    let p = stage(EMPTY, 0, 1, text("x"), rows[0][1]);
    p = stage(p, 0, 2, { t: "null" }, rows[0][2]);
    p = unstage(p, 0, 1);
    expect(pendingCount(p)).toBe(1);
    expect(pendingCount(EMPTY)).toBe(0);
  });

  test("staging never mutates the previous state", () => {
    const p = stage(EMPTY, 0, 1, text("x"), rows[0][1]);
    stage(p, 1, 1, text("y"), rows[1][1]);
    expect(pendingCount(p)).toBe(1);
  });
});

describe("building requests", () => {
  test("one request per changed row, keyed by the declared unique key as read", () => {
    let p = stage(EMPTY, 1, 1, text("y"), rows[1][1]);
    p = stage(p, 0, 2, { t: "null" }, rows[0][2]);
    p = stage(p, 0, 1, text("x"), rows[0][1]);
    const built = buildEditRequests(p, rel, columns, rows, gate);
    if (!built.ok) throw new Error(built.reason);
    expect(built.edits).toEqual([
      {
        relation: rel,
        key: [{ column: "id", value: { t: "int", v: 1 } }],
        set: [
          { column: "name", value: text("x") },
          { column: "score", value: { t: "null" } },
        ],
        originals: [
          { column: "name", value: text("a") },
          { column: "score", value: { t: "float", v: 1.5 } },
        ],
      },
      {
        relation: rel,
        key: [{ column: "id", value: { t: "int", v: 2 } }],
        set: [{ column: "name", value: text("y") }],
        originals: [{ column: "name", value: text("b") }],
      },
    ]);
  });

  test("editing a key column still targets the row by its key as read", () => {
    const p = stage(EMPTY, 0, 0, { t: "numeric", v: "10" }, rows[0][0]);
    const built = buildEditRequests(p, rel, columns, rows, gate);
    if (!built.ok) throw new Error(built.reason);
    expect(built.edits[0].key).toEqual([{ column: "id", value: { t: "int", v: 1 } }]);
    expect(built.edits[0].set).toEqual([{ column: "id", value: { t: "numeric", v: "10" } }]);
  });

  test("a read-only gate refuses with its reason", () => {
    const p = stage(EMPTY, 0, 1, text("x"), rows[0][1]);
    const built = buildEditRequests(p, rel, columns, rows, { state: "read_only", reason: "no key" });
    expect(built).toEqual({ ok: false, reason: "no key" });
  });

  test("a key column missing from the result refuses", () => {
    const p = stage(EMPTY, 0, 1, text("x"), rows[0][1]);
    expect(buildEditRequests(p, rel, columns, rows, { state: "editable", key: ["uuid"] }).ok).toBe(false);
  });
});

test("a committed apply folds the staged values into the rows", () => {
  const p = stage(EMPTY, 1, 1, text("y"), rows[1][1]);
  const after = applyToRows(rows, p);
  expect(after[1][1]).toEqual(text("y"));
  expect(after[0]).toBe(rows[0]);
  expect(rows[1][1]).toEqual(text("b"));
});

describe("cell input", () => {
  test("numbers stay as the text typed, never a JS number", () => {
    expect(parseCellInput(col("n", "integer"), " 42 ")).toEqual({ ok: true, value: { t: "numeric", v: "42" } });
    expect(parseCellInput(col("f", "float"), "0.1")).toEqual({ ok: true, value: { t: "numeric", v: "0.1" } });
    expect(parseCellInput(col("n", "integer"), "4.2").ok).toBe(false);
    expect(parseCellInput(col("f", "float"), "abc").ok).toBe(false);
  });

  test("exact numerics are validated against their domain", () => {
    const c = col("p", "exact_numeric", { exact: { kind: "decimal", precision: 4, scale: 2 } });
    expect(parseCellInput(c, "12.34")).toEqual({ ok: true, value: { t: "numeric", v: "12.34" } });
    expect(parseCellInput(c, "123.45").ok).toBe(false);
  });

  test("booleans accept only true or false", () => {
    expect(parseCellInput(col("b", "boolean"), "TRUE")).toEqual({ ok: true, value: { t: "bool", v: true } });
    expect(parseCellInput(col("b", "boolean"), "yes").ok).toBe(false);
  });

  test("values that never round-trip are not editable", () => {
    expect(cellReadOnlyReason(col("b", "binary"), { t: "oversized", v: { type_name: "bytea", byte_len: 9, handle: null } })).not.toBeNull();
    expect(cellReadOnlyReason(col("u", "unknown"), { t: "unsupported", v: { type_name: "x", rendered: "?" } })).not.toBeNull();
    expect(cellReadOnlyReason(col("j", "structural"), { t: "text", v: "{}" })).not.toBeNull();
    expect(cellReadOnlyReason(col("t", "text"), text("a"))).toBeNull();
  });
});

test("T-058: staged columns that cannot be compared are named", () => {
  const cols = [...columns, col("doc", "structural", { concurrency_comparable: false }), col("f", "float", { concurrency_comparable: false })];
  const r: WireValue[] = [...rows[0], text("{}"), { t: "float", v: 0.5 }];
  let p = stage(EMPTY, 0, 4, { t: "numeric", v: "1" }, r[4]);
  p = stage(p, 0, 1, text("x"), r[1]);
  p = stage(p, 0, 3, text("[]"), r[3]);
  expect(unprotectedColumns(p, cols)).toEqual(["doc", "f"]);
  expect(unprotectedColumns(stage(EMPTY, 0, 1, text("x"), r[1]), cols)).toEqual([]);
});

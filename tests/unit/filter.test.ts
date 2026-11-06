/** T-070 / D-005: the filter bar's pure model. */
import { describe, expect, test } from "bun:test";
import { buildFilter, newRow, opsFor, toPredicate, type FilterRow } from "../../src/lib/filter";
import { OPS, col } from "./fixtures";

const COLS = [col("name", "text"), col("amount", "exact_numeric"), col("doc", "structural")];
const row = (r: Partial<FilterRow>): FilterRow => ({ column: 0, op: "eq", value: "", value2: "", caseInsensitive: false, ...r });

describe("filter model", () => {
  test("operators come from the declarations matching the column's type class", () => {
    expect(opsFor(COLS[0], OPS).map((o) => o.id)).toContain("contains");
    expect(opsFor(COLS[1], OPS).map((o) => o.id)).not.toContain("contains");
    expect(opsFor(COLS[2], OPS).map((o) => o.id)).toEqual(["is_null"]);
    expect(newRow(COLS, OPS).op).toBe("eq");
  });

  test("a flat AND list of predicates, every value as text", () => {
    const tree = buildFilter(
      [row({ value: "bob" }), row({ column: 1, op: "gt", value: "9223372036854775807.000000000001" })],
      COLS,
      OPS,
    );
    expect(tree).toEqual({
      node: "and",
      0: [
        { node: "pred", 0: { column: "name", op: "eq", value: { t: "text", v: "bob" } } },
        // Exact numerics stay text end to end (R-005) — never Number().
        { node: "pred", 0: { column: "amount", op: "gt", value: { t: "text", v: "9223372036854775807.000000000001" } } },
      ],
    });
  });

  test("arity shapes the value: none, a list, or a pair", () => {
    expect(toPredicate(row({ op: "is_null" }), COLS, OPS)?.value).toEqual({ t: "null" });
    expect(toPredicate(row({ op: "in", value: "a, b ,,c" }), COLS, OPS)?.value).toEqual({
      t: "array",
      v: [{ t: "text", v: "a" }, { t: "text", v: "b" }, { t: "text", v: "c" }],
    });
    expect(toPredicate(row({ op: "between", value: "1", value2: "5" }), COLS, OPS)?.value).toEqual({
      t: "array",
      v: [{ t: "text", v: "1" }, { t: "text", v: "5" }],
    });
  });

  test("incomplete rows are skipped and an all-incomplete bar is no filter", () => {
    expect(toPredicate(row({ value: "" }), COLS, OPS)).toBeNull();
    expect(toPredicate(row({ op: "between", value: "1" }), COLS, OPS)).toBeNull();
    expect(toPredicate(row({ column: 2, op: "eq", value: "x" }), COLS, OPS)).toBeNull(); // op not declared for the class
    expect(buildFilter([row({})], COLS, OPS)).toBeNull();
  });

  test("case-insensitivity is sent only where the operator declares the toggle", () => {
    expect(toPredicate(row({ value: "x", caseInsensitive: true }), COLS, OPS)?.case_insensitive).toBe(true);
    expect(toPredicate(row({ op: "gt", value: "x", caseInsensitive: true }), COLS, OPS)?.case_insensitive).toBeUndefined();
  });
});

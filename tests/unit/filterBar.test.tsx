/**
 * T-070 / D-021: the filter bar, rendered in a DOM with fake columns and
 * operator declarations — no Tauri IPC. What the user does in the bar must
 * come out as exactly the wire filter the core compiles.
 */
import { describe, expect, test } from "bun:test";
import { fireEvent, render, screen, within } from "@testing-library/react";

import { FilterBar } from "../../src/components/FilterBar";
import type { FilterTree } from "../../src/lib/contract";
import { OPS, col, pickOption, selectOptions, selectValue } from "./fixtures";

const COLS = [col("name", "text"), col("amount", "exact_numeric"), col("doc", "structural")];

function setup() {
  const applied: (FilterTree | null)[] = [];
  render(<FilterBar columns={COLS} ops={OPS} onApply={(f) => applied.push(f)} />);
  return applied;
}

const rows = () => screen.queryAllByTestId("filter-row");
const inRow = (i: number) => within(rows()[i]);

describe("FilterBar", () => {
  test("starts empty and adds a row with the first column's first operator", () => {
    setup();
    expect(rows()).toHaveLength(0);
    fireEvent.click(screen.getByText("Filter"));
    expect(rows()).toHaveLength(1);
    expect(selectValue(inRow(0).getByLabelText("Operator"))).toBe("eq");
  });

  test("applying emits the AND tree of what was typed", () => {
    const applied = setup();
    fireEvent.click(screen.getByText("Filter"));
    fireEvent.change(inRow(0).getByLabelText("Value"), { target: { value: "o'brien" } });
    fireEvent.click(screen.getByText("And"));
    pickOption(inRow(1).getByLabelText("Column"), "1");
    pickOption(inRow(1).getByLabelText("Operator"), "between");
    fireEvent.change(inRow(1).getByLabelText("Value"), { target: { value: "10" } });
    fireEvent.change(inRow(1).getByLabelText("Upper value"), { target: { value: "20.5" } });
    fireEvent.click(screen.getByText("Apply"));

    expect(applied.at(-1)).toEqual({
      node: "and",
      0: [
        { node: "pred", 0: { column: "name", op: "eq", value: { t: "text", v: "o'brien" } } },
        {
          node: "pred",
          0: { column: "amount", op: "between", value: { t: "array", v: [{ t: "text", v: "10" }, { t: "text", v: "20.5" }] } },
        },
      ],
    });
    expect(screen.getByTestId("filter-applied").textContent).toBe("2 filters applied");
  });

  test("the operator list follows the column's type class", () => {
    setup();
    fireEvent.click(screen.getByText("Filter"));
    const ops = () => selectOptions(inRow(0).getByLabelText("Operator")).map(([v]) => v);
    expect(ops()).toContain("contains");
    pickOption(inRow(0).getByLabelText("Column"), "2");
    expect(ops()).toEqual(["is_null"]);
    // The incompatible operator was replaced, not kept.
    expect(selectValue(inRow(0).getByLabelText("Operator"))).toBe("is_null");
  });

  test("a nullary operator has no value input; the case toggle shows only where declared", () => {
    setup();
    fireEvent.click(screen.getByText("Filter"));
    expect(inRow(0).queryByLabelText("Ignore case")).not.toBeNull();
    pickOption(inRow(0).getByLabelText("Operator"), "is_null");
    expect(inRow(0).queryByLabelText("Value")).toBeNull();
    expect(inRow(0).queryByLabelText("Ignore case")).toBeNull();
  });

  test("the case toggle reaches the predicate", () => {
    const applied = setup();
    fireEvent.click(screen.getByText("Filter"));
    fireEvent.change(inRow(0).getByLabelText("Value"), { target: { value: "Bob" } });
    fireEvent.click(inRow(0).getByLabelText("Ignore case"));
    fireEvent.click(screen.getByText("Apply"));
    const tree = applied.at(-1) as { 0: { 0: { case_insensitive?: boolean } }[] };
    expect(tree[0][0][0].case_insensitive).toBe(true);
  });

  test("removing the last row or clearing re-browses unfiltered", () => {
    const applied = setup();
    fireEvent.click(screen.getByText("Filter"));
    fireEvent.change(inRow(0).getByLabelText("Value"), { target: { value: "x" } });
    fireEvent.click(screen.getByText("Apply"));
    fireEvent.click(inRow(0).getByLabelText("Remove filter"));
    expect(applied.at(-1)).toBeNull();
    expect(rows()).toHaveLength(0);

    fireEvent.click(screen.getByText("Filter"));
    fireEvent.click(screen.getByText("Clear"));
    expect(applied.at(-1)).toBeNull();
  });

  test("an incomplete row is not sent", () => {
    const applied = setup();
    fireEvent.click(screen.getByText("Filter"));
    fireEvent.click(screen.getByText("Apply"));
    expect(applied.at(-1)).toBeNull();
  });

  // T-014 / D-005: an extension operator the UI has never heard of renders
  // from its declaration alone — its arity picks the inputs, its type classes
  // pick the columns — and is sent under its declared id.
  test("renders a driver extension operator from its declaration alone", () => {
    const ext = [
      ...OPS,
      { id: "near", arity: "two" as const, label: "within radius of", type_classes: ["text" as const], case_sensitive: null, case_insensitive_toggle: false },
      { id: "is_blank", arity: "none" as const, label: "is blank", type_classes: ["text" as const], case_sensitive: null, case_insensitive_toggle: false },
    ];
    const applied: (FilterTree | null)[] = [];
    render(<FilterBar columns={COLS} ops={ext} onApply={(f) => applied.push(f)} />);
    fireEvent.click(screen.getByText("Filter"));
    const op = () => inRow(0).getByLabelText("Operator");
    expect(selectOptions(op()).map(([, label]) => label)).toContain("within radius of");

    pickOption(op(), "is_blank");
    expect(inRow(0).queryByLabelText("Value")).toBeNull();

    pickOption(op(), "near");
    fireEvent.change(inRow(0).getByLabelText("Value"), { target: { value: "a" } });
    fireEvent.change(inRow(0).getByLabelText("Upper value"), { target: { value: "b" } });
    fireEvent.click(screen.getByText("Apply"));
    expect(applied.at(-1)).toEqual({
      node: "and",
      0: [{ node: "pred", 0: { column: "name", op: "near", value: { t: "array", v: [{ t: "text", v: "a" }, { t: "text", v: "b" }] } } }],
    });

    // Not offered on a column outside its declared type classes.
    pickOption(inRow(0).getByLabelText("Column"), "1");
    expect(selectOptions(op()).map(([, label]) => label)).not.toContain("within radius of");
  });
});

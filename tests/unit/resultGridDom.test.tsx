/**
 * T-070 / D-021: the result grid mounted in a DOM with fake rows — no Tauri
 * IPC. The static-markup test (resultGrid.test.tsx) cannot see body rows,
 * because the virtualizer renders a window and a server render has no scroll
 * element; here the scroll element is given a size so rows actually render.
 */
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { fireEvent, render, screen } from "@testing-library/react";

import { ResultGrid, type GridEditing } from "../../src/components/ResultGrid";
import type { ColumnDesc, WireValue } from "../../src/lib/contract";
import { EMPTY, stage, type Pending } from "../../src/lib/pendingEdits";
import { col } from "./fixtures";

// happy-dom does no layout; the virtualizer sizes its window from these.
const saved: PropertyDescriptor[] = [];
beforeAll(() => {
  for (const [prop, px] of [["offsetHeight", 600], ["offsetWidth", 1000]] as const) {
    saved.push(Object.getOwnPropertyDescriptor(HTMLElement.prototype, prop)!);
    Object.defineProperty(HTMLElement.prototype, prop, { configurable: true, get: () => px });
  }
});
afterAll(() => {
  for (const [i, prop] of (["offsetHeight", "offsetWidth"] as const).entries()) {
    Object.defineProperty(HTMLElement.prototype, prop, saved[i]);
  }
});

const COLS: ColumnDesc[] = [col("id", "integer"), col("name", "text"), col("amount", "exact_numeric")];
const R = (id: number, name: WireValue, amount: WireValue): WireValue[] => [{ t: "int", v: id }, name, amount];
const ROWS: WireValue[][] = [
  R(1, { t: "text", v: "alice" }, { t: "numeric", v: "123456789012345678901234567890.5" }),
  R(2, { t: "text", v: '<img src=x onerror="alert(1)">' }, { t: "null" }),
  R(3, { t: "missing" }, { t: "numeric", v: "-0.000000000000000001" }),
];

const bodyRows = () => document.querySelectorAll("tbody tr:not([aria-hidden])");

describe("ResultGrid in a DOM", () => {
  test("renders the fake rows as cells", () => {
    render(<ResultGrid sessionId="s" columns={COLS} rows={ROWS} truncated={false} />);
    expect(bodyRows().length).toBe(3);
    expect(screen.getByText("alice")).toBeTruthy();
    expect(screen.getByText(/3 rows · 3 rendered/)).toBeTruthy();
  });

  test("exact numerics are shown verbatim, never through Number()", () => {
    render(<ResultGrid sessionId="s" columns={COLS} rows={ROWS} truncated={false} />);
    expect(screen.getByText("123456789012345678901234567890.5")).toBeTruthy();
    expect(screen.getByText("-0.000000000000000001")).toBeTruthy();
  });

  test("NULL and missing render distinctly, and both read as absent", () => {
    render(<ResultGrid sessionId="s" columns={COLS} rows={ROWS} truncated={false} />);
    const nul = screen.getByText("NULL");
    const missing = screen.getByText("—");
    expect(nul.className).toContain("italic");
    expect(missing.className).toContain("italic");
  });

  test("hostile cell content is a text node, never an element (T-021)", () => {
    render(<ResultGrid sessionId="s" columns={COLS} rows={ROWS} truncated={false} />);
    expect(document.querySelector("tbody img")).toBeNull();
    expect(screen.getByText('<img src=x onerror="alert(1)">')).toBeTruthy();
  });

  test("a truncated result says more rows are available", () => {
    render(<ResultGrid sessionId="s" columns={COLS} rows={ROWS} truncated={true} />);
    expect(screen.getByText(/truncated — more rows available/)).toBeTruthy();
  });

  test("only a window of a large result is mounted", () => {
    const many = Array.from({ length: 5000 }, (_, i) => R(i, { t: "text", v: `r${i}` }, { t: "null" }));
    render(<ResultGrid sessionId="s" columns={COLS} rows={many} truncated={false} />);
    const mounted = bodyRows().length;
    expect(mounted).toBeGreaterThan(0);
    expect(mounted).toBeLessThan(100);
  });
});

describe("ResultGrid editing", () => {
  function editing(pending: Pending, calls: string[], staged: [number, number, WireValue][] = []): GridEditing {
    return {
      gate: { state: "editable", key: ["id"] },
      pending,
      applying: false,
      onStage: (r, c, v) => staged.push([r, c, v]),
      onUnstage: (r, c) => calls.push(`unstage ${r},${c}`),
      onApply: () => calls.push("apply"),
      onDiscard: () => calls.push("discard"),
    };
  }

  test("double-click edits a cell and Enter stages the typed value", () => {
    const staged: [number, number, WireValue][] = [];
    render(<ResultGrid sessionId="s" columns={COLS} rows={ROWS} truncated={false} editing={editing(EMPTY, [], staged)} />);
    fireEvent.doubleClick(screen.getByText("alice").closest("div[title]")!);
    const input = document.querySelector("tbody input[type=text]") as HTMLInputElement;
    expect(input.value).toBe("alice");
    fireEvent.change(input, { target: { value: "alicia" } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(staged).toEqual([[0, 1, { t: "text", v: "alicia" }]]);
  });

  test("pending changes show the count and wire Review & apply / Discard", () => {
    const calls: string[] = [];
    const pending = stage(EMPTY, 0, 1, { t: "text", v: "alicia" }, ROWS[0][1]);
    render(<ResultGrid sessionId="s" columns={COLS} rows={ROWS} truncated={false} editing={editing(pending, calls)} />);
    expect(screen.getByText("1 pending change")).toBeTruthy();
    expect(screen.getByText("alicia")).toBeTruthy(); // the staged value replaces what was read
    fireEvent.click(screen.getByText("Review & apply…"));
    fireEvent.click(screen.getByText("Discard"));
    fireEvent.click(screen.getByTitle("Revert this cell"));
    expect(calls).toEqual(["apply", "discard", "unstage 0,1"]);
  });

  test("a staged change to a non-comparable column says it is not checked", () => {
    const cols = [COLS[0], { ...col("blob", "text"), concurrency_comparable: false }];
    const rows: WireValue[][] = [[{ t: "int", v: 1 }, { t: "text", v: "x" }]];
    const pending = stage(EMPTY, 0, 1, { t: "text", v: "y" }, rows[0][1]);
    render(<ResultGrid sessionId="s" columns={cols} rows={rows} truncated={false} editing={editing(pending, [])} />);
    expect(screen.getByText(/not checked for concurrent changes.*blob/)).toBeTruthy();
  });

  test("a read-only gate leaves cells uneditable", () => {
    render(
      <ResultGrid
        sessionId="s"
        columns={COLS}
        rows={ROWS}
        truncated={false}
        editing={{ ...editing(EMPTY, []), gate: { state: "read_only", reason: "no unique key" } }}
      />,
    );
    fireEvent.doubleClick(screen.getByText("alice"));
    expect(document.querySelector("tbody input")).toBeNull();
  });
});

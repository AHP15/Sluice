/**
 * T-063 / D-018: the grid's column layout over a schemaless relation — pin,
 * hide, reorder, manual add, the per-collection cache and explicit refresh.
 * Inference itself is faked: it is the core's (schema_infer.rs, tested there).
 */
import { afterAll, beforeAll, beforeEach, describe, expect, test } from "bun:test";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";

import { ResultGrid, type GridColumnLayout } from "../../src/components/ResultGrid";
import { layoutCache } from "../../src/lib/columnLayout";
import type { ColumnDesc, ColumnInference, WireValue } from "../../src/lib/contract";
import { col } from "./fixtures";

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
beforeEach(() => layoutCache.clear());

const derived = (c: ColumnDesc): ColumnDesc => ({ ...c, derived: true });
const COLS = [derived(col("_id", "integer")), derived(col("name", "text")), derived(col("addr", "structural"))];
const ROWS: WireValue[][] = [
  [{ t: "int", v: 1 }, { t: "text", v: "alice" }, { t: "document", v: [["city", { t: "text", v: "Oran" }]] }],
  [{ t: "int", v: 2 }, { t: "int", v: 42 }, { t: "missing" }],
];
const INFERENCE: ColumnInference = {
  columns: [
    { path: ["_id"], source: "sample", present: 2, nulls: 0, observed: ["int"], declared: [], type_varies: false },
    { path: ["name"], source: "sample", present: 2, nulls: 0, observed: ["text", "int"], declared: [], type_varies: true },
    { path: ["addr"], source: "sample", present: 1, nulls: 0, observed: ["document"], declared: [], type_varies: false },
  ],
  sampled: 2,
  sample_limit: 1000,
  sample_truncated: false,
  from_validator: false,
};

function fakeInfer(result = INFERENCE) {
  const calls = { n: 0 };
  const layout: GridColumnLayout = {
    key: "s::[\"db\",\"people\"]",
    infer: async () => {
      calls.n++;
      return result;
    },
  };
  return { calls, layout };
}

const headers = () => [...document.querySelectorAll("thead th")].map((th) => th.querySelector("span.truncate")?.textContent);

describe("ResultGrid column layout", () => {
  test("infers once per collection, marks type-varying paths and shows frequency", async () => {
    const { calls, layout } = fakeInfer();
    render(<ResultGrid sessionId="s" columns={COLS} rows={ROWS} truncated={false} layout={layout} />);
    await screen.findByText(/columns inferred from 2 documents/);
    expect(calls.n).toBe(1);
    expect(screen.getByText("· mixed")).toBeTruthy();
    expect(screen.getAllByText(/^· 50%$/).length).toBe(1);
    // The int in the "text" column is shown as the int it is — never coerced.
    expect(screen.getByText("42")).toBeTruthy();
  });

  test("the cache serves a remount of the same collection; refresh is explicit", async () => {
    const { calls, layout } = fakeInfer();
    const first = render(<ResultGrid sessionId="s" columns={COLS} rows={ROWS} truncated={false} layout={layout} />);
    await screen.findByText(/columns inferred from 2 documents/);
    first.unmount();
    render(<ResultGrid sessionId="s" columns={COLS} rows={ROWS} truncated={false} layout={layout} />);
    expect(screen.getByText(/columns inferred from 2 documents/)).toBeTruthy();
    expect(calls.n).toBe(1);
    fireEvent.click(screen.getByText("Refresh columns"));
    await waitFor(() => expect(calls.n).toBe(2));
  });

  test("pin, hide and show, reorder", async () => {
    const { layout } = fakeInfer();
    render(<ResultGrid sessionId="s" columns={COLS} rows={ROWS} truncated={false} layout={layout} />);
    await screen.findByText(/columns inferred/);
    expect(headers()).toEqual(["_id", "name", "addr"]);

    fireEvent.click(screen.getByTitle("Pin addr"));
    expect(headers()).toEqual(["addr", "_id", "name"]);
    fireEvent.click(screen.getByTitle("Unpin addr"));

    fireEvent.click(screen.getByTitle("Move _id right"));
    expect(headers()).toEqual(["name", "_id", "addr"]);

    fireEvent.click(screen.getByTitle("Hide name"));
    expect(headers()).toEqual(["_id", "addr"]);
    // Hidden is never silent: it is offered back.
    fireEvent.click(screen.getByTitle("Show name"));
    expect(headers()).toEqual(["name", "_id", "addr"]);
  });

  test("a path added by hand reads through nested documents, read-only", async () => {
    const { layout } = fakeInfer();
    render(<ResultGrid sessionId="s" columns={COLS} rows={ROWS} truncated={false} layout={layout} />);
    await screen.findByText(/columns inferred/);
    fireEvent.change(screen.getByLabelText("Add a column by path"), { target: { value: "addr.city" } });
    fireEvent.click(screen.getByTitle("Add column"));
    expect(headers()).toEqual(["_id", "name", "addr", "addr.city"]);
    expect(screen.getByText("Oran")).toBeTruthy();
    fireEvent.click(screen.getByTitle("Remove addr.city"));
    expect(headers()).toEqual(["_id", "name", "addr"]);
  });

  test("a page column the inference does not name is still shown", async () => {
    const { layout } = fakeInfer({ ...INFERENCE, columns: INFERENCE.columns.slice(0, 1) });
    render(<ResultGrid sessionId="s" columns={COLS} rows={ROWS} truncated={false} layout={layout} />);
    await screen.findByText(/columns inferred/);
    expect(headers()).toEqual(["_id", "name", "addr"]);
  });

  test("no layout prop: the grid is unchanged", () => {
    render(<ResultGrid sessionId="s" columns={COLS} rows={ROWS} truncated={false} />);
    expect(screen.queryByText("Refresh columns")).toBeNull();
    expect(screen.queryByTitle("Pin addr")).toBeNull();
  });
});

/** T-063 / D-018: the column layout over derived columns — pure functions. */
import { describe, expect, test } from "bun:test";

import {
  EMPTY_LAYOUT,
  addPath,
  frequencyLabel,
  hide,
  layoutCache,
  layoutDesc,
  move,
  parsePath,
  pathKey,
  pathValue,
  removeAdded,
  resolveColumns,
  rowsAsDocuments,
  show,
  togglePin,
} from "../../src/lib/columnLayout";
import type { ColumnInference, InferredColumn, WireValue } from "../../src/lib/contract";
import { col } from "./fixtures";

const inf = (path: string[], present: number, varies = false): InferredColumn => ({
  path,
  source: "sample",
  present,
  nulls: 0,
  observed: varies ? ["text", "int"] : ["text"],
  declared: [],
  type_varies: varies,
});
const INFERENCE: ColumnInference = {
  columns: [inf(["_id"], 4), inf(["name"], 3, true), inf(["tags"], 2)],
  sampled: 4,
  sample_limit: 1000,
  sample_truncated: false,
  from_validator: false,
};
const PAGE = [col("_id", "integer"), col("name", "text"), col("tags", "structural"), col("addr", "structural")];
const labels = (cs: { label: string }[]) => cs.map((c) => c.label);
const k = (...p: string[]) => pathKey(p);

describe("resolveColumns", () => {
  test("inference order first, then page columns inference missed — nothing hidden by inference", () => {
    const { visible, hidden } = resolveColumns(INFERENCE, EMPTY_LAYOUT, PAGE);
    expect(labels(visible)).toEqual(["_id", "name", "tags", "addr"]);
    expect(hidden).toEqual([]);
    expect(visible[1].inferred?.type_varies).toBe(true);
    expect(visible[3].inferred).toBeUndefined();
    expect(visible.map((c) => c.pageIndex)).toEqual([0, 1, 2, 3]);
  });

  test("with no inference yet, the page columns are still all shown", () => {
    expect(labels(resolveColumns(null, EMPTY_LAYOUT, PAGE).visible)).toEqual(["_id", "name", "tags", "addr"]);
  });

  test("pin moves a column to the front; unpin restores it", () => {
    const pinned = togglePin(EMPTY_LAYOUT, k("tags"));
    const { visible } = resolveColumns(INFERENCE, pinned, PAGE);
    expect(labels(visible)).toEqual(["tags", "_id", "name", "addr"]);
    expect(visible[0].pinned).toBe(true);
    expect(labels(resolveColumns(INFERENCE, togglePin(pinned, k("tags")), PAGE).visible)[0]).toBe("_id");
  });

  test("hide is listed back and show restores it", () => {
    const hid = hide(EMPTY_LAYOUT, k("name"));
    const r = resolveColumns(INFERENCE, hid, PAGE);
    expect(labels(r.visible)).toEqual(["_id", "tags", "addr"]);
    expect(labels(r.hidden)).toEqual(["name"]);
    expect(labels(resolveColumns(INFERENCE, show(hid, k("name")), PAGE).visible)).toContain("name");
  });

  test("reorder moves one place and survives a refreshed inference with new paths", () => {
    const current = resolveColumns(INFERENCE, EMPTY_LAYOUT, PAGE).visible.map((c) => c.key);
    const moved = move(EMPTY_LAYOUT, current, k("addr"), -1);
    expect(labels(resolveColumns(INFERENCE, moved, PAGE).visible)).toEqual(["_id", "name", "addr", "tags"]);
    // Out-of-range moves are no-ops.
    expect(move(EMPTY_LAYOUT, current, k("_id"), -1)).toBe(EMPTY_LAYOUT);

    const refreshed: ColumnInference = { ...INFERENCE, columns: [inf(["fresh"], 4), ...INFERENCE.columns] };
    const page = [...PAGE, col("fresh", "text")];
    expect(labels(resolveColumns(refreshed, moved, page).visible)).toEqual(["_id", "name", "addr", "tags", "fresh"]);
  });

  test("manual add appends a path, re-adding is idempotent, remove drops it", () => {
    const added = addPath(addPath(EMPTY_LAYOUT, ["addr", "city"]), ["addr", "city"]);
    expect(added.added).toEqual([["addr", "city"]]);
    const { visible } = resolveColumns(INFERENCE, added, PAGE);
    const city = visible[visible.length - 1];
    expect(city.label).toBe("addr.city");
    expect(city.added).toBe(true);
    expect(city.pageIndex).toBeNull();
    expect(labels(resolveColumns(INFERENCE, removeAdded(added, city.key), PAGE).visible)).not.toContain("addr.city");
  });

  test("adding a hidden path shows it rather than duplicating it", () => {
    const l = addPath(hide(EMPTY_LAYOUT, k("name")), ["name"]);
    expect(l.hidden).toEqual([]);
    expect(labels(resolveColumns(INFERENCE, l, PAGE).visible).filter((n) => n === "name")).toHaveLength(1);
  });
});

describe("pathValue — presentation-only", () => {
  const doc: WireValue = { t: "document", v: [["city", { t: "text", v: "Oran" }]] };
  const tags: WireValue = { t: "array", v: [{ t: "document", v: [["x", { t: "int", v: 1 }]] }] };
  const amount: WireValue = { t: "numeric", v: "123456789012345678901234567890.5" };
  const row: WireValue[] = [amount, { t: "int", v: 7 }, tags, doc];

  test("returns the row's own value object, unconverted, whatever the inferred type", () => {
    expect(pathValue(PAGE, row, ["_id"])).toBe(amount);
    // `name` was inferred as text-ish; an int is shown as the int it is.
    expect(pathValue(PAGE, row, ["name"])).toEqual({ t: "int", v: 7 });
  });

  test("descends documents for nested paths, never arrays", () => {
    expect(pathValue(PAGE, row, ["addr", "city"])).toEqual({ t: "text", v: "Oran" });
    expect(pathValue(PAGE, row, ["tags"])).toBe(tags);
    expect(pathValue(PAGE, row, ["tags", "x"])).toEqual({ t: "missing" });
  });

  test("a path the row does not reach is absent, not null", () => {
    expect(pathValue(PAGE, row, ["addr", "zip"])).toEqual({ t: "missing" });
    expect(pathValue(PAGE, row, ["nope"])).toEqual({ t: "missing" });
  });
});

describe("helpers", () => {
  test("parsePath splits on dots and refuses empty segments", () => {
    expect(parsePath(" addr.city ")).toEqual(["addr", "city"]);
    expect(parsePath("")).toBeNull();
    expect(parsePath("a..b")).toBeNull();
  });

  test("frequency labels never claim 0% for a path that was seen", () => {
    expect(frequencyLabel(inf(["a"], 3), 4)).toBe("75%");
    expect(frequencyLabel(inf(["a"], 1), 1000)).toBe("<1%");
    expect(frequencyLabel(inf(["a"], 0), 4)).toBe("0%");
    expect(frequencyLabel(inf(["a"], 1), 0)).toBeNull();
  });

  test("rowsAsDocuments leaves absent fields out rather than sending null", () => {
    const docs = rowsAsDocuments(PAGE.slice(0, 2), [[{ t: "int", v: 1 }, { t: "missing" }]]);
    expect(docs).toEqual([[["_id", { t: "int", v: 1 }]]]);
  });

  test("layoutDesc marks a page column whose type varies, and synthesises a derived one otherwise", () => {
    const { visible } = resolveColumns(INFERENCE, addPath(EMPTY_LAYOUT, ["addr", "city"]), PAGE);
    expect(layoutDesc(visible[1], PAGE).mixed).toBe(true);
    const city = layoutDesc(visible[visible.length - 1], PAGE);
    expect(city).toMatchObject({ name: "addr.city", derived: true, concurrency_comparable: false });
  });

  test("the cache is per key", () => {
    layoutCache.clear();
    layoutCache.set("a", { inference: INFERENCE, layout: EMPTY_LAYOUT });
    expect(layoutCache.get("a")?.inference).toBe(INFERENCE);
    expect(layoutCache.get("b")).toBeUndefined();
    layoutCache.clear();
  });
});

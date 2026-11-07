/**
 * T-002 / D-023: the result grid is driven by TanStack Table.
 *
 * A server render cannot exercise the virtualizer's window (it has no scroll
 * element, so it renders zero body rows) — what it can prove is that the
 * header, sizing and resize affordances come from TanStack Table's model, and
 * that database text still reaches the markup as escaped text (T-021).
 */
import { describe, expect, test } from "bun:test";
import { renderToStaticMarkup } from "react-dom/server";

import { ResultGrid } from "../../src/components/ResultGrid";
import type { ColumnDesc, WireValue } from "../../src/lib/contract";
import { EMPTY, stage, type Pending } from "../../src/lib/pendingEdits";

const col = (name: string, type_name: string, extra: Partial<ColumnDesc> = {}): ColumnDesc => ({
  name,
  type_name,
  type_class: "text",
  nullable: true,
  mixed: false,
  derived: false,
  exact: null,
  concurrency_comparable: true,
  ...extra,
});

const columns = [
  col("id", "int4", { type_class: "integer" }),
  col("<script>alert(1)</script>", "text"),
  col("id", "int8", { mixed: true }),
];
const rows: WireValue[][] = [[{ t: "int", v: 1 }, { t: "text", v: "x" }, { t: "numeric", v: "9223372036854775807" }]];

const html = renderToStaticMarkup(
  <ResultGrid sessionId="s" columns={columns} rows={rows} truncated={false} />,
);

describe("ResultGrid on TanStack Table", () => {
  test("renders one header per descriptor, including duplicate column names", () => {
    expect(html.match(/<th /g)?.length).toBe(3);
    expect(html).toContain("int4");
    expect(html).toContain("int8");
    expect(html).toContain("mixed");
  });

  test("every column gets a TanStack resize handle", () => {
    expect(html.match(/aria-label="Resize column"/g)?.length).toBe(3);
  });

  test("sizes come from the table model: 3 columns × 180px", () => {
    expect(html).toContain("width:540px");
    expect(html.match(/width:180px/g)?.length).toBe(3);
  });

  test("a hostile column name is escaped text, never markup (T-021)", () => {
    expect(html).not.toContain("<script>");
    expect(html).toContain("&lt;script&gt;");
  });

  test("the footer counts the rows handed to the model", () => {
    expect(html).toContain("1 rows");
  });
});

describe("T-056: staged editing in the footer", () => {
  const editing = (gate: import("../../src/lib/contract").EditGate, pending: Pending) => ({
    gate,
    pending,
    applying: false,
    onStage: () => {},
    onUnstage: () => {},
    onApply: () => {},
    onDiscard: () => {},
    newRows: [],
    onAddRow: () => {},
    onStageNew: () => {},
    onUnstageNew: () => {},
    onRemoveNew: () => {},
  });
  const render = (e: ReturnType<typeof editing>) =>
    renderToStaticMarkup(<ResultGrid sessionId="s" columns={columns} rows={rows} truncated={false} editing={e} />);

  test("a read-only relation says why", () => {
    const out = render(editing({ state: "read_only", reason: "This table has no primary key" }, EMPTY));
    expect(out).toContain("read-only: This table has no primary key");
    expect(out).not.toContain("Review &amp; apply");
  });

  test("pending changes offer apply and discard, and no post-apply undo", () => {
    const p = stage(EMPTY, 0, 1, { t: "text", v: "y" }, rows[0][1]);
    const out = render(editing({ state: "editable", key: ["id"] }, p));
    expect(out).toContain("1 pending change");
    expect(out).toContain("Review &amp; apply");
    expect(out).toContain("Discard");
    expect(out).toContain("no undo after");
    expect(out).not.toMatch(/>Undo</);
  });

  test("nothing staged shows no apply bar", () => {
    expect(render(editing({ state: "editable", key: ["id"] }, EMPTY))).not.toContain("Review &amp; apply");
  });
});

// T-060 / R-035: this used to be a footer banner while edits were pending; it
// now appears per edit in the preview (editPreviewRendered.test.tsx), so it
// stays information rather than furniture.
test("T-060: the footer carries no standing protection banner", () => {
  const cols = [col("id", "int8"), col("doc", "json", { type_class: "structural", concurrency_comparable: false })];
  const r: WireValue[][] = [[{ t: "numeric", v: "1" }, { t: "text", v: "{}" }]];
  const out = renderToStaticMarkup(
    <ResultGrid
      sessionId="s"
      columns={cols}
      rows={r}
      truncated={false}
      editing={{
        gate: { state: "editable", key: ["id"] },
        pending: stage(EMPTY, 0, 1, { t: "text", v: "[]" }, r[0][1]),
        applying: false,
        onStage: () => {},
        onUnstage: () => {},
        onApply: () => {},
        onDiscard: () => {},
        newRows: [],
        onAddRow: () => {},
        onStageNew: () => {},
        onUnstageNew: () => {},
        onRemoveNew: () => {},
      }}
    />,
  );
  expect(out).toContain("1 pending change");
  expect(out).not.toContain("protected by unique key only");
});

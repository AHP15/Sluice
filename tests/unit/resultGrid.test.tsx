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

const col = (name: string, type_name: string, extra: Partial<ColumnDesc> = {}): ColumnDesc => ({
  name,
  type_name,
  type_class: "text",
  nullable: true,
  mixed: false,
  derived: false,
  exact: null,
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

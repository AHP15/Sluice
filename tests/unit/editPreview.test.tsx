/**
 * T-057 / R-003: the preview renders the core's plan and nothing else.
 *
 * Run with `bun test tests/unit`.
 */
import { describe, expect, test } from "bun:test";
import { renderToStaticMarkup } from "react-dom/server";

import { EditPreview } from "../../src/components/EditPreview";
import type { EditPlan } from "../../src/lib/contract";

const plan: EditPlan = {
  digest: "abc",
  statements: [
    {
      statement: 'UPDATE "t" SET "name" = ($1)::text::text WHERE "id" = ($2)::text::int8',
      params: [
        { placeholder: "$1", type_name: "text", value: "o'brien" },
        { placeholder: "$2", type_name: "int8", value: "1" },
      ],
      inline: `UPDATE "t" SET "name" = ('o''brien')::text::text WHERE "id" = ('1')::text::int8`,
      concurrency_omitted: [],
    },
    {
      statement: 'UPDATE "t" SET "bio" = ($1)::text::text, "x" = ($2)::text::text WHERE "id" = ($3)::text::int8',
      params: [
        { placeholder: "$1", type_name: "text", value: "<b>a\\b</b>" },
        { placeholder: "$2", type_name: "text", value: null },
        { placeholder: "$3", type_name: "int8", value: "2" },
      ],
      inline: null,
      concurrency_omitted: ["doc"],
    },
  ],
};

const html = renderToStaticMarkup(<EditPreview plan={plan} applying={false} onConfirm={() => {}} onBack={() => {}} />);

describe("EditPreview", () => {
  test("an inlinable statement is shown as the core's inline rendering", () => {
    expect(html).toContain("(&#x27;o&#x27;&#x27;brien&#x27;)::text::text");
  });

  test("otherwise the parameterized statement and the exact bound values", () => {
    expect(html).toContain("($1)::text::text, &quot;x&quot; = ($2)::text::text");
    expect(html).toContain("a value cannot be inlined faithfully");
    expect(html).toMatch(/italic[^>]*>NULL</);
    // Database content is text, never markup (T-021).
    expect(html).toContain("&lt;b&gt;a\\b&lt;/b&gt;");
    expect(html).not.toContain("<b>");
  });

  test("names the unprotected columns and the statement count", () => {
    expect(html).toContain("protected by unique key only): doc");
    expect(html).toContain("Run 2 statements");
  });
});

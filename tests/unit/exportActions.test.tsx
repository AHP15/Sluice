/** T-074 / D-024: export offered on the truncation notice; rows never come back. */
import { describe, expect, test } from "bun:test";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";

import { ResultGrid } from "../../src/components/ResultGrid";
import type { ExportFormat } from "../../src/lib/ipc";
import { col } from "./fixtures";

const COLS = [col("id", "integer")];
const ROWS = [[{ t: "int", v: 1 }]] as never;

describe("export on the truncation notice", () => {
  test("is offered only when the result is truncated", () => {
    const run = async () => ({ path: "/x", rows: 0 });
    const { rerender } = render(
      <ResultGrid sessionId="s" columns={COLS} rows={ROWS} truncated={false} exporting={{ available: true, run }} />,
    );
    expect(screen.queryByText("CSV")).toBeNull();
    rerender(<ResultGrid sessionId="s" columns={COLS} rows={ROWS} truncated exporting={{ available: true, run }} />);
    expect(screen.getByText("CSV")).toBeTruthy();
  });

  test("runs the chosen format and reports the file, not the rows", async () => {
    const asked: ExportFormat[] = [];
    const run = async (f: ExportFormat) => {
      asked.push(f);
      return { path: "/dl/orders-1.jsonl", rows: 120000 };
    };
    render(<ResultGrid sessionId="s" columns={COLS} rows={ROWS} truncated exporting={{ available: true, run }} />);
    fireEvent.click(screen.getByText("JSONL"));
    await waitFor(() => expect(screen.getByText("120000 rows → /dl/orders-1.jsonl")).toBeTruthy());
    expect(asked).toEqual(["jsonl"]);
  });

  test("says why when the result cannot be re-read", () => {
    render(
      <ResultGrid
        sessionId="s"
        columns={COLS}
        rows={ROWS}
        truncated
        exporting={{ available: false, reason: "run the statement on its own to export it" }}
      />,
    );
    expect(screen.getByText(/run the statement on its own/)).toBeTruthy();
    expect(screen.queryByText("CSV")).toBeNull();
  });
});

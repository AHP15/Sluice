/** T-061 / D-027: the production confirmation prompt (the core re-checks). */
import { describe, expect, test } from "bun:test";
import { fireEvent, render, screen } from "@testing-library/react";
import { ConfirmPanel } from "../../src/components/ConfirmPanel";

const request = {
  phrase: "shop",
  statements: [{ statement: "DELETE FROM orders", reason: "DELETE without WHERE affects every row" }],
};

describe("ConfirmPanel", () => {
  test("lists what was held back and runs only on the exact phrase", () => {
    const confirmed: string[] = [];
    render(<ConfirmPanel request={request} onConfirm={(p) => confirmed.push(p)} onCancel={() => {}} />);
    expect(screen.getByText("DELETE FROM orders")).toBeTruthy();
    const run = screen.getByText("Run on production") as HTMLButtonElement;
    const input = screen.getByLabelText("Confirmation phrase");

    fireEvent.change(input, { target: { value: "Shop" } });
    expect(run.disabled).toBe(true);
    fireEvent.change(input, { target: { value: "shop" } });
    expect(run.disabled).toBe(false);
    fireEvent.click(run);
    expect(confirmed).toEqual(["shop"]);
  });

  test("Escape cancels", () => {
    let cancelled = false;
    render(<ConfirmPanel request={request} onConfirm={() => {}} onCancel={() => (cancelled = true)} />);
    fireEvent.keyDown(screen.getByLabelText("Confirmation phrase"), { key: "Escape" });
    expect(cancelled).toBe(true);
  });
});

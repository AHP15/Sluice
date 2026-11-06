/**
 * T-060 / R-035: reduced protection is surfaced per edit in the preview, and
 * the rendered-form comparison is an explicit opt-in there.
 */
import { describe, expect, test } from "bun:test";
import { fireEvent, render, screen } from "@testing-library/react";
import { EditPreview } from "../../src/components/EditPreview";
import type { EditPlan } from "../../src/lib/contract";

const st = (omitted: string[], rendered: string[] = []) => ({
  statement: "UPDATE t SET doc = $1 WHERE id = $2",
  params: [],
  inline: "UPDATE t SET doc = '{}' WHERE id = 1",
  concurrency_omitted: omitted,
  concurrency_rendered: rendered,
});
const plan = (statements: EditPlan["statements"]): EditPlan => ({ statements, digest: "d" });

describe("EditPreview protection notices", () => {
  test("names unprotected columns on the statement they affect, and offers the opt-in", () => {
    const toggled: boolean[] = [];
    render(
      <EditPreview
        plan={plan([st([]), st(["doc"])])}
        applying={false}
        onConfirm={() => {}}
        onBack={() => {}}
        onCompareRendered={(on) => toggled.push(on)}
      />,
    );
    expect(screen.getAllByText(/not checked for concurrent changes/)).toHaveLength(1);
    fireEvent.click(screen.getByLabelText(/compare unchecked columns by displayed text/));
    expect(toggled).toEqual([true]);
  });

  test("says which columns are checked by displayed text once opted in", () => {
    render(
      <EditPreview
        plan={plan([st([], ["doc"])])}
        applying={false}
        onConfirm={() => {}}
        onBack={() => {}}
        compareRendered
        onCompareRendered={() => {}}
      />,
    );
    expect(screen.getByText(/checked by displayed text.*doc/)).toBeTruthy();
    expect((screen.getByLabelText(/compare unchecked columns/) as HTMLInputElement).checked).toBe(true);
  });

  test("no opt-in is offered when every edit is fully protected", () => {
    render(<EditPreview plan={plan([st([])])} applying={false} onConfirm={() => {}} onBack={() => {}} onCompareRendered={() => {}} />);
    expect(screen.queryByLabelText(/compare unchecked columns/)).toBeNull();
  });
});

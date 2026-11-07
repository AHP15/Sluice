/** T-095 / D-032: status is hue + icon + label, never colour alone. */
import { describe, expect, test } from "bun:test";
import { render } from "@testing-library/react";
import { StatusBanner, StatusPill } from "../../src/components/ui/status";

describe("D-032 status primitives", () => {
  test("each reserved tone carries its own hue, an icon and the label", () => {
    for (const [tone, cls] of [
      ["danger", "text-destructive"],
      ["caution", "text-caution"],
      ["verified", "text-verified"],
      ["info", "text-info"],
    ] as const) {
      const { container, unmount } = render(<StatusPill tone={tone}>Label</StatusPill>);
      const pill = container.firstElementChild!;
      expect(pill.className).toContain(cls);
      expect(pill.querySelector("svg")).toBeTruthy();
      expect(pill.textContent).toBe("Label");
      unmount();
    }
  });

  test("neutral uses no status hue", () => {
    const { container } = render(<StatusPill tone="neutral">history on</StatusPill>);
    const cls = container.firstElementChild!.className;
    for (const hue of ["destructive", "caution", "verified", "info", "primary"]) expect(cls).not.toContain(hue);
  });

  test("a danger banner is announced as an alert, a caution as status", () => {
    const d = render(<StatusBanner tone="danger">boom</StatusBanner>);
    expect(d.getByRole("alert").textContent).toContain("boom");
    d.unmount();
    const c = render(<StatusBanner tone="caution">careful</StatusBanner>);
    expect(c.getByRole("status").textContent).toContain("careful");
    c.unmount();
  });
});

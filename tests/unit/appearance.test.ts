import { describe, expect, test } from "bun:test";
import { applyAppearance, resolveTheme } from "../../src/store/ui";

function fakeRoot() {
  const set = new Set<string>();
  return { set, classList: { toggle: (c: string, on: boolean) => (on ? set.add(c) : set.delete(c)) } };
}

describe("T-092 / D-031: theme and density", () => {
  test("dark and light are explicit; system follows the OS", () => {
    expect(resolveTheme("dark", true)).toBe("dark");
    expect(resolveTheme("light", false)).toBe("light");
    expect(resolveTheme("system", true)).toBe("light");
    expect(resolveTheme("system", false)).toBe("dark");
  });

  test("the palette and density land as classes on the root, and switch cleanly", () => {
    const r = fakeRoot();
    applyAppearance(r, "light", "comfortable");
    expect([...r.set].sort()).toEqual(["comfortable", "light"]);
    applyAppearance(r, "dark", "compact");
    expect([...r.set]).toEqual(["dark"]);
  });
});

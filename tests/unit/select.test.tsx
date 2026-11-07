/**
 * The themed Select: a native select's keyboard model on a control the theme
 * can reach.
 */
import { describe, expect, mock, test } from "bun:test";
import { fireEvent, render, screen } from "@testing-library/react";

import { Select } from "../../src/components/ui/select";
import { selectOptions, selectValue } from "./fixtures";

const OPTIONS = [
  { value: "verify", label: "Verify certificates" },
  { value: "accept", label: "Accept invalid certificates" },
  { value: "off", label: "Disabled" },
];

function setup(value = "verify", extra: { onDismiss?: () => void } = {}) {
  const onChange = mock((_: string) => {});
  const r = render(<Select aria-label="TLS" value={value} options={OPTIONS} onChange={onChange} {...extra} />);
  return { onChange, trigger: screen.getByRole("combobox", { name: "TLS" }), ...r };
}

describe("Select", () => {
  test("shows the current option and lists every option", () => {
    const { trigger, unmount } = setup("accept");
    expect(trigger.textContent).toBe("Accept invalid certificates");
    expect(selectValue(trigger)).toBe("accept");
    expect(selectOptions(trigger).map(([v]) => v)).toEqual(["verify", "accept", "off"]);
    unmount();
  });

  test("clicking an option chooses it and closes the list", () => {
    const { trigger, onChange, unmount } = setup();
    fireEvent.click(trigger);
    expect(trigger.getAttribute("aria-expanded")).toBe("true");
    fireEvent.click(screen.getByRole("option", { name: "Disabled" }));
    expect(onChange).toHaveBeenCalledWith("off");
    expect(screen.queryByRole("listbox")).toBeNull();
    unmount();
  });

  test("arrows move the active option and Enter chooses it", () => {
    const { trigger, onChange, unmount } = setup();
    fireEvent.keyDown(trigger, { key: "ArrowDown" }); // opens on the selected option
    fireEvent.keyDown(trigger, { key: "ArrowDown" });
    fireEvent.keyDown(trigger, { key: "End" });
    fireEvent.keyDown(trigger, { key: "ArrowUp" });
    fireEvent.keyDown(trigger, { key: "Enter" });
    expect(onChange).toHaveBeenCalledWith("accept");
    unmount();
  });

  test("typing a letter while closed selects the matching option, like a native select", () => {
    const { trigger, onChange, unmount } = setup();
    fireEvent.keyDown(trigger, { key: "d" });
    expect(onChange).toHaveBeenCalledWith("off");
    unmount();
  });

  test("Escape and a click outside close without a choice and report the dismissal", () => {
    const onDismiss = mock(() => {});
    const { trigger, onChange, unmount } = setup("verify", { onDismiss });
    fireEvent.click(trigger);
    fireEvent.keyDown(trigger, { key: "Escape" });
    expect(screen.queryByRole("listbox")).toBeNull();
    fireEvent.click(trigger);
    fireEvent.mouseDown(document.body);
    expect(screen.queryByRole("listbox")).toBeNull();
    expect(onDismiss).toHaveBeenCalledTimes(2);
    expect(onChange).not.toHaveBeenCalled();
    unmount();
  });
});

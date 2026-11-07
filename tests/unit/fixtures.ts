/** T-070: fake column descriptors and operator declarations for component tests. */
import { fireEvent } from "@testing-library/react";
import type { ColumnDesc, FilterOpDecl } from "../../src/lib/contract";

export const col = (name: string, type_class: ColumnDesc["type_class"]): ColumnDesc => ({
  name,
  type_name: type_class,
  type_class,
  nullable: true,
  mixed: false,
  derived: false,
  exact: null,
  concurrency_comparable: true,
});

const all: ColumnDesc["type_class"][] = ["text", "integer", "exact_numeric", "timestamp"];
export const OPS: FilterOpDecl[] = [
  { id: "eq", arity: "one", label: "equals", type_classes: all, case_sensitive: true, case_insensitive_toggle: true },
  { id: "gt", arity: "one", label: "greater than", type_classes: all, case_sensitive: null, case_insensitive_toggle: false },
  { id: "in", arity: "list", label: "in", type_classes: all, case_sensitive: true, case_insensitive_toggle: true },
  { id: "between", arity: "two", label: "between", type_classes: all, case_sensitive: null, case_insensitive_toggle: false },
  { id: "is_null", arity: "none", label: "is null", type_classes: [...all, "structural"], case_sensitive: null, case_insensitive_toggle: false },
  { id: "contains", arity: "one", label: "contains", type_classes: ["text"], case_sensitive: true, case_insensitive_toggle: true },
];

// ---- The themed <Select> (src/components/ui/select.tsx) --------------------

/** The value a themed Select currently holds. */
export const selectValue = (trigger: HTMLElement): string | null => trigger.getAttribute("data-value");

/** Open a themed Select and return its options as [value, label] pairs, then close it. */
export function selectOptions(trigger: HTMLElement): [string, string][] {
  fireEvent.click(trigger);
  const list = document.getElementById(trigger.getAttribute("aria-controls")!)!;
  const out = [...list.querySelectorAll<HTMLElement>("[role=option]")].map(
    (o) => [o.dataset.value!, o.textContent ?? ""] as [string, string],
  );
  fireEvent.keyDown(trigger, { key: "Escape" });
  return out;
}

/** Choose `value` in a themed Select the way a user does: open it, click the option. */
export function pickOption(trigger: HTMLElement, value: string) {
  fireEvent.click(trigger);
  const list = document.getElementById(trigger.getAttribute("aria-controls")!)!;
  const option = list.querySelector<HTMLElement>(`[role=option][data-value="${CSS.escape(value)}"]`);
  if (!option) throw new Error(`no option ${value}`);
  fireEvent.click(option);
}

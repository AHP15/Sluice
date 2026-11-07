/** T-070: fake column descriptors and operator declarations for component tests. */
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

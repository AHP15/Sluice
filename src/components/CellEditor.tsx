/**
 * The in-grid cell editor (T-056 / D-027).
 *
 * It only *produces* a value; staging it is the caller's job. Exact numerics
 * delegate to `ExactNumericEditor` (T-019 / R-005), chosen by `column.exact`,
 * never by type class. Booleans get a fixed choice. Everything else is a plain
 * text control validated by `parseCellInput`, and NULL is offered only on
 * nullable columns.
 */
import { useState } from "react";

import type { ColumnDesc, WireValue } from "../lib/contract";
import { editorText, parseCellInput } from "../lib/pendingEdits";
import { ExactNumericEditor } from "./ExactNumericEditor";
import { Select } from "./ui/select";

export function CellEditor({
  column,
  value,
  onCommit,
  onCancel,
}: {
  column: ColumnDesc;
  value: WireValue;
  onCommit: (v: WireValue) => void;
  onCancel: () => void;
}) {
  const [text, setText] = useState(editorText(value));
  const [error, setError] = useState<string | null>(null);

  const nullButton = column.nullable && (
    <button
      type="button"
      title="Set NULL"
      className="focus-glow shrink-0 rounded border border-transparent px-1 text-2xs text-muted-foreground hover:bg-accent"
      // Fires before the input's blur would commit the typed text.
      onMouseDown={(e) => {
        e.preventDefault();
        onCommit({ t: "null" });
      }}
    >
      NULL
    </button>
  );

  if (column.exact) {
    return (
      <div className="flex items-start gap-1">
        <div className="min-w-0 flex-1">
          <ExactNumericEditor
            initial={value.t === "numeric" ? value.v : editorText(value)}
            domain={column.exact}
            onCommit={onCommit}
            onCancel={onCancel}
          />
        </div>
        {nullButton}
      </div>
    );
  }

  if (column.type_class === "boolean") {
    return (
      <Select
        autoFocus
        defaultOpen
        size="sm"
        className="w-full"
        aria-label={column.name}
        value={value.t === "bool" ? String(value.v) : "null"}
        options={[
          { value: "true", label: "true" },
          { value: "false", label: "false" },
          ...(column.nullable ? [{ value: "null", label: "NULL" }] : []),
        ]}
        onChange={(v) => onCommit(v === "null" ? { t: "null" } : { t: "bool", v: v === "true" })}
        onDismiss={onCancel}
      />
    );
  }

  const commit = () => {
    const r = parseCellInput(column, text);
    if (r.ok) onCommit(r.value);
    else setError(r.reason);
  };

  return (
    <div className="flex flex-col gap-1">
      <div className="flex items-center gap-1">
        <input
          type="text"
          autoFocus
          spellCheck={false}
          autoComplete="off"
          aria-invalid={error !== null}
          className="focus-glow w-full min-w-0 rounded border border-primary/70 bg-background px-1.5 font-mono text-data outline-none aria-[invalid=true]:border-destructive"
          value={text}
          onChange={(e) => {
            setText(e.target.value);
            setError(null);
          }}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              commit();
            } else if (e.key === "Escape") {
              e.preventDefault();
              onCancel();
            }
          }}
          onBlur={commit}
        />
        {nullButton}
      </div>
      {error !== null && <span className="text-xs text-destructive">{error}</span>}
    </div>
  );
}

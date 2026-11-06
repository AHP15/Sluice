/**
 * Cell editor for exact numerics (T-019 / R-005, D-006).
 *
 * Chosen by `ColumnDesc.exact`, never by type class: the `number`-class editor
 * is exactly where R-005's silent rounding lives. This is a plain text control
 * operating on the tagged string end to end — no `type="number"` input, no
 * numeric parsing, no `valueAsNumber`. The digits typed are the digits bound.
 * `scripts/check-exact-numeric.mjs` enforces that on this file.
 *
 * The caller retains the original wire value (D-006) and decides what a commit
 * means (staging is T-056's); this component only produces a validated value.
 */
import { useState } from "react";

import type { ExactDomain } from "../lib/contract";
import { checkExact, type NumericWire } from "../lib/exactNumeric";

export interface ExactNumericEditorProps {
  /** The cell's current wire string, shown verbatim. */
  initial: string;
  domain: ExactDomain;
  onCommit: (value: NumericWire) => void;
  onCancel: () => void;
}

export function ExactNumericEditor({ initial, domain, onCommit, onCancel }: ExactNumericEditorProps) {
  const [text, setText] = useState(initial);
  const [error, setError] = useState<string | null>(null);

  const commit = () => {
    const result = checkExact(text, domain);
    if (result.ok) onCommit(result.value);
    else setError(result.reason);
  };

  return (
    <div className="flex flex-col gap-1">
      <input
        type="text"
        inputMode={domain.kind === "integer" ? "numeric" : "decimal"}
        autoFocus
        spellCheck={false}
        autoComplete="off"
        aria-invalid={error !== null}
        className="w-full rounded border border-input bg-background px-1 font-mono text-sm tabular-nums outline-none focus:ring-1 focus:ring-ring aria-[invalid=true]:border-destructive"
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
      {error !== null && <span className="text-xs text-destructive">{error}</span>}
    </div>
  );
}

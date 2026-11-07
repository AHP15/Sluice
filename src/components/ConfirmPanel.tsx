import { useState } from "react";
import { ShieldAlert } from "lucide-react";
import type { ConfirmationRequest } from "../lib/contract";
import { Button } from "./ui/button";

/**
 * T-061 / D-027: a production session held these statements back. They run
 * only when the user types the phrase (the database name) exactly — the core
 * checks it again, so this panel is the prompt, not the guard.
 */
export function ConfirmPanel({
  request,
  onConfirm,
  onCancel,
}: {
  request: ConfirmationRequest;
  onConfirm: (phrase: string) => void;
  onCancel: () => void;
}) {
  const [typed, setTyped] = useState("");
  const ok = typed.trim() === request.phrase;
  return (
    <form
      role="alertdialog"
      aria-label="Confirm production statements"
      className="space-y-1.5 border-b border-destructive/40 bg-destructive/10 px-3 py-2 text-xs"
      onSubmit={(e) => {
        e.preventDefault();
        if (ok) onConfirm(typed.trim());
      }}
    >
      <p className="flex items-center gap-1 font-medium text-destructive">
        <ShieldAlert className="h-3.5 w-3.5" />
        Production connection — nothing has run yet.
      </p>
      <ul className="space-y-0.5">
        {request.statements.map((s, i) => (
          <li key={i}>
            <span className="font-medium">{s.reason}:</span> <span className="font-mono">{s.statement}</span>
          </li>
        ))}
      </ul>
      <div className="flex items-center gap-1">
        <label className="flex items-center gap-1">
          Type <span className="font-mono font-medium">{request.phrase}</span> to run:
          <input
            autoFocus
            aria-label="Confirmation phrase"
            autoComplete="off"
            spellCheck={false}
            className="focus-glow h-6 w-48 rounded border border-destructive/50 bg-background px-1.5 font-mono"
            value={typed}
            onChange={(e) => setTyped(e.target.value)}
            onKeyDown={(e) => e.key === "Escape" && onCancel()}
          />
        </label>
        <Button type="submit" size="sm" variant="destructive" className="h-6" disabled={!ok}>
          Run on production
        </Button>
        <Button type="button" size="sm" variant="ghost" className="h-6" onClick={onCancel}>
          Cancel
        </Button>
      </div>
    </form>
  );
}

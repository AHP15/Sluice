/**
 * The staged-edit preview (T-057 / R-003, D-027).
 *
 * Renders the core's `EditPlan` and nothing else. It never builds a statement
 * from the edit set: R-003 is that a second build can disagree with the one
 * that executes, and a preview that shows a statement the database never runs
 * is worse than none. When a statement's values can all be shown inline the
 * core supplies `inline`; otherwise this shows the parameterized statement and
 * a table of exactly what is bound.
 *
 * Text nodes only (T-021): statements carry database content.
 */
import type { EditPlan, PlannedStatement } from "../lib/contract";

/** Longest bound value shown in full; the rest is elided with its length. */
const SHOW_CHARS = 300;

function shown(v: string | null): { text: string; note: string | null; isNull: boolean } {
  if (v === null) return { text: "NULL", note: null, isNull: true };
  if (v.length <= SHOW_CHARS) return { text: v, note: null, isNull: false };
  return { text: v.slice(0, SHOW_CHARS), note: `… ${v.length} chars in total`, isNull: false };
}

function Statement({ s, n }: { s: PlannedStatement; n: number }) {
  return (
    <li className="flex flex-col gap-1">
      <div className="text-[10px] uppercase tracking-wide text-muted-foreground">
        statement {n}
        {s.inline === null && " · shown parameterized: a value cannot be inlined faithfully"}
      </div>
      <pre className="whitespace-pre-wrap break-all rounded bg-secondary px-2 py-1 font-mono text-xs">
        {s.inline ?? s.statement}
      </pre>
      {s.inline === null && (
        <table className="text-xs">
          <thead>
            <tr className="text-left text-muted-foreground">
              <th className="pr-3 font-normal">param</th>
              <th className="pr-3 font-normal">type</th>
              <th className="font-normal">bound value</th>
            </tr>
          </thead>
          <tbody>
            {s.params.map((p) => {
              const v = shown(p.value);
              return (
                <tr key={p.placeholder} className="align-top">
                  <td className="pr-3 font-mono">{p.placeholder}</td>
                  <td className="pr-3 font-mono">{p.type_name}</td>
                  <td className={v.isNull ? "font-mono italic text-muted-foreground" : "whitespace-pre-wrap break-all font-mono"}>
                    {v.text}
                    {v.note && <span className="text-muted-foreground"> {v.note}</span>}
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      )}
      {/* T-060 / R-035: reduced protection, said here — on the edit it
          affects, when it is about to run — rather than as a standing banner. */}
      {s.concurrency_omitted.length > 0 && (
        <div className="text-xs text-destructive">
          not checked for concurrent changes (protected by unique key only): {s.concurrency_omitted.join(", ")}
        </div>
      )}
      {(s.concurrency_rendered?.length ?? 0) > 0 && (
        <div className="text-xs text-muted-foreground">
          checked by displayed text (these types have no equality): {s.concurrency_rendered!.join(", ")}
        </div>
      )}
    </li>
  );
}

export function EditPreview({
  plan,
  applying,
  onConfirm,
  onBack,
  compareRendered = false,
  onCompareRendered,
}: {
  plan: EditPlan;
  applying: boolean;
  onConfirm: () => void;
  onBack: () => void;
  /** T-060: whether non-comparable columns are being compared by text. */
  compareRendered?: boolean;
  onCompareRendered?: (on: boolean) => void;
}) {
  const n = plan.statements.length;
  // Offered only when it would change something for this plan.
  const offer =
    onCompareRendered &&
    plan.statements.some((s) => s.concurrency_omitted.length > 0 || (s.concurrency_rendered?.length ?? 0) > 0);
  return (
    <div className="flex max-h-[50vh] flex-col border-t bg-background">
      <div className="flex items-center gap-2 border-b px-3 py-1.5 text-xs">
        <span className="font-medium">
          Review {n} statement{n === 1 ? "" : "s"} — exactly what will run, in one transaction
        </span>
        <span className="text-muted-foreground">no undo after it commits</span>
        {offer && (
          <label
            className="flex items-center gap-1 text-muted-foreground"
            title="For types with no equality (json, xml, geometric…), check that the value is still the text you were shown."
          >
            <input
              type="checkbox"
              disabled={applying}
              checked={compareRendered}
              onChange={(e) => onCompareRendered!(e.target.checked)}
            />
            compare unchecked columns by displayed text
          </label>
        )}
        <div className="ml-auto flex gap-2">
          <button
            type="button"
            disabled={applying}
            className="rounded px-2 py-0.5 hover:bg-secondary disabled:opacity-50"
            onClick={onBack}
          >
            Back to editing
          </button>
          <button
            type="button"
            disabled={applying}
            className="rounded bg-primary px-2 py-0.5 text-primary-foreground disabled:opacity-50"
            onClick={onConfirm}
          >
            {applying ? "Running…" : `Run ${n} statement${n === 1 ? "" : "s"}`}
          </button>
        </div>
      </div>
      <ol className="flex min-h-0 flex-col gap-3 overflow-auto px-3 py-2">
        {plan.statements.map((s, i) => (
          <Statement key={i} s={s} n={i + 1} />
        ))}
      </ol>
    </div>
  );
}

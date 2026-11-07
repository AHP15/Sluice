import { useRef, useState } from "react";
import { Loader2 } from "lucide-react";
import { cancelQuery, type ExportFormat, type ExportedFile } from "../lib/ipc";

/**
 * T-074 / D-024: export as the next action on a truncation notice.
 *
 * `available` carries why an export cannot be offered rather than hiding the
 * action, so the user learns what to do instead (run one statement alone).
 */
export type GridExport =
  | { available: true; run: (format: ExportFormat, queryId: string) => Promise<ExportedFile> }
  | { available: false; reason: string };

const FORMATS: { id: ExportFormat; label: string }[] = [
  { id: "csv", label: "CSV" },
  { id: "jsonl", label: "JSONL" },
  { id: "sql_insert", label: "SQL" },
];

export function ExportActions({ exporting }: { exporting: GridExport }) {
  const [state, setState] = useState<
    { kind: "idle" } | { kind: "running" } | { kind: "done"; file: ExportedFile } | { kind: "error"; message: string }
  >({ kind: "idle" });
  const running = useRef<string | null>(null);

  if (!exporting.available) {
    return <span title={exporting.reason}>export unavailable: {exporting.reason}</span>;
  }

  const start = async (format: ExportFormat) => {
    const id = crypto.randomUUID();
    running.current = id;
    setState({ kind: "running" });
    try {
      const file = await exporting.run(format, id);
      setState({ kind: "done", file });
    } catch (e) {
      setState({ kind: "error", message: String((e as { message?: string })?.message ?? e) });
    } finally {
      running.current = null;
    }
  };

  return (
    <span className="flex items-center gap-1" aria-label="export full result">
      <span>export all:</span>
      {FORMATS.map((f) => (
        <button
          key={f.id}
          type="button"
          disabled={state.kind === "running"}
          className="focus-glow h-6 rounded border border-transparent px-1.5 font-mono text-2xs transition-colors duration-interact hover:bg-accent hover:text-foreground disabled:opacity-50"
          onClick={() => void start(f.id)}
        >
          {f.label}
        </button>
      ))}
      {state.kind === "running" && (
        <>
          <Loader2 className="h-3 w-3 animate-spin" />
          <button
            type="button"
            className="focus-glow h-6 rounded border border-transparent px-1.5 transition-colors duration-interact hover:bg-accent hover:text-foreground"
            onClick={() => running.current && void cancelQuery(running.current)}
          >
            Stop
          </button>
        </>
      )}
      {state.kind === "done" && (
        <span className="text-foreground" title={state.file.path}>
          {state.file.rows} rows → {state.file.path}
        </span>
      )}
      {state.kind === "error" && <span className="text-destructive">{state.message}</span>}
    </span>
  );
}

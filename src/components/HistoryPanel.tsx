import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Clock, ShieldOff, Trash2, X } from "lucide-react";
import type { HistoryEntry } from "../lib/contract";
import { clearHistory, listHistory, queryClient } from "../lib/ipc";
import { cn } from "../lib/utils";

const HISTORY_KEY = (profileId: string | null, search: string) => ["history", profileId, search] as const;

/** Refetch after a run so the newest statement shows up. */
export function invalidateHistory() {
  void queryClient.invalidateQueries({ queryKey: ["history"] });
}

/**
 * T-035 / T-025: query history. Text arrives already redacted by the core;
 * an entry whose credential literal was replaced (or whose text was cut at
 * the size cap) is marked and cannot be loaded back as if it would re-run.
 *
 * Clearing is two clicks, inline — no `window.confirm`, which would block the
 * webview's event loop.
 */
export function HistoryPanel({
  profileId,
  onLoad,
  onClose,
}: {
  profileId: string | null;
  onLoad: (sql: string) => void;
  onClose: () => void;
}) {
  const [search, setSearch] = useState("");
  const [allProfiles, setAllProfiles] = useState(false);
  const [arming, setArming] = useState(false);
  const scope = allProfiles ? null : profileId;
  const history = useQuery({ queryKey: HISTORY_KEY(scope, search), queryFn: () => listHistory(scope, search) });

  return (
    <div className="absolute right-2 top-full z-20 mt-1 flex max-h-96 w-[32rem] flex-col rounded border border-border-strong bg-raised shadow-float">
      <div className="flex items-center gap-1 border-b p-1 text-xs">
        <Clock className="h-3.5 w-3.5 shrink-0" />
        <input
          autoFocus
          aria-label="Search history"
          className="min-w-0 flex-1 focus-glow h-6 rounded border border-input bg-background px-1.5"
          placeholder="Search history"
          value={search}
          onChange={(e) => setSearch(e.target.value)}
        />
        {profileId && (
          <label className="flex items-center gap-1 text-muted-foreground">
            <input type="checkbox" checked={allProfiles} onChange={(e) => setAllProfiles(e.target.checked)} />
            all profiles
          </label>
        )}
        <button
          type="button"
          className={cn(
            "flex items-center gap-1 rounded px-1 py-0.5",
            arming ? "bg-destructive text-destructive-foreground" : "text-muted-foreground hover:text-destructive",
          )}
          onClick={async () => {
            if (!arming) return setArming(true);
            setArming(false);
            await clearHistory();
            invalidateHistory();
          }}
        >
          <Trash2 className="h-3.5 w-3.5" />
          {arming ? "Clear all history?" : "Clear"}
        </button>
        <button type="button" aria-label="Close history" className="rounded p-0.5" onClick={onClose}>
          <X className="h-3.5 w-3.5" />
        </button>
      </div>
      <ul className="min-h-0 flex-1 overflow-auto text-xs">
        {history.error && (
          <li className="p-2 text-destructive">
            {String((history.error as { message?: string })?.message ?? history.error)}
          </li>
        )}
        {history.data?.length === 0 && <li className="p-2 text-muted-foreground">No history yet.</li>}
        {history.data?.map((e) => <Entry key={e.id} entry={e} onLoad={onLoad} />)}
      </ul>
    </div>
  );
}

function Entry({ entry: e, onLoad }: { entry: HistoryEntry; onLoad: (sql: string) => void }) {
  const replayable = !e.redacted && !e.truncated;
  const why = e.redacted
    ? "A credential in this statement was replaced with [redacted] before it was stored, so it will not re-run as written."
    : "This statement was longer than the history size cap; only its beginning was kept.";
  return (
    <li className="border-b last:border-0">
      <button
        type="button"
        disabled={!replayable}
        title={replayable ? "Load into the editor" : why}
        className={cn("block w-full px-2 py-1 text-left", replayable ? "hover:bg-accent" : "cursor-not-allowed")}
        onClick={() => replayable && onLoad(e.statement)}
      >
        <div className="flex items-center gap-2 text-2xs text-muted-foreground">
          <span>{new Date(e.executed_at).toLocaleString()}</span>
          <span className={e.outcome === "ok" ? "" : "text-destructive"}>{e.outcome}</span>
          {e.rows != null && <span>{e.rows} rows</span>}
          {e.duration_ms != null && <span>{e.duration_ms} ms</span>}
          {!replayable && (
            <span className="ml-auto flex items-center gap-1 rounded-sm border border-border-strong px-1 text-foreground">
              <ShieldOff className="h-3 w-3" />
              {e.redacted ? "redacted — not replayable" : "truncated — not replayable"}
            </span>
          )}
        </div>
        <pre className="max-h-16 overflow-hidden whitespace-pre-wrap font-mono text-data">{e.statement}</pre>
      </button>
    </li>
  );
}

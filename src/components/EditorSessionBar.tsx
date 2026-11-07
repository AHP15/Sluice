/**
 * The editor's persistent session indicator (T-041 / D-028).
 *
 * Always visible while an editor session exists, because the one thing a user
 * must never be unsure of is whether their statements are sitting in an open
 * transaction. It shows what the *core* last reported (the server's own
 * transaction state, probed after every run), never what the webview assumes.
 *
 * - idle: autocommit, with the opt-in Begin control;
 * - open: the indicator, how long it has idled, the auto-rollback timeout,
 *   and Commit / Rollback;
 * - failed: only Rollback is possible;
 * - lost (or released to fit the budget): a banner naming what was lost and
 *   an explicit Reconnect — nothing runs on the old session, and nothing
 *   reconnects silently (T-043 / R-037);
 * - and, in every live state, the session state the server says it holds.
 */
import { AlertTriangle, CircleDot, Unplug } from "lucide-react";

import type { SessionStatus } from "../lib/ipc";
import { cn } from "../lib/utils";

export function EditorSessionBar({
  status,
  busy,
  notice,
  onAction,
  onReconnect,
}: {
  status: SessionStatus;
  busy: boolean;
  /** A core-initiated message, such as an idle rollback. */
  notice: string | null;
  onAction: (a: "begin" | "commit" | "rollback") => void;
  onReconnect: () => void;
}) {
  if (status.lost) {
    return (
      <div role="alert" className="flex items-center gap-2 border-b border-destructive/40 bg-destructive/10 px-3 py-1.5 text-xs text-destructive">
        <Unplug className="h-3.5 w-3.5 shrink-0" />
        <span className="text-foreground">{status.lost}</span>
        <button
          type="button"
          className="focus-glow ml-auto h-6 rounded border border-border-strong bg-raised px-2 text-foreground transition-colors duration-interact hover:border-muted-foreground/50"
          onClick={onReconnect}
        >
          Reconnect
        </button>
      </div>
    );
  }

  const tx = status.tx.state;
  const mins = Math.round(status.idle_timeout_secs / 60);
  // D-031: compact controls, glow on focus only.
  const btn =
    "focus-glow h-6 rounded border border-transparent px-2 transition-[background-color,border-color] duration-interact disabled:opacity-50";

  return (
    <div
      className={cn(
        "flex min-h-9 flex-wrap items-center gap-2 border-b px-3 py-1 text-xs",
        // D-032: an open transaction is a caution; a failed one is danger.
        tx === "open" && "border-caution/40 bg-caution/10",
        tx === "failed" && "border-destructive/40 bg-destructive/10",
      )}
    >
      {tx === "idle" && (
        <>
          <span className="text-muted-foreground">autocommit · each statement commits as it runs</span>
          <button type="button" disabled={busy} className={cn(btn, "ml-auto border-border-strong bg-raised hover:border-muted-foreground/50")} onClick={() => onAction("begin")}>
            Begin transaction
          </button>
        </>
      )}
      {tx === "open" && (
        <>
          <CircleDot className="h-3.5 w-3.5 shrink-0 text-caution" />
          <span className="font-medium">Transaction open</span>
          <span className="text-muted-foreground">
            nothing is committed until you commit · rolled back (never committed) if idle {mins} min, or if this tab closes
          </span>
          <div className="ml-auto flex gap-1">
            <button type="button" disabled={busy} className={cn(btn, "bg-primary font-semibold text-primary-foreground hover:opacity-90")} onClick={() => onAction("commit")}>
              Commit
            </button>
            <button type="button" disabled={busy} className={cn(btn, "border-border-strong bg-raised hover:border-muted-foreground/50")} onClick={() => onAction("rollback")}>
              Rollback
            </button>
          </div>
        </>
      )}
      {tx === "failed" && (
        <>
          <AlertTriangle className="h-3.5 w-3.5 shrink-0 text-destructive" />
          <span className="font-medium">Transaction failed</span>
          <span className="text-muted-foreground">a statement errored; the server will only accept a rollback</span>
          <button type="button" disabled={busy} className={cn(btn, "ml-auto bg-primary font-semibold text-primary-foreground hover:opacity-90")} onClick={() => onAction("rollback")}>
            Rollback
          </button>
        </>
      )}
      {/* T-043: what this session holds, as the server reports it. Such a
          session is never released to make room for another tab. */}
      {status.holds.filter((h) => h !== "an open transaction").length > 0 && (
        <span className="basis-full text-muted-foreground">
          session holds: {status.holds.filter((h) => h !== "an open transaction").join(", ")} · kept pinned for this tab
        </span>
      )}
      {notice && <span className="basis-full text-muted-foreground">{notice}</span>}
    </div>
  );
}

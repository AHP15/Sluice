import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Cable } from "lucide-react";
import type { ConnectionUsage as Usage } from "../lib/contract";
import { connectionUsage } from "../lib/ipc";
import { cn } from "../lib/utils";

/**
 * T-044 / R-038: the connection-usage indicator. Shows how much of the
 * profile's budget pinned editor sessions hold (the browse floor is never
 * theirs), and on open lists which tabs hold sessions and which of those hold
 * a transaction — so a refused editor tab can see why, and which tab to close.
 */
export function ConnectionUsageIndicator({ sessionId }: { sessionId: string }) {
  const [open, setOpen] = useState(false);
  const usage = useQuery({
    queryKey: ["connection-usage", sessionId],
    queryFn: () => connectionUsage(sessionId),
    // Live while the list is open; cheap — it reads core state, not the server.
    refetchInterval: open ? 2000 : false,
  });
  const u = usage.data;
  if (!u) return null;
  return (
    <div className="relative">
      <button
        type="button"
        aria-expanded={open}
        className="focus-glow flex items-center gap-1 rounded border border-transparent px-1 text-2xs text-muted-foreground transition-colors duration-interact hover:bg-accent hover:text-foreground"
        title="Connections this profile may hold"
        onClick={() => {
          setOpen((v) => !v);
          void usage.refetch();
        }}
      >
        <Cable className="h-3.5 w-3.5" />
        <UsageSummary u={u} />
      </button>
      {open && <UsageList u={u} />}
    </div>
  );
}

export function UsageSummary({ u }: { u: Usage }) {
  const full = u.pinned.length >= u.pinned_limit;
  return (
    <span className={cn(full && "text-caution")}>
      editors {u.pinned.length}/{u.pinned_limit} · {u.browse_floor} kept for browsing
    </span>
  );
}

export function UsageList({ u }: { u: Usage }) {
  return (
    <div
      role="dialog"
      aria-label="Connection usage"
      className="absolute right-0 top-full z-20 mt-1 w-80 rounded border border-border-strong bg-raised p-2 font-sans text-xs shadow-float"
    >
      <p className="mb-1">
        Budget {u.budget}: up to {u.pinned_limit} editor session{u.pinned_limit === 1 ? "" : "s"}, {u.browse_floor} always
        kept for the navigator and grid.
        {u.max_connections != null && ` The server allows ${u.max_connections} connections across all clients.`}
      </p>
      {u.pinned.length === 0 ? (
        <p className="text-muted-foreground">No editor tab holds a session.</p>
      ) : (
        <ul className="space-y-0.5" data-testid="usage-list">
          {u.pinned.map((p) => (
            <li key={p.editor_id} className="flex items-center gap-1">
              <span className="truncate font-medium">{p.tab}</span>
              <span className="text-muted-foreground">editor</span>
              {p.in_transaction && <span className="rounded-sm border border-caution/60 bg-caution/10 px-1 text-caution">transaction open</span>}
              {p.busy && <span className="rounded-sm border border-border-strong px-1">running</span>}
              {p.holds.length > 0 && <span className="truncate text-muted-foreground">holds {p.holds.join(", ")}</span>}
            </li>
          ))}
        </ul>
      )}
      {u.pinned.length >= u.pinned_limit && (
        <p className="mt-1 text-caution">
          Full: a new editor tab is refused unless one of these is idle and holds no state. Commit or roll back, or close a
          tab.
        </p>
      )}
    </div>
  );
}

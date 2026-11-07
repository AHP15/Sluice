import { useState } from "react";
import { ChevronDown, ChevronRight, Loader2, Pencil, Plus, Power } from "lucide-react";
import { ENGINES, type Engine, type ProfileMeta } from "../lib/contract";
import { cn } from "../lib/utils";

/** One row of the sidebar: a saved connection, or a live unsaved session. */
export interface SidebarConnection {
  /** Stable key: the profile id, or `tab:<id>` for an unsaved session. */
  key: string;
  label: string;
  engine?: Engine;
  /** The saved profile, or null for a session nobody saved (a launch). */
  profile: ProfileMeta | null;
  /** The tab showing this connection (live, connecting or failed), if any. */
  tabId: number | null;
  /** A session is open: the row expands into its navigator. */
  connected: boolean;
  connecting: boolean;
  /** The last connect attempt failed (shown until the next try). */
  failed?: boolean;
}

/** A short engine mark for the row — the engine id, never a colour. */
export function engineMark(engine: Engine | undefined): string {
  switch (engine ?? "postgres") {
    case "postgres":
      return "PG";
    case "sqlserver":
      return "MS";
    case "mongodb":
      return "MG";
  }
}

export function engineLabel(engine: Engine | undefined): string {
  return ENGINES.find((e) => e.id === (engine ?? "postgres"))?.label ?? String(engine);
}

/**
 * The one left-hand panel: every saved connection, and — under each one
 * that is connected — its data navigator (T-093 / D-011, D-014).
 *
 * Clicking a connection connects it (or switches to it) and opens its tree
 * as a child list. The tree itself is the tab's own `Navigator`, rendered
 * into the slot under the row (`slotRef`), so selection and browsing stay
 * owned by the tab. Creating and editing happen in the connection dialog;
 * the name can also be changed inline (double-click), written straight to
 * the profile file (T-094).
 */
export function ConnectionsSidebar({
  items,
  activeTabId,
  expanded,
  onOpen,
  onToggle,
  onDisconnect,
  onNew,
  onEdit,
  onRename,
  slotRef,
}: {
  items: SidebarConnection[];
  activeTabId: number;
  expanded: Set<string>;
  onOpen: (item: SidebarConnection) => void;
  onToggle: (key: string) => void;
  onDisconnect: (tabId: number) => void;
  onNew: () => void;
  onEdit: (profile: ProfileMeta) => void;
  onRename: (p: ProfileMeta, label: string) => Promise<void>;
  slotRef: (tabId: number) => (el: HTMLDivElement | null) => void;
}) {
  const [editing, setEditing] = useState<string | null>(null);
  const [draft, setDraft] = useState("");
  const [renameError, setRenameError] = useState<string | null>(null);

  const commit = async (p: ProfileMeta) => {
    const name = draft.trim();
    if (!name) {
      setRenameError("A connection needs a name.");
      return;
    }
    if (name === p.label) {
      setEditing(null);
      return;
    }
    try {
      await onRename(p, name);
      setEditing(null);
      setRenameError(null);
    } catch (e) {
      setRenameError(String((e as { message?: string })?.message ?? e));
    }
  };

  return (
    <nav aria-label="Connections" className="flex w-72 shrink-0 flex-col border-r bg-panel">
      <div className="flex h-9 shrink-0 items-center justify-between px-3">
        <span className="text-2xs font-medium uppercase tracking-label text-muted-foreground">Connections</span>
        <button
          type="button"
          aria-label="New connection"
          title="New connection"
          className="focus-glow flex h-6 items-center gap-1 rounded border border-transparent px-1.5 text-2xs text-muted-foreground transition-colors duration-interact hover:bg-accent hover:text-foreground"
          onClick={onNew}
        >
          <Plus className="h-3.5 w-3.5" />
          New
        </button>
      </div>
      <ul className="min-h-0 flex-1 overflow-auto px-1.5 pb-2">
        {items.length === 0 && (
          <li className="px-2 py-3 text-xs text-muted-foreground">
            No connections yet.{" "}
            <button type="button" className="text-primary underline-offset-2 hover:underline" onClick={onNew}>
              Create one
            </button>
            .
          </li>
        )}
        {items.map((it) => {
          const live = it.connected;
          const active = it.tabId != null && it.tabId === activeTabId;
          const open = live && expanded.has(it.key);
          const stateText = it.connecting ? "connecting" : live ? "connected" : it.failed ? "failed to connect" : "not connected";
          const p = it.profile;
          return (
            <li key={it.key} className="group">
              <div
                className={cn(
                  "flex h-row items-center gap-1.5 rounded pl-1 pr-1.5 transition-colors duration-interact",
                  active ? "bg-primary/10 shadow-inset-accent" : "hover:bg-accent",
                )}
              >
                <button
                  type="button"
                  aria-label={open ? `Collapse ${it.label}` : `Expand ${it.label}`}
                  className="flex h-5 w-4 shrink-0 items-center justify-center text-muted-foreground disabled:opacity-0"
                  disabled={!live}
                  onClick={() => onToggle(it.key)}
                >
                  {open ? <ChevronDown className="h-3 w-3" /> : <ChevronRight className="h-3 w-3" />}
                </button>
                <span
                  aria-hidden
                  className="w-6 shrink-0 rounded-sm border border-border-strong text-center font-mono text-2xs leading-4 text-muted-foreground"
                  title={engineLabel(it.engine)}
                >
                  {engineMark(it.engine)}
                </span>
                {p && editing === p.id ? (
                  <input
                    autoFocus
                    aria-label={`Rename ${p.label}`}
                    className="focus-glow h-6 min-w-0 flex-1 rounded border border-input bg-background px-1.5 text-sm"
                    value={draft}
                    onChange={(e) => {
                      setDraft(e.target.value);
                      setRenameError(null);
                    }}
                    onKeyDown={(e) => {
                      if (e.key === "Enter") void commit(p);
                      if (e.key === "Escape") {
                        setEditing(null);
                        setRenameError(null);
                      }
                    }}
                    onBlur={() => void commit(p)}
                  />
                ) : (
                  <button
                    type="button"
                    className={cn("min-w-0 flex-1 truncate text-left text-sm", !p && "italic")}
                    title={p ? `${engineLabel(p.engine)} · ${p.user}@${p.host}:${p.port}/${p.dbname}` : "Not saved"}
                    aria-label={`${it.label}, ${engineLabel(it.engine)}, ${stateText}`}
                    onClick={() => onOpen(it)}
                    onDoubleClick={() => {
                      if (!p) return;
                      setEditing(p.id);
                      setDraft(p.label);
                      setRenameError(null);
                    }}
                  >
                    {it.label}
                  </button>
                )}
                {it.connecting ? (
                  <Loader2 aria-hidden className="h-3 w-3 shrink-0 animate-spin text-muted-foreground" />
                ) : (
                  // Connection state: shape and label, not a status hue —
                  // being connected is not a safety guarantee (D-032).
                  <span
                    aria-hidden
                    title={stateText}
                    className={cn(
                      "h-1.5 w-1.5 shrink-0 rounded-full",
                      live ? "bg-foreground" : it.failed ? "bg-destructive" : "border border-muted-foreground",
                    )}
                  />
                )}
                {p && (
                  <button
                    type="button"
                    aria-label={`Edit ${it.label}`}
                    title="Edit connection"
                    className="focus-glow rounded border border-transparent p-0.5 text-muted-foreground opacity-0 transition-opacity duration-interact hover:text-foreground focus-visible:opacity-100 group-hover:opacity-100"
                    onClick={() => onEdit(p)}
                  >
                    <Pencil className="h-3 w-3" />
                  </button>
                )}
                {it.tabId != null && (
                  <button
                    type="button"
                    aria-label={`Disconnect ${it.label}`}
                    title="Disconnect"
                    className="focus-glow rounded border border-transparent p-0.5 text-muted-foreground opacity-0 transition-opacity duration-interact hover:text-foreground focus-visible:opacity-100 group-hover:opacity-100"
                    onClick={() => onDisconnect(it.tabId!)}
                  >
                    <Power className="h-3 w-3" />
                  </button>
                )}
              </div>
              {p && editing === p.id && renameError && (
                <p role="alert" className="px-2 py-0.5 text-2xs text-destructive">
                  {renameError}
                </p>
              )}
              {/* The tab's navigator renders here, as this connection's
                  children. Kept mounted while collapsed so the tree keeps its
                  expansion state. */}
              {live && (
                <div ref={slotRef(it.tabId!)} className={cn("ml-3 border-l pl-1", !open && "hidden")} />
              )}
            </li>
          );
        })}
      </ul>
    </nav>
  );
}

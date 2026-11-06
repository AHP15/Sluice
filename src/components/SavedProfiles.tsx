import { useState } from "react";
import { Database, Trash2 } from "lucide-react";
import type { ProfileMeta, WorkspaceView } from "../lib/contract";
import { cn } from "../lib/utils";

/**
 * T-036: the profiles in the portable profile file. Picking one loads its
 * metadata into the connect form; the password stays in the keychain (D-007).
 *
 * Delete is two clicks, inline, rather than a `window.confirm` — a native
 * dialog blocks the webview's event loop.
 */
export function SavedProfiles({
  workspace,
  selectedId,
  onPick,
  onDelete,
}: {
  workspace: WorkspaceView | undefined;
  selectedId: string;
  onPick: (p: ProfileMeta) => void;
  onDelete: (id: string) => void;
}) {
  const [arming, setArming] = useState<string | null>(null);
  const profiles = workspace?.profiles ?? [];
  if (profiles.length === 0) return null;

  return (
    <div className="border-b pb-1">
      <p className="px-2 py-1 text-xs font-medium uppercase text-muted-foreground">Profiles</p>
      <ul>
        {profiles.map((p) => (
          <li key={p.id} className="group flex items-center">
            <button
              type="button"
              className={cn(
                "flex min-w-0 flex-1 items-center gap-1.5 rounded px-2 py-1 text-left text-sm hover:bg-accent",
                p.id === selectedId && "bg-accent",
              )}
              title={`${p.user}@${p.host}:${p.port}/${p.dbname}`}
              onClick={() => {
                setArming(null);
                onPick(p);
              }}
            >
              <Database className="h-3.5 w-3.5 shrink-0" />
              <span className="truncate">{p.label}</span>
            </button>
            <button
              type="button"
              aria-label={arming === p.id ? `Confirm delete ${p.label}` : `Delete ${p.label}`}
              className={cn(
                "mr-1 rounded px-1 py-0.5 text-xs",
                arming === p.id
                  ? "bg-destructive text-destructive-foreground"
                  : "text-muted-foreground opacity-0 hover:text-destructive group-hover:opacity-100",
              )}
              onClick={() => {
                if (arming === p.id) {
                  setArming(null);
                  onDelete(p.id);
                } else {
                  setArming(p.id);
                }
              }}
            >
              {arming === p.id ? "Delete?" : <Trash2 className="h-3.5 w-3.5" />}
            </button>
          </li>
        ))}
      </ul>
      {workspace?.path && (
        <p className="truncate px-2 pt-1 text-[10px] text-muted-foreground" title={workspace.path}>
          {workspace.path}
        </p>
      )}
    </div>
  );
}

export function newProfileId(): string {
  return `profile-${Math.random().toString(36).slice(2, 10)}`;
}

import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Clock, Save, Trash2 } from "lucide-react";
import { HistoryPanel } from "./HistoryPanel";
import { WORKSPACE_KEY, deleteQuery, loadWorkspace, queryClient, saveQuery } from "../lib/ipc";
import { Select } from "./ui/select";

/**
 * T-036: saved queries live beside profiles in the portable profile file.
 * Loading one replaces the editor text; saving names the current text.
 */
export function SavedQueries({
  sql,
  profileId,
  onLoad,
}: {
  sql: string;
  profileId: string | null;
  onLoad: (sql: string) => void;
}) {
  const workspace = useQuery({ queryKey: WORKSPACE_KEY, queryFn: loadWorkspace });
  const [naming, setNaming] = useState(false);
  const [label, setLabel] = useState("");
  const [selected, setSelected] = useState("");
  const [err, setErr] = useState<string | null>(null);
  const [showHistory, setShowHistory] = useState(false);
  const queries = workspace.data?.queries ?? [];

  const run = async (op: () => Promise<unknown>) => {
    setErr(null);
    try {
      queryClient.setQueryData(WORKSPACE_KEY, await op());
    } catch (e) {
      setErr(String((e as { message?: string })?.message ?? e));
    }
  };

  return (
    <div className="relative flex h-9 shrink-0 items-center gap-1.5 border-b px-3 text-xs">
      <Select
        aria-label="Saved queries"
        size="sm"
        className="max-w-48"
        value={selected}
        options={[
          { value: "", label: queries.length ? "Saved queries…" : "No saved queries" },
          ...queries.map((q) => ({ value: q.id, label: q.label })),
        ]}
        onChange={(v) => {
          setSelected(v);
          const q = queries.find((x) => x.id === v);
          if (q) onLoad(q.sql);
        }}
      />
      {selected && (
        <button
          type="button"
          aria-label="Delete saved query"
          className="focus-glow rounded border border-transparent p-1 text-muted-foreground hover:text-destructive"
          onClick={() =>
            void run(async () => {
              const v = await deleteQuery(selected);
              setSelected("");
              return v;
            })
          }
        >
          <Trash2 className="h-3.5 w-3.5" />
        </button>
      )}
      {naming ? (
        <form
          className="flex items-center gap-1"
          onSubmit={(e) => {
            e.preventDefault();
            const name = label.trim();
            if (!name) return;
            void run(async () => {
              const id = `query-${Math.random().toString(36).slice(2, 10)}`;
              const v = await saveQuery({ id, label: name, profile: profileId, sql });
              setNaming(false);
              setLabel("");
              setSelected(id);
              return v;
            });
          }}
        >
          <input
            autoFocus
            aria-label="Query name"
            className="w-40 focus-glow h-6 rounded border border-input bg-background px-1.5"
            placeholder="Name this query"
            value={label}
            onChange={(e) => setLabel(e.target.value)}
            onKeyDown={(e) => e.key === "Escape" && setNaming(false)}
          />
        </form>
      ) : (
        <button
          type="button"
          className="flex items-center gap-1 focus-glow h-6 rounded border border-transparent px-1.5 text-muted-foreground transition-colors duration-interact hover:bg-accent hover:text-foreground"
          onClick={() => setNaming(true)}
        >
          <Save className="h-3.5 w-3.5" />
          Save query
        </button>
      )}
      {err && <span className="truncate text-destructive">{err}</span>}
      <button
        type="button"
        className="ml-auto flex items-center gap-1 focus-glow h-6 rounded border border-transparent px-1.5 text-muted-foreground transition-colors duration-interact hover:bg-accent hover:text-foreground"
        aria-expanded={showHistory}
        onClick={() => setShowHistory((v) => !v)}
      >
        <Clock className="h-3.5 w-3.5" />
        History
      </button>
      {showHistory && (
        <HistoryPanel
          profileId={profileId}
          onLoad={(q) => {
            onLoad(q);
            setShowHistory(false);
          }}
          onClose={() => setShowHistory(false)}
        />
      )}
    </div>
  );
}

import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Save, Trash2 } from "lucide-react";
import { WORKSPACE_KEY, deleteQuery, loadWorkspace, queryClient, saveQuery } from "../lib/ipc";

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
    <div className="flex items-center gap-1 border-b px-2 py-0.5 text-xs">
      <select
        aria-label="Saved queries"
        className="max-w-48 rounded border border-input bg-transparent px-1 py-0.5"
        value={selected}
        onChange={(e) => {
          setSelected(e.target.value);
          const q = queries.find((x) => x.id === e.target.value);
          if (q) onLoad(q.sql);
        }}
      >
        <option value="">{queries.length ? "Saved queries…" : "No saved queries"}</option>
        {queries.map((q) => (
          <option key={q.id} value={q.id}>
            {q.label}
          </option>
        ))}
      </select>
      {selected && (
        <button
          type="button"
          aria-label="Delete saved query"
          className="rounded p-0.5 text-muted-foreground hover:text-destructive"
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
            className="w-40 rounded border border-input bg-transparent px-1 py-0.5"
            placeholder="Name this query"
            value={label}
            onChange={(e) => setLabel(e.target.value)}
            onKeyDown={(e) => e.key === "Escape" && setNaming(false)}
          />
        </form>
      ) : (
        <button
          type="button"
          className="flex items-center gap-1 rounded px-1 py-0.5 text-muted-foreground hover:text-foreground"
          onClick={() => setNaming(true)}
        >
          <Save className="h-3.5 w-3.5" />
          Save query
        </button>
      )}
      {err && <span className="truncate text-destructive">{err}</span>}
    </div>
  );
}

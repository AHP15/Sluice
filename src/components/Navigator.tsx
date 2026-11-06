import { useCallback, useEffect, useState } from "react";
import { ChevronRight, ChevronDown, Loader2, RefreshCw } from "lucide-react";
import type { Capabilities, NavNode, NodeId, NodeKind } from "../lib/contract";
import { navigatorChildren, refreshNavigator } from "../lib/ipc";
import { cn } from "../lib/utils";

/**
 * The generic navigator tree (T-008 / D-029).
 *
 * The load-bearing property: **there is no database/schema/table hierarchy in
 * this file.** Node kinds are strings the driver declared in its capability
 * descriptor, and this component only asks two questions of a kind — can it have
 * children, and can it be browsed. That is what let Mongo's two-level
 * `database → collection` tree fit the same component with no placeholder node
 * (T-073 F-6), and it is what keeps engine checks out of the React layer (D-003).
 *
 * If you ever find yourself writing `if (kind === "schema")` here, the
 * capability descriptor is missing a field.
 */
export function Navigator({
  sessionId,
  capabilities,
  selected,
  onSelect,
}: {
  sessionId: string;
  capabilities: Capabilities;
  selected: NodeId | null;
  onSelect: (node: NavNode, kind: NodeKind) => void;
}) {
  const kindsById = new Map(capabilities.node_kinds.map((k) => [k.id, k]));
  // T-053 / D-016: the core caches each level; refresh drops that cache and
  // remounts the tree, which re-fetches whatever is currently open.
  const [generation, setGeneration] = useState(0);
  const [refreshing, setRefreshing] = useState(false);

  return (
    <div className="text-sm">
      <div className="flex items-center justify-end px-1 pb-1">
        <button
          type="button"
          className="flex items-center gap-1 rounded px-1 text-xs text-muted-foreground hover:bg-accent"
          title="Re-read the catalog (changes made outside this app are not detected automatically)"
          disabled={refreshing}
          onClick={async () => {
            setRefreshing(true);
            try {
              await refreshNavigator(sessionId);
              setGeneration((g) => g + 1);
            } finally {
              setRefreshing(false);
            }
          }}
        >
          <RefreshCw className={cn("h-3 w-3", refreshing && "animate-spin")} />
          Refresh
        </button>
      </div>
      <Level
        key={generation}
        sessionId={sessionId}
        parent={null}
        depth={0}
        kindsById={kindsById}
        selected={selected}
        onSelect={onSelect}
      />
    </div>
  );
}

function sameNode(a: NodeId | null, b: NodeId): boolean {
  return !!a && a.kind === b.kind && a.path.length === b.path.length && a.path.every((s, i) => s === b.path[i]);
}

function Level({
  sessionId,
  parent,
  depth,
  kindsById,
  selected,
  onSelect,
}: {
  sessionId: string;
  parent: NodeId | null;
  depth: number;
  kindsById: Map<string, NodeKind>;
  selected: NodeId | null;
  onSelect: (node: NavNode, kind: NodeKind) => void;
}) {
  const [nodes, setNodes] = useState<NavNode[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [expanded, setExpanded] = useState<Set<string>>(new Set());

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setNodes(await navigatorChildren(sessionId, parent));
    } catch (e) {
      setError(String((e as { message?: string })?.message ?? e));
    } finally {
      setLoading(false);
    }
  }, [sessionId, parent]);

  // A level mounts only when it should be visible — roots at startup, deeper
  // levels when their parent expands — so fetching on mount IS the lazy load.
  useEffect(() => {
    void load();
  }, [load]);

  if (error) {
    return <p className="px-2 py-1 text-xs text-destructive">{error}</p>;
  }
  if (nodes === null || loading) {
    return (
      <p className="flex items-center gap-1 px-2 py-1 text-xs text-muted-foreground">
        <Loader2 className="h-3 w-3 animate-spin" /> loading…
      </p>
    );
  }
  if (nodes.length === 0) {
    return <p className="px-2 py-1 text-xs text-muted-foreground">empty</p>;
  }

  return (
    <ul>
      {nodes.map((node) => {
        const key = `${node.id.kind}:${node.id.path.join("/")}`;
        const kind = kindsById.get(node.id.kind);
        const isOpen = expanded.has(key);
        // Trust the node's own answer, falling back to the declared kind. A
        // kind the driver did not declare still renders as a leaf rather than
        // breaking the tree — D-029 requires degrading gracefully on
        // unrecognised kinds.
        const canExpand = node.has_children || (kind?.has_children ?? false);
        const isRelation = kind?.is_relation ?? false;

        return (
          <li key={key}>
            <button
              type="button"
              className={cn(
                "flex w-full items-center gap-1 rounded px-1 py-0.5 text-left hover:bg-accent",
                sameNode(selected, node.id) && "bg-accent font-medium",
              )}
              style={{ paddingLeft: `${depth * 12 + 4}px` }}
              onClick={() => {
                if (canExpand) {
                  setExpanded((prev) => {
                    const next = new Set(prev);
                    if (next.has(key)) next.delete(key);
                    else next.add(key);
                    return next;
                  });
                }
                if (isRelation && kind) onSelect(node, kind);
              }}
              title={kind?.label ?? node.id.kind}
            >
              {canExpand ? (
                isOpen ? (
                  <ChevronDown className="h-3 w-3 shrink-0" />
                ) : (
                  <ChevronRight className="h-3 w-3 shrink-0" />
                )
              ) : (
                <span className="w-3 shrink-0" />
              )}
              {/* Labels are database content: rendered as a text node, never
                  as markup (T-021/D-007). */}
              <span className="truncate">{node.label}</span>
              <span className="ml-auto shrink-0 pl-2 text-[10px] uppercase text-muted-foreground">
                {kind?.label ?? node.id.kind}
              </span>
            </button>

            {canExpand && isOpen && (
              <Level
                sessionId={sessionId}
                parent={node.id}
                depth={depth + 1}
                kindsById={kindsById}
                selected={selected}
                onSelect={onSelect}
              />
            )}
          </li>
        );
      })}
    </ul>
  );
}

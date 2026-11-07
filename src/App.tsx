import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { useQuery } from "@tanstack/react-query";
import { Bug, Database, Monitor, Play, Rows3, Save, Square, Sun, Moon, Plus, X } from "lucide-react";
import { Navigator } from "./components/Navigator";
import { QueryEditor } from "./components/QueryEditor";
import { EditPreview } from "./components/EditPreview";
import { EditorSessionBar } from "./components/EditorSessionBar";
import { editorSessionPolicy } from "./lib/sessionPolicy";
import { ResultGrid } from "./components/ResultGrid";
import { pathKey, rowsAsDocuments } from "./lib/columnLayout";
import {
  EMPTY,
  NO_NEW_ROWS,
  addNewRow,
  applyToRows,
  buildEditRequests,
  clearNewCell,
  removeNewRow,
  setNewCell,
  stage,
  totalPending,
  unstage,
  type NewRows,
  type Pending,
} from "./lib/pendingEdits";
import { Button } from "./components/ui/button";
import { StatusBanner, StatusPill } from "./components/ui/status";
import type {
  CatalogSupport,
  ColumnDesc,
  ConfirmationRequest,
  DriverError,
  EditGate,
  EditPlan,
  EditRequest,
  FilterTree,
  Guardrails,
  LaunchOutcome,
  NavNode,
  NodeId,
  Outcome,
  ProfileMeta,
  SessionInfo,
  WireValue,
  Engine,
} from "./lib/contract";
import {
  LAUNCH_PENDING_EVENT,
  RowStream,
  cancelQuery,
  connect,
  applyEdits,
  previewEdits,
  EDITOR_EVENT,
  closeEditor,
  editorTransaction,
  openEditor,
  runInEditor,
  type EditorEvent,
  type SessionStatus,
  connectLaunch,
  coreVersion,
  loadWorkspace,
  queryClient,
  saveProfile,
  WORKSPACE_KEY,
  diagnosticsPreview,
  disconnect,
  exportResult,
  type ExportSource,
  inferColumns,
  runScript,
  saveSessionAsProfile,
  setVerboseDiagnostics,
  takeLaunches,
  verboseDiagnostics,
} from "./lib/ipc";
import { applyAppearance, resolveTheme, useUiStore } from "./store/ui";
import { cn } from "./lib/utils";
import { SavedQueries } from "./components/SavedQueries";
import { FilterBar } from "./components/FilterBar";
import { ConfirmPanel } from "./components/ConfirmPanel";
import { ConnectionsSidebar, type SidebarConnection } from "./components/ConnectionsSidebar";
import { ConnectionDialog, type DialogMode } from "./components/ConnectionDialog";
import { createPortal } from "react-dom";
import { ConnectionUsageIndicator } from "./components/ConnectionUsage";
import { invalidateHistory } from "./components/HistoryPanel";

interface RowsView {
  columns: ColumnDesc[];
  rows: WireValue[][];
  truncated: boolean;
  label: string;
  /** T-056: set for a structured browse of one relation; raw results have none. */
  relation?: NodeId;
  edit?: EditGate;
  /** T-074: how to re-read this result in full for export, or why it can't be. */
  source?: ExportSource | { kind: "unavailable"; reason: string };
}

/** What a tab was opened for. */
type TabSeed =
  | { kind: "blank" }
  | { kind: "launch"; outcome: LaunchOutcome }
  /** T-093: opened from the connections sidebar; connects at once. */
  | { kind: "profile"; profile: ProfileMeta };

/** T-093: what a tab reports about its session, for the sidebar. */
interface TabSession {
  /** The saved profile the session is on (the session id's prefix). */
  profileId: string;
  state: "connecting" | "connected" | "failed";
  engine?: Engine;
}

interface Tab {
  id: number;
  seed: TabSeed;
  title: string;
}

function titleFor(o: LaunchOutcome): string {
  switch (o.kind) {
    case "connect":
      return `${o.target.database}@${o.target.host}`;
    case "choose":
      return "New connection";
    case "error":
      return "Launch failed";
  }
}

/**
 * The tab shell (T-026 / D-012). One process, one window: the first launch and
 * every later `sluice` invocation each arrive as a launch outcome from the core
 * and open a tab here. Tabs stay mounted while hidden so each keeps its state.
 */
/** T-092 / D-031: theme and density live as classes on <html>; `system`
 *  follows the OS preference live. */
function useAppearance() {
  const theme = useUiStore((s) => s.theme);
  const density = useUiStore((s) => s.density);
  useEffect(() => {
    const mq = window.matchMedia?.("(prefers-color-scheme: light)");
    const apply = () => applyAppearance(document.documentElement, resolveTheme(theme, !!mq?.matches), density);
    apply();
    mq?.addEventListener?.("change", apply);
    return () => mq?.removeEventListener?.("change", apply);
  }, [theme, density]);
}

export function App() {
  useAppearance();
  const [tabs, setTabs] = useState<Tab[]>([{ id: 0, seed: { kind: "blank" }, title: "New connection" }]);
  const [active, setActive] = useState(0);
  const nextId = useRef(1);
  const [diagOpen, setDiagOpen] = useState(false);
  // T-093: which tab holds a session on which saved profile.
  const [tabSessions, setTabSessions] = useState<Record<number, TabSession | null>>({});
  const workspace = useQuery({ queryKey: WORKSPACE_KEY, queryFn: loadWorkspace });
  const profiles = workspace.data?.profiles ?? [];
  const reportSession = useCallback((tabId: number, s: TabSession | null) => {
    setTabSessions((prev) => (JSON.stringify(prev[tabId]) === JSON.stringify(s) ? prev : { ...prev, [tabId]: s }));
  }, []);
  // T-093: which tab shows which profile, and in what state.
  const byProfile = useMemo(() => {
    const out: Record<string, { tabId: number; s: TabSession }> = {};
    for (const t of tabs) {
      const s = tabSessions[t.id];
      if (s && !out[s.profileId]) out[s.profileId] = { tabId: t.id, s };
    }
    return out;
  }, [tabs, tabSessions]);
  // Connections whose tree is open in the sidebar.
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const toggleExpanded = (key: string) =>
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  // A connection that just connected opens its tree.
  const seenConnected = useRef(new Set<number>());
  useEffect(() => {
    for (const [id, s] of Object.entries(tabSessions)) {
      const tabId = Number(id);
      if (s?.state === "connected" && !seenConnected.current.has(tabId)) {
        seenConnected.current.add(tabId);
        const key = profiles.some((p) => p.id === s.profileId) ? s.profileId : `tab:${tabId}`;
        setExpanded((prev) => new Set(prev).add(key));
      }
    }
  }, [tabSessions, profiles]);
  // The sidebar slot each tab's navigator renders into.
  const [slots, setSlots] = useState<Record<number, HTMLDivElement | null>>({});
  const slotRefs = useRef(new Map<number, (el: HTMLDivElement | null) => void>());
  const slotRef = useCallback((tabId: number) => {
    let f = slotRefs.current.get(tabId);
    if (!f) {
      f = (el) => setSlots((prev) => (prev[tabId] === el ? prev : { ...prev, [tabId]: el }));
      slotRefs.current.set(tabId, f);
    }
    return f;
  }, []);
  // Create / edit a connection, in a modal.
  const [dialog, setDialog] = useState<DialogMode | null>(null);

  const open = useCallback((outcomes: LaunchOutcome[], replaceInitial: boolean) => {
    const created: Tab[] = outcomes.map((o) => ({
      id: nextId.current++,
      seed: { kind: "launch", outcome: o },
      title: titleFor(o),
    }));
    if (created.length === 0) return;
    // The first launch replaces the placeholder tab rather than sitting beside it.
    setTabs((prev) =>
      replaceInitial && prev.length === 1 && prev[0].id === 0 ? created : [...prev, ...created],
    );
    setActive(created[created.length - 1].id);
  }, []);

  useEffect(() => {
    let disposed = false;
    // Pull on mount: the first launch may have resolved before this listener
    // existed. Then pull again every time the core says there is more.
    const drain = async (replaceInitial: boolean) => {
      const outcomes = await takeLaunches().catch(() => [] as LaunchOutcome[]);
      if (!disposed) open(outcomes, replaceInitial);
    };
    void drain(true);
    const unlisten = listen(LAUNCH_PENDING_EVENT, () => void drain(false));
    return () => {
      disposed = true;
      void unlisten.then((f) => f());
    };
  }, [open]);

  const close = (id: number) => {
    setTabSessions((prev) => {
      const { [id]: _gone, ...rest } = prev;
      return rest;
    });
    setTabs((prev) => {
      const next = prev.filter((t) => t.id !== id);
      if (active === id && next.length > 0) setActive(next[next.length - 1].id);
      return next.length > 0 ? next : [{ id: nextId.current++, seed: { kind: "blank" }, title: "New connection" }];
    });
  };

  const addBlank = () => {
    const id = nextId.current++;
    setTabs((prev) => [...prev, { id, seed: { kind: "blank" }, title: "New connection" }]);
    setActive(id);
  };

  /** T-093: switch to the profile's live tab, or open one that connects. An
   *  idle blank tab in front is reused rather than left beside it. */
  const openProfile = (p: ProfileMeta) => {
    const live = byProfile[p.id];
    setExpanded((prev) => new Set(prev).add(p.id));
    if (live) {
      setActive(live.tabId);
      return;
    }
    const id = nextId.current++;
    const tab: Tab = { id, seed: { kind: "profile", profile: p }, title: p.label };
    setTabs((prev) => {
      const current = prev.find((t) => t.id === active);
      const reusable = current && current.seed.kind === "blank" && !tabSessions[current.id];
      return reusable ? prev.map((t) => (t.id === active ? tab : t)) : [...prev, tab];
    });
    setActive(id);
  };

  /** T-094: rename in place, written to the portable profile file. */
  const renameProfile = async (p: ProfileMeta, label: string) => {
    queryClient.setQueryData(WORKSPACE_KEY, await saveProfile({ ...p, label }));
  };

  /** A tab on a saved profile is titled by the profile's current name. */
  const titleOf = (t: Tab) => {
    const pid = tabSessions[t.id]?.profileId ?? (t.seed.kind === "profile" ? t.seed.profile.id : null);
    return profiles.find((p) => p.id === pid)?.label ?? t.title;
  };

  // The sidebar: every saved connection, then live sessions nobody saved
  // (a command-line launch), each expandable into its navigator.
  const sidebarItems: SidebarConnection[] = [
    ...profiles.map((p) => {
      const live = byProfile[p.id];
      return {
        key: p.id,
        label: p.label,
        engine: p.engine,
        profile: p,
        tabId: live?.tabId ?? null,
        connected: live?.s.state === "connected",
        connecting: live?.s.state === "connecting",
        failed: live?.s.state === "failed",
      };
    }),
    ...tabs
      .filter((t) => {
        const s = tabSessions[t.id];
        return s?.state === "connected" && !profiles.some((p) => p.id === s.profileId);
      })
      .map((t) => ({
        key: `tab:${t.id}`,
        label: titleOf(t),
        engine: tabSessions[t.id]?.engine,
        profile: null,
        tabId: t.id,
        connected: true,
        connecting: false,
      })),
  ];

  /** Replace a tab that is not connected with a fresh one on `p`'s current
   *  settings — the tab's seed is a copy taken when it opened, so retrying
   *  on it would reuse settings the user has since changed. */
  const reconnectTab = (tabId: number, p: ProfileMeta) => {
    const id = nextId.current++;
    setTabSessions((prev) => {
      const { [tabId]: _old, ...rest } = prev;
      return rest;
    });
    setTabs((prev) => prev.map((t) => (t.id === tabId ? { id, seed: { kind: "profile", profile: p }, title: p.label } : t)));
    setActive(id);
    setExpanded((prev) => new Set(prev).add(p.id));
  };

  const onDialogSaved = (p: ProfileMeta, connect: boolean) => {
    if (!connect) return;
    const live = byProfile[p.id];
    // A failed or pending connect retries with the settings just saved; a
    // live session is switched to (its settings change on next connect).
    if (live && live.s.state !== "connected") reconnectTab(live.tabId, p);
    else openProfile(p);
  };

  /** Retry from a failed tab: always with the profile as saved now. */
  const retryTab = (tabId: number, profileId: string) => {
    const latest = profiles.find((p) => p.id === profileId);
    if (latest) reconnectTab(tabId, latest);
  };

  return (
    <div className="flex h-screen flex-col bg-background text-foreground">
      {/* T-095 / D-031: one title bar — the mark, the tabs, the appearance
          controls. Session state lives in each tab's own toolbar. */}
      <header className="flex h-10 shrink-0 items-center gap-3 border-b bg-panel pl-3 pr-2">
        <span className="flex select-none items-center gap-2 text-xs font-semibold tracking-label">
          <span aria-hidden className="block h-2.5 w-2.5 rotate-45 border-[1.5px] border-primary" />
          SLUICE
        </span>
        <nav className="flex min-w-0 items-center gap-1 overflow-x-auto" role="tablist" aria-label="Connection tabs">
          {tabs.map((t) => {
            const on = t.id === active;
            return (
              <div
                key={t.id}
                role="tab"
                aria-selected={on}
                className={cn(
                  "group flex h-7 shrink-0 items-center gap-1.5 rounded border pl-2.5 pr-1 text-xs transition-[background-color,border-color,box-shadow] duration-interact",
                  on
                    ? "glow border-primary/60 bg-raised text-foreground"
                    : "border-transparent text-muted-foreground hover:bg-accent hover:text-foreground",
                )}
              >
                <button type="button" onClick={() => setActive(t.id)} className="max-w-48 truncate font-mono">
                  {titleOf(t)}
                </button>
                <button
                  type="button"
                  aria-label={`Close ${titleOf(t)}`}
                  className="rounded p-0.5 opacity-60 hover:bg-accent hover:opacity-100"
                  onClick={() => close(t.id)}
                >
                  <X className="h-3 w-3" />
                </button>
              </div>
            );
          })}
          <button
            type="button"
            aria-label="New tab"
            className="focus-glow rounded border border-transparent p-1 text-muted-foreground hover:bg-accent hover:text-foreground"
            onClick={addBlank}
          >
            <Plus className="h-3.5 w-3.5" />
          </button>
        </nav>
        <div className="ml-auto flex items-center gap-1">
          <AppearanceControls />
          <Button size="sm" variant="ghost" onClick={() => setDiagOpen(true)}>
            <Bug className="h-3.5 w-3.5" />
            Diagnostics
          </Button>
        </div>
      </header>
      {diagOpen && <DiagnosticsPanel onClose={() => setDiagOpen(false)} />}
      {dialog && (
        <ConnectionDialog
          // A fresh form for each opening.
          key={dialog.kind === "edit" ? `edit:${dialog.profile.id}:${dialog.reentry ? 1 : 0}` : "new"}
          mode={dialog}
          onClose={() => setDialog(null)}
          onSaved={onDialogSaved}
        />
      )}
      <div className="flex min-h-0 flex-1">
        <ConnectionsSidebar
          items={sidebarItems}
          activeTabId={active}
          expanded={expanded}
          onOpen={(it) => {
            if (it.profile) openProfile(it.profile);
            else if (it.tabId != null) {
              setActive(it.tabId);
              setExpanded((prev) => new Set(prev).add(it.key));
            }
          }}
          onToggle={toggleExpanded}
          onDisconnect={close}
          onNew={() => setDialog({ kind: "new" })}
          onEdit={(profile) => setDialog({ kind: "edit", profile })}
          onRename={renameProfile}
          slotRef={slotRef}
        />
        <div className="flex min-h-0 min-w-0 flex-1 flex-col">
          {tabs.map((t) => (
            <div key={t.id} className={t.id === active ? "flex min-h-0 flex-1 flex-col" : "hidden"}>
              <Workspace
                seed={t.seed}
                onSession={(s) => reportSession(t.id, s)}
                navSlot={slots[t.id] ?? null}
                onNewConnection={() => setDialog({ kind: "new" })}
                onEditProfile={(profile, reentry, reason) =>
                  // Edit the profile as saved now, not the tab's copy of it.
                  setDialog({
                    kind: "edit",
                    profile: profiles.find((p) => p.id === profile.id) ?? profile,
                    reentry,
                    reason,
                  })
                }
                onRetryProfile={(profileId) => retryTab(t.id, profileId)}
              />
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}

/** T-095 / D-031: theme (dark → light → system) and density, always to hand. */
function AppearanceControls() {
  const theme = useUiStore((s) => s.theme);
  const cycleTheme = useUiStore((s) => s.cycleTheme);
  const density = useUiStore((s) => s.density);
  const setDensity = useUiStore((s) => s.setDensity);
  const ThemeIcon = theme === "dark" ? Moon : theme === "light" ? Sun : Monitor;
  return (
    <>
      <Button size="sm" variant="ghost" onClick={cycleTheme} aria-label={`Theme: ${theme}. Switch theme`} title="Switch theme">
        <ThemeIcon className="h-3.5 w-3.5" />
        <span className="capitalize">{theme}</span>
      </Button>
      <Button
        size="sm"
        variant="ghost"
        onClick={() => setDensity(density === "compact" ? "comfortable" : "compact")}
        aria-label={`Density: ${density}. Switch density`}
        title="Switch row density"
      >
        <Rows3 className="h-3.5 w-3.5" />
        <span className="capitalize">{density}</span>
      </Button>
    </>
  )
}

/** One tab: one connection, its navigator, editor and results. */
function Workspace({
  seed,
  onSession,
  navSlot,
  onNewConnection,
  onEditProfile,
  onRetryProfile,
}: {
  seed: TabSeed;
  onSession: (s: TabSession | null) => void;
  /** The sidebar element this tab's navigator renders into, under its row. */
  navSlot: HTMLElement | null;
  onNewConnection: () => void;
  onEditProfile: (profile: ProfileMeta, reentry?: boolean, reason?: string) => void;
  /** Reconnect with the profile's current saved settings. */
  onRetryProfile: (profileId: string) => void;
}) {
  const initialQuery = useUiStore((s) => s.draftQuery);
  const [draftQuery, setDraftQuery] = useState(initialQuery);

  const core = useQuery({ queryKey: ["core-version"], queryFn: coreVersion });

  const launch = seed.kind === "launch" ? seed.outcome : null;
  const profileSeed = seed.kind === "profile" ? seed.profile : null;
  const [session, setSession] = useState<SessionInfo | null>(null);
  const [launchError, setLaunchError] = useState<string | null>(
    launch?.kind === "error" ? launch.message : null,
  );
  const [connecting, setConnecting] = useState(false);
  const started = useRef(false);

  // D-012: a launched connection connects immediately. The ref keeps React's
  // dev double-mount from connecting twice.
  const connectLaunched = useCallback(async () => {
    if (launch?.kind !== "connect") return;
    setConnecting(true);
    setLaunchError(null);
    try {
      setSession(await connectLaunch(launch.launch_id));
    } catch (e) {
      setLaunchError(String((e as { message?: string })?.message ?? e));
    } finally {
      setConnecting(false);
    }
  }, [launch]);

  // T-093: a tab opened from the sidebar connects its profile at once. On
  // failure the connect form opens on that profile, so a password can be
  // re-entered (T-071) — the one time the form is needed.
  const connectSeedProfile = useCallback(async () => {
    if (!profileSeed) return;
    setConnecting(true);
    setLaunchError(null);
    try {
      setSession(await connect(profileSeed));
    } catch (e) {
      const de = e as DriverError;
      const msg = String(de?.message ?? e);
      setLaunchError(msg);
      // T-071 / R-016: the stored password could not be used — ask for it
      // again in the connection dialog rather than leaving a dead end.
      if (de?.hints?.credential_reentry) onEditRef.current(profileSeed, true, msg);
    } finally {
      setConnecting(false);
    }
  }, [profileSeed]);
  const onEditRef = useRef(onEditProfile);
  onEditRef.current = onEditProfile;

  useEffect(() => {
    if (started.current) return;
    started.current = true;
    void connectLaunched();
    void connectSeedProfile();
  }, [connectLaunched, connectSeedProfile]);

  // T-093: tell the shell which profile this tab is on, for the sidebar.
  const onSessionRef = useRef(onSession);
  onSessionRef.current = onSession;
  useEffect(() => {
    onSessionRef.current(
      session
        ? {
            profileId: session.session_id.split("::")[0],
            state: "connected",
            engine: session.capabilities.engine as Engine,
          }
        : profileSeed && (connecting || launchError)
          ? { profileId: profileSeed.id, state: connecting ? "connecting" : "failed", engine: profileSeed.engine }
          : null,
    );
  }, [session, connecting, launchError, profileSeed]);

  // Closing the tab releases its connection.
  const sessionRef = useRef<SessionInfo | null>(null);
  sessionRef.current = session;
  useEffect(
    () => () => {
      if (sessionRef.current) void disconnect(sessionRef.current.session_id).catch(() => {});
    },
    [],
  );
  // T-041 / D-028: on an engine that can pin a connection, the editor gets
  // one for this tab's lifetime. Decided from the declared affinity, never
  // the engine name (D-003).
  const [editor, setEditor] = useState<{ id: string; status: SessionStatus } | null>(null);
  const [editorNotice, setEditorNotice] = useState<string | null>(null);
  const editorRef = useRef<string | null>(null);
  editorRef.current = editor?.id ?? null;
  const pinsSocket = session ? editorSessionPolicy(session.capabilities).pin === "socket" : false;

  const openEditorSession = useCallback(async () => {
    if (!session) return;
    try {
      const opened = await openEditor(session.session_id);
      setEditor({ id: opened.editor_id, status: opened.status });
      setEditorNotice(null);
    } catch (e) {
      setEditorNotice(`Could not open an editor session: ${String((e as { message?: string })?.message ?? e)}`);
    }
  }, [session]);

  useEffect(() => {
    if (pinsSocket && !editorRef.current) void openEditorSession();
  }, [pinsSocket, openEditorSession]);

  // The tab closing rolls the editor back — never commits (the core does it).
  useEffect(
    () => () => {
      if (editorRef.current) void closeEditor(editorRef.current).catch(() => {});
    },
    [],
  );

  // Core-initiated changes: an idle transaction was rolled back.
  useEffect(() => {
    const un = listen<EditorEvent>(EDITOR_EVENT, (ev) => {
      if (ev.payload.editor_id !== editorRef.current) return;
      setEditor((e) => (e ? { ...e, status: ev.payload.status } : e));
      setEditorNotice(ev.payload.message);
    });
    return () => {
      void un.then((f) => f());
    };
  }, []);

  const onEditorAction = useCallback(async (a: "begin" | "commit" | "rollback") => {
    const id = editorRef.current;
    if (!id) return;
    setBusy(true);
    setError(null);
    try {
      const status = await editorTransaction(id, a);
      setEditor((e) => (e ? { ...e, status } : e));
      setEditorNotice(a === "commit" ? "Committed." : a === "rollback" ? "Rolled back." : null);
    } catch (e) {
      setError(String((e as { message?: string })?.message ?? e));
    } finally {
      setBusy(false);
    }
  }, []);

  /** Loss is explicit: the old session is closed and a new one opened. */
  const onReconnect = useCallback(async () => {
    const old = editorRef.current;
    setEditor(null);
    editorRef.current = null;
    if (old) await closeEditor(old).catch(() => {});
    await openEditorSession();
    setEditorNotice("Reconnected with a new session: earlier temp tables, SET options and transactions are not carried over.");
  }, [openEditorSession]);

  const [selected, setSelected] = useState<NodeId | null>(null);
  const [view, setView] = useState<RowsView | null>(null);
  const [outcomes, setOutcomes] = useState<Outcome[] | null>(null);
  const [summary, setSummary] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  // T-056 / D-027: staged edits for the relation in view, until applied or
  // discarded. Nothing replaces the view while any are pending, so they are
  // never dropped silently.
  const [pending, setPending] = useState<Pending>(EMPTY);
  // Rows added in the grid, staged with the cell edits and applied with them.
  const [newRows, setNewRows] = useState<NewRows>(NO_NEW_ROWS);
  const [applying, setApplying] = useState(false);
  // Read through a ref so a callback created before a commit (the reload
  // after an insert) sees the pending set as it is now, not as it was.
  const heldRef = useRef(0);
  heldRef.current = totalPending(pending, newRows);
  const holdForPending = () => {
    const n = heldRef.current;
    if (n === 0) return false;
    setError(`Apply or discard ${n} pending change${n === 1 ? "" : "s"} first.`);
    return true;
  };

  // T-049: the live browse stream, and how many of its batches have been
  // applied to state but not yet rendered. Credit goes back only after render.
  const streamRef = useRef<RowStream | null>(null);
  const unretired = useRef(0);

  useEffect(() => {
    if (unretired.current > 0) {
      streamRef.current?.retire(unretired.current);
      unretired.current = 0;
    }
  }, [view]);

  // A closed tab stops its stream.
  useEffect(() => () => streamRef.current?.cancel(), []);

  // T-070 / D-005: the relation on screen, so the filter bar can re-browse it.
  const browsedNode = useRef<NavNode | null>(null);

  const onSelectRelation = useCallback(
    (node: NavNode, filter: FilterTree | null = null) => {
      if (!session) return;
      if (holdForPending()) return;
      streamRef.current?.cancel();
      unretired.current = 0;
      setSelected(node.id);
      setBusy(true);
      setError(null);
      setOutcomes(null);
      setSummary(null);
      browsedNode.current = node;
      const label = node.id.path.join(".");
      // T-009: the grid issues ONLY this structured request. It never builds
      // SQL, and it does not know what engine is on the other end.
      const request = { relation: node.id, filter, sort: [], limit: 500, cursor: null };
      const source: ExportSource = { kind: "browse", request };
      const stream = RowStream.open(
        session.session_id,
        request,
        (e) => {
          if (streamRef.current !== stream) return; // superseded
          switch (e.kind) {
            case "columns":
              setView({ columns: e.columns, rows: [], truncated: false, label, relation: node.id, edit: e.edit, source });
              break;
            case "rows":
              unretired.current += 1;
              setView((v) => (v ? { ...v, rows: v.rows.concat(e.rows) } : v));
              break;
            case "done":
              setView((v) => (v ? { ...v, truncated: e.truncated } : v));
              setBusy(false);
              break;
            case "cancelled":
              setBusy(false);
              break;
            case "timed_out":
              setError("The browse took too long and was stopped.");
              setBusy(false);
              break;
            case "error":
              setError(e.error.message);
              setBusy(false);
              break;
          }
        },
      );
      streamRef.current = stream;
    },
    [session, pending],
  );

  // T-051: the running script's id, so Stop can cancel it.
  const runningQuery = useRef<string | null>(null);

  // T-061: statements a production session held back, awaiting typed confirmation.
  const [confirmation, setConfirmation] = useState<ConfirmationRequest | null>(null);

  const onRun = useCallback(async (confirm?: string) => {
    if (!session) return;
    if (holdForPending()) return;
    setBusy(true);
    setError(null);
    setConfirmation(null);
    const queryId = crypto.randomUUID();
    runningQuery.current = queryId;
    try {
      let run;
      if (pinsSocket) {
        // T-041: a pinned editor never falls back to a pooled connection —
        // that would run the user's statements outside their session.
        const id = editorRef.current;
        if (!id || editor?.status.lost) {
          setError("The editor has no live session. Reconnect to continue.");
          return;
        }
        try {
          const r = await runInEditor(id, draftQuery, queryId, confirm);
          setEditor((e) => (e ? { ...e, status: r.status } : e));
          run = r.run;
        } catch (e) {
          const err = e as { kind?: string; message?: string };
          if (err.kind === "connection_failed") {
            setEditor((ed) => (ed ? { ...ed, status: { ...ed.status, lost: String(err.message ?? e) } } : ed));
          }
          throw e;
        }
      } else {
        run = await runScript(session.session_id, draftQuery, queryId, confirm);
      }
      setOutcomes(run.result.outcomes);
      setSummary(run.summary);
      const first = run.result.outcomes.find((o) => o.kind === "rows");
      if (first && first.kind === "rows") {
        setView({
          columns: first.columns,
          rows: first.rows,
          truncated: first.truncated,
          label: "query result",
          // T-074: export re-runs the query read-only on a pooled connection.
          // Only a lone statement can be re-run without trusting the
          // display-only splitter (R-040), and a pinned session's result can
          // depend on state (an open transaction, temp tables) a fresh
          // connection would not see — so those say why instead.
          source: pinsSocket
            ? { kind: "unavailable", reason: "the result came from a pinned editor session" }
            : run.result.outcomes.length !== 1
              ? { kind: "unavailable", reason: "run the statement on its own to export it" }
              : { kind: "query", script: draftQuery },
        });
      }
    } catch (e) {
      const held = (e as DriverError)?.hints?.confirmation;
      if (held) setConfirmation(held);
      else setError(String((e as { message?: string })?.message ?? e));
    } finally {
      runningQuery.current = null;
      setBusy(false);
      // T-035: the core recorded it (or not, per the profile's policy).
      invalidateHistory();
    }
  }, [session, draftQuery, pending, pinsSocket, editor]);

  // T-057 / R-003: Apply opens the core's preview of exactly what will run;
  // only Confirm executes it, and only that previewed plan (by digest).
  const [review, setReview] = useState<{ edits: EditRequest[]; plan: EditPlan; pending: Pending } | null>(null);

  const onApply = useCallback(async () => {
    if (!session || !view?.relation || !view.edit) return;
    const built = buildEditRequests(pending, view.relation, view.columns, view.rows, view.edit, newRows);
    if (!built.ok) {
      setError(built.reason);
      return;
    }
    setApplying(true);
    setError(null);
    try {
      const plan = await previewEdits(session.session_id, built.edits);
      setReview({ edits: built.edits, plan, pending });
    } catch (e) {
      setError(`${String((e as { message?: string })?.message ?? e)} — nothing was applied; your changes are still pending.`);
    } finally {
      setApplying(false);
    }
  }, [session, view, pending, newRows]);

  const onConfirm = useCallback(async () => {
    if (!session || !review) return;
    const { edits, plan, pending: applied } = review;
    setApplying(true);
    setError(null);
    try {
      const run = await applyEdits(session.session_id, edits, plan.digest);
      setReview(null);
      if (run.result.tx.disposition.state === "committed") {
        // There is no undo past this point (D-027): the staged values are
        // now the rows, and the pending set is gone.
        setView((v) => (v ? { ...v, rows: applyToRows(v.rows, applied) } : v));
        setPending(EMPTY);
        setNewRows(NO_NEW_ROWS);
        const inserted = edits.filter((e) => e.op === "insert").length;
        const updated = edits.length - inserted;
        const plural = (n: number, verb: string) => `${n} row${n === 1 ? "" : "s"} ${verb}`;
        const done = [updated > 0 && plural(updated, "updated"), inserted > 0 && plural(inserted, "inserted")]
          .filter(Boolean)
          .join(", ");
        if (inserted > 0 && browsedNode.current) {
          // New rows take values only the database knows (defaults, serials,
          // generated columns), so the relation is read again to show them.
          heldRef.current = 0;
          onSelectRelation(browsedNode.current, view?.source?.kind === "browse" ? view.source.request.filter : null);
        }
        setSummary(done);
      } else {
        // Rolled back: nothing was written, so the changes stay staged.
        const failed = run.result.outcomes.find((o) => o.kind === "error");
        setError(
          `${failed && failed.kind === "error" ? failed.error.message : run.summary} — nothing was applied; your changes are still pending.`,
        );
      }
    } catch (e) {
      setReview(null);
      const err = e as { kind?: string; message?: string };
      // A refusal (including a plan that changed since preview) comes back
      // before the transaction opens. A timeout or a dropped connection does
      // not say whether COMMIT reached the server, so claiming "nothing was
      // applied" there could be false.
      const unknown = err.kind === "timeout" || err.kind === "connection_failed" || err.kind === "driver_bug";
      setError(
        unknown
          ? `${String(err.message ?? e)} — whether the changes were applied is unknown; reload the relation to check before applying again.`
          : `${String(err.message ?? e)} — nothing was applied; your changes are still pending.`,
      );
    } finally {
      setApplying(false);
    }
  }, [session, review, view, onSelectRelation]);

  const onStop = useCallback(() => {
    if (runningQuery.current) void cancelQuery(runningQuery.current);
  }, []);

  const caps = session?.capabilities;
  const production = session?.guardrails.production.armed ?? false;
  const relationLabel = view?.relation ? view.relation.path.join(" / ") : view ? view.label : null;

  return (
    <>
      {/* D-032: a production session carries a persistent red rule. */}
      {production && <div aria-hidden className="h-0.5 shrink-0 bg-destructive" />}

      {/* T-095: the session toolbar — what this tab is connected to, its
          guardrails as status pills (D-032), and the primary action. */}
      <div className="flex h-10 shrink-0 items-center gap-2 border-b bg-background px-3">
        {session ? (
          <span className="flex min-w-0 items-center gap-1.5 text-xs text-muted-foreground">
            <Database aria-hidden className="h-3.5 w-3.5 shrink-0" />
            <span className="truncate font-mono">
              {session.guardrails.provenance === "manual" ? "" : `${PROVENANCE_LABEL[session.guardrails.provenance]} · `}
              {relationLabel ? <span className="text-foreground">{relationLabel}</span> : "no relation selected"}
            </span>
          </span>
        ) : (
          <span className="text-xs text-muted-foreground">{connecting ? "Connecting…" : "Not connected"}</span>
        )}
        <div className="ml-auto flex min-w-0 items-center gap-1.5">
          {session && <GuardrailPills guardrails={session.guardrails} tls={!!session.tls_warning} />}
          {busy && runningQuery.current ? (
            <Button size="sm" variant="secondary" onClick={onStop}>
              <Square className="h-3.5 w-3.5" />
              Stop
            </Button>
          ) : (
            <Button size="sm" onClick={() => void onRun()} disabled={!session || busy} title="Run (⌘↵)">
              <Play className="h-3.5 w-3.5" />
              Run
            </Button>
          )}
        </div>
      </div>
      {/* D-031 §6: the one looping motion, while a query runs. */}
      {busy && (
        <div className="scan-line shrink-0" role="progressbar" aria-label="Running">
          <i />
        </div>
      )}

      {/* D-007: an opt-out from TLS verification must be visible wherever the
          connection is. D-032: it is a caution, not a danger. */}
      {session?.tls_warning && <StatusBanner tone="caution">{session.tls_warning}</StatusBanner>}
      {/* T-044 / R-038: a raised budget is a deliberate choice, said plainly. */}
      {session?.budget_warning && <StatusBanner tone="caution">{session.budget_warning}</StatusBanner>}

      {session && (
        <GuardrailBar
          guardrails={session.guardrails}
          origin={launch?.kind === "connect" ? launch : null}
          onSave={async (label) => {
            const saved = await saveSessionAsProfile(session.session_id, label);
            setSession({ ...session, guardrails: saved.guardrails });
            void queryClient.invalidateQueries({ queryKey: WORKSPACE_KEY });
            return { label: saved.profile.label, notice: saved.notice ?? null };
          }}
        />
      )}

      <div className="flex min-h-0 flex-1">
        {/* The navigator lives in the sidebar, under this connection's row. */}
        {session &&
          navSlot &&
          createPortal(
            <div className="py-1 pr-1">
              <CatalogNote catalog={session.capabilities.catalog} />
              <Navigator
                sessionId={session.session_id}
                capabilities={session.capabilities}
                selected={selected}
                onSelect={(node) => onSelectRelation(node)}
              />
            </div>,
            navSlot,
          )}

        <main className="flex min-h-0 min-w-0 flex-1 flex-col">
          {confirmation && (
            <ConfirmPanel
              request={confirmation}
              onConfirm={(phrase) => void onRun(phrase)}
              onCancel={() => setConfirmation(null)}
            />
          )}

          {error ? (
            <StatusBanner tone="danger">{error}</StatusBanner>
          ) : (
            summary && (
              <div role="status" className="border-b px-3 py-1.5 font-mono text-xs text-muted-foreground">
                {summary}
              </div>
            )
          )}

          {outcomes && outcomes.length > 1 && <OutcomeStrip outcomes={outcomes} />}

          {session && view?.relation && (
            <FilterBar
              // A new relation starts with an empty bar.
              key={view.relation.path.join(".")}
              columns={view.columns}
              ops={session.capabilities.filter_ops}
              onApply={(f) => browsedNode.current && onSelectRelation(browsedNode.current, f)}
            />
          )}

          <section className="min-h-0 flex-1">
            {session && view ? (
              <ResultGrid
                sessionId={session.session_id}
                columns={view.columns}
                rows={view.rows}
                truncated={view.truncated}
                layout={
                  // T-063 / D-018: declared, never inferred from the engine
                  // name (D-003). Until T-064 supplies a dedicated sample,
                  // the page in view is the sample.
                  session.capabilities.column_inference?.mode === "inferred" && view.relation
                    ? {
                        key: `${session.session_id}::${pathKey(view.relation.path)}`,
                        infer: () =>
                          inferColumns(rowsAsDocuments(view.columns, view.rows), null, {
                            sample_limit:
                              session.capabilities.column_inference?.mode === "inferred"
                                ? session.capabilities.column_inference.sample_limit
                                : 1000,
                            max_depth: 1,
                          }),
                      }
                    : undefined
                }
                exporting={
                  view.source
                    ? view.source.kind === "unavailable"
                      ? { available: false, reason: view.source.reason }
                      : {
                          available: true,
                          run: (format, queryId) =>
                            exportResult(session.session_id, view.source as ExportSource, format, view.label, queryId),
                        }
                    : undefined
                }
                editing={
                  view.edit
                    ? {
                        gate: view.edit,
                        pending,
                        // Staging is frozen while a preview is open, so what
                        // runs is what was reviewed.
                        applying: applying || review !== null,
                        onStage: (r, c, v) => setPending((p) => stage(p, r, c, v, view.rows[r][c])),
                        onUnstage: (r, c) => setPending((p) => unstage(p, r, c)),
                        onApply,
                        onDiscard: () => {
                          setPending(EMPTY);
                          setNewRows(NO_NEW_ROWS);
                          setError(null);
                        },
                        newRows,
                        onAddRow: () => setNewRows((n) => addNewRow(n)),
                        onStageNew: (r, c, v) => setNewRows((n) => setNewCell(n, r, c, v)),
                        onUnstageNew: (r, c) => setNewRows((n) => clearNewCell(n, r, c)),
                        onRemoveNew: (r) => setNewRows((n) => removeNewRow(n, r)),
                      }
                    : undefined
                }
              />
            ) : (
              <EmptyState
                connected={!!session}
                onNewConnection={onNewConnection}
                notice={
                  session ? null : (
                    <>
                      <LaunchBanner
                        launch={launch}
                        connecting={connecting}
                        error={launchError}
                        onRetry={launch?.kind === "connect" ? connectLaunched : undefined}
                      />
                      {profileSeed && (connecting || launchError) && (
                        <div className="space-y-2 text-sm" role={launchError ? "alert" : "status"}>
                          <p>
                            {connecting ? "Connecting to " : "Could not connect to "}
                            <span className="font-medium">{profileSeed.label}</span>
                            {connecting && "…"}
                          </p>
                          {launchError && <p className="text-xs text-destructive">{launchError}</p>}
                          {launchError && (
                            <div className="flex justify-center gap-2">
                              <Button size="sm" variant="secondary" onClick={() => onRetryProfile(profileSeed.id)}>
                                Retry
                              </Button>
                              <Button size="sm" variant="secondary" onClick={() => onEditProfile(profileSeed)}>
                                Edit connection
                              </Button>
                            </div>
                          )}
                        </div>
                      )}
                    </>
                  )
                }
              />
            )}
          </section>

          {review && (
            <EditPreview
              plan={review.plan}
              applying={applying}
              onConfirm={onConfirm}
              onBack={() => setReview(null)}
              compareRendered={review.edits.some((e) => e.compare_rendered)}
              onCompareRendered={async (on) => {
                // T-060: re-preview; apply then runs exactly this plan.
                const edits = review.edits.map((e) => ({ ...e, compare_rendered: on }));
                try {
                  const plan = await previewEdits(session!.session_id, edits);
                  setReview({ ...review, edits, plan });
                } catch (e2) {
                  setError(String((e2 as { message?: string })?.message ?? e2));
                }
              }}
            />
          )}

          {/* T-095 / D-031: the editor docks below the results, like the mock. */}
          <section aria-label="Query editor" className="flex h-56 shrink-0 flex-col border-t border-border-strong bg-panel">
            {editor && (
              <EditorSessionBar
                status={editor.status}
                busy={busy}
                notice={editorNotice}
                onAction={onEditorAction}
                onReconnect={onReconnect}
              />
            )}
            {!editor && editorNotice && <StatusBanner tone="danger">{editorNotice}</StatusBanner>}
            <SavedQueries
              sql={draftQuery}
              profileId={session ? session.session_id.split("::")[0] : null}
              onLoad={setDraftQuery}
            />
            <div className="min-h-0 flex-1">
              <QueryEditor value={draftQuery} onChange={setDraftQuery} onRun={() => void onRun()} />
            </div>
          </section>
        </main>
      </div>

      {/* T-095: the status bar — engine, budget, collation, core. */}
      <footer className="flex h-6 shrink-0 items-center gap-4 border-t bg-background px-3 font-mono text-2xs text-muted-foreground">
        {caps ? (
          <span className="text-foreground">
            {caps.engine}
            {/* T-054: the parsed flavour and the product's own version. */}
            {caps.flavour && caps.flavour !== caps.engine ? ` · ${caps.flavour}` : ""}
            {caps.server_version ? ` ${caps.server_version}` : ""}
          </span>
        ) : (
          <span>not connected</span>
        )}
        {session && <ConnectionUsageIndicator sessionId={session.session_id} />}
        {caps?.string_collation && <span>collation {caps.string_collation}</span>}
        {busy && <span className="motion-reduce-label text-primary">Running…</span>}
        <span className="ml-auto">{core.isPending ? "starting…" : core.isError ? "core unreachable" : core.data}</span>
      </footer>
    </>
  );
}

/** T-095: what an empty results area says, quietly. */
function EmptyState({
  connected,
  onNewConnection,
  notice,
}: {
  connected: boolean;
  onNewConnection: () => void;
  notice?: React.ReactNode;
}) {
  return (
    <div className="flex h-full items-center justify-center p-6">
      <div className="max-w-md space-y-3 text-center">
        <span aria-hidden className="mx-auto block h-3 w-3 rotate-45 border-[1.5px] border-primary/60" />
        {notice}
        <p className="text-sm text-muted-foreground">
          {connected
            ? "Pick a relation in the navigator, or run a query."
            : "Pick a connection on the left, or create one."}
        </p>
        {!connected && (
          <Button size="sm" onClick={onNewConnection}>
            <Plus className="h-3.5 w-3.5" />
            New connection
          </Button>
        )}
      </div>
    </div>
  );
}

/**
 * T-023 / D-020: "copy diagnostics". The core builds the text; this shows all
 * of it, and Copy copies that same string — nothing is sent anywhere, and
 * nothing is copied that was not on screen.
 */
function DiagnosticsPanel({ onClose }: { onClose: () => void }) {
  const [text, setText] = useState<string | null>(null);
  const [verbose, setVerbose] = useState(false);
  const [copied, setCopied] = useState(false);

  const refresh = useCallback(async () => {
    setCopied(false);
    setVerbose(await verboseDiagnostics().catch(() => false));
    setText(await diagnosticsPreview().catch((e) => `diagnostics unavailable: ${String(e)}`));
  }, []);
  useEffect(() => void refresh(), [refresh]);

  const copy = async () => {
    if (text === null) return;
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      // Older webviews: select-and-copy from a hidden textarea.
      const ta = document.createElement("textarea");
      ta.value = text;
      document.body.appendChild(ta);
      ta.select();
      document.execCommand("copy");
      ta.remove();
    }
    setCopied(true);
  };

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-overlay/60" role="dialog" aria-label="Diagnostics">
      <div className="flex max-h-[85vh] w-[46rem] flex-col gap-2 rounded border border-border-strong bg-raised p-3 text-xs shadow-float">
        <div className="flex items-center gap-2">
          <span className="text-sm font-semibold">Diagnostics</span>
          <button type="button" className="focus-glow ml-auto rounded border border-transparent p-0.5 text-muted-foreground hover:text-foreground" aria-label="Close" onClick={onClose}>
            <X className="h-4 w-4" />
          </button>
        </div>
        <label className="flex items-start gap-2">
          <input
            type="checkbox"
            checked={verbose}
            onChange={async (e) => {
              await setVerboseDiagnostics(e.target.checked);
              await refresh();
            }}
          />
          <span>
            <span className="font-medium">Verbose diagnostics</span> — also log full query text, for this run
            only. Query text can contain sensitive values, including passwords in statements like{" "}
            <code>CREATE ROLE … PASSWORD</code>.
          </span>
        </label>
        <p className="text-muted-foreground">
          This is everything that will be copied. It stays on this machine unless you paste it somewhere.
        </p>
        <pre className="min-h-0 flex-1 overflow-auto rounded border bg-background p-2 font-mono text-2xs whitespace-pre-wrap">
          {text ?? "loading…"}
        </pre>
        <div className="flex items-center gap-2">
          <Button size="sm" variant="secondary" onClick={() => void refresh()}>
            Refresh
          </Button>
          <Button size="sm" onClick={() => void copy()} disabled={text === null}>
            Copy diagnostics
          </Button>
          {copied && <span className="text-muted-foreground">Copied.</span>}
        </div>
      </div>
    </div>
  );
}

const PROVENANCE_LABEL: Record<Guardrails["provenance"], string> = {
  manual: "Entered manually",
  discovered: "Discovered",
  evaluated: "From project config",
  command_line: "From command line",
};

/**
 * T-027 / R-002: the session's guardrails, always visible. A discovered or
 * command-line session says it is unsaved and offers to become a profile; the
 * production guard says *why* it is armed; history and safety mode are stated
 * rather than implied.
 */
/**
 * T-027 / R-002 + T-095: the session's guardrails as status pills in the
 * toolbar. D-032 decides the hue: production is danger; read-only the
 * database enforces is verified; read-only asked for but not enforced, and a
 * TLS opt-out, are caution. History and write access are neutral facts.
 */
function GuardrailPills({ guardrails: g, tls }: { guardrails: Guardrails; tls: boolean }) {
  return (
    <div className="flex min-w-0 items-center gap-1.5 overflow-hidden">
      {g.production.armed && (
        <StatusPill tone="danger" title={`Production confirmation armed:\n${g.production.reasons.join("\n")}`}>
          Production
        </StatusPill>
      )}
      <SafetyBadge mode={g.safety_mode} />
      {tls && <StatusPill tone="caution">TLS unverified</StatusPill>}
      <StatusPill tone="neutral" icon={false} title="Query history for this connection">
        history {g.history}
      </StatusPill>
    </div>
  );
}

/**
 * T-027: where a launched connection came from, and saving it as a profile.
 * Shown only while there is something to say — a saved, hand-entered
 * connection needs no row.
 */
function GuardrailBar({
  guardrails: g,
  origin,
  onSave,
}: {
  guardrails: Guardrails;
  origin: Extract<LaunchOutcome, { kind: "connect" }> | null;
  onSave: (label: string) => Promise<{ label: string; notice: string | null }>;
}) {
  const [savedAs, setSavedAs] = useState<{ label: string; notice: string | null } | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const t = origin?.target;
  const source =
    origin?.origin.kind === "discovered"
      ? ` · ${origin.origin.source} in ${origin.origin.dir}`
      : origin?.origin.kind === "evaluated"
        ? ` · ${origin.origin.config}`
        : "";
  const unsaved = !g.saved && g.provenance !== "manual";
  if (!t && !unsaved && !savedAs && !err) return null;

  return (
    <div className="flex flex-wrap items-center gap-2 border-b bg-panel px-3 py-1.5 text-xs">
      <span className="font-medium">
        {PROVENANCE_LABEL[g.provenance]}
        {unsaved ? " · not saved" : ""}
      </span>
      {t && (
        <span className="font-mono text-muted-foreground">
          {t.user}@{t.host}:{t.port}/{t.database}
          {source}
        </span>
      )}
      {unsaved && (
        <Button
          size="sm"
          variant="secondary"
          className="ml-auto h-6"
          disabled={busy}
          onClick={async () => {
            setBusy(true);
            setErr(null);
            try {
              setSavedAs(await onSave(t ? `${t.database}@${t.host}` : "Saved connection"));
            } catch (e) {
              setErr(String((e as { message?: string })?.message ?? e));
            } finally {
              setBusy(false);
            }
          }}
        >
          <Save className="h-3 w-3" />
          Save as profile
        </Button>
      )}
      {savedAs && (
        <span className="ml-auto text-muted-foreground">
          Saved as “{savedAs.label}”{savedAs.notice ? ` — ${savedAs.notice}` : ""}
        </span>
      )}
      {err && <span className="text-destructive">{err}</span>}
    </div>
  );
}

/**
 * T-055 / R-028: say at connect, not mid-navigation, when introspection on
 * this server will be partial or its flavour is unverified. D-032: a note.
 */
function CatalogNote({ catalog }: { catalog: CatalogSupport | undefined }) {
  if (!catalog) return null;
  const missing = catalog.features.filter((f) => !f.available).map((f) => f.label);
  if (catalog.flavour_verified && missing.length === 0) return null;
  return (
    <p className="mb-1 rounded border border-info/40 bg-info/10 px-2 py-1 text-2xs text-info" role="note">
      {!catalog.flavour_verified && "Unrecognized server: treated as vanilla, unverified. "}
      {missing.length > 0 ? `Not available on this server: ${missing.join(", ")}.` : "Every catalog probe succeeded."}
    </p>
  );
}

/**
 * T-061 / D-027: read-only is shown as what the *database* enforces, beside
 * what the profile asked for. Asked-for but not enforced is the case that
 * must be loud — the app does not close that gap, the database role does.
 */
function SafetyBadge({ mode }: { mode: Guardrails["safety_mode"] }) {
  const db = mode.database;
  if (db.state === "enforced") {
    return (
      <StatusPill tone="verified" title={db.basis}>
        Read-only enforced
      </StatusPill>
    );
  }
  if (mode.requested) {
    const why = db.state === "not_enforced" ? db.reasons.join("\n") : db.reason;
    return (
      <StatusPill tone="caution" title={`Read-only requested but NOT enforced by the database:\n${why}`}>
        Read-only not enforced
      </StatusPill>
    );
  }
  return (
    <StatusPill tone="neutral" icon={false} title={db.state === "not_enforced" ? db.reasons.join("\n") : db.reason}>
      {db.state === "not_enforced" ? "writable" : "write access unknown"}
    </StatusPill>
  );
}

/**
 * What a launch resolved to, shown until the tab is connected (T-026). A
 * config that failed (R-032) or a declined consent (D-026) explains itself
 * here, above the connect form it degraded to.
 */
function LaunchBanner({
  launch,
  connecting,
  error,
  onRetry,
}: {
  launch: LaunchOutcome | null;
  connecting: boolean;
  error: string | null;
  onRetry?: () => void;
}) {
  if (!launch) return null;
  const target =
    launch.kind === "connect"
      ? `${launch.target.scheme}://${launch.target.user}@${launch.target.host}:${launch.target.port}/${launch.target.database}`
      : null;
  return (
    <div className="space-y-1.5 border-b p-3 text-xs">
      {target && (
        <p>
          {connecting ? "Connecting to " : "Launch target "}
          <span className="font-mono">{target}</span>
        </p>
      )}
      {launch.kind === "choose" && <p className="text-muted-foreground">{launch.reason}</p>}
      {error && <p className="text-destructive">{error}</p>}
      {error && onRetry && (
        <Button size="sm" variant="secondary" onClick={onRetry}>
          Retry
        </Button>
      )}
    </div>
  );
}

/**
 * The outcome sequence (T-010/D-029). A script that fails at statement three
 * shows two successes and one error, not one global failure.
 */
function OutcomeStrip({ outcomes }: { outcomes: Outcome[] }) {
  return (
    <div className="flex flex-wrap gap-1 border-b px-3 py-1.5 font-mono text-2xs">
      {outcomes.map((o, i) => (
        <span
          key={i}
          className={
            // D-032: a failed statement is danger; the rest are neutral facts.
            o.kind === "error"
              ? "rounded-sm border border-destructive/60 bg-destructive/10 px-1.5 py-0.5 text-destructive"
              : "rounded-sm border border-border-strong bg-raised px-1.5 py-0.5 text-muted-foreground"
          }
          title={
            // T-011: only offer a position when attribution is a real span.
            o.attribution.how === "span"
              ? `characters ${o.attribution.start_utf16}–${o.attribution.end_utf16}`
              : "position unknown (splitter disagreed with the engine)"
          }
        >
          {i + 1}.{" "}
          {o.kind === "rows"
            ? `${o.rows.length} rows`
            : o.kind === "affected"
              ? `${o.count} affected`
              : o.kind === "status"
                ? o.message
                : o.error.message}
        </span>
      ))}
    </div>
  );
}

import { useCallback, useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { useQuery } from "@tanstack/react-query";
import { AlertTriangle, Bug, Database, Play, Save, ShieldAlert, Square, Sun, Moon, Plug, Plus, X } from "lucide-react";
import { Navigator } from "./components/Navigator";
import { QueryEditor } from "./components/QueryEditor";
import { EditPreview } from "./components/EditPreview";
import { EditorSessionBar } from "./components/EditorSessionBar";
import { editorSessionPolicy } from "./lib/sessionPolicy";
import { ResultGrid } from "./components/ResultGrid";
import {
  EMPTY,
  applyToRows,
  buildEditRequests,
  pendingCount,
  stage,
  unstage,
  type Pending,
} from "./lib/pendingEdits";
import { Button } from "./components/ui/button";
import type {
  ColumnDesc,
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
  deleteProfile,
  importConnectionUrl,
  loadWorkspace,
  queryClient,
  saveProfile,
  WORKSPACE_KEY,
  diagnosticsPreview,
  disconnect,
  runScript,
  saveSessionAsProfile,
  setVerboseDiagnostics,
  storeCredential,
  takeLaunches,
  verboseDiagnostics,
} from "./lib/ipc";
import { useUiStore } from "./store/ui";
import { cn } from "./lib/utils";
import { SavedProfiles, newProfileId } from "./components/SavedProfiles";
import { SavedQueries } from "./components/SavedQueries";
import { FilterBar } from "./components/FilterBar";
import { invalidateHistory } from "./components/HistoryPanel";

interface RowsView {
  columns: ColumnDesc[];
  rows: WireValue[][];
  truncated: boolean;
  label: string;
  /** T-056: set for a structured browse of one relation; raw results have none. */
  relation?: NodeId;
  edit?: EditGate;
}

/** What a tab was opened for. */
type TabSeed = { kind: "blank" } | { kind: "launch"; outcome: LaunchOutcome };

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
export function App() {
  const theme = useUiStore((s) => s.theme);
  const [tabs, setTabs] = useState<Tab[]>([{ id: 0, seed: { kind: "blank" }, title: "New connection" }]);
  const [active, setActive] = useState(0);
  const nextId = useRef(1);
  const [diagOpen, setDiagOpen] = useState(false);

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

  return (
    <div className={theme === "dark" ? "dark" : undefined}>
      <div className="flex h-screen flex-col bg-background text-foreground">
        <nav className="flex items-center gap-0.5 border-b bg-muted/40 px-1 pt-1 text-xs" role="tablist">
          {tabs.map((t) => (
            <div
              key={t.id}
              role="tab"
              aria-selected={t.id === active}
              className={
                "flex items-center gap-1 rounded-t border border-b-0 px-2 py-1 " +
                (t.id === active ? "bg-background" : "border-transparent text-muted-foreground")
              }
            >
              <button type="button" onClick={() => setActive(t.id)} className="max-w-48 truncate">
                {t.title}
              </button>
              <button type="button" aria-label={`Close ${t.title}`} onClick={() => close(t.id)}>
                <X className="h-3 w-3" />
              </button>
            </div>
          ))}
          <button type="button" aria-label="New tab" className="px-1.5 py-1" onClick={addBlank}>
            <Plus className="h-3.5 w-3.5" />
          </button>
          <button
            type="button"
            className="ml-auto flex items-center gap-1 px-1.5 py-1 text-muted-foreground"
            onClick={() => setDiagOpen(true)}
          >
            <Bug className="h-3.5 w-3.5" />
            Diagnostics
          </button>
        </nav>
        {diagOpen && <DiagnosticsPanel onClose={() => setDiagOpen(false)} />}
        {tabs.map((t) => (
          <div key={t.id} className={t.id === active ? "flex min-h-0 flex-1 flex-col" : "hidden"}>
            <Workspace seed={t.seed} />
          </div>
        ))}
      </div>
    </div>
  );
}

/** One tab: one connection, its navigator, editor and results. */
function Workspace({ seed }: { seed: TabSeed }) {
  const theme = useUiStore((s) => s.theme);
  const toggleTheme = useUiStore((s) => s.toggleTheme);
  const initialQuery = useUiStore((s) => s.draftQuery);
  const [draftQuery, setDraftQuery] = useState(initialQuery);

  const core = useQuery({ queryKey: ["core-version"], queryFn: coreVersion });

  const launch = seed.kind === "launch" ? seed.outcome : null;
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

  useEffect(() => {
    if (started.current) return;
    started.current = true;
    void connectLaunched();
  }, [connectLaunched]);

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
  const [applying, setApplying] = useState(false);
  const holdForPending = () => {
    const n = pendingCount(pending);
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
      const stream = RowStream.open(
        session.session_id,
        { relation: node.id, filter, sort: [], limit: 500, cursor: null },
        (e) => {
          if (streamRef.current !== stream) return; // superseded
          switch (e.kind) {
            case "columns":
              setView({ columns: e.columns, rows: [], truncated: false, label, relation: node.id, edit: e.edit });
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

  const onRun = useCallback(async () => {
    if (!session) return;
    if (holdForPending()) return;
    setBusy(true);
    setError(null);
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
          const r = await runInEditor(id, draftQuery, queryId);
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
        run = await runScript(session.session_id, draftQuery, queryId);
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
        });
      }
    } catch (e) {
      setError(String((e as { message?: string })?.message ?? e));
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
    const built = buildEditRequests(pending, view.relation, view.columns, view.rows, view.edit);
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
  }, [session, view, pending]);

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
        setSummary(`${edits.length} row${edits.length === 1 ? "" : "s"} updated`);
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
  }, [session, review]);

  const onStop = useCallback(() => {
    if (runningQuery.current) void cancelQuery(runningQuery.current);
  }, []);

  return (
    <>
        <header className="flex items-center gap-3 border-b px-3 py-2">
          <Database className="h-4 w-4 text-primary" />
          <span className="text-sm font-semibold">Sluice</span>
          <span className="text-xs text-muted-foreground">
            {core.isPending ? "starting…" : core.isError ? "core unreachable" : core.data}
          </span>
          {session && (
            <span className="rounded bg-secondary px-1.5 py-0.5 text-xs">
              {session.capabilities.engine}
              {/* T-054: the parsed flavour and the product's own version. */}
              {session.capabilities.flavour && session.capabilities.flavour !== session.capabilities.engine
                ? ` · ${session.capabilities.flavour}`
                : ""}
              {session.capabilities.server_version ? ` ${session.capabilities.server_version}` : ""}
            </span>
          )}
          <div className="ml-auto flex items-center gap-2">
            {busy && runningQuery.current ? (
              <Button size="sm" variant="secondary" onClick={onStop}>
                <Square className="h-3.5 w-3.5" />
                Stop
              </Button>
            ) : (
              <Button size="sm" variant="secondary" onClick={onRun} disabled={!session || busy}>
                <Play className="h-3.5 w-3.5" />
                Run
              </Button>
            )}
            <Button size="icon" variant="ghost" onClick={toggleTheme} aria-label="Toggle theme">
              {theme === "dark" ? <Sun className="h-4 w-4" /> : <Moon className="h-4 w-4" />}
            </Button>
          </div>
        </header>

        {/* D-007: an opt-out from TLS verification must be visible wherever the
            connection is. The text comes from the core with the session. */}
        {session?.tls_warning && (
          <div className="flex items-center gap-2 border-b bg-destructive px-3 py-1 text-xs text-destructive-foreground">
            <AlertTriangle className="h-3.5 w-3.5 shrink-0" />
            <span>{session.tls_warning}</span>
          </div>
        )}

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
          <aside className="flex w-64 shrink-0 flex-col border-r">
            {session ? (
              <div className="min-h-0 flex-1 overflow-auto p-1">
                <Navigator
                  sessionId={session.session_id}
                  capabilities={session.capabilities}
                  selected={selected}
                  onSelect={(node) => onSelectRelation(node)}
                />
              </div>
            ) : (
              <>
                <LaunchBanner
                  launch={launch}
                  connecting={connecting}
                  error={launchError}
                  onRetry={launch?.kind === "connect" ? connectLaunched : undefined}
                />
                {!connecting && <ConnectPanel onConnected={setSession} />}
              </>
            )}
          </aside>

          <main className="flex min-h-0 min-w-0 flex-1 flex-col">
            {editor && (
              <EditorSessionBar
                status={editor.status}
                busy={busy}
                notice={editorNotice}
                onAction={onEditorAction}
                onReconnect={onReconnect}
              />
            )}
            {!editor && editorNotice && (
              <div className="border-b px-3 py-1 text-xs text-destructive">{editorNotice}</div>
            )}
            <SavedQueries
              sql={draftQuery}
              profileId={session ? session.session_id.split("::")[0] : null}
              onLoad={setDraftQuery}
            />
            <section className="h-40 shrink-0 border-b">
              <QueryEditor value={draftQuery} onChange={setDraftQuery} onRun={onRun} />
            </section>

            {(summary || error) && (
              <div
                className={
                  error
                    ? "border-b bg-destructive px-3 py-1 text-xs text-destructive-foreground"
                    : "border-b px-3 py-1 text-xs text-muted-foreground"
                }
              >
                {error ?? summary}
              </div>
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
                            setError(null);
                          },
                        }
                      : undefined
                  }
                />
              ) : (
                <div className="p-6 text-sm text-muted-foreground">
                  {session ? "Pick a relation, or run a query." : "Connect to a database to start."}
                </div>
              )}
            </section>

            {review && (
              <EditPreview
                plan={review.plan}
                applying={applying}
                onConfirm={onConfirm}
                onBack={() => setReview(null)}
              />
            )}
          </main>
        </div>
    </>
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
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/40" role="dialog" aria-label="Diagnostics">
      <div className="flex max-h-[85vh] w-[46rem] flex-col gap-2 rounded border bg-background p-3 text-xs">
        <div className="flex items-center gap-2">
          <span className="text-sm font-semibold">Diagnostics</span>
          <button type="button" className="ml-auto" aria-label="Close" onClick={onClose}>
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
        <pre className="min-h-0 flex-1 overflow-auto rounded border bg-muted/40 p-2 font-mono text-[11px] whitespace-pre-wrap">
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

  return (
    <div className="flex flex-wrap items-center gap-2 border-b px-3 py-1 text-xs">
      <span className="font-medium">
        {PROVENANCE_LABEL[g.provenance]}
        {g.saved ? "" : g.provenance === "manual" ? "" : " · not saved"}
      </span>
      {t && (
        <span className="font-mono text-muted-foreground">
          {t.user}@{t.host}:{t.port}/{t.database}
          {source}
        </span>
      )}
      <span className="rounded bg-secondary px-1.5 py-0.5">history {g.history}</span>
      <span className="rounded bg-secondary px-1.5 py-0.5" title="Whether the database itself enforces read-only access is not known yet.">
        safety mode: {g.safety_mode}
      </span>
      {g.production.armed && (
        <span
          className="flex items-center gap-1 rounded bg-destructive px-1.5 py-0.5 text-destructive-foreground"
          title={g.production.reasons.join("\n")}
        >
          <ShieldAlert className="h-3 w-3" />
          production confirmation armed: {g.production.reasons[0]}
        </span>
      )}
      {!g.saved && g.provenance !== "manual" && (
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
    <div className="space-y-1 border-b p-2 text-xs">
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
    <div className="flex flex-wrap gap-1 border-b px-3 py-1 text-xs">
      {outcomes.map((o, i) => (
        <span
          key={i}
          className={
            o.kind === "error"
              ? "rounded bg-destructive px-1.5 py-0.5 text-destructive-foreground"
              : "rounded bg-secondary px-1.5 py-0.5"
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

/**
 * D-007: the password is sent inward to be stored in the keychain and is never
 * read back. The webview keeps the profile id and metadata only.
 */
function ConnectPanel({ onConnected }: { onConnected: (s: SessionInfo) => void }) {
  const [form, setForm] = useState<ProfileMeta>({
    id: newProfileId(),
    label: "Local",
    host: "localhost",
    port: 55432,
    user: "postgres",
    dbname: "sluice_dev",
    tls: "disabled",
  });
  // T-036: the portable profile file, re-read from disk by the core.
  const workspace = useQuery({ queryKey: WORKSPACE_KEY, queryFn: loadWorkspace });
  const [saveNote, setSaveNote] = useState<string | null>(null);
  // T-034: a pasted URL is a credential until the core strips it, so it is
  // held only until submitted and rendered as a password field.
  const [importUrl, setImportUrl] = useState("");
  const [secret, setSecret] = useState("");
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  // T-071 / R-016: the core says the stored password could not be used (an
  // unreadable keychain item after a signing change, or a server refusal).
  // Not a dead end: ask for it again; storing it re-creates the keychain item
  // under this build's identity.
  const [reentry, setReentry] = useState(false);

  const field = (k: keyof ProfileMeta, label: string, type = "text") => (
    <label className="block px-2 py-1 text-xs">
      <span className="text-muted-foreground">{label}</span>
      <input
        type={type}
        className="mt-0.5 w-full rounded border border-input bg-transparent px-1.5 py-1 text-sm"
        value={String(form[k])}
        onChange={(e) =>
          setForm({ ...form, [k]: k === "port" ? Number(e.target.value) : e.target.value })
        }
      />
    </label>
  );

  return (
    <form
      className="flex flex-col gap-0.5 overflow-auto p-1"
      onSubmit={async (e) => {
        e.preventDefault();
        setBusy(true);
        setErr(null);
        try {
          if (secret) await storeCredential(form.id, secret);
          setSecret("");
          setReentry(false);
          onConnected(await connect(form));
        } catch (e2) {
          const de = e2 as DriverError;
          setReentry(de?.hints?.credential_reentry === true);
          setErr(String(de?.message ?? e2));
        } finally {
          setBusy(false);
        }
      }}
    >
      <SavedProfiles
        workspace={workspace.data}
        selectedId={form.id}
        onPick={(p) => {
          setForm(p);
          setSecret("");
          setReentry(false);
          setErr(null);
          setSaveNote(null);
        }}
        onDelete={async (id) => {
          try {
            queryClient.setQueryData(WORKSPACE_KEY, await deleteProfile(id));
            if (id === form.id) setForm({ ...form, id: newProfileId() });
          } catch (e2) {
            setErr(String((e2 as { message?: string })?.message ?? e2));
          }
        }}
      />
      {workspace.error && (
        <p className="px-2 py-1 text-xs text-destructive">
          {String((workspace.error as { message?: string })?.message ?? workspace.error)}
        </p>
      )}
      <div className="flex items-center gap-1 px-2 py-1">
        <input
          type="password"
          aria-label="Import connection URL"
          autoComplete="off"
          className="min-w-0 flex-1 rounded border border-input bg-transparent px-1.5 py-1 text-xs"
          placeholder="Paste postgres:// URL to import"
          value={importUrl}
          onChange={(e) => setImportUrl(e.target.value)}
        />
        <Button
          type="button"
          size="sm"
          variant="secondary"
          className="h-7"
          disabled={!importUrl.trim()}
          onClick={async () => {
            setErr(null);
            setSaveNote(null);
            const url = importUrl;
            setImportUrl("");
            try {
              const view = await importConnectionUrl(url, "");
              queryClient.setQueryData(WORKSPACE_KEY, view);
              const added = view.profiles[view.profiles.length - 1];
              if (added) setForm(added);
              setSaveNote(view.notice ?? "Imported. Any password in the URL is now in the keychain.");
            } catch (e2) {
              setErr(String((e2 as { message?: string })?.message ?? e2));
            }
          }}
        >
          Import
        </Button>
      </div>
      <div className="flex items-center justify-between px-2 py-1">
        <p className="text-xs font-medium uppercase text-muted-foreground">Connect</p>
        <button
          type="button"
          className="text-xs text-muted-foreground hover:text-foreground"
          onClick={() => {
            setForm({ ...form, id: newProfileId(), label: "New profile" });
            setSecret("");
            setSaveNote(null);
          }}
        >
          New
        </button>
      </div>
      {field("label", "Name")}
      {field("host", "Host")}
      {field("port", "Port", "number")}
      {field("user", "User")}
      {field("dbname", "Database")}
      <label className="block px-2 py-1 text-xs">
        <span className="text-muted-foreground">Connection budget</span>
        <input
          type="text"
          inputMode="numeric"
          className="mt-0.5 w-full rounded border border-input bg-transparent px-1.5 py-1 text-sm"
          value={form.connection_budget == null ? "" : String(form.connection_budget)}
          placeholder="5 — shared by browsing and editor tabs"
          onChange={(e) => {
            const t = e.target.value.trim();
            setForm({ ...form, connection_budget: /^\d+$/.test(t) ? Number(t) : null });
          }}
        />
      </label>
      <label className="block px-2 py-1 text-xs">
        <span className="text-muted-foreground">
          {reentry ? "Re-enter password — it will be stored again in the keychain" : "Password"}
        </span>
        <input
          type="password"
          className={cn(
            "mt-0.5 w-full rounded border bg-transparent px-1.5 py-1 text-sm",
            reentry ? "border-destructive" : "border-input",
          )}
          value={secret}
          autoFocus={reentry}
          required={reentry}
          onChange={(e) => setSecret(e.target.value)}
          placeholder="stored in the OS keychain"
        />
      </label>
      <label className="block px-2 py-1 text-xs">
        <span className="text-muted-foreground">TLS</span>
        <select
          className="mt-0.5 w-full rounded border border-input bg-transparent px-1.5 py-1 text-sm"
          value={form.tls}
          onChange={(e) => setForm({ ...form, tls: e.target.value as ProfileMeta["tls"] })}
        >
          <option value="verify">Verify certificates (default)</option>
          <option value="accept_invalid_certs">Accept invalid certificates</option>
          <option value="disabled">Disabled</option>
        </select>
      </label>
      <label className="block px-2 py-1 text-xs">
        <span className="text-muted-foreground">Query history</span>
        <select
          className="mt-0.5 w-full rounded border border-input bg-transparent px-1.5 py-1 text-sm"
          value={form.history ?? ""}
          onChange={(e) =>
            setForm({ ...form, history: e.target.value === "" ? null : (e.target.value as "on" | "off") })
          }
        >
          <option value="">Default — off for production, on otherwise</option>
          <option value="on">Keep history for this profile</option>
          <option value="off">Never keep history for this profile</option>
        </select>
      </label>
      {err && <p className="px-2 py-1 text-xs text-destructive">{err}</p>}
      <div className="px-2 py-1">
        <Button type="submit" size="sm" disabled={busy} className="w-full">
          <Plug className="h-3.5 w-3.5" />
          {busy ? "Connecting…" : "Connect"}
        </Button>
        <Button
          type="button"
          size="sm"
          variant="ghost"
          disabled={busy}
          className="mt-1 w-full"
          onClick={async () => {
            setErr(null);
            setSaveNote(null);
            try {
              // D-007: the password goes to the keychain; the file gets metadata.
              if (secret) await storeCredential(form.id, secret);
              setSecret("");
              const view = await saveProfile(form);
              queryClient.setQueryData(WORKSPACE_KEY, view);
              // T-037 / R-019: the one-time "this file names your hosts" notice.
              setSaveNote(view.notice ?? "Saved to the profile file.");
            } catch (e2) {
              setErr(String((e2 as { message?: string })?.message ?? e2));
            }
          }}
        >
          <Save className="h-3.5 w-3.5" />
          Save profile
        </Button>
        {saveNote && <p className="pt-1 text-xs text-muted-foreground">{saveNote}</p>}
      </div>
    </form>
  );
}

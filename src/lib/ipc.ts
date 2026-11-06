import { Channel, invoke } from "@tauri-apps/api/core";
import { QueryClient } from "@tanstack/react-query";
import type {
  BrowseRequest,
  ColumnDesc,
  EditGate,
  EditPlan,
  EditRequest,
  ExecutionResult,
  LaunchOutcome,
  NavNode,
  NodeId,
  ProfileMeta,
  SavedProfile,
  SavedQuery,
  SessionInfo,
  WireValue,
  WorkspaceView,
} from "./contract";

/**
 * The single door to the Rust core (D-001). Every piece of engine data the UI
 * shows arrives through here; the webview opens no sockets of its own.
 *
 * D-007: note what is missing. There is no function that returns a credential,
 * because no command returns one — `storeCredential` sends a secret inward and
 * gets back only which backend accepted it.
 */
export const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      // IPC to a local process — no network flakiness to paper over, and a
      // silent retry of a database call is the wrong default.
      retry: false,
      refetchOnWindowFocus: false,
      staleTime: 30_000,
    },
  },
});

export function coreVersion(): Promise<string> {
  return invoke<string>("core_version");
}

/** T-036: the portable profile file. Re-read from disk on every call. */
export function loadWorkspace(): Promise<WorkspaceView> {
  return invoke<WorkspaceView>("load_workspace");
}

export function saveProfile(profile: ProfileMeta): Promise<WorkspaceView> {
  return invoke<WorkspaceView>("save_profile", { profile });
}

/** Removes the profile and its stored password. */
export function deleteProfile(profileId: string): Promise<WorkspaceView> {
  return invoke<WorkspaceView>("delete_profile", { profileId });
}

/**
 * T-034: import a pasted connection URL. It goes inward only; the core splits
 * the password off into the keychain and persists the rest.
 */
export function importConnectionUrl(url: string, label: string): Promise<WorkspaceView> {
  return invoke<WorkspaceView>("import_connection_url", { url, label });
}

export function saveQuery(query: SavedQuery): Promise<WorkspaceView> {
  return invoke<WorkspaceView>("save_query", { query });
}

export function deleteQuery(queryId: string): Promise<WorkspaceView> {
  return invoke<WorkspaceView>("delete_query", { queryId });
}

export const WORKSPACE_KEY = ["workspace"] as const;

/** T-033: disposable UI state from the cache dir; `null` when there is none. */
export function loadUiState(): Promise<unknown> {
  return invoke<unknown>("load_ui_state");
}

export function saveUiState(state: unknown): Promise<void> {
  return invoke<void>("save_ui_state", { state });
}

export function storeCredential(
  profileId: string,
  secret: string,
): Promise<"platform_keychain" | "encrypted_file"> {
  return invoke("store_credential", { profileId, secret });
}

export function connect(profile: ProfileMeta): Promise<SessionInfo> {
  return invoke<SessionInfo>("connect", { profile });
}

/** T-026: drain launches the core resolved (first launch, second invocations). */
export function takeLaunches(): Promise<LaunchOutcome[]> {
  return invoke<LaunchOutcome[]>("take_launches");
}

/** T-026: connect a launch by id; its URL and password stay in the core. */
export function connectLaunch(launchId: string): Promise<SessionInfo> {
  return invoke<SessionInfo>("connect_launch", { launchId });
}

/**
 * T-027: save a discovered or command-line session as a profile. Any inline
 * password moves into the keychain inside the core; only metadata returns.
 */
export function saveSessionAsProfile(sessionId: string, label: string): Promise<SavedProfile> {
  return invoke<SavedProfile>("save_session_as_profile", { sessionId, label });
}

/** Fired by the core when `takeLaunches` has something to return. */
export const LAUNCH_PENDING_EVENT = "launch-pending";

export function disconnect(sessionId: string): Promise<void> {
  return invoke("disconnect", { sessionId });
}

export function navigatorChildren(
  sessionId: string,
  parent: NodeId | null,
): Promise<NavNode[]> {
  return invoke<NavNode[]>("navigator_children", { sessionId, parent });
}

/** T-053: drop the core's navigator cache so the next expand re-reads it. */
export function refreshNavigator(sessionId: string): Promise<void> {
  return invoke("refresh_navigator", { sessionId });
}

export function browse(
  sessionId: string,
  request: BrowseRequest,
): Promise<ExecutionResult> {
  return invoke<ExecutionResult>("browse", { sessionId, request });
}

export interface ScriptRun {
  result: ExecutionResult;
  /** Computed in the core (R-041), never re-derived here. */
  summary: string;
}

export function runScript(sessionId: string, script: string, queryId?: string): Promise<ScriptRun> {
  return invoke<ScriptRun>("run_script", { sessionId, script, queryId: queryId ?? null });
}

/**
 * T-057 / R-003: the exact statements staged edits compile to, built by the
 * same core code path that runs them.
 */
export function previewEdits(sessionId: string, edits: EditRequest[]): Promise<EditPlan> {
  return invoke<EditPlan>("preview_edits", { sessionId, edits });
}

/**
 * T-056 / D-027: apply staged edits, all in one transaction or none. The
 * result's `tx.disposition` says which; there is no undo after it commits.
 *
 * T-057: only a previewed plan can be applied — `digest` is that plan's, and
 * the core refuses if the edits no longer compile to exactly it.
 */
export function applyEdits(sessionId: string, edits: EditRequest[], digest: string): Promise<ScriptRun> {
  return invoke<ScriptRun>("apply_edits", { sessionId, edits, expectedDigest: digest, queryId: null });
}

/** T-041: the editor's pinned session, as the core reports it. */
export interface SessionStatus {
  tx: { state: "idle" } | { state: "open" } | { state: "failed" };
  tx_idle_secs: number | null;
  idle_timeout_secs: number;
  /** The connection is gone; set means nothing more runs until reconnect. */
  lost: string | null;
  /**
   * T-043: session state the server says this session holds (SET, temp
   * tables, prepared statements, cursors, locks, LISTEN, a transaction).
   */
  holds: string[];
}

export interface EditorOpened {
  editor_id: string;
  status: SessionStatus;
}

export interface EditorRun {
  run: ScriptRun;
  status: SessionStatus;
}

/** T-041 / T-042: core-initiated session changes (idle rollback, release). */
export interface EditorEvent {
  editor_id: string;
  /** `released`: the session was released to fit the profile's budget (T-042). */
  kind: "idle_rollback" | "released";
  message: string;
  status: SessionStatus;
}

export const EDITOR_EVENT = "editor-session";

/** T-041 / D-028: pin a connection for this editor tab's lifetime. */
export function openEditor(sessionId: string): Promise<EditorOpened> {
  return invoke<EditorOpened>("open_editor", { sessionId });
}

export function runInEditor(editorId: string, script: string, queryId?: string): Promise<EditorRun> {
  return invoke<EditorRun>("run_in_editor", { editorId, script, queryId: queryId ?? null });
}

/** T-041: the opt-in transaction controls. */
export function editorTransaction(editorId: string, action: "begin" | "commit" | "rollback"): Promise<SessionStatus> {
  return invoke<SessionStatus>("editor_transaction", { editorId, action });
}

/** T-041: the tab is closing; the core rolls back — never commits. */
export function closeEditor(editorId: string): Promise<void> {
  return invoke("close_editor", { editorId });
}

/**
 * T-051: cancel a running query by the id it was started with. The server
 * stops it; if it does not stop in time, its connection is dropped.
 */
export function cancelQuery(queryId: string): Promise<boolean> {
  return invoke<boolean>("cancel_query", { queryId });
}

/** T-018: pull one oversized value on demand. */
export function fetchValue(sessionId: string, handle: string): Promise<WireValue> {
  return invoke<WireValue>("fetch_value", { sessionId, handle });
}

export interface SavedFile {
  path: string;
  bytes: number;
}

/**
 * T-018: stream an oversized value to a file. The bytes go server-side straight
 * to disk — they never enter the webview, which is what keeps this compatible
 * with T-021's deliberately narrow capability set.
 */
export function saveValueToFile(
  sessionId: string,
  handle: string,
  suggestedName: string,
): Promise<SavedFile> {
  return invoke<SavedFile>("save_value_to_file", { sessionId, handle, suggestedName });
}

/** T-049: what a browse stream delivers, in order. */
export type StreamEvent =
  | { kind: "columns"; columns: ColumnDesc[]; edit: EditGate }
  | { kind: "rows"; seq: number; rows: WireValue[][] }
  | { kind: "done"; rows: number; truncated: boolean; page: import("./contract").PageInfo | null }
  | { kind: "cancelled" }
  | { kind: "timed_out" }
  | { kind: "error"; error: { message: string } };

/**
 * T-049 / R-006: a credit-gated browse stream.
 *
 * The core sends a batch only against credit, and credit comes back only
 * through `retire` — which the grid calls after a batch has been rendered — so
 * the webview never holds more than a few unrendered batches, however fast
 * the server streams. Events can arrive before `open_browse_stream` has
 * returned the id; credit owed in that window is granted once it does.
 */
export class RowStream {
  private id: string | null = null;
  private owed = 0;
  private stopped = false;

  private constructor() {}

  static open(
    sessionId: string,
    request: BrowseRequest,
    onEvent: (e: StreamEvent) => void,
  ): RowStream {
    const stream = new RowStream();
    const channel = new Channel<StreamEvent>();
    channel.onmessage = onEvent;
    void invoke<string>("open_browse_stream", { sessionId, request, onEvent: channel })
      .then((id) => {
        stream.id = id;
        if (stream.stopped) void invoke("cancel_stream", { streamId: id });
        else if (stream.owed > 0) stream.retire(0);
      })
      .catch((e) => onEvent({ kind: "error", error: { message: String(e?.message ?? e) } }));
    return stream;
  }

  /** Return credit for `batches` rendered batches. */
  retire(batches: number) {
    this.owed += batches;
    if (this.stopped || this.id === null || this.owed === 0) return;
    const n = this.owed;
    this.owed = 0;
    void invoke("grant_stream_credit", { streamId: this.id, batches: n }).catch(() => {});
  }

  /** Stop the stream, even if the core is blocked waiting for credit. */
  cancel() {
    if (this.stopped) return;
    this.stopped = true;
    if (this.id !== null) void invoke("cancel_stream", { streamId: this.id }).catch(() => {});
  }
}

/** T-023: the exact text "copy diagnostics" will copy. */
export function diagnosticsPreview(): Promise<string> {
  return invoke<string>("diagnostics_preview");
}

/** T-023: the run-scoped verbose toggle (off at every launch). */
export function setVerboseDiagnostics(enabled: boolean): Promise<boolean> {
  return invoke<boolean>("set_verbose_diagnostics", { enabled });
}

export function verboseDiagnostics(): Promise<boolean> {
  return invoke<boolean>("verbose_diagnostics");
}

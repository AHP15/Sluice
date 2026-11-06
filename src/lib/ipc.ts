import { Channel, invoke } from "@tauri-apps/api/core";
import { QueryClient } from "@tanstack/react-query";
import type {
  BrowseRequest,
  ColumnDesc,
  ExecutionResult,
  LaunchOutcome,
  NavNode,
  NodeId,
  ProfileMeta,
  SavedProfile,
  SessionInfo,
  WireValue,
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
  | { kind: "columns"; columns: ColumnDesc[] }
  | { kind: "rows"; seq: number; rows: WireValue[][] }
  | { kind: "done"; rows: number; truncated: boolean }
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

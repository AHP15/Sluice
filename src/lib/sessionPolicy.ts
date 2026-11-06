/**
 * T-045 / D-028 / R-039: what an editor tab does for each declared session
 * affinity.
 *
 * D-003 rules out engine checks in the React layer, so the editor never asks
 * "is this Mongo?". It reads the driver's declared `affinity` (and
 * `transactions`) and derives its behaviour here, in one exhaustive place. A
 * fourth affinity value fails to compile until this function covers it.
 *
 * - `connection` — a socket is pinned for the tab's lifetime (Postgres, SQL
 *   Server): session state such as `SET` and temp tables persists between
 *   statements, opt-in transactions are offered, and connection loss is
 *   surfaced, never silently reconnected (D-028, R-037).
 * - `session` — no socket, but a logical session is pinned (Mongo, per T-046's
 *   verdict): causal consistency holds across the tab's statements and
 *   interactive transactions are offered, but there is no connection-scoped
 *   state, and the server may impose an idle-transaction ceiling that the tab
 *   must state up front. Loss is surfaced, as for `connection`.
 * - `none` — nothing is pinned: each statement is independent, the tab offers
 *   no transaction controls and says so instead of showing controls that cannot
 *   work, and a dropped connection may be replaced transparently because there
 *   is no state to lose.
 */
import type { Capabilities } from "./contract";

export type SessionAffinity = Capabilities["affinity"];

export interface EditorSessionPolicy {
  /** What the tab holds for its lifetime. */
  pin: "socket" | "logical_session" | "none";
  /** `SET` options, temp tables and the like survive between statements. */
  sessionStatePersists: boolean;
  /** Reads in the tab observe the tab's own earlier writes. */
  causalConsistency: boolean;
  /** Offer begin / commit / rollback controls. */
  offerTransactions: boolean;
  /**
   * Server-side ceiling on an idle open transaction, shown next to the
   * transaction indicator. `null` means none is imposed.
   */
  transactionIdleCeilingSeconds: number | null;
  /**
   * On connection loss: `surface` marks the tab detached and tells the user;
   * `transparent` may reconnect silently. Only an engine with nothing pinned
   * gets `transparent`.
   */
  onConnectionLoss: "surface" | "transparent";
  /** A persistent notice shown in the tab, or `null` for none. */
  notice: string | null;
}

function assertNever(x: never): never {
  throw new Error(`undeclared session affinity: ${String(x)}`);
}

export function editorSessionPolicy(
  caps: Pick<Capabilities, "affinity" | "transactions">,
): EditorSessionPolicy {
  const interactive = caps.transactions?.interactive ?? false;
  const ceiling = caps.transactions?.max_idle_seconds ?? null;

  switch (caps.affinity) {
    case "connection":
      return {
        pin: "socket",
        sessionStatePersists: true,
        causalConsistency: true,
        offerTransactions: interactive,
        transactionIdleCeilingSeconds: interactive ? ceiling : null,
        onConnectionLoss: "surface",
        notice: interactive ? ceilingNotice(ceiling) : NO_TRANSACTIONS,
      };
    case "session":
      return {
        pin: "logical_session",
        sessionStatePersists: false,
        causalConsistency: true,
        offerTransactions: interactive,
        transactionIdleCeilingSeconds: interactive ? ceiling : null,
        onConnectionLoss: "surface",
        notice: interactive ? ceilingNotice(ceiling) : NO_TRANSACTIONS,
      };
    case "none":
      return {
        pin: "none",
        sessionStatePersists: false,
        causalConsistency: false,
        offerTransactions: false,
        transactionIdleCeilingSeconds: null,
        onConnectionLoss: "transparent",
        notice:
          "Each statement runs independently: session-scoped state and interactive transactions are unavailable here.",
      };
    default:
      return assertNever(caps.affinity);
  }
}

const NO_TRANSACTIONS = "Interactive transactions are unavailable here.";

function ceilingNotice(ceiling: number | null): string | null {
  return ceiling === null
    ? null
    : `The server aborts a transaction left open longer than ${ceiling}s.`;
}

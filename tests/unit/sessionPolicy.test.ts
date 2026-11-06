/**
 * T-045 / R-039: each declared affinity value has defined editor-tab behaviour.
 *
 * Run with `bun test tests/unit`.
 */
import { describe, expect, test } from "bun:test";

import type { Capabilities } from "../../src/lib/contract";
import { editorSessionPolicy } from "../../src/lib/sessionPolicy";

type Caps = Pick<Capabilities, "affinity" | "transactions">;

const POSTGRES: Caps = { affinity: "connection", transactions: { interactive: true, max_idle_seconds: null } };
// T-046's measured Mongo declaration: a logical session, a 60s server ceiling.
const MONGO_REPLICA_SET: Caps = { affinity: "session", transactions: { interactive: true, max_idle_seconds: 60 } };
const STATELESS: Caps = { affinity: "none", transactions: null };

describe("connection affinity", () => {
  const p = editorSessionPolicy(POSTGRES);
  test("pins a socket whose session state persists", () => {
    expect(p.pin).toBe("socket");
    expect(p.sessionStatePersists).toBe(true);
  });
  test("offers transactions with no ceiling and no notice", () => {
    expect(p.offerTransactions).toBe(true);
    expect(p.transactionIdleCeilingSeconds).toBeNull();
    expect(p.notice).toBeNull();
  });
  test("surfaces connection loss rather than reconnecting", () => {
    expect(p.onConnectionLoss).toBe("surface");
  });
});

describe("session affinity", () => {
  const p = editorSessionPolicy(MONGO_REPLICA_SET);
  test("pins a logical session, not a socket, with causal consistency", () => {
    expect(p.pin).toBe("logical_session");
    expect(p.sessionStatePersists).toBe(false);
    expect(p.causalConsistency).toBe(true);
  });
  test("offers transactions and states the server ceiling", () => {
    expect(p.offerTransactions).toBe(true);
    expect(p.transactionIdleCeilingSeconds).toBe(60);
    expect(p.notice).toContain("60s");
  });
  test("surfaces connection loss, since the session carries state", () => {
    expect(p.onConnectionLoss).toBe("surface");
  });
  test("a session without interactive transactions (standalone mongod) offers none", () => {
    const q = editorSessionPolicy({ affinity: "session", transactions: null });
    expect(q.offerTransactions).toBe(false);
    expect(q.notice).toContain("unavailable");
  });
});

describe("no affinity", () => {
  const p = editorSessionPolicy(STATELESS);
  test("pins nothing and offers no transaction controls", () => {
    expect(p.pin).toBe("none");
    expect(p.offerTransactions).toBe(false);
    expect(p.sessionStatePersists).toBe(false);
  });
  test("ignores a transactions declaration it cannot honour", () => {
    const q = editorSessionPolicy({ affinity: "none", transactions: { interactive: true, max_idle_seconds: null } });
    expect(q.offerTransactions).toBe(false);
  });
  test("says what is unavailable instead of hiding it", () => {
    expect(p.notice).toContain("interactive transactions are unavailable");
  });
  test("may reconnect transparently, having nothing to lose", () => {
    expect(p.onConnectionLoss).toBe("transparent");
  });
});

test("an undeclared affinity is an error, never a silent default", () => {
  expect(() => editorSessionPolicy({ affinity: undefined as never, transactions: null })).toThrow(
    "undeclared session affinity",
  );
});

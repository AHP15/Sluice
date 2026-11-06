/**
 * T-041 / D-028: the editor's session indicator is persistent and honest.
 *
 * Run with `bun test tests/unit`.
 */
import { describe, expect, test } from "bun:test";
import { renderToStaticMarkup } from "react-dom/server";

import { EditorSessionBar } from "../../src/components/EditorSessionBar";
import type { SessionStatus } from "../../src/lib/ipc";

const base: SessionStatus = { tx: { state: "idle" }, tx_idle_secs: null, idle_timeout_secs: 900, lost: null, holds: [] };
const render = (status: SessionStatus, notice: string | null = null) =>
  renderToStaticMarkup(
    <EditorSessionBar status={status} busy={false} notice={notice} onAction={() => {}} onReconnect={() => {}} />,
  );

describe("EditorSessionBar", () => {
  test("idle: autocommit is stated and the transaction is opt-in", () => {
    const out = render(base);
    expect(out).toContain("autocommit");
    expect(out).toContain("Begin transaction");
    expect(out).not.toContain("Commit<");
  });

  test("open: a persistent indicator, the rollback rules, commit and rollback", () => {
    const out = render({ ...base, tx: { state: "open" }, tx_idle_secs: 3 });
    expect(out).toContain("Transaction open");
    expect(out).toContain("rolled back (never committed) if idle 15 min, or if this tab closes");
    expect(out).toContain(">Commit<");
    expect(out).toContain(">Rollback<");
  });

  test("failed: only rollback is offered", () => {
    const out = render({ ...base, tx: { state: "failed" } });
    expect(out).toContain("Transaction failed");
    expect(out).toContain(">Rollback<");
    expect(out).not.toContain(">Commit<");
  });

  test("lost: says what was lost and requires an explicit reconnect", () => {
    const out = render({ ...base, lost: "The editor's connection was lost. Its session state (temp tables, SET options, prepared statements) is gone." });
    expect(out).toContain('role="alert"');
    expect(out).toContain("temp tables, SET options, prepared statements");
    expect(out).toContain(">Reconnect<");
    expect(out).not.toContain("Begin transaction");
  });

  test("an idle rollback notice is shown", () => {
    expect(render(base, "The open transaction was idle for over 15 minutes and was rolled back.")).toContain("was rolled back");
  });
});

test("T-043: the session state held is shown", () => {
  const out = render({ ...base, holds: ["SET search_path", "1 temp table"] });
  expect(out).toContain("session holds: SET search_path, 1 temp table");
});

test("T-043: a released session's banner is persistent and needs reconnect", () => {
  const out = render({
    ...base,
    lost: "This editor's session was released to make room for another tab within the profile's connection budget. It held no session state, so only the connection itself is gone. Reconnect to continue in a new session.",
  });
  expect(out).toContain("released to make room");
  expect(out).toContain(">Reconnect<");
});

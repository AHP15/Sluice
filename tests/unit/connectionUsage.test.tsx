/** T-044 / R-038: the connection-usage indicator, with fake usage data. */
import { describe, expect, test } from "bun:test";
import { render, screen } from "@testing-library/react";
import { UsageList, UsageSummary } from "../../src/components/ConnectionUsage";
import type { ConnectionUsage } from "../../src/lib/contract";

const usage = (pinned: ConnectionUsage["pinned"]): ConnectionUsage => ({
  budget: 5,
  browse_floor: 2,
  pinned_limit: 3,
  pinned,
  max_connections: 100,
});
const ed = (tab: string, in_transaction = false, holds: string[] = []) => ({
  editor_id: `e-${tab}`,
  tab,
  in_transaction,
  holds,
  busy: false,
});

describe("connection usage", () => {
  test("the summary counts editors against the pinned limit, not the whole budget", () => {
    render(<UsageSummary u={usage([ed("orders")])} />);
    expect(screen.getByText("editors 1/3 · 2 kept for browsing")).toBeTruthy();
  });

  test("lists which tabs hold sessions and which hold transactions", () => {
    render(<UsageList u={usage([ed("orders", true), ed("users", false, ["a temp table"])])} />);
    const list = screen.getByTestId("usage-list");
    expect(list.textContent).toContain("orders");
    expect(list.textContent).toContain("transaction open");
    expect(list.textContent).toContain("holds a temp table");
    expect(screen.getByText(/allows 100 connections across all clients/)).toBeTruthy();
  });

  test("says when it is full and what to do", () => {
    render(<UsageList u={usage([ed("a", true), ed("b", true), ed("c", true)])} />);
    expect(screen.getByText(/Full: a new editor tab is refused/)).toBeTruthy();
  });
});

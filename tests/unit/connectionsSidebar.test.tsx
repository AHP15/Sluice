/**
 * T-093 / T-094: the merged connections panel, in a DOM, no IPC.
 */
import { describe, expect, mock, test } from "bun:test";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";

import { ConnectionsSidebar, type SidebarConnection } from "../../src/components/ConnectionsSidebar";
import type { ProfileMeta } from "../../src/lib/contract";

const P = (id: string, label: string, engine?: ProfileMeta["engine"]): ProfileMeta => ({
  id,
  label,
  engine,
  host: "h",
  port: 1,
  user: "u",
  dbname: "d",
  tls: "verify",
});
const PROFILES = [P("a", "Shop prod"), P("b", "Warehouse", "sqlserver"), P("c", "Events", "mongodb")];
const item = (p: ProfileMeta, over: Partial<SidebarConnection> = {}): SidebarConnection => ({
  key: p.id,
  label: p.label,
  engine: p.engine,
  profile: p,
  tabId: null,
  connected: false,
  connecting: false,
  ...over,
});
const ITEMS = [item(PROFILES[0], { tabId: 7, connected: true }), item(PROFILES[1]), item(PROFILES[2])];

function mount(over: Partial<Parameters<typeof ConnectionsSidebar>[0]> = {}) {
  const props = {
    items: ITEMS,
    activeTabId: 7,
    expanded: new Set(["a"]),
    onOpen: mock(() => {}),
    onToggle: mock(() => {}),
    onDisconnect: mock(() => {}),
    onNew: mock(() => {}),
    onEdit: mock(() => {}),
    onRename: mock(async () => {}),
    slotRef: () => () => {},
    ...over,
  };
  const r = render(<ConnectionsSidebar {...props} />);
  return { ...r, props };
}

describe("T-093: one panel for connections and their navigators", () => {
  test("every saved connection is listed by name, engine and connection state", () => {
    const { container, unmount } = mount();
    expect(screen.getByRole("button", { name: "Shop prod, Postgres, connected" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Warehouse, SQL Server, not connected" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Events, MongoDB, not connected" })).toBeTruthy();
    expect(container.textContent).toContain("PG");
    unmount();
  });

  test("clicking connects or switches; the connected row expands into its navigator", () => {
    const { props, unmount } = mount();
    fireEvent.click(screen.getByRole("button", { name: /^Warehouse/ }));
    expect(props.onOpen).toHaveBeenCalledWith(ITEMS[1]);
    fireEvent.click(screen.getByRole("button", { name: "Collapse Shop prod" }));
    expect(props.onToggle).toHaveBeenCalledWith("a");
    fireEvent.click(screen.getByRole("button", { name: "Disconnect Shop prod" }));
    expect(props.onDisconnect).toHaveBeenCalledWith(7);
    expect(screen.queryByRole("button", { name: "Disconnect Warehouse" })).toBeNull();
    unmount();
  });

  test("only a connected row hosts a navigator slot", () => {
    const seen: number[] = [];
    const { unmount } = mount({ slotRef: (id) => (el) => el && seen.push(id) });
    expect(seen).toEqual([7]);
    unmount();
  });

  test("New and Edit open the connection dialog", () => {
    const { props, unmount } = mount();
    fireEvent.click(screen.getByRole("button", { name: "New connection" }));
    expect(props.onNew).toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Edit Warehouse" }));
    expect(props.onEdit).toHaveBeenCalledWith(PROFILES[1]);
    unmount();
  });
});

describe("T-094: rename inline, never to an empty name", () => {
  test("an empty name is refused and nothing is written", async () => {
    const { props, unmount } = mount();
    fireEvent.doubleClick(screen.getByRole("button", { name: /^Warehouse/ }));
    const input = screen.getByRole("textbox", { name: "Rename Warehouse" });
    fireEvent.change(input, { target: { value: "   " } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(await screen.findByRole("alert")).toBeTruthy();
    expect(props.onRename).not.toHaveBeenCalled();
    unmount();
  });

  test("a new name is trimmed and written through onRename", async () => {
    const { props, unmount } = mount();
    fireEvent.doubleClick(screen.getByRole("button", { name: /^Events/ }));
    const input = screen.getByRole("textbox", { name: "Rename Events" });
    fireEvent.change(input, { target: { value: "  Event store  " } });
    fireEvent.keyDown(input, { key: "Enter" });
    await waitFor(() => expect(props.onRename).toHaveBeenCalledWith(PROFILES[2], "Event store"));
    unmount();
  });
});

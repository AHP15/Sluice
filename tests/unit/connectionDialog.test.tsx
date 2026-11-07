/**
 * The connection dialog: create/edit in a modal, a name is required, and the
 * profile is saved before the password is stored. IPC is faked with mockIPC.
 */
import { afterEach, describe, expect, mock, test } from "bun:test";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";

import { ConnectionDialog } from "../../src/components/ConnectionDialog";
import type { ProfileMeta } from "../../src/lib/contract";

afterEach(() => clearMocks());

function fakeCore() {
  const calls: { cmd: string; args: Record<string, unknown> }[] = [];
  mockIPC((cmd, args) => {
    calls.push({ cmd, args: (args ?? {}) as Record<string, unknown> });
    if (cmd === "save_profile") return { profiles: [(args as { profile: ProfileMeta }).profile], queries: [], path: "/x" };
    if (cmd === "store_credential") return "platform_keychain";
    return null;
  });
  return calls;
}

describe("connection dialog", () => {
  test("saving without a name is refused, and nothing reaches the core", async () => {
    const calls = fakeCore();
    const onSaved = mock(() => {});
    const { unmount } = render(<ConnectionDialog mode={{ kind: "new" }} onClose={() => {}} onSaved={onSaved} />);
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    expect((await screen.findByRole("alert")).textContent).toContain("Name this connection");
    expect(calls.length).toBe(0);
    expect(onSaved).not.toHaveBeenCalled();
    unmount();
  });

  test("Save & connect writes the profile, then the password, then asks to connect", async () => {
    const calls = fakeCore();
    const onSaved = mock(() => {});
    const onClose = mock(() => {});
    const { unmount } = render(<ConnectionDialog mode={{ kind: "new" }} onClose={onClose} onSaved={onSaved} />);
    fireEvent.change(screen.getByRole("textbox", { name: "Name" }), { target: { value: "  Local dev  " } });
    fireEvent.change(screen.getByLabelText("Password"), { target: { value: "sluice_dev" } });
    fireEvent.click(screen.getByRole("button", { name: "Save & connect" }));
    await waitFor(() => expect(onSaved).toHaveBeenCalled());
    expect(calls.map((c) => c.cmd)).toEqual(["save_profile", "store_credential"]);
    const saved = (calls[0].args.profile as ProfileMeta);
    expect(saved.label).toBe("Local dev");
    // D-007: the password never travels inside the profile.
    expect(JSON.stringify(calls[0].args)).not.toContain("sluice_dev");
    expect((onSaved.mock.calls[0] as unknown[])[1]).toBe(true);
    expect(onClose).toHaveBeenCalled();
    unmount();
  });

  test("editing keeps the stored password unless a new one is typed", async () => {
    const calls = fakeCore();
    const profile: ProfileMeta = { id: "p1", label: "Shop", host: "h", port: 5432, user: "u", dbname: "d", tls: "verify" };
    const { unmount } = render(<ConnectionDialog mode={{ kind: "edit", profile }} onClose={() => {}} onSaved={() => {}} />);
    expect((screen.getByRole("textbox", { name: "Name" }) as HTMLInputElement).value).toBe("Shop");
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(calls.map((c) => c.cmd)).toEqual(["save_profile"]));
    unmount();
  });

  test("the eye toggle reveals and re-masks the password", () => {
    fakeCore();
    const { unmount } = render(<ConnectionDialog mode={{ kind: "new" }} onClose={() => {}} onSaved={() => {}} />);
    const input = screen.getByLabelText("Password") as HTMLInputElement;
    expect(input.type).toBe("password");
    fireEvent.click(screen.getByRole("button", { name: "Show password" }));
    expect(input.type).toBe("text");
    fireEvent.click(screen.getByRole("button", { name: "Hide password" }));
    expect(input.type).toBe("password");
    unmount();
  });
});

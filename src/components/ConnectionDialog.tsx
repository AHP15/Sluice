import { useEffect, useId, useRef, useState } from "react";
import { Eye, EyeOff, Plug, Save, Trash2, X } from "lucide-react";
import { ENGINES, type Engine, type ProfileMeta } from "../lib/contract";
import { withEngine } from "../lib/engine";
import { deleteProfile, importConnectionUrl, queryClient, saveProfile, storeCredential, WORKSPACE_KEY } from "../lib/ipc";
import { cn } from "../lib/utils";
import { Button } from "./ui/button";
import { Select } from "./ui/select";
import { newProfileId } from "./SavedProfiles";

export type DialogMode =
  | { kind: "new" }
  /** `reentry`: the core could not use the stored password (T-071 / R-016). */
  | { kind: "edit"; profile: ProfileMeta; reentry?: boolean; reason?: string };

export function blankProfile(): ProfileMeta {
  return {
    id: newProfileId(),
    label: "",
    engine: "postgres",
    host: "localhost",
    port: 5432,
    user: "postgres",
    dbname: "postgres",
    tls: "verify",
  };
}

const inputCls =
  "focus-glow h-8 w-full rounded border border-input bg-background px-2.5 text-sm transition-[border-color,box-shadow] duration-interact";

/**
 * Create, edit, import and delete a saved connection, in a modal.
 *
 * T-094: a connection must be named to be saved, and both buttons save — so
 * every connection made here is a saved, named one. D-007: the password goes
 * to the keychain via the core and is never read back; the profile file
 * gets metadata only. The profile is written FIRST, so a keychain hiccup
 * cannot lose the connection itself — it is reported, and the password can
 * be entered again.
 */
export function ConnectionDialog({
  mode,
  onClose,
  onSaved,
}: {
  mode: DialogMode;
  onClose: () => void;
  /** After a save; `connect` is true for "Save & connect". */
  onSaved: (profile: ProfileMeta, connect: boolean) => void;
}) {
  const editing = mode.kind === "edit";
  const [form, setForm] = useState<ProfileMeta>(editing ? mode.profile : blankProfile());
  const [secret, setSecret] = useState("");
  const [showSecret, setShowSecret] = useState(false);
  const secretId = useId();
  const [importUrl, setImportUrl] = useState("");
  const [err, setErr] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [armDelete, setArmDelete] = useState(false);
  const [isSaved, setIsSaved] = useState(editing);
  const reentry = editing && !!mode.reentry;
  const nameRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (!reentry) nameRef.current?.focus();
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose, reentry]);

  const set = <K extends keyof ProfileMeta>(k: K, v: ProfileMeta[K]) => setForm((f) => ({ ...f, [k]: v }));

  const save = async (connect: boolean) => {
    setErr(null);
    setNote(null);
    const label = form.label.trim();
    if (!label) {
      setErr("Name this connection before saving it.");
      nameRef.current?.focus();
      return;
    }
    if (reentry && !secret) {
      setErr("Enter the password again — the stored one could not be used.");
      return;
    }
    setBusy(true);
    const profile = { ...form, label };
    try {
      const view = await saveProfile(profile);
      queryClient.setQueryData(WORKSPACE_KEY, view);
      setIsSaved(true);
      if (secret) {
        try {
          await storeCredential(profile.id, secret);
          setSecret("");
        } catch (e) {
          setErr(`Saved, but the password could not be stored: ${String((e as { message?: string })?.message ?? e)}`);
          return;
        }
      }
      onSaved(profile, connect);
      if (!connect && view.notice) setNote(view.notice);
      else onClose();
    } catch (e) {
      setErr(String((e as { message?: string })?.message ?? e));
    } finally {
      setBusy(false);
    }
  };

  const field = (k: "host" | "user" | "dbname", label: string, placeholder?: string) => (
    <label className="block">
      <span className="text-2xs uppercase tracking-label text-muted-foreground">{label}</span>
      <input
        className={cn(inputCls, "mt-1 font-mono")}
        value={String(form[k] ?? "")}
        placeholder={placeholder}
        onChange={(e) => set(k, e.target.value)}
      />
    </label>
  );

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-overlay/60 p-6"
      onMouseDown={(e) => e.target === e.currentTarget && onClose()}
    >
      <form
        role="dialog"
        aria-modal="true"
        aria-label={editing ? `Edit connection ${mode.profile.label}` : "New connection"}
        className="flex max-h-full w-[34rem] flex-col rounded-lg border border-border-strong bg-raised shadow-float"
        onSubmit={(e) => {
          e.preventDefault();
          void save(true);
        }}
      >
        <header className="flex h-11 items-center gap-2 border-b px-4">
          <span aria-hidden className="block h-2 w-2 rotate-45 border-[1.5px] border-primary" />
          <h2 className="text-sm font-semibold">{editing ? "Edit connection" : "New connection"}</h2>
          <button
            type="button"
            aria-label="Close"
            className="focus-glow ml-auto rounded border border-transparent p-1 text-muted-foreground hover:text-foreground"
            onClick={onClose}
          >
            <X className="h-4 w-4" />
          </button>
        </header>

        <div className="min-h-0 space-y-3 overflow-auto px-4 py-4">
          {reentry && (
            <p role="alert" className="rounded border border-caution/60 bg-caution/10 px-2.5 py-1.5 text-xs text-caution">
              {mode.kind === "edit" && mode.reason ? `${mode.reason} ` : ""}Enter the password again; it will be stored
              in the keychain under this build.
            </p>
          )}

          {!editing && (
            <div className="flex items-end gap-2">
              <label className="block flex-1">
                <span className="text-2xs uppercase tracking-label text-muted-foreground">Import from URL</span>
                {/* T-034: a pasted URL is a credential until the core splits
                    the password off, so it is a password field. */}
                <input
                  type="password"
                  autoComplete="off"
                  aria-label="Import connection URL"
                  className={cn(inputCls, "mt-1 font-mono")}
                  placeholder="postgres://… · sqlserver://… · mongodb://…"
                  value={importUrl}
                  onChange={(e) => setImportUrl(e.target.value)}
                />
              </label>
              <Button
                type="button"
                variant="secondary"
                disabled={!importUrl.trim() || busy}
                onClick={async () => {
                  setErr(null);
                  const url = importUrl;
                  setImportUrl("");
                  try {
                    const view = await importConnectionUrl(url, form.label.trim());
                    queryClient.setQueryData(WORKSPACE_KEY, view);
                    const added = view.profiles[view.profiles.length - 1];
                    if (added) {
                      setForm(added);
                      setIsSaved(true);
                    }
                    setNote(view.notice ?? "Imported and saved. Any password in the URL is now in the keychain — rename it if you like.");
                  } catch (e) {
                    setErr(String((e as { message?: string })?.message ?? e));
                  }
                }}
              >
                Import
              </Button>
            </div>
          )}

          <label className="block">
            <span className="text-2xs uppercase tracking-label text-muted-foreground">Name</span>
            <input
              ref={nameRef}
              aria-label="Name"
              required
              className={cn(inputCls, "mt-1")}
              placeholder="Name this connection — e.g. Shop production"
              value={form.label}
              onChange={(e) => set("label", e.target.value)}
            />
          </label>

          <div className="grid grid-cols-3 gap-3">
            <label className="block">
              <span className="text-2xs uppercase tracking-label text-muted-foreground">Engine</span>
              <Select
                aria-label="Engine"
                className="mt-1 w-full"
                value={form.engine ?? "postgres"}
                options={ENGINES.map((en) => ({ value: en.id, label: en.label }))}
                onChange={(v) => setForm((f) => withEngine(f, v as Engine))}
              />
            </label>
            <div className="col-span-2 grid grid-cols-[1fr_6rem] gap-3">
              {field("host", "Host")}
              <label className="block">
                <span className="text-2xs uppercase tracking-label text-muted-foreground">Port</span>
                <input
                  type="number"
                  className={cn(inputCls, "mt-1 font-mono")}
                  value={form.port}
                  onChange={(e) => set("port", Number(e.target.value))}
                />
              </label>
            </div>
          </div>

          <div className="grid grid-cols-2 gap-3">
            {field("user", "User")}
            {field("dbname", "Database")}
          </div>

          <div>
            <label htmlFor={secretId} className="text-2xs uppercase tracking-label text-muted-foreground">
              {reentry ? "Password — required" : editing ? "Password — leave empty to keep the stored one" : "Password"}
            </label>
            {/* The show/hide toggle sits outside the <label> so it is not part of the field's accessible name. */}
            <div className="relative mt-1">
              <input
                id={secretId}
                type={showSecret ? "text" : "password"}
                aria-label="Password"
                autoComplete="off"
                spellCheck={false}
                autoFocus={reentry}
                className={cn(inputCls, "pr-8", reentry && "border-caution")}
                placeholder="stored in the OS keychain, never in the profile file"
                value={secret}
                onChange={(e) => setSecret(e.target.value)}
              />
              <button
                type="button"
                aria-label={showSecret ? "Hide password" : "Show password"}
                aria-pressed={showSecret}
                title={showSecret ? "Hide password" : "Show password"}
                className="absolute inset-y-0 right-0 flex w-8 items-center justify-center rounded-r text-muted-foreground transition-colors duration-interact hover:text-foreground focus-visible:text-foreground focus-visible:outline-none"
                onClick={() => setShowSecret((v) => !v)}
              >
                {showSecret ? <EyeOff className="h-4 w-4" /> : <Eye className="h-4 w-4" />}
              </button>
            </div>
          </div>

          <div className="grid grid-cols-2 gap-3">
            <label className="block">
              <span className="text-2xs uppercase tracking-label text-muted-foreground">TLS</span>
              <Select<ProfileMeta["tls"]>
                aria-label="TLS"
                className="mt-1 w-full"
                value={form.tls}
                options={[
                  { value: "verify", label: "Verify certificates (default)" },
                  { value: "accept_invalid_certs", label: "Accept invalid certificates" },
                  { value: "disabled", label: "Disabled" },
                ]}
                onChange={(v) => set("tls", v)}
              />
            </label>
            <label className="block">
              <span className="text-2xs uppercase tracking-label text-muted-foreground">Query history</span>
              <Select<"" | "on" | "off">
                aria-label="Query history"
                className="mt-1 w-full"
                value={form.history ?? ""}
                options={[
                  { value: "", label: "Default — off for production" },
                  { value: "on", label: "Keep history" },
                  { value: "off", label: "Never keep history" },
                ]}
                onChange={(v) => set("history", v === "" ? null : v)}
              />
            </label>
          </div>
          {form.tls !== "verify" && (
            <p className="text-2xs text-caution">
              {form.tls === "disabled"
                ? "TLS disabled: traffic, including the password, is unencrypted. Only sane for a local container."
                : "Encrypted, but the server is not authenticated, so the connection can be intercepted."}
            </p>
          )}

          <div className="grid grid-cols-2 gap-3">
            <label className="block">
              <span className="text-2xs uppercase tracking-label text-muted-foreground">Connection budget</span>
              <input
                inputMode="numeric"
                className={cn(inputCls, "mt-1 font-mono")}
                placeholder="5"
                value={form.connection_budget == null ? "" : String(form.connection_budget)}
                onChange={(e) => {
                  const t = e.target.value.trim();
                  set("connection_budget", /^\d+$/.test(t) ? Number(t) : null);
                }}
              />
            </label>
            <label className="flex items-start gap-2 pt-5 text-xs">
              <input
                type="checkbox"
                className="mt-0.5"
                checked={form.read_only ?? false}
                onChange={(e) => set("read_only", e.target.checked)}
              />
              <span>
                Read-only
                <span className="block text-2xs text-muted-foreground">Sluice checks whether the server enforces it.</span>
              </span>
            </label>
          </div>
          {/* T-044 / R-038: raising the budget is allowed, and explicit. */}
          {(form.connection_budget ?? 5) > 5 && (
            <p className="text-2xs text-caution">
              Above the default of 5. Every connection comes out of the server's max_connections, which all its clients
              share.
            </p>
          )}

          {err && (
            <p role="alert" className="rounded border border-destructive/50 bg-destructive/10 px-2.5 py-1.5 text-xs text-destructive">
              {err}
            </p>
          )}
          {note && <p className="text-xs text-muted-foreground">{note}</p>}
        </div>

        <footer className="flex items-center gap-2 border-t px-4 py-3">
          {isSaved && (
            <Button
              type="button"
              variant={armDelete ? "destructive" : "ghost"}
              size="sm"
              disabled={busy}
              onClick={async () => {
                if (!armDelete) {
                  setArmDelete(true);
                  return;
                }
                try {
                  queryClient.setQueryData(WORKSPACE_KEY, await deleteProfile(form.id));
                  onClose();
                } catch (e) {
                  setErr(String((e as { message?: string })?.message ?? e));
                }
              }}
            >
              <Trash2 className="h-3.5 w-3.5" />
              {armDelete ? "Delete — and its stored password?" : "Delete"}
            </Button>
          )}
          <div className="ml-auto flex items-center gap-2">
            <Button type="button" variant="ghost" size="sm" onClick={onClose}>
              Cancel
            </Button>
            <Button type="button" variant="secondary" size="sm" disabled={busy} onClick={() => void save(false)}>
              <Save className="h-3.5 w-3.5" />
              Save
            </Button>
            <Button type="submit" size="sm" disabled={busy}>
              <Plug className="h-3.5 w-3.5" />
              {busy ? "Saving…" : "Save & connect"}
            </Button>
          </div>
        </footer>
      </form>
    </div>
  );
}

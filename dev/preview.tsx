/**
 * Dev-only UI preview: the real <App/> against fixture data, in a plain
 * browser, with the Tauri core replaced by `mockIPC`. For eyeballing the
 * D-031 / D-032 restyle (T-095 / T-096) without a database or a Tauri build.
 *
 *   bun run dev  →  http://localhost:5173/dev/preview.html
 *
 * Not part of the app bundle: Vite builds only index.html. Nothing here talks
 * to a database; every answer below is a fixture.
 */
import { mockIPC } from "@tauri-apps/api/mocks";
import type { ColumnDesc, NavNode, ProfileMeta, WireValue } from "../src/lib/contract";

const profiles: ProfileMeta[] = [
  { id: "p-shop", label: "Shop prod", engine: "postgres", host: "db.prod.internal", port: 5432, user: "app", dbname: "shop", tls: "accept_invalid_certs" },
  { id: "p-wh", label: "Warehouse", engine: "sqlserver", host: "sql.internal", port: 1433, user: "sa", dbname: "warehouse", tls: "verify" },
  { id: "p-ev", label: "Event store", engine: "mongodb", host: "mongo.internal", port: 27017, user: "", dbname: "events", tls: "verify" },
];

const all = ["text", "integer", "exact_numeric", "timestamp"] as const;
const ops = [
  { id: "eq", arity: "one", label: "=", type_classes: [...all], case_sensitive: true, case_insensitive_toggle: true },
  { id: "gt", arity: "one", label: ">", type_classes: [...all], case_sensitive: null, case_insensitive_toggle: false },
  { id: "in", arity: "list", label: "in", type_classes: [...all], case_sensitive: true, case_insensitive_toggle: true },
  { id: "is_null", arity: "none", label: "is null", type_classes: [...all], case_sensitive: null, case_insensitive_toggle: false },
  { id: "contains", arity: "one", label: "contains", type_classes: ["text"], case_sensitive: false, case_insensitive_toggle: true },
];

const kind = (id: string, label: string, has_children: boolean, is_relation: boolean) => ({ id, label, has_children, is_relation });
const capabilities = {
  engine: "postgres",
  server_version: "17.2",
  flavour: "vanilla",
  affinity: "connection",
  reset: "native",
  cancel: "native",
  transactions: { interactive: true, max_idle_seconds: null },
  raw_payload: "script",
  node_kinds: [
    kind("schema", "Schema", true, false),
    kind("table", "Table", true, true),
    kind("view", "View", true, true),
    kind("column", "Column", false, false),
    kind("index", "Index", false, false),
    kind("notice", "Note", false, false),
  ],
  filter_ops: ops,
  fixed_schema: true,
  supports_script: true,
  string_collation: "en_US.UTF-8",
  read_only: { state: "enforced", basis: "the role holds no write privilege" },
  catalog: { flavour_verified: true, features: [] },
};

const node = (k: string, path: string[], label = path[path.length - 1], has_children = true): NavNode => ({
  id: { kind: k, path },
  label,
  has_children,
});

const col = (name: string, type_name: string, type_class: ColumnDesc["type_class"], nullable = true): ColumnDesc => ({
  name,
  type_name,
  type_class,
  nullable,
  mixed: false,
  derived: false,
  exact: null,
  concurrency_comparable: true,
});
const columns = [
  col("id", "int4", "integer", false),
  col("customer", "text", "text", false),
  col("status", "text", "text", false),
  col("total", "numeric(19,4)", "exact_numeric", false),
  col("placed_at", "timestamptz", "timestamp", false),
  col("note", "text", "text"),
];
const names = ["Ada Lovelace", "Grace Hopper", "Edsger Dijkstra", "Barbara Liskov", "Ken Thompson", "Radia Perlman", "Leslie Lamport", "Frances Allen", "Donald Knuth", "Margaret Hamilton"];
const rows: WireValue[][] = Array.from({ length: 60 }, (_, i) => [
  { t: "int", v: 10041 + i },
  { t: "text", v: names[i % names.length] },
  { t: "text", v: i % 3 === 1 ? "held" : "open" },
  { t: "numeric", v: i === 1 ? "922337203685477.5807" : `${(100 + i * 37.25).toFixed(4)}` },
  { t: "timestamp", v: `2026-11-0${(i % 6) + 1} 0${i % 10}:12:33.12+00` },
  i % 4 === 0 ? { t: "null" } : { t: "text", v: ["rush", "gift wrap", "split ship"][i % 3] },
]);

const send = (channel: { id: number }, index: number, message: unknown) =>
  (window as unknown as { __TAURI_INTERNALS__: { runCallback: (id: number, m: unknown) => void } }).__TAURI_INTERNALS__.runCallback(
    channel.id,
    { index, message },
  );

let streams = 0;
mockIPC(
  (cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    switch (cmd) {
      case "take_launches":
        return [];
      case "core_version":
        return "sluice-core 0.1.0 (preview)";
      case "load_ui_state":
      case "save_ui_state":
        return null;
      case "load_workspace":
      case "save_profile":
        if (cmd === "save_profile") {
          const p = a.profile as ProfileMeta;
          const i = profiles.findIndex((x) => x.id === p.id);
          if (i >= 0) profiles[i] = p;
          else profiles.push(p);
        }
        return { profiles, queries: [], path: "~/Library/Application Support/sluice/profiles.toml", notice: null };
      case "connect": {
        const p = a.profile as ProfileMeta;
        return {
          session_id: `${p.id}::preview`,
          capabilities,
          tls_warning:
            p.tls === "accept_invalid_certs"
              ? "Certificate verification is disabled for this profile. The connection is encrypted but the server is not authenticated, so it can be intercepted."
              : null,
          guardrails: {
            provenance: "manual",
            saved: true,
            production: { armed: p.id === "p-shop", reasons: ["host name contains `prod`"] },
            history: "on",
            safety_mode: { requested: true, database: capabilities.read_only },
          },
          budget_warning: null,
        };
      }
      case "navigator_children": {
        const parent = a.parent as NavNode["id"] | null;
        if (!parent) return [node("schema", ["app"]), node("schema", ["audit"]), node("schema", ["public"])];
        if (parent.kind === "schema" && parent.path[0] === "app")
          return [node("table", ["app", "orders"]), node("table", ["app", "customers"]), node("view", ["app", "big_orders"])];
        if (parent.kind === "schema")
          return [node("notice", [...parent.path, "n"], "not permitted to read this part of the catalog", false)];
        return [
          node("column", [...parent.path, "id"], "id  integer · not null", false),
          node("column", [...parent.path, "customer"], "customer  text · not null", false),
          node("column", [...parent.path, "total"], "total  numeric(19,4) · not null", false),
          node("index", [...parent.path, "pk"], "orders_pkey  btree · primary key", false),
        ];
      }
      case "open_browse_stream": {
        const ch = a.onEvent as { id: number };
        const id = `stream-${++streams}`;
        setTimeout(() => {
          send(ch, 0, { kind: "columns", columns, edit: { state: "editable", key: ["id"] } });
          send(ch, 1, { kind: "rows", seq: 1, rows });
          send(ch, 2, { kind: "done", rows: rows.length, truncated: true, page: null });
        }, 120);
        return id;
      }
      case "open_editor":
        return {
          editor_id: "editor-preview",
          status: { tx: { state: "open" }, tx_idle_secs: 240, idle_timeout_secs: 900, lost: null, holds: ["an open transaction"] },
        };
      case "connection_usage":
        return { budget: 5, browse_floor: 2, pinned_limit: 3, pinned: [], max_connections: 100 };
      case "list_history":
        return [];
      default:
        return null;
    }
  },
  { shouldMockEvents: true },
);

await import("../src/main");

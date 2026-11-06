/**
 * TypeScript mirror of the Rust contract (`crates/sluice-core`).
 *
 * Hand-written rather than generated, and deliberately structural: the tags
 * below are serde's `#[serde(tag = "t", content = "v")]` output, so a mismatch
 * shows up as a type error at the one place values are decoded rather than as a
 * silent `undefined` deep in the grid.
 */

export type TypeClass =
  | "boolean"
  | "integer"
  | "float"
  | "exact_numeric"
  | "text"
  | "binary"
  | "timestamp"
  | "uuid"
  | "structural"
  | "opaque"
  | "oversized"
  | "unknown";

/**
 * D-006's tagged value. Only `bool`, `int` and `float` carry raw JSON
 * primitives; everything else is a string plus its tag, because JavaScript
 * would silently damage the rest.
 */
export type WireValue =
  | { t: "null" }
  /** A document field that was absent, which is NOT the same as null. */
  | { t: "missing" }
  | { t: "bool"; v: boolean }
  | { t: "int"; v: number }
  | { t: "float"; v: number }
  /** Exact numeric as literal text — never parse this with Number(). */
  | { t: "numeric"; v: string }
  | { t: "text"; v: string }
  /** base64. */
  | { t: "bytes"; v: string }
  | { t: "timestamp"; v: string }
  | { t: "uuid"; v: string }
  | { t: "document"; v: [string, WireValue][] }
  | { t: "array"; v: WireValue[] }
  | { t: "opaque"; v: { type_name: string; rendered: string } }
  | { t: "oversized"; v: { type_name: string; byte_len: number; handle: string | null } }
  | { t: "unsupported"; v: { type_name: string; rendered: string } };

export interface ColumnDesc {
  name: string;
  type_name: string;
  type_class: TypeClass;
  nullable: boolean;
  /** Values in this column were not all the same type (schemaless engines). */
  mixed: boolean;
  /** Inferred by sampling rather than read from a catalog. */
  derived: boolean;
  /**
   * T-019 / R-005: set for exact numerics (arbitrary-precision decimals, and
   * integers wider than a JS number holds exactly). A column carrying this gets
   * `ExactNumericEditor`, which never turns the value into a JS number.
   */
  exact: ExactDomain | null;
}

/**
 * The domain an exact numeric is validated against — as strings only. Integer
 * bounds are decimal text: `9223372036854775807` as a JSON number would already
 * be rounded by `JSON.parse`.
 */
export type ExactDomain =
  | { kind: "integer"; min: string; max: string }
  | { kind: "decimal"; precision: number | null; scale: number | null };

export interface NodeId {
  kind: string;
  path: string[];
}

export interface NavNode {
  id: NodeId;
  label: string;
  has_children: boolean;
}

export interface NodeKind {
  id: string;
  label: string;
  has_children: boolean;
  is_relation: boolean;
}

export interface FilterOpDecl {
  id: string;
  label: string;
  type_classes: TypeClass[];
  case_sensitive: boolean | null;
  /** T-015: the operator honours `Predicate.case_insensitive`. */
  case_insensitive_toggle: boolean;
}

export interface Capabilities {
  engine: string;
  server_version: string | null;
  flavour: string | null;
  affinity: "connection" | "session" | "none";
  reset: "native" | "reopen_only" | "not_applicable";
  cancel: "native" | "kill_session" | "none";
  transactions: { interactive: boolean; max_idle_seconds: number | null } | null;
  raw_payload: "script" | "command_document";
  node_kinds: NodeKind[];
  filter_ops: FilterOpDecl[];
  fixed_schema: boolean;
  supports_script: boolean;
  /** T-015: the server's default string collation (what `case_sensitive` reflects). */
  string_collation: string | null;
}

export type Predicate = {
  column: string;
  op: string;
  value: WireValue;
  /** T-015: compare ignoring case; only on ops declaring `case_insensitive_toggle`. */
  case_insensitive?: boolean;
};

export type FilterTree =
  | { node: "and"; 0: FilterTree[] }
  | { node: "or"; 0: FilterTree[] }
  | { node: "pred"; 0: Predicate };

export interface SortKey {
  column: string;
  descending: boolean;
}

export interface BrowseRequest {
  relation: NodeId;
  filter: FilterTree | null;
  sort: SortKey[];
  limit: number;
  cursor: WireValue[] | null;
}

export interface QueryPosition {
  utf16_offset: number;
  char_offset: number;
}

export interface DriverError {
  kind: string;
  message: string;
  hints?: {
    position?: QueryPosition | null;
    constraint?: string | null;
    column?: string | null;
    table?: string | null;
    schema?: string | null;
    retryable?: boolean | null;
  };
  payload?: {
    code?: string | null;
    message?: string | null;
    severity?: string | null;
    extra?: [string, string][];
  };
}

/**
 * T-011: `span` means the splitter agreed with the engine and the text may be
 * highlighted. `ordinal` means it did not — render NO highlighting rather than
 * pointing at text that may belong to a different statement.
 */
export type Attribution =
  | { how: "span"; ordinal: number; start_utf16: number; end_utf16: number }
  | { how: "ordinal"; ordinal: number };

export type OutcomeBody =
  | { kind: "rows"; columns: ColumnDesc[]; rows: WireValue[][]; truncated: boolean }
  | {
      kind: "affected";
      count: number;
      statement_kind: string | null;
      concurrency_omitted: string[];
      /** D-017's per-document partial failure. Empty on Postgres. */
      document_errors: { index: number; id: string | null; error: DriverError }[];
    }
  | { kind: "status"; message: string; document: string | null }
  | { kind: "error"; error: DriverError };

export type Outcome = { attribution: Attribution } & OutcomeBody;

export type TxDisposition =
  | { state: "committed" }
  | { state: "rolled_back"; rolled_back: number[] }
  | { state: "open" };

export interface ExecutionResult {
  outcomes: Outcome[];
  tx: {
    mode: "implicit_single" | "explicit_user" | "autocommit_per_statement";
    disposition: TxDisposition;
  };
}

export interface ProfileMeta {
  id: string;
  label: string;
  host: string;
  port: number;
  user: string;
  dbname: string;
  tls: "verify" | "accept_invalid_certs" | "disabled";
}

/** T-026: where a launched connection came from (drives T-027's guardrails). */
export type LaunchOrigin =
  | { kind: "discovered"; dir: string; source: string }
  | { kind: "evaluated"; dir: string; config: string }
  | { kind: "url" };

/** A launched connection, as the UI sees it — never with a credential (D-007). */
export interface LaunchTarget {
  scheme: string;
  host: string;
  port: number;
  user: string;
  database: string;
  tls: ProfileMeta["tls"];
}

export type LaunchOutcome =
  | { kind: "connect"; launch_id: string; origin: LaunchOrigin; target: LaunchTarget }
  | { kind: "choose"; reason: string }
  | { kind: "error"; message: string };

/** T-027 / R-002: the guardrail field set every session carries. */
export interface Guardrails {
  provenance: "manual" | "discovered" | "evaluated" | "command_line";
  saved: boolean;
  production: { armed: boolean; reasons: string[] };
  history: "on" | "off";
  safety_mode: "unknown";
}

export interface SavedProfile {
  profile: ProfileMeta;
  guardrails: Guardrails;
}

export interface SessionInfo {
  session_id: string;
  capabilities: Capabilities;
  tls_warning: string | null;
  guardrails: Guardrails;
}

/**
 * Render a value for the grid.
 *
 * Returns a plain string, always — the caller puts it in a text node. Nothing
 * here produces markup (T-021), and nothing calls `Number()` on an exact type,
 * which would undo D-006's whole point at the last step.
 */
export function renderValue(v: WireValue): string {
  switch (v.t) {
    case "null":
      return "NULL";
    case "missing":
      return "—";
    case "bool":
      return v.v ? "true" : "false";
    case "int":
    case "float":
      return String(v.v);
    case "numeric":
    case "text":
    case "timestamp":
    case "uuid":
      return v.v;
    case "bytes":
      return `${v.v.length} b64 chars`;
    case "document":
    case "array":
      return JSON.stringify(v.v);
    case "opaque":
    case "unsupported":
      return v.v.rendered;
    case "oversized":
      return `${formatBytes(v.v.byte_len)} ${v.v.type_name}`;
  }
}

/** True for values the grid should style as absent rather than as content. */
export function isAbsent(v: WireValue): boolean {
  return v.t === "null" || v.t === "missing";
}

export function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}

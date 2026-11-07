/**
 * T-063 / D-018: the grid's column layout over a schemaless relation.
 *
 * The core derives columns (`schema_infer.rs`); this module is what the user
 * does with them — pin, hide, reorder, add a path by hand — and the
 * per-collection cache that keeps both the inference and those choices until
 * an explicit refresh.
 *
 * Presentation-only, like the inference it sits on:
 *
 * - a value is only ever *looked up* here (`pathValue`) and returned as the
 *   very object the row holds — never converted, never re-typed;
 * - nothing is hidden that the user did not hide: a page column the inference
 *   missed is still shown (`resolveColumns`), and the hidden list is always
 *   offered back;
 * - an array is one cell: `pathValue` descends documents, never arrays.
 */
import type { ColumnDesc, ColumnInference, InferredColumn, WireValue } from "./contract";

export type Path = string[];

/** A stable key for a path. JSON, not a dotted join: field names may contain dots. */
export const pathKey = (p: Path): string => JSON.stringify(p);
export const pathLabel = (p: Path): string => p.join(".");

/** What the user chose, per collection. Keys are `pathKey`s. */
export interface ColumnLayout {
  /** Explicit order, once the user has reordered; empty means inference order. */
  order: string[];
  pinned: string[];
  hidden: string[];
  /** Paths added by hand. */
  added: Path[];
}

export const EMPTY_LAYOUT: ColumnLayout = { order: [], pinned: [], hidden: [], added: [] };

export interface LayoutColumn {
  key: string;
  path: Path;
  label: string;
  /** What inference said about this path; absent for page-only and added paths. */
  inferred?: InferredColumn;
  /** The page column this path is, when it is one — the only kind that can be edited positionally. */
  pageIndex: number | null;
  pinned: boolean;
  added: boolean;
}

/**
 * The columns to show, and the ones hidden, in display order: pinned first,
 * then the user's order, then inference order — which is validator-declared,
 * then by frequency. Page columns the inference does not name come next, so a
 * stale or partial inference never hides a field the page carries; then paths
 * added by hand.
 */
export function resolveColumns(
  inference: ColumnInference | null,
  layout: ColumnLayout,
  page: ColumnDesc[],
): { visible: LayoutColumn[]; hidden: LayoutColumn[] } {
  const base: LayoutColumn[] = [];
  const seen = new Set<string>();
  const push = (path: Path, inferred: InferredColumn | undefined, added: boolean) => {
    const key = pathKey(path);
    if (seen.has(key)) return;
    seen.add(key);
    base.push({
      key,
      path,
      label: pathLabel(path),
      inferred,
      pageIndex: pageIndexOf(page, path),
      pinned: false,
      added,
    });
  };
  for (const c of inference?.columns ?? []) push(c.path, c, false);
  for (const c of page) push([c.name], undefined, false);
  for (const p of layout.added) push(p, undefined, true);

  const rank = new Map(layout.order.map((k, i) => [k, i]));
  const ordered = base
    .map((c, i) => ({ c, i }))
    .sort((a, b) => {
      const ra = rank.get(a.c.key);
      const rb = rank.get(b.c.key);
      if (ra !== undefined && rb !== undefined) return ra - rb;
      if (ra !== undefined) return -1;
      if (rb !== undefined) return 1;
      return a.i - b.i;
    })
    .map(({ c }) => ({ ...c, pinned: layout.pinned.includes(c.key) }));

  const hidden = new Set(layout.hidden);
  const shown = ordered.filter((c) => !hidden.has(c.key));
  return {
    visible: [...shown.filter((c) => c.pinned), ...shown.filter((c) => !c.pinned)],
    hidden: ordered.filter((c) => hidden.has(c.key)),
  };
}

function pageIndexOf(page: ColumnDesc[], path: Path): number | null {
  if (path.length !== 1) return null;
  const i = page.findIndex((c) => c.name === path[0]);
  return i === -1 ? null : i;
}

/**
 * The value at `path` in one row. A page column of that exact name wins; else
 * the first segment's column is descended through nested documents. Arrays are
 * not descended (D-018: one cell). A path the row does not reach is `missing`
 * — absent, which is what it is; never `null`, which would be a claim.
 */
export function pathValue(page: ColumnDesc[], row: WireValue[], path: Path): WireValue {
  const label = pathLabel(path);
  const exact = page.findIndex((c) => c.name === label);
  if (exact !== -1) return row[exact] ?? MISSING;
  const head = page.findIndex((c) => c.name === path[0]);
  if (head === -1) return MISSING;
  let v: WireValue = row[head] ?? MISSING;
  for (const seg of path.slice(1)) {
    if (v.t !== "document") return MISSING;
    const hit: [string, WireValue] | undefined = v.v.find(([k]) => k === seg);
    v = hit ? hit[1] : MISSING;
  }
  return v;
}

const MISSING: WireValue = { t: "missing" };

// ---- user operations: each returns a new layout --------------------------

export function togglePin(l: ColumnLayout, key: string): ColumnLayout {
  return { ...l, pinned: l.pinned.includes(key) ? l.pinned.filter((k) => k !== key) : [...l.pinned, key] };
}

export function hide(l: ColumnLayout, key: string): ColumnLayout {
  return l.hidden.includes(key) ? l : { ...l, hidden: [...l.hidden, key] };
}

export function show(l: ColumnLayout, key: string): ColumnLayout {
  return { ...l, hidden: l.hidden.filter((k) => k !== key) };
}

/**
 * Move `key` one place left or right among `current` (the displayed order).
 * The first reorder materialises the whole order, so later inference refreshes
 * keep the user's arrangement and append new paths after it.
 */
export function move(l: ColumnLayout, current: string[], key: string, delta: -1 | 1): ColumnLayout {
  const order = [...current];
  const i = order.indexOf(key);
  const j = i + delta;
  if (i === -1 || j < 0 || j >= order.length) return l;
  [order[i], order[j]] = [order[j], order[i]];
  // Hidden keys keep their place relative to the rest.
  const rest = l.order.filter((k) => !order.includes(k));
  return { ...l, order: [...order, ...rest] };
}

/** Add a path by hand. Re-adding a hidden path shows it instead. */
export function addPath(l: ColumnLayout, path: Path): ColumnLayout {
  const key = pathKey(path);
  const shown = show(l, key);
  if (shown.added.some((p) => pathKey(p) === key)) return shown;
  return { ...shown, added: [...shown.added, path] };
}

export function removeAdded(l: ColumnLayout, key: string): ColumnLayout {
  return { ...l, added: l.added.filter((p) => pathKey(p) !== key), pinned: l.pinned.filter((k) => k !== key) };
}

/**
 * Parse a typed path: dot-separated segments, none empty. A field whose name
 * itself contains a dot cannot be typed this way; it is still a column when
 * observed, since inference carries real paths.
 */
export function parsePath(input: string): Path | null {
  const t = input.trim();
  if (!t) return null;
  const segs = t.split(".").map((s) => s.trim());
  return segs.some((s) => s === "") ? null : segs;
}

/**
 * The descriptor a layout column renders with. A page column keeps its own,
 * gaining the inference's variance marker; any other path is derived, of
 * unknown class, and never vouched comparable.
 */
export function layoutDesc(c: LayoutColumn, page: ColumnDesc[]): ColumnDesc {
  const varies = !!c.inferred?.type_varies;
  if (c.pageIndex !== null) {
    const d = page[c.pageIndex];
    return { ...d, mixed: d.mixed || varies };
  }
  const tags = c.inferred ? (c.inferred.observed.length ? c.inferred.observed : c.inferred.declared) : [];
  return {
    name: c.label,
    type_name: tags.length ? tags.join("|") : c.added ? "added" : "unknown",
    type_class: "unknown",
    nullable: true,
    mixed: varies,
    derived: true,
    exact: null,
    concurrency_comparable: false,
  };
}

/** "87%" of sampled documents carry the path; "<1%" rather than a false 0%. */
export function frequencyLabel(c: InferredColumn, sampled: number): string | null {
  if (sampled <= 0) return null;
  if (c.present === 0) return "0%";
  const pct = Math.floor((c.present * 100) / sampled);
  return pct === 0 ? "<1%" : `${pct}%`;
}

/** A page row as a document for inference: absent fields are left out, not sent as null. */
export function rowsAsDocuments(page: ColumnDesc[], rows: WireValue[][]): [string, WireValue][][] {
  return rows.map((r) =>
    page.flatMap((c, i): [string, WireValue][] => (r[i] && r[i].t !== "missing" ? [[c.name, r[i]]] : [])),
  );
}

// ---- per-collection cache -------------------------------------------------

export interface CachedLayout {
  inference: ColumnInference | null;
  layout: ColumnLayout;
}

const cache = new Map<string, CachedLayout>();

/**
 * D-018: inference is cached per collection — the key names one — and only an
 * explicit refresh replaces it. The user's layout survives a refresh; paths
 * that disappear simply stop resolving, and new ones join after the user's.
 */
export const layoutCache = {
  get(key: string): CachedLayout | undefined {
    return cache.get(key);
  },
  set(key: string, entry: CachedLayout): void {
    cache.set(key, entry);
  },
  clear(): void {
    cache.clear();
  },
};

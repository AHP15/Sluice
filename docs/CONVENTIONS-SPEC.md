# Decision, Risk, Task, Retrospective Conventions — Spec

Spec-version: 1

This is the schema. For exemplars, see `CONVENTIONS-EXAMPLES.md`.

This spec version is frozen into a project at adoption time: the bootstrap
writes it to `docs/.treenail-version`, and the record-parsing scripts refuse
loudly (rather than guess) on a project stamped with a spec version they do not
support. Bump `Spec-version` on any breaking schema change (T-022, R-012).

---

## Storage layout

```
/docs/
  CONVENTIONS-SPEC.md                # this file
  CONVENTIONS-EXAMPLES.md            # exemplars
  decisions/INDEX.md, D-NNN.md, archive/INDEX.md, archive/D-NNN.md
  risks/INDEX.md, R-NNN.md, archive/INDEX.md, archive/R-NNN.md
  tasks/INDEX.md, T-NNN.md, archive/INDEX.md, archive/T-NNN.md
  retrospectives/INDEX.md, YYYY-MM-DD.md
  features/<feature>/<same structure>
```

Per-record files use zero-padded 3-digit IDs (`D-001.md`). IDs are
unique within scope (project vs feature) across active and archive.
Minting an ID follows the "ID reservation protocol" below — never just
"read INDEX for the max," which races.

INDEX.md is a one-line-per-record summary, pipe-separated, sorted by
ID. Per-record files are authoritative; **INDEX is a pure deterministic
projection of them** — see "INDEX projection" below. It carries no
information that is not already in a record body or derivable from the
record's location on disk, so it can be regenerated from scratch at any
time and drift is always repairable without judgment.

**The index is partitioned to match the record dirs.** Each artifact has
two indexes: `<artifact>/INDEX.md` projects the *active* records, and
`<artifact>/archive/INDEX.md` projects the *archived* records. A record's
INDEX line lives in whichever index matches its file's location, so the
active index stays bounded to in-flight work — it never accumulates a
line per terminal record. There is no `[archived]` marker: a line's
location (which index file it sits in) is the archived signal, derived
from the record file's location, never authored.

---

## INDEX projection

INDEX is not authored independently — it is a function of the
per-record files. The `reindex` skill rebuilds any INDEX from scratch
by reading the records in the matching directory (active dir for
`INDEX.md`, `archive/` for `archive/INDEX.md`) and emitting one line per
record. Two runs over the same records MUST produce byte-identical output.

This projection is implemented by `~/.treenail/scripts/reindex.py` (run it
via the `reindex` skill); because byte-identical determinism is a
property of code, not of hand-typed lines, the script is the canonical
executor and an incremental hand-append is only ever a shortcut for what
it would produce.

The projection is fixed. Each of the two indexes is an independent
projection of its own directory:

- **Source set**: for `<artifact>/INDEX.md`, every `<ID>.md` directly in
  `<artifact>/`; for `<artifact>/archive/INDEX.md`, every `<ID>.md` in
  `<artifact>/archive/`. A record contributes a line to exactly one
  index — the one matching where its file lives.
- **Order**: ascending by numeric ID (zero-padded sort is equivalent).
- **Columns**: exactly the per-artifact line format defined in each
  section below, every field copied verbatim from the record body. No
  `[archived]` marker — location is encoded by which index the line is in.
- No blank lines, no trailing whitespace, one `\n` per line.

**Writers may append incrementally** (the cheap common path: one new
record → one new sorted line), because a single appended line is a
deterministic projection of the single new record. But the append is
only ever a shortcut for the full projection — if an append and a full
`reindex` would ever disagree, the `reindex` output wins. This is what
makes drift impossible to "get wrong": INDEX has no authoritative
state of its own, so the repair is always "recompute," never "decide."

When `audit-artifacts` reports an INDEX/file mismatch, the remedy is
always `/reindex <scope>` — never hand-editing the INDEX line. The
mismatch is detected by `~/.treenail/scripts/audit.py`, which decides it
using `reindex.py`'s own projection, so "INDEX matches its records" is
checked by the same code that rebuilds the index.

---

## ID reservation protocol

Every skill that mints a new ID (`challenge-me`, `red-team-me`,
`triage-risks`'s fix flow, `decompose-plan`, `promote`) MUST use this
protocol, implemented by `~/.treenail/scripts/mint_id.py <artifact-dir>
<PREFIX>` — the canonical executor (the steps below are what it does, and
the hand fallback when no shell is available). It makes the filesystem —
not the INDEX, and not the skill's
in-memory count — the arbiter, so two skills, sessions, or branches
minting at the same time cannot collide.

INDEX may lag (it is only a projection and a writer may not have
appended yet). So the max is computed over **files**, never the INDEX:

1. **Compute the candidate.** Glob `<artifact>/<PREFIX>-*.md` AND
   `<artifact>/archive/<PREFIX>-*.md` in scope. The candidate is
   `max(existing numeric IDs) + 1` (1 if none). Both dirs, because IDs
   are unique across active and archive.

2. **Claim it atomically.** Create the file with an exclusive,
   no-clobber write that fails if the path already exists — e.g.
   `set -C` (noclobber) then `: > <artifact>/<PREFIX>-NNN.md`, or
   `mkdir`-based locking. The create is the claim; do it before writing
   the body.

3. **On collision, increment and retry.** If the exclusive create
   fails because the file exists, a concurrent minter won that ID.
   Increment the candidate and retry from step 2. Bounded by the number
   of concurrent minters; in practice one or two retries.

4. **Then fill the body and append the INDEX line.** Only after the
   empty file exists do you write its fields and append the projected
   INDEX line. If the process dies between claim and body-write, you
   have an empty malformed record — `audit-artifacts` flags it and
   `reindex` reports it; it never causes an ID collision.

When minting several IDs in one skill run (e.g. `decompose-plan`
writing many tasks), claim each one through steps 1-3 before moving to
the next, so the just-claimed file is counted in the next candidate.

IDs are immutable once claimed. Never renumber to "close a gap" — a gap
from a failed claim is harmless; renumbering breaks every inbound link.

---

## DECISIONS

### `D-NNN.md` fields (in order)

ID and Title are **not** bullet fields — they live in the record's
markdown header, `## D-NNN: <Title>` (this is what `reindex.py` parses;
a record written without that header is flagged malformed). The bullet
fields below follow the header, in order:

- **Status**: `accepted` | `superseded` | `deprecated` | `promoted`
- **Date**: YYYY-MM-DD of acceptance
- **Date superseded**: YYYY-MM-DD when `superseded`; empty otherwise
- **Date deprecated**: YYYY-MM-DD when `deprecated`; empty otherwise
- **Date promoted**: YYYY-MM-DD when `promoted`; empty otherwise
- **Context**: 2-3 sentences on the problem and constraints
- **Decision**: the choice, declarative
- **Rationale**: 2-3 sentences on why this over alternatives
- **Supersedes** / **Superseded by**: optional, intra-scope D-NNN links
- **Promoted to** / **Promoted from**: optional cross-scope link in the
  form `project:D-NNN` or `feature:<name>/D-NNN`

### `decisions/INDEX.md` line format

```
D-NNN | <status> | <title>
```

For superseded: append `(→ D-MMM)` to title. For promoted: append
`(→ project:D-MMM)`. Archived decisions project into
`decisions/archive/INDEX.md` with the identical line format — no extra
marker; the index file they live in is the archived signal.

### Update rules

- New decisions append next unused ID. Create `D-NNN.md`, add INDEX line.
- **Superseding**: old decision's Status → `superseded`, add
  `Date superseded: YYYY-MM-DD` and `Superseded by: D-NNN`. New decision
  file with `Supersedes: D-NNN`. Old body is historical — never edit
  Context/Decision/Rationale.
- **Promotion** is executed by the `promote` skill. Source: Status →
  `promoted`, add `Date promoted: YYYY-MM-DD` and `Promoted to:
  project:D-NNN`. Destination: Status `accepted`, add `Promoted from:
  feature:<name>/D-NNN`. Source body frozen.
- A decision cannot be both superseded and promoted (mutually exclusive).
- IDs immutable. Never delete records.
- **Deprecation** = no longer in effect, no successor. Fill
  `Date deprecated`.
- Dates ISO (YYYY-MM-DD).
- **Implementation status** is not stored. It is derived on demand by
  `reconcile-status` from current task states; see that skill for the
  view rule.

---

## RISKS

### `R-NNN.md` fields (in order)

ID and Title live in the header `## R-NNN: <Title>` (Title states what
breaks, one line), not as bullet fields. The bullet fields below follow
the header, in order:

- **Severity**: `low` | `medium` | `high` | `critical`
- **Likelihood**: `low` | `medium` | `high`
- **Disposition**: `pending` | `fix` | `mitigate` | `accept` | `defer`
- **Date**: YYYY-MM-DD disposition was assigned
- **Date resolved**: YYYY-MM-DD when a `fix` or `defer` risk has been
  explicitly confirmed spent and is ready to reap; empty otherwise
- **Description**: 2-3 sentences on the concrete failure mode
- **Disposition rationale**: 1-2 sentences on why this disposition
- **Action**: links to decision changes, mitigation T-NNN IDs, defer
  triggers, or "none"
- **Related decisions**: optional D-NNN links
- **Promoted to** / **Promoted from**: optional cross-scope link

### `risks/INDEX.md` line format

```
R-NNN | <disposition> | <severity> | <title>
```

For mitigate, append referenced task IDs: `[tasks: T-NNN,T-MMM]`.
Archived risks project into `risks/archive/INDEX.md` with the identical
line format — no extra marker.

### Update rules

- New risks append next ID. Create `R-NNN.md`, add INDEX line.
- `red-team-me` creates with `Disposition: pending`, leaving Date,
  Disposition rationale, and Action empty. `triage-risks` fills them
  in. Persistent `pending` is a bug.
- `triage-risks` writes `mitigate` Actions narratively. `decompose-plan`
  mints concrete `T-NNN` and backfills.
- **Dispositions evolve in place** — update disposition, date,
  rationale. No supersession; the risk is durable, the disposition is
  current state.
- Promotion as for decisions, but risks have no Status field; the
  presence of `Promoted to` is the marker on the source.
- Risk fully resolved by superseding decision: Disposition `fix`,
  Action links to the superseding decision.
- Risk turns out non-issue: Disposition `accept` with rationale
  explaining why. Don't delete.

---

## TASKS

### `T-NNN.md` fields (in order)

ID and Title live in the header `## T-NNN: <Title>` (Title is a
declarative one-liner), not as bullet fields. The bullet fields below
follow the header, in order:

- **Status**: `todo` | `in_progress` | `done` | `dropped`
- **Size**: `S` (<1d) | `M` (1-3d) | `L` (3-10d). Larger must split.
- **Implements**: optional D-NNN links
- **Mitigates**: optional R-NNN links
- **Depends on**: optional T-NNN links
- **Acceptance**: one sentence checkable against a diff
- **Evidence**: optional; one or more code anchors that demonstrate the
  task landed in the tree. Each anchor is one of:
  - `file:<path>` — the path exists (relative to repo root)
  - `grep:<regex>[ in <pathglob>]` — the pattern is found (optionally
    scoped to a path glob)
  - `symbol:<path>:<regex>` — the pattern is found within that file
  - `test:<id-or-path>` — a test file/name expected to exist
  - `git:<short-sha>:<anchor>` — a history pin: one of the four forms
    above, resolved at the named commit (via `git show`/`git grep`)
    instead of the working tree. Used only when the evidenced fact has
    been abolished from the tree — see anchor maintenance under Update
    rules.
  Every anchor must have a non-empty target. `symbol:` always requires
  both a path and a regex; use `file:` only when existence of the file is
  the evidence.
  Anchors assert *presence*, not passing — they are cheap, greppable
  proof that the code referenced by a `done` task is actually there.
  `reconcile-status` resolves them read-only; it does not run tests.
  Prefer the most specific anchor the Acceptance supports: a `symbol:` /
  `grep:` / `test:` that names the behavior beats a bare `file:`, because a
  bare `file:` keeps resolving as long as the file exists at all — it cannot
  notice that the relevant code *inside* was ripped out, which is exactly
  the drift detection is meant to catch. Reserve `file:` for acceptance that
  is genuinely about a file's existence.
- **Date done**: YYYY-MM-DD when `done`; empty otherwise
- **Date dropped**: YYYY-MM-DD when `dropped`; empty otherwise. The
  machine-readable terminal date for a dropped task — it is what the INDEX
  projects in the terminal-date column and what `archive-cycle` reads to
  compact the body. The human "why it was dropped" prose still goes in
  Notes; only the date moves to this field.
- **Notes**: optional; required for `dropped`

At least one of `Implements` or `Mitigates` is required. `Evidence` is
optional but strongly encouraged on any task whose Acceptance maps to a
specific file, symbol, or test — it is what lets `reconcile-status`
detect plan-vs-code drift rather than trusting the status flag.

### `tasks/INDEX.md` line format

```
T-NNN | <status> | <size> | <implements/mitigates> | <title> | <terminal date>
```

`<terminal date>` is status-gated: `Date done` for a `done` task,
`Date dropped` for a `dropped` task, and empty for `todo`/`in_progress`
even if a stale terminal-date field remains after reopening. Archived
tasks project into `tasks/archive/INDEX.md` with the identical line format
— no extra marker.

### Update rules

- New tasks append next ID. Create `T-NNN.md`, add INDEX line.
- IDs immutable. Status evolves in place.
- Status lifecycle: `todo` → `in_progress` → `done` (or → `dropped`
  from any non-done state). Flip to `in_progress` when work actually
  starts — this is the only writer of `in_progress`, and it is what
  `reconcile-status` reads to derive a decision as `in_progress` rather
  than `not_started`. Skipping straight from `todo` to `done` is allowed
  for trivially small tasks but loses that in-flight signal.
- `dropped` is a terminal state, not a deletion. Fill **Date dropped**
  (the terminal date the INDEX projects and compaction reads) and a
  **Notes** line saying why. Reverting from `dropped`: clear Date dropped.
- Status → `done`: fill Date done, and fill `Evidence` with the
  anchor(s) for the code that landed if the task maps to a concrete
  file/symbol/test. Reverting from `done`: clear Date done; leave
  Evidence (stale-but-harmless) or update it.
- `Evidence` anchors assert presence only. A `done` task whose Evidence
  anchors no longer resolve is plan-vs-code drift, surfaced by
  `reconcile-status` (not stored).
- Anchor maintenance — the same-edit rule: an edit that moves or
  abolishes an anchor's target repairs the anchor **in that same
  change**, in one of two modes. **Moved** (the evidenced fact persists
  under a new path/string — a rename or relocation): repoint the anchor
  to the equivalent new target. **Abolished** (the fact ceases to exist
  in the working tree): pin the anchor to history instead — rewrite it
  as `git:<short-sha>:<original-anchor>`, where the sha names the last
  commit at which the original anchor resolved. Pinning preserves the
  original claim verbatim; never rewrite a record's prose to match a
  rename. The mode is determined by whether the original claim still has
  a truthful in-tree witness — not by editor judgment.
- A dangling anchor discovered on a `done` task later (by
  `audit-artifacts` or `reconcile-status`) is repaired per the same
  rule — repointed if the fact moved, pinned at the last resolving
  commit if it was abolished — not merely reported.
- On a `done` task, every `Evidence` entry MUST be a machine-resolvable
  anchor (`file:<path>` / `grep:<regex>[ in <pathglob>]` /
  `symbol:<path>:<regex>` / `test:<id-or-path>`), not prose or a malformed
  placeholder — `audit-artifacts` treats malformed Evidence on a done task
  as a Blocker, because it cannot be resolved against the tree. Human
  explanation belongs in `Notes` (or, post-archival, `Summary`).
- Every task carries `Implements` and/or `Mitigates`; every reference
  resolves (active or archive in same scope).
- Decision/mitigation with zero pointing tasks is a smell — built,
  environmental, or planning gap. `audit-artifacts` surfaces.
- Size in absolute days. Anything >L must split.
- Promotion does not migrate tasks. Existing tasks keep feature-level
  references; new tasks target the promoted (project-level) ID.

---

## RETROSPECTIVES

### `YYYY-MM-DD.md` fields

- **Cycle**: period covered (date range, milestone, "since last
  retrospective", phase). Human-readable; the boundary is part of the
  record.
- **Boundary-end**: YYYY-MM-DD, the last day this retrospective's cycle
  covers — the **machine-readable** end of the period `Cycle` describes
  in prose. This is the authoritative compaction frontier: `archive-cycle`
  compacts a reaped record only once its terminal date is at or before
  some retrospective's `Boundary-end`. Required, and must be ISO — a
  retrospective without a parseable `Boundary-end` advances no frontier,
  so compaction safely skips (records stay uncompacted in `archive/`).
  For an open-ended "since last retrospective" cycle, set it to the day
  the retro is written (anything terminal up to then was harvestable).
- **Date**: YYYY-MM-DD written
- **Evidence**: ID + one-line observation, grouped by category:
  - *Risks that materialized*
  - *Risks that didn't*
  - *Decisions that held*
  - *Decisions that broke*
  - *Silent reversals*
  - *Tasks dropped mid-cycle*
  - *Spikes*
  - *Prior watch-for review*
- **Patterns**: 2-5 calibration claims, each backed by ≥2 evidence
  items
- **Watch-for**: 3-7 items for next cycle, each tied to a pattern
- **Prior watch-for review**: which prior items came true, partially,
  or didn't, plus misses (hits not on the list). Empty on first retro
  only.

### `retrospectives/INDEX.md` line format

```
YYYY-MM-DD | <cycle name>
```

### Update rules

- New retros append. Create `YYYY-MM-DD.md`, add INDEX line.
- Prior retros never rewritten — editing destroys calibration signal.
- Retros do not modify decisions/risks/tasks. Recommendations live in
  Watch-for; supersession flows through `challenge-me`, re-triage
  through `triage-risks`.
- Watch-for is the load-bearing output for next-cycle `challenge-me`
  and `red-team-me`.
- Prior watch-for review mandatory from second retrospective onward.

---

## ARCHIVAL

Archival is **two decoupled steps with different cadences**, both
performed by `archive-cycle`, which is idempotent and safe to run any
time. Separating them is what bounds the active read path to in-flight
work *within* a cycle, not just at its boundary.

**Step 1 — Reap (continuous; keeps the active set bounded).** The moment
a record reaches a terminal state it is eligible to leave the active
directory. `archive-cycle` runs `~/.treenail/scripts/archive_cycle.py` to move
eligible records from `<artifact>/` to `<artifact>/archive/` with full
prose retained, then rebuilds the active and archive indexes from the
records. Run this on cadence (e.g. session start), not only at cycle
boundaries. Eligibility:

- **Decisions**: Status ∈ {`superseded`, `deprecated`, `promoted`}.
- **Risks**: no disposition reaps purely on its own state, because a
  resolved risk is still a standing concern that next cycle's
  `challenge-me`/`red-team-me` (active-INDEX readers) should keep seeing
  until it has been reviewed.
  - `accept` reaps once a **retrospective has reviewed it** — its `Date`
    is at or before the latest retrospective's `Boundary-end` (the same
    machine-readable frontier compaction uses) — or on explicit user
    confirmation. An accepted risk newer than the last retro stays active
    as live calibration signal.
  - `fix` and `defer` reap **only after user confirmation**, because
    eligibility turns on state the record itself does not carry. A `fix`
    is spent only once its superseding decision is actually
    *implemented*, and implementation status is **not stored** — it is
    derived by `reconcile-status` — so `archive-cycle` must not infer it
    from the record. A `defer` is spent only once its trigger has passed
    without firing, which is a judgment. List both kinds and confirm
    (optionally citing `reconcile-status`'s implementation view for the
    fixing decision) before reaping. Confirmation fills `Date resolved`
    so later compaction has a machine-readable terminal date.
- **Tasks**: Status ∈ {`done`, `dropped`}. No date gate — full prose is
  preserved in `archive/`, so the retrospective can still read it.

**Step 2 — Compact (at cycle boundary; shrinks consumed records).** Once
a retrospective has covered a record's terminal date, its prose has been
harvested and the body can collapse. The consumption frontier is the
latest retrospective's machine-readable **Boundary-end** field (never
fuzzy `Cycle` prose): `archive-cycle` compacts any *uncompacted* record
in `archive/` whose terminal date is at or before that frontier. The
eligible set is computed deterministically by
`~/.treenail/scripts/compact-select.py` — **if no retrospective carries a
parseable `Boundary-end`, that script reports no frontier and compaction
is skipped entirely** (reaped records simply stay uncompacted in
`archive/`, already off the hot path, until a retrospective harvests
them). This fail-safe is what guarantees prose is never collapsed before
a retrospective could read it. For each eligible record, replace the body
with a 3-5 line summary preserving ID, Title, terminal Status/Disposition,
the **projection-required fields** so the compacted record still yields a
complete INDEX line (**Size** for tasks, **Severity** — and Likelihood —
for risks; decisions need only Status), terminal date (`Date superseded` /
`Date deprecated` / `Date promoted` / `Date resolved` / `Date done` /
`Date dropped` as applicable), **Date archived** (today), key links
(Superseded by / Supersedes / Promoted to / Promoted from / Implements /
Mitigates / Action / **Evidence**), and a 1-2 sentence collapse of the
prose. Full content remains in git history.

A record with no parseable terminal date is **held uncompacted** —
`compact-select.py` lists it as `SKIP` rather than guessing, and it stays
full-bodied in `archive/` (harmless — already off the hot path) until it
carries one. A well-formed terminal record always has one (`Date
superseded` / `Date deprecated` / `Date promoted` for terminal decisions,
`Date` for accepted risks, `Date resolved` for confirmed fix/defer risks,
`Date done` for a `done` task, `Date dropped` for a `dropped` task), so a
SKIP flags a record whose terminal date field was left empty — that is the
thing to fix, not a permanent state.

So `archive/` holds two states:

- **archived, uncompacted** — full body, terminal, not yet consumed by a
  retrospective. No `Date archived`/`Summary`. Out of the active read
  path but fully readable (the retrospective reads these).
- **archived, compacted** — ≤5-line summary, post-retrospective; has
  `Date archived` + `Summary`.

Compaction never moves the INDEX line — the reap already moved it to
`<artifact>/archive/INDEX.md`, and a record's archive-index line is
identical whether its body is still full or compacted (location, not
compaction state, is what the line encodes). Status, Disposition, and
supersession links never change at either step — archival is never a state
transition. The only field archive-cycle may add during reap is `Date
resolved` on explicitly confirmed `fix`/`defer` risks.

Skills resolving an ID reference must check both `<artifact>/<ID>.md`
and `<artifact>/archive/<ID>.md`. Likewise, skills that need the full
record set — `reconcile-status` (because `done` tasks are reaped),
`retrospective`, and `audit-artifacts`' cross-link pass — read both
`<artifact>/INDEX.md` and `<artifact>/archive/INDEX.md`; day-to-day
skills read only the active `<artifact>/INDEX.md`.

---

## SCOPE

- Project-level: `/docs/<artifact>/`
- Feature-level: `/docs/features/<feature>/<artifact>/`

Same structure both. IDs are scope-local — a feature's D-001 is not
the project's D-001.

Cross-scope linking only via `Promoted to` / `Promoted from`, in the
form `project:D-NNN` or `feature:<name>/D-NNN`. The `promote` skill
executes the mechanics.

Phased plans (from `sequence-work`) are ephemeral and do not follow
these conventions. `tasks/` is durable; the ordering over tasks is
not.

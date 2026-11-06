# Conventions — Exemplars

Companion to `CONVENTIONS-SPEC.md`. Loaded only when a human is
tailoring conventions; skills load the spec only.

---

## Decision exemplar — accepted

`docs/decisions/D-007.md`:

```markdown
## D-007: Use Postgres as the primary datastore

- **Status**: accepted
- **Date**: 2025-03-14
- **Context**: We need a transactional store for billing records and
  user accounts. Reads are modest; writes must be consistent across
  tenants; we expect to add full-text search and vector similarity
  later.
- **Decision**: Use Postgres 15+ as the primary datastore. Enable the
  `pgvector` extension from day one. No second datastore until we have
  measured evidence that Postgres is the bottleneck.
- **Rationale**: Postgres handles our transactional needs and extends
  to search and embeddings without introducing a second system. A
  single datastore is simpler to back up, monitor, and reason about.
  Moving off Postgres later is painful but well-trodden; running two
  stores now is complexity we can't yet justify.
```

INDEX line: `D-007 | accepted | Use Postgres as the primary datastore`

---

## Decision exemplar — superseding pair

`docs/decisions/D-011.md` (after supersession):

```markdown
## D-011: Authenticate via session cookies

- **Status**: superseded
- **Date**: 2025-04-02
- **Context**: We need to authenticate browser clients for the admin
  panel. No mobile or third-party integrations are in scope yet.
- **Decision**: Issue opaque session cookies signed server-side, stored
  in Postgres, 14-day sliding expiry.
- **Rationale**: Cookies are the path of least resistance for a
  browser-only product. Opaque tokens keep us from leaking claims on
  the wire. JWTs would add revocation complexity we don't need.
- **Superseded by**: D-019
```

`docs/decisions/D-019.md`:

```markdown
## D-019: Authenticate via OAuth2 with refresh tokens

- **Status**: accepted
- **Date**: 2025-08-21
- **Context**: Mobile clients and a partner integration are now in
  scope. Session cookies don't extend cleanly to non-browser clients,
  and the partner requires standards-based auth.
- **Decision**: Use OAuth2 authorization code flow with PKCE. Access
  tokens are short-lived JWTs (15 min); refresh tokens are opaque and
  stored server-side with per-device revocation.
- **Rationale**: OAuth2 is the expected shape for mobile and
  third-party clients. Short-lived JWTs avoid the revocation problem
  for access tokens; opaque refresh tokens keep revocation under our
  control where it matters.
- **Supersedes**: D-011
```

INDEX lines:
```
D-011 | superseded | Authenticate via session cookies (→ D-019)
D-019 | accepted   | Authenticate via OAuth2 with refresh tokens
```

When D-011 became `superseded`, the next `archive-cycle` reap moved it
to `docs/decisions/archive/D-011.md` **with its full body intact** —
out of the active read path immediately, not waiting for a cycle
boundary — and moved its line out of `decisions/INDEX.md` into
`decisions/archive/INDEX.md`. Later, once a retrospective had harvested
it, a subsequent `archive-cycle` compacted that body in place:

```markdown
## D-011: Authenticate via session cookies

- **Status**: superseded
- **Date**: 2025-04-02
- **Date archived**: 2025-10-01
- **Superseded by**: D-019
- **Summary**: Browser-only opaque session cookies, replaced by D-019
  when mobile/partner scope required standards-based auth.
```

After the reap, the active `decisions/INDEX.md` no longer lists D-011 —
only D-019 remains:
```
D-019 | accepted   | Authenticate via OAuth2 with refresh tokens
```
and D-011's line now lives, unchanged, in `decisions/archive/INDEX.md`
(same format, no marker — its location is the archived signal):
```
D-011 | superseded | Authenticate via session cookies (→ D-019)
```

---

## Decision exemplar — promotion (feature → project)

Source — `docs/features/oauth-login/decisions/D-003.md`:

```markdown
## D-003: Use OAuth2 PKCE for all first-party clients

- **Status**: promoted
- **Date**: 2025-09-15
- **Context**: Originally scoped to the mobile login feature; other
  surfaces subsequently adopted the same flow.
- **Decision**: Use OAuth2 authorization code flow with PKCE for every
  first-party client. Refresh tokens are opaque, stored server-side.
- **Rationale**: PKCE avoids shared secrets on the client. A single
  flow across clients simplifies server-side auth logic.
- **Promoted to**: project:D-042
```

Destination — `docs/decisions/D-042.md`:

```markdown
## D-042: Use OAuth2 PKCE for all first-party clients

- **Status**: accepted
- **Date**: 2025-10-03
- **Context**: Originally scoped to the mobile login feature; other
  surfaces subsequently adopted the same flow.
- **Decision**: Use OAuth2 authorization code flow with PKCE for every
  first-party client. Refresh tokens are opaque, stored server-side.
- **Rationale**: PKCE avoids shared secrets on the client. A single
  flow across clients simplifies server-side auth logic.
- **Promoted from**: feature:oauth-login/D-003
```

---

## Risk exemplar — defer

`docs/risks/R-004.md`:

```markdown
## R-004: Postgres write throughput becomes the bottleneck under tenant growth

- **Severity**: high
- **Likelihood**: medium
- **Disposition**: defer
- **Date**: 2025-03-20
- **Description**: Our current per-tenant write pattern issues a burst
  of small transactions at billing-cycle boundaries. At ~500 active
  tenants on a single Postgres instance we expect to see lock
  contention on the invoice table during month-end.
- **Disposition rationale**: We're at 40 tenants. The risk is real but
  the trigger is far enough away to revisit rather than pre-solve, and
  pre-solving would commit us to partitioning we might not need.
- **Action**: Revisit when active tenant count exceeds 300, or when
  month-end write latency p95 exceeds 500ms, whichever comes first.
  Add a dashboard alert on the latter now.
- **Related decisions**: D-007
```

INDEX line: `R-004 | defer | high | Postgres write throughput becomes the bottleneck under tenant growth`

---

## Risk exemplar — mitigate

`docs/risks/R-009.md`:

```markdown
## R-009: Refresh tokens leaked from client storage enable long-term access

- **Severity**: high
- **Likelihood**: medium
- **Disposition**: mitigate
- **Date**: 2025-08-23
- **Description**: Refresh tokens live in mobile client storage. If a
  device is compromised or a token is exfiltrated, the attacker
  retains access until manual revocation.
- **Disposition rationale**: Can't eliminate — refresh tokens have to
  live somewhere on the client — but can bound the blast radius with
  rotation and anomaly detection.
- **Action**: Implement refresh token rotation on every use;
  invalidate the entire token family on reuse detection; log device
  fingerprint per refresh and alert on fingerprint changes. Tracked
  as tasks T-031, T-032, T-033.
- **Related decisions**: D-019
```

INDEX line: `R-009 | mitigate | high | Refresh tokens leaked from client storage [tasks: T-031,T-032,T-033]`

---

## Risk exemplar — accept

`docs/risks/R-012.md`:

```markdown
## R-012: CSV export can be triggered by any authenticated user

- **Severity**: low
- **Likelihood**: low
- **Disposition**: accept
- **Date**: 2025-09-01
- **Description**: Any authenticated user can request a CSV export of
  their own data. A malicious user could script repeated exports to
  load the export worker.
- **Disposition rationale**: Users can only export their own data, so
  the blast radius is self-inflicted. Rate-limiting adds complexity
  for a problem we haven't seen and couldn't meaningfully worsen.
- **Action**: None. Revisit if export worker load becomes a real
  signal.
- **Related decisions**: D-014
```

---

## Task exemplar — done

`docs/tasks/T-031.md`:

```markdown
## T-031: Rotate refresh token on every use

- **Status**: done
- **Size**: M
- **Implements**: D-019
- **Mitigates**: R-009
- **Depends on**: T-029
- **Acceptance**: every successful /token/refresh call invalidates the
  old refresh token and issues a new one; tests cover
  reuse-after-rotation.
- **Evidence**:
  - `symbol:src/auth/refresh.ts:rotateRefreshToken`
  - `grep:invalidate.*old refresh in src/auth`
  - `test:auth/refresh.reuse.spec.ts`
- **Date done**: 2025-09-04
```

The `Evidence` anchors let `reconcile-status` confirm D-019 is actually
implemented in the tree — if `rotateRefreshToken` is later renamed or
the test removed, reconcile flags *implementation drift* instead of
trusting the `done` flag.

INDEX line: `T-031 | done | M | D-019,R-009 | Rotate refresh token on every use | 2025-09-04`

---

## Task exemplar — dropped

`docs/tasks/T-044.md`:

```markdown
## T-044: Add per-device anomaly detection on refresh

- **Status**: dropped
- **Size**: L
- **Mitigates**: R-009
- **Acceptance**: log device fingerprint per refresh and alert on
  fingerprint changes.
- **Date done**:
- **Date dropped**: 2025-10-12
- **Notes**: D-019 was superseded by D-023 (move to hardware-attested
  clients), which makes fingerprint anomaly detection redundant. See
  T-061 for the replacement.
```

The terminal date lives in `Date dropped` (a parseable field), not in the
Notes prose, so `reindex.py` can project it and `archive-cycle` can compact
the body once a retrospective has covered it.

INDEX line: `T-044 | dropped | L | R-009 | Add per-device anomaly detection on refresh | 2025-10-12`

---

## Retrospective exemplar

`docs/retrospectives/2025-10-01.md`:

```markdown
## Retrospective: 2025-10-01

- **Cycle**: OAuth rollout phase — covers 2025-07-01 through
  2025-09-30, from D-019 acceptance through mobile GA.
- **Boundary-end**: 2025-09-30
- **Date**: 2025-10-01
- **Evidence**:
  - *Risks that materialized*: R-009 (refresh token leakage) — one
    customer incident 2025-08-30; mitigations T-031/T-032 contained
    blast radius. R-014 (over-long JWT expiry) flipped from `accept`
    to `fix` after ops feedback.
  - *Risks that didn't*: R-017 (mobile library compatibility) —
    `defer` trigger well outside firing range. R-021 (refresh rotation
    thundering herd) — `mitigate` guard shipped but no observed
    contention; likely over-engineered.
  - *Decisions that held*: D-019 (OAuth2 PKCE) — implemented, no
    supersession, no materialized related risks.
  - *Decisions that broke*: D-014 (session cookie fallback for admin
    panel) superseded by D-023.
  - *Silent reversals*: none this cycle.
  - *Tasks dropped mid-cycle*: T-044 (per-device anomaly detection)
    — D-019 superseded, approach redundant. T-051 — redundant with
    T-047.
  - *Spikes*: T-003 (hardware-attested client feasibility) resolved
    cleanly into D-023. T-005 re-split twice — signal that D-017 was
    taken prematurely in challenge-me.
  - *Prior watch-for review*: previous retro flagged "watch for
    auth-flow coupling to admin panel" — hit (drove D-023). Flagged
    "watch for refresh-rotation contention" — did not hit. Missed
    entirely: no watch-for covered hardware-attested clients, which
    ended up driving the cycle's biggest supersession.
- **Patterns**:
  - We over-predict contention risks (R-021 this cycle, two
    prior-cycle examples). Red-team-me should stop front-loading
    thundering-herd scenarios until we have traffic evidence.
  - We under-predict scope-drift supersessions (D-014 → D-023, D-011 →
    D-019 last cycle). Challenge-me's "what's in scope" framing
    freezes too early.
  - Spike re-splits reliably indicate a premature decision upstream
    (T-005 this cycle, T-002 last cycle). Treat re-splits as a signal
    to revisit the triggering decision, not just the spike.
- **Watch-for** (next cycle):
  - Auth boundaries for partner integrations — D-019 was scoped to
    first-party clients; partner scope is likely next.
  - Don't let red-team-me lead with contention scenarios without a
    load-evidence anchor.
  - Any spike that re-splits once: pause and re-challenge the upstream
    decision before re-splitting further.
  - Admin panel surfaces still assuming cookie auth — D-014 is
    superseded but code paths may not have caught up.
- **Prior watch-for review**: see Evidence → Prior watch-for review.
```

INDEX line: `2025-10-01 | OAuth rollout phase`

---

## Interview exemplar — rationale-first (challenge-me step 5b)

Every interview question must carry a visible recommendation **and** its
rationale as prose, so the user sees *why this over the alternatives* —
never just the bare question, and never only terse option labels. This is
the empirical premise the equal-support reframe (D-028) rests on: it holds
identically whether the agent asks in prose or also renders a structured
option list.

**Prose surface (Codex CLI, opencode) — required shape:**

```
For the primary datastore I recommend Postgres 15+ with pgvector from day
one, rather than adding a second store for search later — one system is
simpler to back up and reason about, and pgvector covers the embedding
need we anticipate. Does that work, or do you want search/vector handled
by a dedicated store?
```

**Structured-option surface (Claude Code `AskUserQuestion`) — the prose
recommendation + rationale still appears in the message, and does not
collapse into the option descriptions:**

```
I recommend Postgres 15+ with pgvector — one system to back up and reason
about, and it extends to search/embeddings without a second store. Picking
a dedicated search store instead only pays off once Postgres is measurably
the bottleneck, which we have no evidence of yet.

  [ Postgres + pgvector (Recommended) ]  [ Postgres + dedicated search store ]
```

Anti-pattern — a question with no visible "why" (option labels are **not**
a rationale):

```
Which datastore?
  [ Postgres + pgvector ]  [ Postgres + dedicated search store ]
```

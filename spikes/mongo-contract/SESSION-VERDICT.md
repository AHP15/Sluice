# T-046 — What `mongodb::ClientSession` delivers for an editor tab

**Task**: T-046 · **Mitigates**: R-039
**Run date**: 2026-08-02 · **Crate**: `mongodb` 3.8.0 · **Server**: `mongo:7`,
single-node replica set.
Probe: `src/session_probe.rs`. Run alongside T-073 against the same instance, as
R-039's Action directs.

## Verdict

**Mongo should declare `affinity: session`.**

R-039's premise is confirmed on every point that matters. `Client::start_session()`
returns a real, ownable, pinnable logical session that carries both properties an
editor tab needs. Declaring `affinity: none` would have stated in the contract and
in the UI that interactive transactions are unavailable on Mongo **when they
demonstrably work**.

## Evidence

| Property | Probe | Result |
|---|---|---|
| Session handle exists | `client.start_session().await` | ✅ owned `ClientSession`, server-assigned `lsid` |
| Interactive multi-document transaction | 2 collections + app-controlled 250ms await, held open | ✅ `outside=0, inside=1` — snapshot isolation held |
| Commit | `commit_transaction()` | ✅ visible outside after commit |
| Abort | `abort_transaction()` | ✅ 0 of 1 rows visible |
| Causal consistency | `start_session().causal_consistency(true)` | ✅ session tracked `operationTime` after write |

The transaction test is the load-bearing one. The transaction stayed open across
an await point **the application controls**, spanning two collections, and a
concurrent non-session read could not see the uncommitted writes. That is a
genuinely interactive transaction — not an all-in-one-call
`with_transaction(closure)` helper, which would *not* have been enough for an
editor tab, because a tab's transaction boundary is driven by the user, not by a
callback the driver runs to completion.

Causal consistency gives a tab read-your-own-writes across statements even when
they land on different members of a replica set — the property that makes a
sequence of statements in one tab behave like one coherent session.

## Two corrections to R-039's text

**1. The API surface named in the risk no longer exists.** R-039 refers to
`..._with_session` operation variants. Those are the **mongodb 2.x** API; 3.x
removed them in favour of `.session(&mut s)` on the action builder:

```rust
// R-039's text (2.x, will not compile against 3.x):
coll.find_with_session(filter, opts, &mut session).await?;
// Actual 3.x:
coll.find(filter).session(&mut session).await?;
```

The risk's substance is correct — the capability is there. Only the spelling is
stale. Worth knowing before T-045 is written from the risk text verbatim.

**2. A replica set is mandatory.** A standalone `mongod` cannot open a
transaction at all. Any future Mongo integration test, and any user pointing
Sluice at a standalone dev server, gets *no* transactions regardless of the
declared capability. `affinity: session` is a statement about the deployment as
much as the engine.

## The caveat that does not change the verdict but does change the UX

**`transactionLifetimeLimitSeconds` = 60** (the server default, read live from
`getParameter`).

The server **aborts any transaction older than 60 seconds**. An editor tab that
opens an explicit transaction and sits idle while the user thinks will be killed
server-side, and the next statement fails with a transient error.

Postgres and SQL Server hold an idle transaction open indefinitely. So the
pinning capability alone does **not** make the explicit-transaction tab UX
portable across the three engines: `affinity` says a session *can* be pinned, and
says nothing about **how long the pin survives**.

Two consequences for T-041 and T-045:

- Mongo editor tabs with an open explicit transaction need either a keepalive or
  an honest "your transaction expired" surface. Silently reconnecting is exactly
  what R-037 forbids.
- Consider whether the capability model needs a companion field — a declared
  maximum idle-transaction lifetime — rather than leaving a 60-second server-side
  guillotine undeclared. This is a **new risk**, not covered by the recorded set;
  logged for `triage-risks` rather than decided here.

## Consequences for the recorded plan

- **T-045** should declare `SessionAffinity::Session` for Mongo, and be written
  against the 3.x `.session()` builder API, not R-039's 2.x spelling.
- **R-039 is retired on evidence.** The guard worked: the value was left unset
  until checked, and checking overturned the first reading.
- The `ClientSession` handle is owned and lifetime-free, so it can be held by the
  pool for a tab's lifetime **without appearing in any `Driver` method
  signature** — D-003's no-handles-in-signatures rule is not threatened.

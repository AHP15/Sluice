# T-072 — tiberius surface verdict

**Task**: T-072 · **Mitigates**: R-024
**Run date**: 2026-08-02
**Crate**: `tiberius` 0.12.3, features `["tds73", "rustls", "chrono"]`
**Server**: `mcr.microsoft.com/mssql/server:2022-latest`, linux/amd64 under
emulation, SQL authentication.

Reproduce:

```sh
docker run -d --name sluice-spike-mssql --platform linux/amd64 \
  -e ACCEPT_EULA=Y -e MSSQL_SA_PASSWORD='Sluice_Spike_2026!' -e MSSQL_PID=Developer \
  -p 14333:1433 mcr.microsoft.com/mssql/server:2022-latest
cd spikes/tiberius-probe && cargo run
```

> ## ⚠ Scope limit — this verdict is INCOMPLETE against T-072's acceptance
>
> T-072 requires a verdict "against the real SQL Server instances in daily use",
> specifically covering "the auth modes actually used". This run was against a
> **container**, which accepts SQL authentication only. **The auth-mode question —
> the single thing R-024 is most concerned about — remains unvalidated.** Four of
> the five required surfaces are answered; auth is not. T-072 is therefore held at
> `in_progress`, not `done`.

## Summary

| Surface | Verdict |
|---|---|
| Auth — SQL Server authentication | ✅ COVERED |
| Auth — Windows / AD / integrated | ⚠️ **UNTESTED — blocking** |
| TLS | ✅ COVERED, verification is real |
| TDS type coverage | ⚠️ PARTIAL — 20/23, **3 panics** |
| Type exactness | ⚠️ PARTIAL — `MONEY` is lossy |
| Connection reset | ❌ MISSING — confirms R-012 |
| Query cancellation | ❌ MISSING — worse than "limited" |

## Findings

### 1. ❌ Three ordinary types make tiberius **panic**, not error

`sql_variant`, `geography`, and `hierarchyid` hit an unreached `todo!()` in
`tiberius/src/tds/codec/type_info.rs:317` and **unwind the thread**. These are not
exotic: `sql_variant` appears in real audit and EAV schemas, `geography` in any
spatial column, `hierarchyid` in org trees. A user browsing such a table
panics the driver.

Measured behaviour after catching the unwind: **the connection remained usable**
(`SELECT 1` succeeded). That is better than assumed, but it is an accident of
where the `todo!()` sits, not a guarantee.

Consequences:

- **T-006 is mandatory, not defensive.** D-003's `catch_unwind` rule now has a
  concrete, reproducible trigger reachable from ordinary user data.
- **R-022's premise is confirmed and refined.** The risk assumed a caught panic
  leaves poisoned state. Here it did not — but the driver must still treat a
  post-panic connection as suspect rather than trusting this observation.
- Also worth noting: `tiberius::Row::get` **panics** on a type mismatch;
  `try_get` is the non-panicking form. The probe hit this itself on `@@SPID`
  (`smallint`, not `int`). Any Sluice code must use `try_get` exclusively.

### 2. ❌ Query cancellation is not available, and the failed attempt costs the connection

tiberius exposes **no cancel/attention API**. The only lever is dropping the
query future. Measured:

- tokio timeout fired at ~800ms against `WAITFOR DELAY '00:00:10'`
- **the connection was unusable afterwards — reuse itself hung**
- **the server-side request kept running** (`sys.dm_exec_requests` still showed
  the `WAITFOR` on that spid)

So dropping the future cancels nothing: it abandons a live server-side query
*and* burns the connection. R-024 records that limited attention handling was
"already accepted", but the accepted position understates it — this is not
"cancellation is slow", it is "cancellation does not exist and attempting it
leaks a connection and a server-side query".

**T-051 ("real query cancellation") cannot be honoured on SQL Server through
tiberius alone.** The workable fallback is a second connection issuing
`KILL <spid>`, which requires `ALTER ANY CONNECTION` — a permission many
day-to-day accounts will not have.

### 3. ❌ No reachable connection-reset primitive — R-012 confirmed

`sp_reset_connection` is a **TDS protocol RPC** signalled by a bit in the packet
header, not callable T-SQL. Executing it as a statement fails with
`Invalid object name 'sp_reset_connection'` (error 208), and tiberius exposes no
API to set the header bit.

Verified the session stays dirty across the attempt: `@@LOCK_TIMEOUT` remained
`1234` and the `#spike_temp` temp table was still present.

Consequences: D-028's pool must declare `ResetSupport::ReopenOnly` for SQL
Server — exactly the connect-per-query degradation **R-036** warns about, which
makes **T-040** (reset only connections actually dirtied) load-bearing rather
than an optimisation.

### 4. ⚠️ `MONEY` cannot be round-tripped losslessly

Measured against the server's own text rendering:

| Type | Server | tiberius | |
|---|---|---|---|
| `MONEY` | `922337203685477.5807` | `922337203685477.6` | ❌ **LOSSY** |
| `SMALLMONEY` | `214748.3647` | `214748.3647` | ✅ exact |
| `DECIMAL(38,20)` | `1.00000000000000000001` | `1.00000000000000000001` | ✅ exact |

`MONEY` is an exact `decimal(19,4)` on the server, but tiberius decodes it into
`ColumnData::F64` — the precision is destroyed *before the driver has a value*,
so no amount of care in Sluice's own encoding layer recovers it. **This directly
contradicts D-006/T-016's lossless requirement.** `DECIMAL` is safe, decoded via
`ColumnData::Numeric`; `SMALLMONEY` survives only because its range fits f64.

Workaround for T-016: have the browse compiler emit `CONVERT(VARCHAR(64), col, 2)`
for `money`/`smallmoney` columns and carry them as `WireValue::Numeric` text.
This is a per-engine special case in the browse compiler, which T-009 should
budget for.

*(The probe also flagged `float` as lossy; that is a false positive — the server
rendered `1.000000000000000e-001` and tiberius `0.1`, the same value in different
notation. `FLOAT` is a binary float on both sides and is not an exactness claim.)*

### 5. ✅ TLS behaves correctly, and verification is real

- `EncryptionLevel::Required` + `trust_cert()` → connects.
- `EncryptionLevel::Required` **without** `trust_cert()` → **refused**
  (`invalid peer certificate: Other(UnsupportedCertVersion)`).

Verification is genuinely enforced, so **T-021 can default to verify-on** and
require an explicit per-profile opt-out. One nuance: the rejection here was for
certificate *version* (SQL Server's auto-generated self-signed cert is X.509 v1,
which rustls refuses outright) rather than an untrusted chain. The default
posture is correct either way, but the error surfaced to users for the
overwhelmingly common "self-signed dev server" case will be this confusing
`UnsupportedCertVersion` string — **T-066's error model should special-case it**
into "this server's certificate is self-signed; enable 'trust server certificate'
for this profile".

Note the backend matters: with tiberius's default `native-tls` feature the
handshake failed outright (`Tls("connection closed via error")`). **`rustls` is
the working backend** on macOS against SQL Server 2022; this should be pinned in
the app's Cargo.toml, not discovered again later.

### 6. ✅ SQL authentication works; ⚠️ everything else is unvalidated

`AuthMethod::sql_server(user, pass)` connected cleanly. On the modes that matter
for R-024, established by reading the crate's API rather than by execution:

- `AuthMethod::windows(user, pass)` exists — NTLM, behind the `winauth` feature,
  and effective only from a Windows host.
- `AuthMethod::aad_token(token)` exists, but takes a **caller-supplied bearer
  token**; tiberius does not acquire AAD tokens itself, so Sluice would own the
  entire MSAL/device-code flow.
- **There is no integrated/SSPI single sign-on from a non-Windows host.**

If the daily-use instances are reached with Windows integrated auth from a Mac,
that is a hard gap and no amount of type coverage compensates for it.

## Verdict

**Conditional go on the tested surface; the decision gate is auth, and it is
still open.**

Nothing found so far justifies invoking R-024's escape path (reordering to Mongo
second, or reopening D-001's no-sidecar rule). The type layer is good, TLS is
good, and the two hard gaps — reset and cancellation — were both already known
(R-012, and cancellation "already accepted" in R-024), though **cancellation is
materially worse than the accepted position described**, and that is worth
re-triaging.

What would flip this to a no-go: **Windows integrated auth against the real
instances**. That single question carries more weight than everything validated
here, and it cannot be answered from a container.

### Recommended next step

Run this same probe against one real instance with the auth mode actually used
day to day. It needs only a host, a port, and an `AuthMethod` swap in
`base_config()`. Until then R-024 is **not** retired.

### Items for other tasks

- **T-006** — panic containment now has a reproducible trigger; use `try_get`
  everywhere; treat post-panic connections as suspect.
- **T-009 / T-016** — budget a per-engine `CONVERT` special case for `MONEY`.
- **T-021** — verify-on by default is validated as workable.
- **T-040** — promoted from optimisation to load-bearing by finding 3.
- **T-051** — needs an explicit decision on the `KILL <spid>` fallback and its
  permission requirement. **This is a new decision, not covered by the recorded
  set**; routed to `challenge-me` rather than settled here.
- **T-066** — special-case the `UnsupportedCertVersion` TLS error.
- **T-068** — the 23 cases in `probe_types` are the seed for the SQL Server half
  of the type-fidelity corpus.

## Re-run 2026-11-06 (Docker, same image) and a correction

Re-ran against `mcr.microsoft.com/mssql/server:2022-latest` (linux/amd64 under
emulation, SQL auth). **Every verdict above reproduced unchanged**: SQL auth
covered; TLS covered with real verification; 20/23 types with `sql_variant`,
`geography` and `hierarchyid` panicking; `MONEY` lossy through `F64`; no
reachable `sp_reset_connection`; no cancel/attention API (dropping the future
left the connection hung and the server-side `WAITFOR` running).

**Correction to the auth finding.** The probe's text says tiberius "does NOT
implement integrated/SSPI single-sign-on against a domain from a non-Windows
host". That is wrong for tiberius 0.12.3: `AuthMethod::Integrated` exists on
Unix behind the `integrated-auth-gssapi` feature (Kerberos via `libgssapi`).
What *is* Windows-only is `AuthMethod::windows(user, pass)` — NTLM with a
`DOMAIN\user` and password. Corrected auth matrix:

| Mode | macOS/Linux client | Windows client |
|---|---|---|
| SQL Server login | ✅ tested | ✅ |
| Integrated (Kerberos SSO) | available (`integrated-auth-gssapi`), **untested** | SSPI (`winauth`), untested |
| NTLM `DOMAIN\user` + password | ❌ not available | `winauth`, untested |
| Entra ID / Azure AD | token only — caller must acquire it (`aad_token`) | same |

The open question is therefore narrower than recorded: it is *which of these
modes the instances in daily use require*. A container cannot answer that, and
cannot exercise Kerberos or NTLM without a domain.

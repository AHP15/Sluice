R-001 | fix | critical | Running the CLI in any directory executes that directory's config as code
R-002 | mitigate | high | Auto-discovered connections bypass every production guardrail [tasks: T-027]
R-003 | mitigate | high | The staged-edit preview can show a statement different from the one executed [tasks: T-057]
R-004 | fix | high | The optimistic-concurrency predicate silently no-ops on NULL and errors on non-comparable types
R-005 | mitigate | high | Cell editors reintroduce the precision loss the wire encoding prevents on read [tasks: T-019]
R-006 | mitigate | high | Tauri IPC offers no backpressure, so streamed batches can exhaust webview memory [tasks: T-049]
R-007 | mitigate | medium | A row-count cap does not bound payload size [tasks: T-050]
R-008 | mitigate | high | Large binary and text values base64-encoded into one payload can kill the webview [tasks: T-018]
R-009 | mitigate | medium | The universal contains and eq operators return different results per engine [tasks: T-015]
R-010 | mitigate | medium | Composite-key keyset pagination degenerates to an unindexed OR-chain on SQL Server [tasks: T-048]
R-011 | fix | high | The mongodb crate owns its own pool, so the pooling and session model does not map to it
R-012 | fix | medium | No connection-reset primitive is reachable on SQL Server, so session state leaks between browse queries
R-013 | fix | medium | Pinned editor sessions exceed the deliberately small pool budget as tabs accumulate
R-014 | mitigate | high | tokio-postgres rejects unknown type OIDs, so exotic types cannot reach the UI as strings [tasks: T-017]
R-015 | mitigate | medium | Sampling documents in natural order surfaces the collection's oldest schema [tasks: T-064]
R-016 | mitigate | medium | Unsigned rebuilds churn the macOS keychain ACL, and signing later can orphan stored secrets [tasks: T-071]
R-017 | mitigate | medium | Credentials entering from the environment bypass the Secret wrapper [tasks: T-022]
R-018 | mitigate | high | The env-var-gated type-fidelity corpus sits outside CI and will rot unnoticed [tasks: T-069]
R-019 | mitigate | medium | Committed profile files leak production infrastructure topology [tasks: T-037]
R-020 | fix | high | The single columns-and-rows result shape cannot express DDL, affected rows, or multiple result sets
R-021 | defer | medium | Capability flags proliferate until the UI is as coupled as the engine checks they replaced
R-022 | mitigate | medium | catch_unwind survives the panic but leaves poisoned locks and leaked connections [tasks: T-006]
R-023 | mitigate | medium | Substring redaction of credentials fails on URL-encoded or split password forms [tasks: T-024]
R-024 | mitigate | high | tiberius feature gaps block SQL Server as the second engine [tasks: T-072]
R-025 | mitigate | medium | The UI offers sort universally, but Mongo sorts on unindexed fields fail at the memory limit [tasks: T-065]
R-026 | mitigate | medium | Mongo $set on array indices and dotted field names writes to the wrong location [tasks: T-062]
R-027 | defer | medium | Lazy introspection cannot supply the foreign-key graph relation navigation needs
R-028 | mitigate | medium | Unrecognized Postgres flavours fall through to vanilla catalog queries and fail mid-navigation [tasks: T-055]
R-029 | mitigate | medium | A Mongo stub written by the contract's author confirms the contract instead of testing it [tasks: T-073]
R-030 | mitigate | medium | The SQL editor writes credentials into query history and logs, around the trust boundary [tasks: T-025]
R-031 | mitigate | high | Consent keyed to the config file's hash ignores everything that file imports [tasks: T-030]
R-032 | mitigate | high | Scrubbing the subprocess environment breaks the configs the subprocess exists to read [tasks: T-031]
R-033 | mitigate | high | The no-keychain-access claim for the config subprocess does not hold on Linux [tasks: T-032]
R-034 | mitigate | high | Null-safe comparison against a string original needs a per-type cast, and without one ordinary edits fail [tasks: T-059]
R-035 | mitigate | medium | The concurrency-comparable exclusion list covers enough real columns to hollow out the guarantee [tasks: T-060]
R-036 | mitigate | high | Reopening the connection on release turns the SQL Server pool into connect-per-query [tasks: T-040]
R-037 | mitigate | high | LRU release of an idle pinned session silently discards the state that made it pinned [tasks: T-043]
R-038 | mitigate | medium | One shared budget lets pinned editor sessions starve browse traffic to zero [tasks: T-044]
R-039 | mitigate | medium | Declaring Mongo affinity: none forecloses ClientSession, which the crate does expose [tasks: T-045,T-046]
R-040 | mitigate | high | Attaching a text span to each outcome forces the in-app SQL parsing the design rejected [tasks: T-011]
R-041 | mitigate | high | The outcome sequence carries no transactional context, so a partial-success report may be false [tasks: T-012]
R-042 | mitigate | medium | Credit-based backpressure across a multi-result script pauses the server mid-transaction [tasks: T-052]
R-043 | mitigate | medium | D-010's row cap is per query, so it no longer bounds one execution [tasks: T-050]
R-044 | fix | high | Replica-set discovery connects around an `ssh -L` tunnel to the members' real hostnames
R-045 | fix | high | Production guardrails evaluate one configured host string, not the servers actually connected
R-046 | mitigate | medium | Through mongos, one profile's pool fills on every router, multiplying the budget [tasks: T-114]
R-047 | mitigate | medium | SRV and TXT handling exist only behind the crate's connection-string parser, so typed options must be re-serialised into a URI [tasks: T-106]
R-048 | mitigate | medium | The default auth source fails the most common local Mongo setup once a database is chosen [tasks: T-110]
R-049 | fix | medium | Refusing unrecognised URL parameters breaks launches from ORM-style URLs that work today
R-050 | fix | medium | D-037's harmlessness test misses parameters some servers require, such as `retryWrites=false`
R-051 | fix | medium | No setting carries a custom CA bundle or client certificate, so common managed databases cannot verify TLS
R-052 | accept | low | Fixing reads to the primary forbids browsing a secondary or a replica set with no primary
R-053 | mitigate | medium | One version-2 profile makes the whole synced file unreadable on an older build [tasks: T-101]
R-054 | mitigate | medium | SRV lookups through the crate's own resolver can ignore OS split-DNS that VPN users depend on [tasks: T-112]
R-055 | mitigate | medium | A single seed with `replicaSet` stops meaning discovery, so the most common replica-set URL loses failover [tasks: T-104]
R-056 | mitigate | low | The loopback-seed refusal rejects standard local Docker replica sets that are not tunnels [tasks: T-107]
R-057 | mitigate | medium | Forwarders on non-loopback addresses are not recognised, so discovery still routes around them [tasks: T-108]
R-058 | mitigate | medium | Honouring `pgbouncer=true` as unnamed statements misses what transaction pooling also breaks [tasks: T-105]
R-059 | fix | medium | Adding the CA to the system roots weakens what `sslrootcert` and `tlsCAFile` users asked for
R-060 | mitigate | low | Certificate and key paths in a synced profile file break on every other machine [tasks: T-100]

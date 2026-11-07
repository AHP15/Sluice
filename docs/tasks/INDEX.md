T-001 | done | M | D-001 | Scaffold Tauri v2 workspace with a Rust core and React webview | 2026-08-02
T-002 | done | M | D-023 | Establish the React UI stack against the WebKitGTK CSS floor | 2026-11-06
T-003 | done | S | D-025 | Rename the project off drizzle-studio and add a permissive license | 2026-11-06
T-004 | done | M | D-019 | Build-only CI matrix publishing unsigned cross-platform artifacts | 2026-11-06
T-005 | done | M | D-003 | Define the Driver trait and capability descriptor | 2026-08-02
T-006 | done | M | D-003,R-022 | Contain driver panics without leaving poisoned or leaked state | 2026-08-02
T-007 | done | L | D-002,D-003 | Implement the Postgres driver on tokio-postgres | 2026-08-03
T-008 | done | M | D-029 | Generic navigator tree of driver-declared node kinds | 2026-08-03
T-009 | done | M | D-029 | Structured browse request compiled per driver | 2026-08-03
T-010 | done | M | D-029 | Statement-outcome sequence as the execution result envelope | 2026-08-03
T-011 | done | M | D-029,R-040 | Dialect-aware script splitting used only for display attribution | 2026-08-03
T-012 | done | M | D-029,R-041 | Transactional context on the execution result | 2026-08-03
T-013 | done | M | D-005 | Core filter operator set with AND/OR tree and type classes | 2026-11-06
T-014 | done | S | D-005 | Driver-declared filter extension operators | 2026-11-06
T-015 | done | M | D-005,R-009 | Literal string matching and declared case-sensitivity for filters | 2026-11-06
T-016 | done | L | D-006 | Lossless wire value encoding with per-column type descriptors | 2026-08-03
T-017 | done | M | D-006,R-014 | Unknown type OID fallback so exotic types reach the UI as strings | 2026-08-03
T-018 | done | M | D-006,R-008 | Per-value size ceiling with on-demand full-value fetch | 2026-08-03
T-019 | done | M | D-006,R-005 | String-preserving cell editors for exact numeric types | 2026-11-06
T-020 | done | M | D-007 | Keychain credential storage with a webview that never sees secrets | 2026-08-03
T-021 | done | M | D-007 | TLS verification defaults and webview hardening | 2026-08-03
T-022 | done | M | D-020,R-017 | Wrap credentials in Secret<String> at every parse boundary | 2026-08-03
T-023 | done | M | D-020 | Shape-only structured logging with verbose diagnostics behind a toggle | 2026-11-06
T-024 | done | M | D-017,D-020,R-023 | Structural credential redaction across the full error chain | 2026-11-06
T-025 | done | M | D-014,R-030 | Redact credential-bearing statements on the history and diagnostics paths | 2026-11-06
T-026 | done | M | D-012 | Context-aware launcher with a fixed argument grammar | 2026-11-06
T-027 | done | M | D-012,R-002 | Ephemeral profile with conservative defaults for discovered connections | 2026-11-06
T-028 | done | S | D-026 | Native config discovery for DATABASE_URL, .env, and JSON/TOML | 2026-08-05
T-029 | done | M | D-026 | Consent-gated TypeScript and JavaScript config evaluation | 2026-08-05
T-030 | done | M | D-026,R-031 | Broaden the consent fingerprint beyond the config file | 2026-11-06
T-031 | done | M | D-026,R-032 | Precise subprocess environment policy with loud failure | 2026-11-06
T-032 | done | M | D-026,R-033 | Platform-accurate consent text and Linux subprocess sandboxing | 2026-11-06
T-033 | done | L | D-014 | Three-store app state with versioning and atomic writes | 2026-11-06
T-034 | done | S | D-014,D-007 | Strip passwords out of imported connection URLs | 2026-11-06
T-035 | done | M | D-014 | Query history caps, purge, and per-profile opt-out | 2026-11-06
T-036 | done | M | D-011 | Portable profile and saved-query file format | 2026-11-06
T-037 | done | S | D-011,R-019 | Correct the safe-to-commit claim on the profile file | 2026-11-06
T-038 | done | S | D-008 | Model transport variants and credential providers in the profile schema | 2026-11-06
T-039 | done | M | D-028 | Per-profile pool with capability-declared connection reset | 2026-11-06
T-040 | done | M | D-028,R-036 | Reset only connections that were actually dirtied | 2026-11-06
T-041 | done | M | D-028 | Pinned editor sessions with opt-in explicit transactions | 2026-11-06
T-042 | done | M | D-028 | Single per-profile connection budget covering pooled and pinned | 2026-11-06
T-043 | done | M | D-028,R-037 | Restrict LRU eviction to provably stateless tabs and never reconnect silently | 2026-11-06
T-044 | done | M | D-028,R-038 | Reserve a browse floor inside the connection budget | 2026-11-06
T-045 | done | S | D-028,R-039 | Three-valued session affinity capability | 2026-11-06
T-046 | done | S | R-039 | Spike: verify what mongodb ClientSession delivers for an editor tab | 2026-08-02
T-047 | done | L | D-010 | Keyset pagination with declared fallback strategy | 2026-11-06
T-048 | todo | M | D-010,R-010 | Per-relation keyset declaration accounting for composite key arity |
T-049 | done | M | D-010,R-006 | Credit-based backpressure protocol for streamed row batches | 2026-11-06
T-050 | done | M | D-010,R-007,R-043 | Per-execution row and byte budget shared across the outcome sequence | 2026-11-06
T-051 | done | M | D-010 | Real query cancellation and per-profile statement timeout | 2026-11-06
T-052 | dropped | M | D-010,R-042 | Decouple socket draining from webview consumption with a spill buffer | 2026-11-06
T-053 | done | L | D-016 | Native catalog introspection loaded lazily with explicit refresh | 2026-11-06
T-054 | done | M | D-016 | Server version and flavour detection into the capability descriptor | 2026-11-06
T-055 | done | M | D-016,R-028 | Probe catalog capabilities rather than trusting the version banner | 2026-11-06
T-056 | done | L | D-027 | Staged edits applied in one transaction behind a unique-key gate | 2026-11-06
T-057 | done | M | D-027,R-003 | Derive the edit preview from the same artifact that executes | 2026-11-06
T-058 | done | M | D-027 | Null-safe optimistic concurrency with declared column comparability | 2026-11-06
T-059 | done | M | D-027,R-034 | Bind concurrency originals with an explicit per-type cast | 2026-11-06
T-060 | done | M | D-027,R-035 | Determine concurrency comparability by probing the catalog | 2026-11-06
T-061 | done | M | D-027 | Database-enforced safety mode and production confirmation | 2026-11-06
T-062 | todo | M | D-027,D-018,R-026 | Classify Mongo document paths as editable or read-only |
T-063 | in_progress | L | D-018 | Column derivation for schemaless collections |
T-064 | todo | M | D-018,R-015 | Sample both ends of a collection and show inference coverage |
T-065 | todo | M | D-018,D-029,R-025 | Declare sortability per relation and per path |
T-066 | done | L | D-017 | Layered error model with normalized kind, hints, and verbatim payload | 2026-08-03
T-067 | done | M | D-021 | Unit tests over pure driver-independent logic | 2026-11-06
T-068 | done | M | D-021,D-006 | Per-engine type-fidelity corpus with hostile edge values | 2026-11-06
T-069 | done | M | D-021,R-018 | Run the type-fidelity corpus in CI against a service container | 2026-11-06
T-070 | done | M | D-021 | React component tests over the grid and filter bar | 2026-11-06
T-071 | in_progress | M | D-007,D-019,R-016 | Stable development signing identity and resilient keychain access |
T-072 | in_progress | M | R-024 | Spike: validate the tiberius surface before SQL Server work begins |
T-073 | done | M | D-022,R-029 | Spike: validate the cross-paradigm contract against a real mongod | 2026-08-02
T-074 | done | M | D-024 | Streamed export to CSV, JSONL, and SQL INSERT | 2026-11-06
T-075 | done | M | D-011,D-022 | Carry the database engine on profiles, imported URLs, and discovered connections | 2026-11-06
T-076 | done | M | D-003,D-029 | Lift streamed browse, export, and value fetch into the Driver trait | 2026-11-06
T-077 | done | M | D-003,D-028,D-010 | Lift cancellation, pinned sessions, and pool statistics into the Driver trait | 2026-11-06
T-078 | done | M | D-003,D-022 | Hold and dispatch sessions as engine-neutral drivers in the app core | 2026-11-06
T-079 | done | M | D-003,D-022 | Build each session's driver from its engine at connect time | 2026-11-06
T-080 | done | L | D-002,D-022,D-028 | SQL Server driver crate: connect, TLS, auth, and pooled reset over tiberius | 2026-11-06
T-081 | done | M | D-016,D-029 | SQL Server navigator and lazy catalog introspection | 2026-11-06
T-082 | in_progress | L | D-005,D-006,D-010,D-029 | SQL Server browse compilation, lossless type encoding, and core filters |
T-083 | todo | M | D-029,D-010 | SQL Server raw scripts as a budgeted statement-outcome sequence |
T-084 | todo | M | D-030,D-010 | SQL Server cancellation: close-and-declare baseline with verified KILL |
T-085 | todo | M | D-027 | Staged edits on SQL Server behind the unique-key gate |
T-086 | todo | M | D-021,D-006 | SQL Server type-fidelity corpus in CI against a service container |
T-087 | in_progress | L | D-002,D-022,D-028 | MongoDB driver crate: connect, capability descriptor, and navigator |
T-088 | todo | L | D-005,D-006,D-010,D-029 | MongoDB browse with BSON-faithful encoding and operator-safe filters |
T-089 | todo | M | D-029,D-010 | MongoDB raw command documents as an outcome sequence |
T-090 | todo | M | D-021,D-006 | MongoDB fidelity corpus in CI against a mongod service container |
T-091 | done | S | D-023 | Spike: choose the visual design direction for the UI overhaul | 2026-11-06
T-092 | done | M | D-023,D-031,D-032 | Design tokens and theme implementing the chosen direction | 2026-11-06
T-093 | done | M | D-011,D-014 | Persistent sidebar listing saved connections | 2026-11-06
T-094 | done | S | D-011 | Name and rename connections | 2026-11-06
T-095 | done | L | D-023,D-031,D-032 | Restyle the app shell, navigator, and editor to the new design | 2026-11-06
T-096 | done | M | D-023,D-031,D-032 | Restyle the result grid, filter bar, and edit and confirm panels | 2026-11-06

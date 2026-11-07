D-001 | accepted | Rust core owns all database connections
D-002 | accepted | Use low-level driver crates, not sqlx
D-003 | accepted | In-tree wire-shaped Driver trait with declared capabilities
D-004 | superseded | Three-part contract for relational and document engines (→ D-029)
D-005 | accepted | Filter language: core operators plus driver-declared extensions
D-006 | accepted | Lossless wire values via column type descriptors
D-007 | accepted | Credentials in OS keychain; the webview never receives secrets
D-008 | accepted | Model transport variants and credential providers up front
D-009 | superseded | Staged edits with statement preview and DB-enforced safety modes (→ D-027)
D-010 | accepted | Keyset pagination, capped streaming, and real cancellation
D-011 | accepted | Local-only single user with a portable profile file
D-012 | accepted | Context-aware GUI launcher with reserved subcommands
D-013 | superseded | TypeScript config read via the user's own runtime (→ D-026)
D-014 | accepted | Split app-state store: config file, SQLite, cache dir
D-015 | superseded | Pooled browse connections, pinned editor sessions (→ D-028)
D-016 | accepted | Native catalog introspection with version and flavour gating
D-017 | accepted | Layered error model: kind, hints, verbatim payload
D-018 | accepted | Column derivation for schemaless collections
D-019 | accepted | CI matrix early, signing and auto-update deferred
D-020 | accepted | Shape-only logging with type-enforced redaction
D-021 | accepted | Unit tests and manual verification, plus a type-fidelity corpus
D-022 | accepted | MVP feature floor and Postgres -> SQL Server -> Mongo sequencing
D-023 | accepted | UI stack: TanStack grid, CodeMirror 6, Tailwind/shadcn
D-024 | accepted | Export formats: CSV, JSONL, SQL INSERT, streamed to file
D-025 | accepted | Rename off drizzle-studio; permissive license
D-026 | accepted | TypeScript config evaluated only under explicit per-directory consent
D-027 | accepted | Staged edits with null-safe, type-aware optimistic concurrency
D-028 | accepted | Capability-declared session model under one per-profile connection budget
D-029 | accepted | Three-part contract with a statement-outcome sequence result model
D-030 | accepted | SQL Server cancellation: close-and-declare baseline, KILL only when verified
D-031 | accepted | Instrument-panel visual direction for the UI overhaul
D-032 | accepted | Status colours are reserved, never carried by the accent or by colour alone
D-033 | superseded | Typed, closed per-engine connection options (→ D-042)
D-034 | superseded | Engine-neutral connection address: host, seed list, or SRV (→ D-040)
D-035 | accepted | Driver monitoring connections are declared overhead outside the budget
D-036 | accepted | Mongo authSource is an option; the auth mechanism follows the credential provider
D-037 | superseded | Connection URL parameters are mapped, ignore-listed, or refused (→ D-041)
D-038 | accepted | Profile file format version 2, written only when needed
D-039 | accepted | SRV TXT options: accept replicaSet and authSource, refuse loadBalanced
D-040 | accepted | Engine-neutral connection address, with discovery confined to seed lists and SRV
D-041 | accepted | Connection URL parameters are mapped, honoured as behaviour, ignore-listed, or refused
D-042 | superseded | Typed, closed per-engine connection options, with TLS trust anchors and client identity (→ D-043)
D-043 | accepted | Typed, closed per-engine connection options, with a pinned TLS trust anchor and client identity

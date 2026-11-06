# Sluice

A database studio for Postgres, SQL Server and MongoDB. Tauri v2 shell, Rust
core, React webview.

## Layout

```
Cargo.toml            cargo workspace (excludes spikes/)
src-tauri/            the Rust core — owns every database connection (D-001)
src/                  React webview — holds no sockets, talks over IPC only
spikes/               throwaway engine probes, independent crates
docs/                 decisions, risks, tasks (Treenail records)
```

## Prerequisites

- Rust stable (`rustup`)
- [Bun](https://bun.sh) — package manager and Vite driver only; **not** shipped
- Platform webview deps: WebKitGTK 4.1 on Linux, WebView2 on Windows, none on macOS

## Develop

```sh
bun install
bun run tauri dev
```

## Build

```sh
bun run tauri build
```

## Architecture invariants

These are decisions, not preferences — changing one means superseding its record.

- **The Rust process owns every database connection** (D-001). The webview links
  no database crates and opens no sockets.
- **Bun is build tooling only** (D-001). No sidecar, no JS runtime in the bundle.
- **`panic = "abort"` is forbidden** (D-003). Every IPC command boundary is
  wrapped in `catch_unwind`, which requires unwinding panics.
- **Engine differences are declared capability data** (D-003), never engine
  checks in the React layer.
- **WebKitGTK 4.1 is the CSS floor** (D-019/D-023).

## License

MIT — see [LICENSE](LICENSE).

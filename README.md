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

## Install

No Rust, Bun or Node needed — these download a prebuilt release.

**macOS / Linux**

```sh
curl -fsSL https://raw.githubusercontent.com/AHP15/Sluice/main/install.sh | sh
```

**Windows** (PowerShell)

```powershell
irm https://raw.githubusercontent.com/AHP15/Sluice/main/install.ps1 | iex
```

Or download a `.dmg`, `.AppImage` or `-setup.exe` from
[Releases](https://github.com/AHP15/Sluice/releases/latest).

- Pin a version: `SLUICE_VERSION=v0.2.0` (both installers).
- macOS installs to `/Applications` (or `~/Applications`); Linux to
  `~/.local/opt/sluice` with a `sluice` launcher in `~/.local/bin` and a menu
  entry — uninstall with `sh install.sh --uninstall`. Windows installs
  per-user; uninstall from Settings → Apps.
- Builds are not yet Developer ID / Authenticode signed (D-019). The
  installers handle this (ad-hoc seal, quarantine / Mark-of-the-Web cleared);
  a manually downloaded `.dmg` needs right-click → Open the first time.
- Linux needs no FUSE: the launcher falls back to extract-and-run. WebKitGTK
  is bundled in the AppImage. Windows fetches WebView2 if it is missing.

## Release

```sh
scripts/release.sh 0.2.0        # bump all manifests, commit, tag v0.2.0, push
```

The tag runs `.github/workflows/release.yml`: it checks that the tag matches
every manifest version, builds macOS (arm64, x86_64), Linux x86_64 and
Windows x86_64, uploads the assets under stable names plus `SHA256SUMS`, and
publishes the release. A prerelease tag (`0.2.0-rc.1`) never becomes
"latest", so the one-line installers ignore it.

## Development prerequisites

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

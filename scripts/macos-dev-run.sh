#!/usr/bin/env bash
# T-071 / R-016: cargo runner for macOS targets (wired in .cargo/config.toml).
# Re-signs the freshly built binary with the stable development identity before
# running it, so `cargo run` and `bun tauri dev` present the keychain with the
# same identity on every rebuild. Tests run through this too; signing them is
# harmless and keeps one code path.
#
# Without the identity (scripts/macos-dev-identity.sh has not been run) it says
# so once per invocation and runs the binary as built — never a hard failure,
# because a missing dev identity must not block a build.
set -euo pipefail

bin="$1"; shift
NAME="${SLUICE_DEV_IDENTITY:-Sluice Development}"

if [[ "$(uname)" == "Darwin" && -z "${SLUICE_SKIP_DEV_SIGN:-}" ]]; then
  if security find-identity -v -p codesigning 2>/dev/null | grep -q "\"$NAME\""; then
    # A fixed --identifier: the designated requirement is identifier + leaf
    # certificate, both constant, so it survives every rebuild.
    codesign --force --sign "$NAME" --identifier dev.sluice.app "$bin" >/dev/null 2>&1 \
      || echo "macos-dev-run: codesign failed; running unsigned-stable (keychain may re-prompt)" >&2
  else
    echo "macos-dev-run: no \"$NAME\" identity — run scripts/macos-dev-identity.sh once so the keychain stops re-prompting after every rebuild (R-016)" >&2
  fi
fi

exec "$bin" "$@"

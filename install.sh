#!/bin/sh
# Sluice installer for macOS and Linux. Needs nothing beyond what the OS ships
# with (sh, curl or wget, tar, shasum/sha256sum) — no Rust, Bun or Node.
#
#   curl -fsSL https://raw.githubusercontent.com/AHP15/Sluice/main/install.sh | sh
#
# Options (environment variables):
#   SLUICE_VERSION=v0.2.0   install a specific release instead of the latest
#   SLUICE_INSTALL_DIR=...  macOS: where Sluice.app goes   (default /Applications,
#                                  falling back to ~/Applications if not writable)
#                           Linux: where the AppImage goes (default ~/.local/opt/sluice)
#   SLUICE_BIN_DIR=...      Linux: where the `sluice` launcher goes (default ~/.local/bin)
#
# Uninstall:
#   macOS: rm -rf /Applications/Sluice.app   (or ~/Applications/Sluice.app)
#   Linux: sh install.sh --uninstall
set -eu

REPO="AHP15/Sluice"
APP_NAME="Sluice"

say()  { printf '\033[1msluice:\033[0m %s\n' "$*"; }
warn() { printf '\033[33msluice:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[31msluice:\033[0m %s\n' "$*" >&2; exit 1; }
has()  { command -v "$1" >/dev/null 2>&1; }

download() { # url dest
  if has curl; then curl -fL --proto '=https' --tlsv1.2 --progress-bar -o "$2" "$1"
  elif has wget; then wget -q --show-progress -O "$2" "$1"
  else die "need curl or wget to download"
  fi
}

sha256() {
  if has sha256sum; then sha256sum "$1" | cut -d' ' -f1
  elif has shasum; then shasum -a 256 "$1" | cut -d' ' -f1
  else echo ""
  fi
}

base_url() {
  if [ -n "${SLUICE_DOWNLOAD_URL:-}" ]; then   # mirror / local testing
    echo "$SLUICE_DOWNLOAD_URL"
  elif [ -n "${SLUICE_VERSION:-}" ]; then
    v="$SLUICE_VERSION"; case "$v" in v*) ;; *) v="v$v" ;; esac
    echo "https://github.com/$REPO/releases/download/$v"
  else
    echo "https://github.com/$REPO/releases/latest/download"
  fi
}

verify() { # file asset-name
  download "$(base_url)/SHA256SUMS" "$TMP/SHA256SUMS" 2>/dev/null || {
    warn "no SHA256SUMS in this release; skipping checksum verification"; return 0; }
  want=$(grep " $2\$" "$TMP/SHA256SUMS" | cut -d' ' -f1)
  got=$(sha256 "$1")
  [ -n "$want" ] || die "$2 is not listed in SHA256SUMS"
  [ -n "$got" ]  || { warn "no sha256 tool found; skipping checksum verification"; return 0; }
  [ "$want" = "$got" ] || die "checksum mismatch for $2 (expected $want, got $got)"
  say "checksum verified"
}

linux_paths() {
  LINUX_DIR="${SLUICE_INSTALL_DIR:-$HOME/.local/opt/sluice}"
  BIN_DIR="${SLUICE_BIN_DIR:-$HOME/.local/bin}"
  DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
  DESKTOP_FILE="$DATA_HOME/applications/dev.sluice.app.desktop"
  ICON_FILE="$DATA_HOME/icons/hicolor/128x128/apps/dev.sluice.app.png"
}

uninstall_linux() {
  linux_paths
  rm -rf "$LINUX_DIR" "$BIN_DIR/sluice" "$DESKTOP_FILE" "$ICON_FILE"
  has update-desktop-database && update-desktop-database "$DATA_HOME/applications" 2>/dev/null || true
  say "uninstalled (your saved connections in ~/.config and the keyring were left alone)"
}

install_macos() {
  case "$(uname -m)" in
    arm64)  arch=arm64 ;;
    x86_64) arch=x86_64
            # Rosetta reports x86_64; prefer the native build on Apple Silicon.
            [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ] && arch=arm64 ;;
    *) die "unsupported Mac architecture: $(uname -m)" ;;
  esac
  asset="sluice-macos-$arch.app.tar.gz"

  dest="${SLUICE_INSTALL_DIR:-/Applications}"
  if [ -z "${SLUICE_INSTALL_DIR:-}" ] && [ ! -w "$dest" ]; then
    dest="$HOME/Applications"
  fi
  mkdir -p "$dest"

  say "downloading $asset"
  download "$(base_url)/$asset" "$TMP/$asset"
  verify "$TMP/$asset" "$asset"

  tar -xzf "$TMP/$asset" -C "$TMP"
  [ -d "$TMP/$APP_NAME.app" ] || die "archive did not contain $APP_NAME.app"

  if pgrep -x "$APP_NAME" >/dev/null 2>&1 || pgrep -x sluice >/dev/null 2>&1; then
    warn "Sluice is running; quit it to finish the update (it will be replaced now)"
  fi
  rm -rf "$dest/$APP_NAME.app"
  mv "$TMP/$APP_NAME.app" "$dest/"

  # Builds are not yet Developer ID signed (D-019). curl does not set the
  # quarantine flag, but strip it in case the archive was fetched another way,
  # and make sure the bundle carries a valid (ad-hoc) seal so it launches.
  xattr -dr com.apple.quarantine "$dest/$APP_NAME.app" 2>/dev/null || true
  if ! codesign --verify --deep "$dest/$APP_NAME.app" >/dev/null 2>&1; then
    codesign --force --deep --sign - "$dest/$APP_NAME.app" >/dev/null 2>&1 || \
      warn "could not ad-hoc sign the app; if macOS blocks it, right-click > Open"
  fi

  say "installed $dest/$APP_NAME.app"
  say "launch it from Launchpad/Spotlight, or: open -a \"$dest/$APP_NAME.app\""
}

install_linux() {
  case "$(uname -m)" in
    x86_64|amd64) arch=x86_64 ;;
    *) die "no Linux build for $(uname -m) yet (only x86_64 is published)" ;;
  esac
  asset="sluice-linux-$arch.AppImage"
  linux_paths

  say "downloading $asset"
  download "$(base_url)/$asset" "$TMP/$asset"
  verify "$TMP/$asset" "$asset"

  mkdir -p "$LINUX_DIR" "$BIN_DIR" "$(dirname "$DESKTOP_FILE")" "$(dirname "$ICON_FILE")"
  install -m 0755 "$TMP/$asset" "$LINUX_DIR/Sluice.AppImage"

  # AppImages normally need libfuse2, which many current distros no longer
  # ship. The launcher falls back to extract-and-run so FUSE is never a
  # prerequisite. WebKitGTK and the rest are bundled inside the AppImage.
  cat > "$BIN_DIR/sluice" <<EOF
#!/bin/sh
if ! { [ -e /dev/fuse ] && ldconfig -p 2>/dev/null | grep -q 'libfuse\.so\.2'; }; then
  export APPIMAGE_EXTRACT_AND_RUN=1
fi
exec "$LINUX_DIR/Sluice.AppImage" "\$@"
EOF
  chmod 0755 "$BIN_DIR/sluice"

  # Desktop entry + icon, pulled from the AppImage itself.
  ( cd "$TMP" && "$LINUX_DIR/Sluice.AppImage" --appimage-extract '*.png' >/dev/null 2>&1 ) || true
  icon_src=$(find "$TMP/squashfs-root" -maxdepth 1 -name '*.png' 2>/dev/null | head -n1)
  [ -n "$icon_src" ] && cp -L "$icon_src" "$ICON_FILE" 2>/dev/null || true

  cat > "$DESKTOP_FILE" <<EOF
[Desktop Entry]
Type=Application
Name=Sluice
Comment=Database studio for Postgres, SQL Server and MongoDB
Exec=$BIN_DIR/sluice %U
Icon=$ICON_FILE
Terminal=false
Categories=Development;Database;
StartupWMClass=sluice
EOF
  has update-desktop-database && update-desktop-database "$(dirname "$DESKTOP_FILE")" 2>/dev/null || true

  say "installed $LINUX_DIR/Sluice.AppImage"
  say "launcher: $BIN_DIR/sluice (also in your application menu)"
  case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) warn "$BIN_DIR is not on your PATH; add: export PATH=\"$BIN_DIR:\$PATH\"" ;;
  esac
}

main() {
  os="$(uname -s)"
  if [ "${1:-}" = "--uninstall" ]; then
    [ "$os" = Linux ] || die "--uninstall is Linux-only; on macOS delete Sluice.app"
    uninstall_linux; exit 0
  fi

  TMP="$(mktemp -d 2>/dev/null || mktemp -d -t sluice)"
  trap 'rm -rf "$TMP"' EXIT INT TERM

  case "$os" in
    Darwin) install_macos ;;
    Linux)  install_linux ;;
    MINGW*|MSYS*|CYGWIN*) die "on Windows, run in PowerShell: irm https://raw.githubusercontent.com/$REPO/main/install.ps1 | iex" ;;
    *) die "unsupported OS: $os" ;;
  esac
}

main "$@"

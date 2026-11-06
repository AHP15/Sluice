#!/usr/bin/env bash
# T-071 / R-016: create the ONE stable code-signing identity development builds
# sign with, so the macOS keychain sees the same application identity across
# rebuilds.
#
# Why: the linker ad-hoc signs every arm64 binary, and an ad-hoc signature's
# identity is its code hash — new on every build. Keychain item ACLs are keyed
# to that identity, so every rebuild re-prompts and "Always Allow" piles up
# stale entries. A self-signed certificate gives a designated requirement
# ("identifier dev.sluice.app and certificate leaf = H\"…\"") that does not
# change when the code does.
#
# Run once per machine. It is idempotent: an existing identity is left alone.
# It asks macOS for permission twice — to import the key into the login
# keychain and to trust the certificate for code signing — and touches nothing
# else. Remove it later with:
#   security delete-identity -c "Sluice Development"
set -euo pipefail

NAME="${SLUICE_DEV_IDENTITY:-Sluice Development}"
KEYCHAIN="${HOME}/Library/Keychains/login.keychain-db"

if [[ "$(uname)" != "Darwin" ]]; then
  echo "macos-dev-identity: only needed on macOS; nothing to do." >&2
  exit 0
fi

if security find-identity -v -p codesigning "$KEYCHAIN" | grep -q "\"$NAME\""; then
  echo "macos-dev-identity: \"$NAME\" already exists and is valid for code signing."
  exit 0
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

cat > "$work/cert.cnf" <<CNF
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = $NAME
[ext]
basicConstraints = critical,CA:false
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
CNF

openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
  -keyout "$work/key.pem" -out "$work/cert.pem" -config "$work/cert.cnf" >/dev/null 2>&1

# A throwaway transport password: `security import` will not take an empty one
# from every OpenSSL build. OpenSSL 3 needs -legacy for the PKCS#12 MAC that
# `security` understands; LibreSSL (the macOS default) rejects the flag.
pass="sluice-dev-$$"
legacy=()
openssl version | grep -q '^OpenSSL 3' && legacy=(-legacy)
openssl pkcs12 -export "${legacy[@]}" -inkey "$work/key.pem" -in "$work/cert.pem" \
  -name "$NAME" -out "$work/id.p12" -passout "pass:$pass"

# -T: codesign may use the key without a prompt on every build.
security import "$work/id.p12" -k "$KEYCHAIN" -P "$pass" -T /usr/bin/codesign
security add-trusted-cert -p codeSign -k "$KEYCHAIN" "$work/cert.pem"

if security find-identity -v -p codesigning "$KEYCHAIN" | grep -q "\"$NAME\""; then
  echo "macos-dev-identity: created \"$NAME\". Development builds will now sign with it."
else
  echo "macos-dev-identity: imported \"$NAME\" but it is not listed as valid for code signing." >&2
  echo "Check Keychain Access → login → Certificates → \"$NAME\" → Trust → Code Signing." >&2
  exit 1
fi

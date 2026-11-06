# Signing and the keychain

T-071 · D-007 · D-019 · R-016

macOS scopes a keychain item's access to the **identity of the app that
created it**. Sluice keeps every saved password in the keychain (D-007), so
the signing identity decides whether a build can read the passwords that an
earlier build saved. There are three phases. Each one fails differently if
nobody plans for it.

## 1. Development: one stable identity

The linker ad-hoc signs every arm64 binary. An ad-hoc identity is the code
hash, which changes with every rebuild. Without a fix, the keychain treats each
rebuild as a different app: it prompts on every run, and every "Always Allow"
leaves another stale entry in the item's access list.

Fix: run this once per machine.

```sh
scripts/macos-dev-identity.sh
```

The script creates a self-signed **"Sluice Development"** code-signing
certificate in the login keychain. macOS asks twice: once to import the key,
once to trust the certificate for code signing. From then on,
`.cargo/config.toml` routes every `cargo run`, `cargo test` and
`bun tauri dev` binary through `scripts/macos-dev-run.sh`, which re-signs it
with that certificate and the fixed identifier `dev.sluice.app`. The
designated requirement is the identifier plus the leaf certificate. Both stay
fixed, so the identity survives every rebuild.

Without the identity, the runner prints a one-line reminder and runs the
binary as built. A build never fails because the identity is missing.

- To skip re-signing for one run: `SLUICE_SKIP_DEV_SIGN=1`
- To remove the identity: `security delete-identity -c "Sluice Development"`

## 2. A keychain read failure is never fatal

Sometimes a saved password can't be read. The item may have been stored by
a differently-signed build, or an access prompt may have been denied. When
that happens:

1. The core reports the item as unreadable. It does not report the item as
   missing, and it does not return an error that stops the connection
   (`CredentialError::Unreadable`).
2. The connection attempt goes ahead without the password. Trust auth,
   `.pgpass` and service files can still succeed.
3. If the server refuses, the error carries `hints.credential_reentry`. The
   connect form then asks for the password again, and the message names the
   unreadable keychain item as the likely cause.
4. Storing the re-entered password replaces the old item, so the keychain
   item is recreated under the **current** identity.

## 3. Changeover to Developer ID (at first distribution, per D-019)

The Developer ID certificate is a different leaf certificate from "Sluice
Development", so the designated requirement changes. Items saved by
development builds will usually **not** be readable by the first signed
build. This is expected, and the re-entry flow in section 2 handles it. The
changeover only needs to confirm that it does.

Changeover checklist. Do it on a **clean test machine**, meaning a macOS
user account that has never run Sluice:

1. Install a *development* build signed with "Sluice Development". Save two
   profiles with passwords. Quit.
2. Install the first *Developer ID* build over it.
3. Connect each profile. Expected: either the connection succeeds silently
   (the items carried over), or the connect form asks for the password again,
   naming the keychain, and the connection succeeds after it is entered.
   **Not acceptable:** a crash, a connection that fails with no prompt, or a
   prompt that keeps coming back after the password is re-entered.
4. Quit, relaunch, and connect again. Expected: no prompt. The re-stored
   item now belongs to the Developer ID identity.
5. In Keychain Access, open each `dev.sluice.app` item → Access Control, and
   confirm that the Developer ID build is listed.
6. Record the result, the date and the macOS version in the release notes
   for that build. Tell users that saved passwords may need entering once
   after upgrading.

**Status:** sections 1 and 2 are implemented. Section 3 is pending until the
first Developer ID build exists (D-019 defers signing until then), and T-071
stays `in_progress` until the checklist has been run.

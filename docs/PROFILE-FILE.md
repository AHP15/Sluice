# The profile file

T-036 · D-011 · D-014

Sluice keeps connection profiles and saved queries in a single plain TOML
file. You can read it, edit it by hand, diff it, and copy it between
machines with any tool you already use for files. Sluice has no accounts, no
server, no sync and no telemetry. If the file reaches another machine, you
put it there.

| OS | Location |
|---|---|
| Linux | `~/.config/sluice/profiles.toml` |
| macOS | `~/Library/Application Support/sluice/profiles.toml` |
| Windows | `%APPDATA%\sluice\profiles.toml` |

The connect panel shows the exact path under the profile list.

## Format

```toml
version = 1

[[profile]]
id = "profile-k3x9a1qz"     # stable; also the keychain account name for its password
label = "Local dev"
engine = "postgres"         # optional; default "postgres"
host = "localhost"
port = 5432
user = "postgres"
database = "app_dev"
tls = "disabled"            # optional: "verify" (default) | "accept_invalid_certs" | "disabled"
statement_timeout_secs = 30 # optional; 0 = none; unset = the app default
connection_budget = 5       # optional; pooled + pinned connections for this profile

[[query]]
id = "query-7c1b2e0d"
label = "Orders today"
profile = "profile-k3x9a1qz" # optional; the profile it was saved against
sql = """
select * from orders where created_at >= current_date
"""
```

Rules:

- **`version`** is the format number. A file without it is read as
  version 1. A file with a *newer* version than your build understands is
  refused, and Sluice won't save any changes to it until you update. That
  way an older build on a second machine can't silently drop fields that a
  newer one wrote.
- **Unknown or misspelled keys are errors**, and the error names the key.
  For example, `dbname` instead of `database` fails to load. Ignoring it
  would quietly connect somewhere else.
- **`id` is stable.** The saved password is stored in the OS keychain under
  the profile's `id`. Renaming the `id` loses that link, and you'll be asked
  for the password again. Change `label` freely.
- **Passwords never appear in this file.** They live in the OS keychain
  (D-007). Deleting a profile in the app also deletes its stored password.
- **Hand edits are safe while Sluice is open.** Sluice re-reads the file
  before every change it makes and writes atomically (temp file plus
  rename), so your edit is kept and a crash never leaves half a file.
- When Sluice writes the file, it puts a short comment header at the top
  and drops optional keys that are at their defaults. Other comments you add
  by hand are not preserved when the app saves.

## Not in this file

- **Query history** is in a SQLite database in the data dir. It is
  machine-local by design and isn't meant to be synced (D-014).
- **Window and panel layout** is disposable state in the cache dir and is
  safe to delete.

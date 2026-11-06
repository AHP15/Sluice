//! Containment for the config subprocess (T-032 / R-033).
//!
//! D-026 says the consented config runs "with no access to the keychain". On
//! macOS and Windows the platform scopes keychain items by application, so a
//! spawned `node` cannot silently read this app's items. **libsecret has no
//! per-application boundary**: any process running as the user can ask the
//! session's secret service. On Linux, then, the isolation has to be built —
//! and where it cannot be, the consent text must say so (R-033).
//!
//! The Linux mechanism is `bubblewrap`. This module only *plans* the wrapper
//! (pure argument construction, so it is testable and inert); `evaluate.rs`,
//! the one module permitted to spawn, probes that bwrap works and runs it.
//!
//! What the sandbox withholds, and why that set:
//!
//! - **The D-Bus session bus** — how libsecret, gnome-keyring and KWallet are
//!   reached. Its socket normally lives in `$XDG_RUNTIME_DIR`, which is covered
//!   with an empty tmpfs (that also hides gnome-keyring's control socket). A bus
//!   socket elsewhere has `/dev/null` bound over it. An *abstract* socket
//!   (`unix:abstract=…`) has no path to hide, and abstract sockets are scoped by
//!   network namespace — so only `--unshare-net` withholds it, at the cost of
//!   network access for that config. The variable itself is unset.
//! - **This app's data directory** — the encrypted-file credential fallback
//!   keeps its key beside the ciphertext (D-007), so hiding the directory is
//!   what keeps "key material only in the app process" true against a child.
//!   It also hides the consent store, so a config cannot grant itself consent
//!   for some other directory.
//!
//! Deliberately **not** withheld: the rest of the filesystem and (normally) the
//! network. R-032 is that a config must see what it would see in the user's
//! terminal, or it silently takes a different branch; the sandbox targets the
//! credential paths, not the config's ordinary inputs.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Absolute locations only — never `PATH`, for the same reason the interpreter
/// is never resolved from it (D-026): a project can put things on `PATH`.
pub const BWRAP_CANDIDATES: &[&str] = &["/usr/bin/bwrap", "/bin/bwrap"];

/// How the config subprocess is contained.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Sandbox {
    /// Run directly. The consent text must not claim isolation.
    #[default]
    Off,
    /// Run under bubblewrap, hiding `hide` (paths that exist) in addition to the
    /// session bus.
    Bubblewrap { bwrap: PathBuf, hide: Vec<PathBuf> },
}

impl Sandbox {
    pub fn is_active(&self) -> bool {
        matches!(self, Sandbox::Bubblewrap { .. })
    }
}

/// Where the session bus is, from `DBUS_SESSION_BUS_ADDRESS`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BusAddress {
    Path(PathBuf),
    Abstract,
    /// Unset, or a transport (tcp) this does not model.
    Other,
}

pub fn parse_bus_address(value: Option<&str>) -> BusAddress {
    let Some(v) = value else { return BusAddress::Other };
    // Several addresses may be listed, `;`-separated; any abstract one forces
    // the network namespace.
    let mut path = None;
    for addr in v.split(';') {
        let Some(rest) = addr.strip_prefix("unix:") else { continue };
        for kv in rest.split(',') {
            if kv.starts_with("abstract=") {
                return BusAddress::Abstract;
            }
            if let Some(p) = kv.strip_prefix("path=") {
                path.get_or_insert_with(|| PathBuf::from(p));
            }
        }
    }
    path.map(BusAddress::Path).unwrap_or(BusAddress::Other)
}

/// The bwrap arguments that precede the interpreter command line.
///
/// `runtime_dir` is `$XDG_RUNTIME_DIR`; `keep` lists files under a hidden path
/// that the child still needs (the evaluation shim, if the temp dir happens to
/// live there) and are bound back read-only on top.
pub fn bwrap_args(
    hide: &[PathBuf],
    runtime_dir: Option<&Path>,
    bus: &BusAddress,
    cwd: &Path,
    keep: &[PathBuf],
) -> Vec<OsString> {
    let mut a: Vec<OsString> = ["--die-with-parent", "--new-session"]
        .into_iter()
        .map(OsString::from)
        .collect();

    // The user's filesystem as their terminal sees it (R-032)…
    a.extend(["--bind", "/", "/"].map(OsString::from));

    // …minus the session bus and the keyring's control socket…
    if let Some(rt) = runtime_dir.filter(|p| p.is_dir()) {
        a.push("--tmpfs".into());
        a.push(rt.into());
    }
    match bus {
        BusAddress::Path(p) if runtime_dir.is_none_or(|rt| !p.starts_with(rt)) && p.exists() => {
            a.push("--ro-bind".into());
            a.push("/dev/null".into());
            a.push(p.into());
        }
        BusAddress::Abstract => a.push("--unshare-net".into()),
        _ => {}
    }
    a.push("--unsetenv".into());
    a.push("DBUS_SESSION_BUS_ADDRESS".into());

    // …and minus this app's own data (credential key, consent store).
    for h in hide.iter().filter(|p| p.is_dir()) {
        a.push("--tmpfs".into());
        a.push(h.into());
    }

    for k in keep {
        a.push("--ro-bind".into());
        a.push(k.into());
        a.push(k.into());
    }

    a.push("--chdir".into());
    a.push(cwd.into());
    a.push("--".into());
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[OsString]) -> Vec<String> {
        v.iter().map(|x| x.to_string_lossy().into_owned()).collect()
    }

    fn tmp(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let d = std::env::temp_dir().join("sluice-sandbox").join(format!("{tag}-{nanos}"));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn bus_addresses_parse() {
        assert_eq!(
            parse_bus_address(Some("unix:path=/run/user/1000/bus")),
            BusAddress::Path("/run/user/1000/bus".into())
        );
        assert_eq!(parse_bus_address(Some("unix:abstract=/tmp/dbus-XYZ,guid=1")), BusAddress::Abstract);
        assert_eq!(
            parse_bus_address(Some("unix:path=/a/bus;unix:abstract=x")),
            BusAddress::Abstract,
            "any abstract address forces the network namespace"
        );
        assert_eq!(parse_bus_address(None), BusAddress::Other);
        assert_eq!(parse_bus_address(Some("tcp:host=h,port=1")), BusAddress::Other);
    }

    #[test]
    fn the_runtime_dir_and_app_data_are_hidden_and_the_bus_variable_unset() {
        let rt = tmp("rt");
        let data = tmp("data");
        let cwd = tmp("cwd");
        let args = s(&bwrap_args(
            &[data.clone()],
            Some(&rt),
            &BusAddress::Path(rt.join("bus")),
            &cwd,
            &[],
        ));
        let joined = args.join(" ");
        assert!(joined.contains(&format!("--tmpfs {}", rt.display())), "{joined}");
        assert!(joined.contains(&format!("--tmpfs {}", data.display())), "{joined}");
        assert!(joined.contains("--unsetenv DBUS_SESSION_BUS_ADDRESS"), "{joined}");
        assert!(joined.contains("--bind / /"), "R-032: the filesystem stays visible: {joined}");
        assert!(!joined.contains("--unshare-net"), "network kept for a path bus: {joined}");
        assert_eq!(args.last().map(String::as_str), Some("--"));
    }

    #[test]
    fn an_abstract_bus_is_withheld_by_the_network_namespace() {
        let args = s(&bwrap_args(&[], None, &BusAddress::Abstract, &tmp("c"), &[]));
        assert!(args.iter().any(|a| a == "--unshare-net"), "{args:?}");
    }

    #[test]
    fn a_bus_socket_outside_the_runtime_dir_is_covered() {
        let elsewhere = tmp("bus");
        let sock = elsewhere.join("bus");
        std::fs::write(&sock, "").unwrap();
        let args = s(&bwrap_args(&[], None, &BusAddress::Path(sock.clone()), &tmp("c"), &[]));
        let joined = args.join(" ");
        assert!(joined.contains(&format!("--ro-bind /dev/null {}", sock.display())), "{joined}");
    }

    #[test]
    fn a_needed_file_under_a_hidden_dir_is_bound_back_after_the_tmpfs() {
        let rt = tmp("rt2");
        let shim = rt.join("shim.mjs");
        std::fs::write(&shim, "").unwrap();
        let args = s(&bwrap_args(&[], Some(&rt), &BusAddress::Other, &tmp("c"), &[shim.clone()]));
        let tmpfs = args.iter().position(|a| a == "--tmpfs").unwrap();
        let bind_back = args.iter().rposition(|a| a == "--ro-bind").unwrap();
        assert!(bind_back > tmpfs, "the bind-back must be mounted on top: {args:?}");
        assert_eq!(args[bind_back + 1], shim.to_string_lossy());
    }
}

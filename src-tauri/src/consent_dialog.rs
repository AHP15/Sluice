//! The native consent dialog (T-029 / D-026, R-001).
//!
//! `sluice-config` owns the *policy* — fingerprinting, interpreter validation,
//! the store — and takes the decision through the [`ConsentPrompt`] trait so it
//! stays testable and free of any UI dependency. This module supplies the real
//! dialog.
//!
//! ## Why a native dialog rather than a webview modal
//!
//! Two reasons, both structural:
//!
//! 1. **Timing.** D-012's launcher discovers and connects as the app starts. A
//!    webview modal cannot be shown until the webview is up, which would mean
//!    either delaying the prompt until after startup or evaluating first and
//!    asking later — and "evaluate first" is the entire failure R-001 describes.
//! 2. **Spoofing.** The question being asked is "may this repository's code
//!    run". Rendering that prompt inside the same webview that untrusted content
//!    could influence puts the attacker and the guard on the same surface.
//!
//! The dialog is deliberately *not* styled to look routine. Its default button
//! is the safe one.

use std::path::Path;

use sluice_config::consent::{ConsentDecision, ConsentPrompt, ConsentRequest};
use sluice_config::evaluate::{TargetConfirmation, TargetDecision, TargetSummary};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

/// Shows a blocking native dialog on the app handle.
pub struct NativeConsentPrompt {
    app: tauri::AppHandle,
}

impl NativeConsentPrompt {
    pub fn new(app: tauri::AppHandle) -> Self {
        Self { app }
    }
}

/// The platform the text describes. A parameter rather than `cfg!` so every
/// combination is tested on every host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    Linux,
    MacOs,
    Windows,
}

impl Os {
    pub fn current() -> Self {
        if cfg!(target_os = "linux") {
            Os::Linux
        } else if cfg!(target_os = "macos") {
            Os::MacOs
        } else {
            Os::Windows
        }
    }
}

/// The blast-radius sentence for this machine (T-032 / R-033).
///
/// `sandboxed` must be the same answer the evaluator will act on
/// (`probe_sandbox`), and `file_fallback` whether D-007's encrypted-file store
/// holds anything — so the dialog states what is actually true here rather
/// than what is true on some platforms.
pub fn blast_radius_text(sandboxed: bool, file_fallback: bool) -> String {
    blast_radius_for(Os::current(), sandboxed, file_fallback)
}

/// R-033: macOS and Windows scope keychain items per application, so a spawned
/// interpreter cannot silently read this app's items; libsecret has no such
/// boundary, so on Linux only a sandbox withholding the session bus does. And
/// the encrypted-file fallback keeps its key beside the ciphertext, so unless a
/// sandbox hides it, a config can read it on any platform.
pub fn blast_radius_for(os: Os, sandboxed: bool, file_fallback: bool) -> String {
    const BASE: &str =
        "This code will run with your user privileges and can read every file your account can read.";
    match os {
        Os::Linux if sandboxed => format!(
            "{BASE} It runs in a sandbox without access to your login keyring or to Sluice's \
             saved passwords."
        ),
        Os::Linux => {
            let fallback = if file_fallback {
                ", the passwords Sluice keeps in its encrypted file,"
            } else {
                ""
            };
            format!(
                "This code will run with your full user privileges, including access to your \
                 login keyring{fallback} and every file your account can read. Installing \
                 bubblewrap lets Sluice run it without keyring access."
            )
        }
        Os::MacOs | Os::Windows => {
            let store = if os == Os::MacOs { "the macOS Keychain" } else { "Windows Credential Manager" };
            if file_fallback {
                format!(
                    "{BASE} It cannot read passwords in {store}, but it can read the ones Sluice \
                     keeps in its encrypted file."
                )
            } else {
                format!("{BASE} It cannot read Sluice's saved database passwords, which {store} scopes to this app.")
            }
        }
    }
}

fn body(request: &ConsentRequest) -> String {
    format!(
        "{dir}\n\nwants to run its configuration file as code to work out which database to \
         open.\n\nConfig file:\n{config}\n\nWill be run by:\n{interp}\n\n{blast}\n\nOnly allow \
         this for projects you trust.",
        dir = display(&request.directory),
        config = display(&request.config_path),
        interp = display(&request.interpreter_path),
        blast = request.blast_radius,
    )
}

fn display(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

impl ConsentPrompt for NativeConsentPrompt {
    fn ask(&self, request: &ConsentRequest) -> ConsentDecision {
        // `blocking_show` is correct here: the caller is deciding whether to
        // execute code and must not proceed until the answer is in.
        let allowed = self
            .app
            .dialog()
            .message(body(request))
            .title("Run this project's config file?")
            .kind(MessageDialogKind::Warning)
            .buttons(MessageDialogButtons::OkCancelCustom(
                format!("Run with {}", request.interpreter_label),
                "Don't run".to_string(),
            ))
            .blocking_show();

        if allowed {
            ConsentDecision::GrantAndRemember
        } else {
            ConsentDecision::Decline
        }
    }
}

/// Shows where an evaluated config is about to connect, before any connection
/// opens (T-031 / R-032).
///
/// A config that took a different branch than the user expected — its
/// production fallback, say — is only catchable if the destination is visible.
/// `sluice-config` makes the URL unreachable until a [`TargetDecision`] is
/// supplied; this is the surface that supplies it. Native for the same reasons
/// as the consent prompt, and its default button is the safe one.
pub struct NativeTargetConfirmation {
    app: tauri::AppHandle,
}

impl NativeTargetConfirmation {
    pub fn new(app: tauri::AppHandle) -> Self {
        Self { app }
    }
}

fn target_body(summary: &TargetSummary) -> String {
    format!(
        "This project's config resolved to:\n\nHost: {host}\nPort: {port}\nDatabase: {db}\n\n\
         If this is not the database you expected, the config may have taken a different branch \
         (for example a production fallback). Check before connecting.",
        host = summary.host.as_deref().unwrap_or("(none)"),
        port = summary.port.map(|p| p.to_string()).unwrap_or_else(|| "(default)".into()),
        db = summary.database.as_deref().unwrap_or("(none)"),
    )
}

impl TargetConfirmation for NativeTargetConfirmation {
    fn confirm_target(&self, summary: &TargetSummary) -> TargetDecision {
        let go = self
            .app
            .dialog()
            .message(target_body(summary))
            .title("Connect to this database?")
            .kind(MessageDialogKind::Info)
            .buttons(MessageDialogButtons::OkCancelCustom(
                "Connect".to_string(),
                "Cancel".to_string(),
            ))
            .blocking_show();
        if go {
            TargetDecision::Connect
        } else {
            TargetDecision::Cancel
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn request() -> ConsentRequest {
        ConsentRequest {
            directory: PathBuf::from("/Users/pro/work/shop"),
            config_path: PathBuf::from("/Users/pro/work/shop/drizzle.config.ts"),
            interpreter_path: PathBuf::from("/opt/homebrew/bin/bun"),
            interpreter_label: "bun",
            blast_radius: blast_radius_text(false, false),
        }
    }

    /// D-026 requires the dialog to name the config file's absolute path and the
    /// absolute path of the interpreter that will run it. This asserts the text
    /// actually contains both, rather than trusting the struct carried them.
    #[test]
    fn the_dialog_names_both_absolute_paths() {
        let text = body(&request());
        assert!(text.contains("/Users/pro/work/shop/drizzle.config.ts"), "{text}");
        assert!(text.contains("/opt/homebrew/bin/bun"), "{text}");
    }

    #[test]
    fn the_dialog_states_that_this_is_code_execution() {
        let text = body(&request());
        assert!(text.contains("run its configuration file as code"), "{text}");
        assert!(text.contains("trust"), "{text}");
    }

    /// R-032: the host, port, and database are all shown before connecting.
    #[test]
    fn the_target_dialog_shows_host_port_and_database() {
        let text = target_body(&TargetSummary {
            scheme: "postgres".into(),
            host: Some("db.prod.internal".into()),
            port: Some(6432),
            database: Some("shop".into()),
            user: Some("app".into()),
        });
        assert!(text.contains("Host: db.prod.internal"), "{text}");
        assert!(text.contains("Port: 6432"), "{text}");
        assert!(text.contains("Database: shop"), "{text}");
    }

    /// R-033: every platform/sandbox/fallback combination says only what is
    /// true there. Linux never claims keychain isolation without a sandbox, and
    /// no platform claims the file fallback is out of reach unless sandboxed.
    #[test]
    fn the_blast_radius_text_is_true_for_every_combination() {
        let unsandboxed_linux = blast_radius_for(Os::Linux, false, false);
        assert!(unsandboxed_linux.contains("login keyring"), "{unsandboxed_linux}");
        assert!(!unsandboxed_linux.contains("cannot read"), "{unsandboxed_linux}");
        assert!(!unsandboxed_linux.contains("without access"), "{unsandboxed_linux}");
        assert!(unsandboxed_linux.contains("bubblewrap"), "says how to get the sandbox");

        assert!(blast_radius_for(Os::Linux, false, true).contains("encrypted file"));

        let sandboxed = blast_radius_for(Os::Linux, true, true);
        assert!(sandboxed.contains("without access to your login keyring"), "{sandboxed}");
        assert!(sandboxed.contains("saved passwords"), "{sandboxed}");

        for os in [Os::MacOs, Os::Windows] {
            let keychain_only = blast_radius_for(os, false, false);
            assert!(keychain_only.contains("cannot read"), "{keychain_only}");
            let with_fallback = blast_radius_for(os, false, true);
            assert!(
                with_fallback.contains("can read the ones Sluice keeps in its encrypted file"),
                "the fallback must not be claimed out of reach: {with_fallback}"
            );
        }
    }
}

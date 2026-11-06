//! Credential storage (T-020 / D-007).
//!
//! D-007's trust boundary: *secrets live in the platform keychain; the core
//! resolves a profile id to credentials at connect time and never sends a
//! credential outward. The webview holds profile ids and metadata only.*
//!
//! The payoff is bounded blast radius — an XSS in the webview becomes "someone
//! read one table" instead of credential exfiltration for every database ever
//! connected to. That only holds if **no IPC response can carry a secret**,
//! which is asserted by a test at the bottom of `commands.rs`.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use chacha20poly1305::aead::{Aead, KeyInit, OsRng};
use chacha20poly1305::{AeadCore, ChaCha20Poly1305, Key, Nonce};
use serde::{Deserialize, Serialize};
use sluice_core::Secret;

const SERVICE: &str = "dev.sluice.app";

#[derive(Debug)]
pub enum CredentialError {
    NotFound,
    Backend(String),
    /// T-071 / R-016: the keychain holds *something* for this profile but would
    /// not hand it over — on macOS, typically an item stored by a build with a
    /// different signing identity, or an access prompt that was denied. Not
    /// fatal: the caller connects without it and, if the server then refuses,
    /// asks the user to re-enter the password, which re-stores it under the
    /// current identity.
    Unreadable(String),
}

impl std::fmt::Display for CredentialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CredentialError::NotFound => write!(f, "no stored credential for that profile"),
            // Never interpolate the secret itself, only the backend's own text.
            CredentialError::Backend(e) => write!(f, "credential store unavailable: {e}"),
            CredentialError::Unreadable(e) => write!(
                f,
                "the stored password could not be read from the keychain ({e}); \
                 it may have been saved by a differently-signed build — enter it again to re-store it"
            ),
        }
    }
}

impl std::error::Error for CredentialError {}

/// Which store actually answered. Surfaced in diagnostics (T-023) so a silent
/// downgrade to the file fallback is visible rather than assumed.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    /// macOS Keychain, Windows Credential Manager, or libsecret.
    PlatformKeychain,
    /// D-007's fallback "where no secret-service daemon exists" — typically a
    /// headless Linux box with no `libsecret` provider running.
    EncryptedFile,
}

pub struct CredentialStore {
    /// Overridable so tests never touch the developer's real keychain.
    fallback_dir: Option<PathBuf>,
    force_fallback: bool,
}

impl Default for CredentialStore {
    fn default() -> Self {
        Self { fallback_dir: None, force_fallback: false }
    }
}

impl CredentialStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Test seam: use an explicit directory and skip the platform keychain.
    pub fn with_fallback_dir(dir: PathBuf) -> Self {
        Self { fallback_dir: Some(dir), force_fallback: true }
    }

    pub fn store(&self, profile_id: &str, secret: &Secret<String>) -> Result<Backend, CredentialError> {
        if !self.force_fallback {
            if let Ok(entry) = keyring::Entry::new(SERVICE, profile_id) {
                if entry.set_password(secret.expose()).is_ok() {
                    return Ok(Backend::PlatformKeychain);
                }
                // T-071 / R-016: an item left by a differently-signed build can
                // refuse an in-place update. Replacing it re-creates the item
                // under the *current* identity, which is what re-entry is for.
                let _ = entry.delete_credential();
                if entry.set_password(secret.expose()).is_ok() {
                    return Ok(Backend::PlatformKeychain);
                }
            }
        }
        self.file_store(profile_id, secret)?;
        Ok(Backend::EncryptedFile)
    }

    /// Resolve a profile id to its secret. Called at connect time, in the core,
    /// and the result never travels outward.
    ///
    /// T-022/R-017: the keychain and the decrypted-file path are both credential
    /// *parse boundaries* — the value crosses from opaque storage into process
    /// memory here — so it is wrapped before it is returned, never after.
    ///
    /// T-071 / R-016: a keychain read that fails for any reason other than "no
    /// such item" is reported as [`CredentialError::Unreadable`] (unless the
    /// file fallback has the secret), never folded into `NotFound` — the caller
    /// needs to know the difference to ask for re-entry instead of failing.
    pub fn resolve(&self, profile_id: &str) -> Result<Secret<String>, CredentialError> {
        let mut keychain_failure = None;
        if !self.force_fallback {
            if let Ok(entry) = keyring::Entry::new(SERVICE, profile_id) {
                match entry.get_password() {
                    Ok(p) => return Ok(Secret::new(p)),
                    Err(keyring::Error::NoEntry) => {}
                    Err(e) => keychain_failure = Some(keychain_error_label(&e)),
                }
            }
        }
        merge_fallback(self.file_resolve(profile_id), keychain_failure)
    }

    pub fn delete(&self, profile_id: &str) -> Result<(), CredentialError> {
        if !self.force_fallback {
            if let Ok(entry) = keyring::Entry::new(SERVICE, profile_id) {
                let _ = entry.delete_credential();
            }
        }
        let path = self.file_path(profile_id)?;
        let _ = fs::remove_file(path);
        Ok(())
    }

    // ---------------------------------------------------------- file fallback

    /// T-032: whether D-007's encrypted-file fallback holds key material on
    /// disk — which a config subprocess can read unless a sandbox hides it, so
    /// the consent text must say so. Checked without creating the directory.
    pub fn file_fallback_present(&self) -> bool {
        let dir = match &self.fallback_dir {
            Some(d) => Some(d.clone()),
            None => dirs::data_dir().map(|d| d.join("sluice").join("credentials")),
        };
        dir.is_some_and(|d| d.join("key").is_file())
    }

    fn base_dir(&self) -> Result<PathBuf, CredentialError> {
        let dir = match &self.fallback_dir {
            Some(d) => d.clone(),
            None => dirs::data_dir()
                .ok_or_else(|| CredentialError::Backend("no data dir".into()))?
                .join("sluice")
                .join("credentials"),
        };
        fs::create_dir_all(&dir).map_err(|e| CredentialError::Backend(e.to_string()))?;
        restrict(&dir)?;
        Ok(dir)
    }

    fn file_path(&self, profile_id: &str) -> Result<PathBuf, CredentialError> {
        // Profile ids come from our own config, but a path separator in one
        // would escape the directory, so they are never used raw.
        let safe: String = profile_id
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
            .collect();
        Ok(self.base_dir()?.join(format!("{safe}.bin")))
    }

    /// The AEAD key.
    ///
    /// Honest about what this does and does not buy: with no secret service
    /// there is nowhere safe to put a master key, so the key sits beside the
    /// ciphertext with 0600 permissions. That defends against casual reads,
    /// stray backups, and sync clients — **not** against a local attacker
    /// already running as this user. D-007 accepts that; the alternative it
    /// rejected (a master password) means owning key derivation and unlock UX.
    fn key(&self) -> Result<Key, CredentialError> {
        let path = self.base_dir()?.join("key");
        if let Ok(bytes) = fs::read(&path) {
            if bytes.len() == 32 {
                return Ok(*Key::from_slice(&bytes));
            }
        }
        let key = ChaCha20Poly1305::generate_key(&mut OsRng);
        write_private(&path, &key)?;
        Ok(key)
    }

    fn file_store(&self, profile_id: &str, secret: &Secret<String>) -> Result<(), CredentialError> {
        let key = self.key()?;
        let cipher = ChaCha20Poly1305::new(&key);
        let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
        let ct = cipher
            .encrypt(&nonce, secret.expose().as_bytes())
            .map_err(|e| CredentialError::Backend(e.to_string()))?;

        let mut blob = nonce.to_vec();
        blob.extend_from_slice(&ct);
        write_private(&self.file_path(profile_id)?, &blob)
    }

    fn file_resolve(&self, profile_id: &str) -> Result<Secret<String>, CredentialError> {
        let blob = fs::read(self.file_path(profile_id)?).map_err(|_| CredentialError::NotFound)?;
        if blob.len() < 12 {
            return Err(CredentialError::NotFound);
        }
        let (nonce, ct) = blob.split_at(12);
        let cipher = ChaCha20Poly1305::new(&self.key()?);
        let plain = cipher
            .decrypt(Nonce::from_slice(nonce), ct)
            .map_err(|_| CredentialError::Backend("credential failed authentication".into()))?;
        String::from_utf8(plain)
            .map(Secret::new)
            .map_err(|_| CredentialError::Backend("corrupt credential".into()))
    }
}

/// A shape-only label for a keychain error: the variant, never the backend's
/// text, which is free-form and not ours to vouch for.
fn keychain_error_label(e: &keyring::Error) -> String {
    match e {
        keyring::Error::NoStorageAccess(_) => "keychain access denied or unavailable",
        keyring::Error::PlatformFailure(_) => "keychain platform error",
        keyring::Error::BadEncoding(_) => "stored value is not valid text",
        _ => "keychain error",
    }
    .to_string()
}

/// The file fallback's answer wins when it has one; otherwise a keychain read
/// failure is surfaced as `Unreadable` rather than hidden behind `NotFound`.
fn merge_fallback(
    file: Result<Secret<String>, CredentialError>,
    keychain_failure: Option<String>,
) -> Result<Secret<String>, CredentialError> {
    match (file, keychain_failure) {
        (Err(CredentialError::NotFound), Some(why)) => Err(CredentialError::Unreadable(why)),
        (other, _) => other,
    }
}

fn write_private(path: &PathBuf, bytes: &[u8]) -> Result<(), CredentialError> {
    let mut f = fs::File::create(path).map_err(|e| CredentialError::Backend(e.to_string()))?;
    f.write_all(bytes).map_err(|e| CredentialError::Backend(e.to_string()))?;
    drop(f);
    restrict(path)
}

#[cfg(unix)]
fn restrict(path: &std::path::Path) -> Result<(), CredentialError> {
    use std::os::unix::fs::PermissionsExt;
    let meta = fs::metadata(path).map_err(|e| CredentialError::Backend(e.to_string()))?;
    let mode = if meta.is_dir() { 0o700 } else { 0o600 };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|e| CredentialError::Backend(e.to_string()))
}

#[cfg(not(unix))]
fn restrict(_path: &std::path::Path) -> Result<(), CredentialError> {
    // Windows inherits the user's profile ACL for %APPDATA%.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("sluice-cred-test-{}", std::process::id()))
            .join(format!("{:?}", std::time::SystemTime::now()));
        let _ = fs::create_dir_all(&d);
        d
    }

    #[test]
    fn encrypted_file_fallback_round_trips() {
        let store = CredentialStore::with_fallback_dir(tmp());
        let backend = store.store("profile-a", &Secret::new("hunter2".into())).unwrap();
        assert_eq!(backend, Backend::EncryptedFile);
        assert_eq!(store.resolve("profile-a").unwrap().expose(), "hunter2");
    }

    /// The fallback must actually be encrypted — a plaintext file with tight
    /// permissions would satisfy the words of D-007 but not the intent.
    #[test]
    fn the_fallback_file_does_not_contain_the_plaintext() {
        let dir = tmp();
        let store = CredentialStore::with_fallback_dir(dir.clone());
        store.store("profile-b", &Secret::new("correct-horse-battery-staple".into())).unwrap();

        let path = store.file_path("profile-b").unwrap();
        let bytes = fs::read(&path).unwrap();
        let as_text = String::from_utf8_lossy(&bytes);
        assert!(
            !as_text.contains("correct-horse-battery-staple"),
            "secret must not be recoverable by reading the file"
        );
    }

    #[test]
    fn tampering_is_detected_rather_than_returning_garbage() {
        let dir = tmp();
        let store = CredentialStore::with_fallback_dir(dir);
        store.store("profile-c", &Secret::new("s3cret".into())).unwrap();

        let path = store.file_path("profile-c").unwrap();
        let mut bytes = fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        fs::write(&path, &bytes).unwrap();

        // AEAD: authentication failure, not a silently wrong password.
        assert!(matches!(
            store.resolve("profile-c"),
            Err(CredentialError::Backend(_))
        ));
    }

    #[test]
    fn missing_profiles_are_not_found_rather_than_erroring_obscurely() {
        let store = CredentialStore::with_fallback_dir(tmp());
        assert!(matches!(store.resolve("nope"), Err(CredentialError::NotFound)));
    }

    #[test]
    fn profile_ids_cannot_escape_the_credential_directory() {
        let dir = tmp();
        let store = CredentialStore::with_fallback_dir(dir.clone());
        let path = store.file_path("../../etc/passwd").unwrap();
        assert!(path.starts_with(&dir), "path traversal: {path:?}");
        assert!(!path.to_string_lossy().contains(".."));
    }

    #[cfg(unix)]
    #[test]
    fn stored_files_are_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let store = CredentialStore::with_fallback_dir(tmp());
        store.store("profile-d", &Secret::new("x".into())).unwrap();
        let path = store.file_path("profile-d").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "got {mode:o}");
    }

    /// Error text is rendered in the UI and written to logs, so it must never
    /// carry the secret it failed to handle.
    #[test]
    fn a_keychain_read_failure_is_unreadable_not_not_found() {
        let r = merge_fallback(Err(CredentialError::NotFound), Some("keychain platform error".into()));
        assert!(matches!(r, Err(CredentialError::Unreadable(_))));
        let text = r.err().unwrap().to_string();
        assert!(text.contains("enter it again"), "{text}");
    }

    #[test]
    fn the_file_fallback_still_answers_when_the_keychain_fails() {
        let r = merge_fallback(Ok(Secret::new("pw".into())), Some("keychain error".into()));
        assert_eq!(r.unwrap().expose(), "pw");
        // And no keychain failure means plain NotFound is preserved.
        assert!(matches!(merge_fallback(Err(CredentialError::NotFound), None), Err(CredentialError::NotFound)));
    }

    #[test]
    fn credential_errors_never_embed_the_secret() {
        let e = CredentialError::Backend("libsecret: no such collection".into());
        let text = e.to_string();
        assert!(!text.contains("hunter2"));
        assert!(text.contains("credential store unavailable"));
    }
}

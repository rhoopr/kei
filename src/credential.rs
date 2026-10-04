//! Credential storage with OS keyring and encrypted file fallback.
//!
//! Passwords are stored via the OS-native credential store when available
//! (macOS Keychain, Linux Secret Service, Windows Credential Manager). In
//! environments without a keyring backend (Docker, headless servers), an
//! encrypted file in the config directory is used instead.
//!
//! # Encrypted file backend — threat model
//!
//! The encrypted file backend prevents casual exposure — passwords won't
//! appear in `grep`, `cat`, or accidental backups. However, the encryption
//! key resides in the same directory as the ciphertext. An attacker with
//! filesystem access to the config volume can decrypt the password. For
//! stronger protection, use `--password-command` with an external secret
//! manager or the OS keyring on native platforms.

use std::path::{Path, PathBuf};

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm};
use anyhow::{Context, Result};
use secrecy::SecretString;

/// Service name used for keyring entries.
const KEYRING_SERVICE: &str = "kei";

#[derive(Debug)]
enum DeleteOutcome {
    Deleted,
    NotFound,
    Failed(anyhow::Error),
}

/// Backend that accepted a stored credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CredentialBackend {
    Keyring,
    EncryptedFile,
}

impl CredentialBackend {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Keyring => "keyring",
            Self::EncryptedFile => "encrypted-file",
        }
    }
}

/// Credential store that tries the OS keyring first, falling back to an
/// AES-256-GCM encrypted file in the config directory.
#[derive(Debug)]
pub(crate) struct CredentialStore {
    username: String,
    config_dir: PathBuf,
    realm: String,
}

impl CredentialStore {
    pub(crate) fn new(username: &str, config_dir: &Path, realm: &str) -> Self {
        Self {
            username: username.to_string(),
            config_dir: config_dir.to_path_buf(),
            realm: realm.to_string(),
        }
    }

    /// Store a password. Tries keyring first, falls back to encrypted file.
    pub(crate) fn store(&self, password: &str) -> Result<CredentialBackend> {
        match self.keyring_store(password) {
            Ok(()) => {
                tracing::debug!(backend = "keyring", "Credential stored");
                Ok(CredentialBackend::Keyring)
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Keyring unavailable; falling back to encrypted file in config directory"
                );
                self.file_store(password)?;
                tracing::debug!(backend = "encrypted-file", "Credential stored");
                Ok(CredentialBackend::EncryptedFile)
            }
        }
    }

    /// Retrieve a stored password. Tries keyring first, falls back to encrypted file.
    pub(crate) fn retrieve(&self) -> Result<Option<SecretString>> {
        match self.keyring_retrieve() {
            Ok(Some(pw)) => {
                tracing::debug!(backend = "keyring", "Credential retrieved");
                return Ok(Some(pw));
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Keyring unavailable; trying encrypted file in config directory"
                );
            }
        }
        let result = self.file_retrieve()?;
        if result.is_some() {
            tracing::debug!(backend = "encrypted-file", "Credential retrieved");
        }
        Ok(result)
    }

    /// Delete stored credentials from all backends.
    pub(crate) fn delete(&self) -> Result<()> {
        finish_delete(
            &self.username,
            self.keyring_delete_outcome(),
            self.file_delete_outcome(),
        )
    }

    /// Check whether a credential exists in any backend (keyring or file).
    pub(crate) fn has_credential(&self) -> bool {
        if self.keyring_retrieve().ok().flatten().is_some() {
            return true;
        }
        self.credential_file_path().exists()
    }

    /// Return the name of the currently active backend.
    pub(crate) fn backend_name(&self) -> &'static str {
        if self
            .keyring_entry()
            .and_then(|e| e.get_password().map_err(Into::into))
            .is_ok()
        {
            return "keyring";
        }
        if self.credential_file_path().exists() {
            "encrypted-file"
        } else {
            "none"
        }
    }

    // ── Keyring backend ──────────────────���─────────────────────────

    fn keyring_entry(&self) -> Result<keyring::Entry> {
        keyring::Entry::new(KEYRING_SERVICE, &self.username)
            .context("Could not open the OS keyring entry")
    }

    fn keyring_store(&self, password: &str) -> Result<()> {
        let entry = self.keyring_entry()?;
        entry
            .set_password(password)
            .context("Could not store password in the OS keyring")
    }

    fn keyring_retrieve(&self) -> Result<Option<SecretString>> {
        let entry = self.keyring_entry()?;
        match entry.get_password() {
            Ok(pw) => Ok(Some(SecretString::from(pw))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => {
                Err(anyhow::anyhow!(e).context("Could not read password from the OS keyring"))
            }
        }
    }

    fn keyring_delete_outcome(&self) -> DeleteOutcome {
        let entry = match self.keyring_entry() {
            Ok(entry) => entry,
            Err(e) => return DeleteOutcome::Failed(e),
        };
        match entry.delete_credential() {
            Ok(()) => DeleteOutcome::Deleted,
            Err(e) => keyring_delete_error_outcome(e),
        }
    }

    // ── Encrypted file backend ─────────────────────────────────────

    fn key_file_path(&self) -> PathBuf {
        self.config_dir.join(".kei-state")
    }

    fn credential_file_path(&self) -> PathBuf {
        let sanitized = crate::account::namespace(&self.username, &self.realm);
        self.config_dir.join(format!("{sanitized}.credential"))
    }

    /// Load or generate the AES-256 key.
    fn load_or_create_key(&self) -> Result<[u8; 32]> {
        let key_path = self.key_file_path();
        if key_path.exists() {
            let data = std::fs::read(&key_path).with_context(|| {
                format!("Could not read credential key file {}", key_path.display())
            })?;
            anyhow::ensure!(
                data.len() == 32,
                "Credential key file is corrupt: expected 32 bytes, got {}.",
                data.len()
            );
            let mut key = [0u8; 32];
            key.copy_from_slice(&data);
            Ok(key)
        } else {
            let key: [u8; 32] = rand::random();
            atomic_write_sync(&key_path, &key)?;
            Ok(key)
        }
    }

    fn file_store(&self, password: &str) -> Result<()> {
        std::fs::create_dir_all(&self.config_dir).with_context(|| {
            format!(
                "Could not create config directory {}",
                self.config_dir.display()
            )
        })?;

        let key_bytes = self.load_or_create_key()?;
        let cipher = Aes256Gcm::new_from_slice(&key_bytes)
            .context("Could not initialize credential encryption")?;
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ciphertext = cipher
            .encrypt(&nonce, password.as_bytes())
            .map_err(|e| anyhow::anyhow!("Could not encrypt credential: {e}"))?;

        // File format: 12-byte nonce ‖ ciphertext
        let mut data = Vec::with_capacity(12 + ciphertext.len());
        data.extend_from_slice(&nonce);
        data.extend_from_slice(&ciphertext);

        atomic_write_sync(&self.credential_file_path(), &data)
    }

    fn file_retrieve(&self) -> Result<Option<SecretString>> {
        let cred_path = self.credential_file_path();
        if !cred_path.exists() {
            return Ok(None);
        }

        let key_path = self.key_file_path();
        anyhow::ensure!(
            key_path.exists(),
            "Credential file exists but key file is missing: {}",
            key_path.display()
        );

        let key_bytes = self.load_or_create_key()?;
        let data = std::fs::read(&cred_path)
            .with_context(|| format!("Could not read credential file {}", cred_path.display()))?;

        anyhow::ensure!(
            data.len() > 12,
            "Credential file is corrupt or incomplete: {}",
            cred_path.display()
        );

        let (nonce_bytes, ciphertext) = data.split_at(12);
        let nonce = aes_gcm::Nonce::from_slice(nonce_bytes);
        let cipher = Aes256Gcm::new_from_slice(&key_bytes)
            .context("Could not initialize credential decryption")?;
        let plaintext = cipher.decrypt(nonce, ciphertext).map_err(|_e| {
            anyhow::anyhow!("Could not decrypt stored credential. The key may be wrong or the credential file may be corrupt.")
        })?;

        let password =
            String::from_utf8(plaintext).context("Stored credential is not valid UTF-8")?;
        Ok(Some(SecretString::from(password)))
    }

    #[cfg(test)]
    fn file_delete(&self) -> Result<()> {
        match self.file_delete_outcome() {
            DeleteOutcome::Deleted => Ok(()),
            DeleteOutcome::NotFound => {
                anyhow::bail!(
                    "No credential file found at {}",
                    self.credential_file_path().display()
                )
            }
            DeleteOutcome::Failed(e) => Err(e),
        }
    }

    fn file_delete_outcome(&self) -> DeleteOutcome {
        let cred_path = self.credential_file_path();
        if !cred_path.exists() {
            return DeleteOutcome::NotFound;
        }
        match std::fs::remove_file(&cred_path) {
            Ok(()) => DeleteOutcome::Deleted,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => DeleteOutcome::NotFound,
            Err(e) => DeleteOutcome::Failed(anyhow::anyhow!(e).context(format!(
                "Could not delete credential file {}",
                cred_path.display()
            ))),
        }
    }
}

fn keyring_delete_error_outcome(error: keyring::Error) -> DeleteOutcome {
    match error {
        keyring::Error::NoEntry => DeleteOutcome::NotFound,
        keyring::Error::PlatformFailure(e) if is_keyring_platform_unavailable(e.as_ref()) => {
            tracing::debug!(
                error = %e,
                "Keyring unavailable during credential deletion; treating it as absent"
            );
            DeleteOutcome::NotFound
        }
        e => DeleteOutcome::Failed(
            anyhow::anyhow!(e).context("Could not delete the OS keyring credential"),
        ),
    }
}

fn is_keyring_platform_unavailable(error: &(dyn std::error::Error + Send + Sync)) -> bool {
    let message = error.to_string();
    message.contains("org.freedesktop.secrets")
        || message.contains("The name is not activatable")
        || (message.contains("Failed to connect")
            && message.contains("/run/user/")
            && message.contains("/bus"))
}

fn finish_delete(
    username: &str,
    keyring: DeleteOutcome,
    encrypted_file: DeleteOutcome,
) -> Result<()> {
    let mut deleted = Vec::new();
    let mut failed = Vec::new();

    match keyring {
        DeleteOutcome::Deleted => deleted.push("keyring"),
        DeleteOutcome::NotFound => {}
        DeleteOutcome::Failed(e) => failed.push(("keyring", e)),
    }
    match encrypted_file {
        DeleteOutcome::Deleted => deleted.push("encrypted-file"),
        DeleteOutcome::NotFound => {}
        DeleteOutcome::Failed(e) => failed.push(("encrypted-file", e)),
    }

    if failed.is_empty() {
        if deleted.is_empty() {
            anyhow::bail!("No stored credential was found for {username}.");
        }
        return Ok(());
    }

    let failed_names = failed
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(", ");
    let details = failed
        .into_iter()
        .map(|(name, err)| format!("{name}: {err:#}"))
        .collect::<Vec<_>>()
        .join("; ");
    if deleted.is_empty() {
        anyhow::bail!("Could not delete credential from {failed_names}: {details}");
    }
    anyhow::bail!(
        "Credential was deleted from {}, but kei could not delete it from {failed_names}: {details}",
        deleted.join(", ")
    );
}

/// Atomically write data to a file with 0o600 permissions (synchronous).
///
/// The temp file is fsynced before the rename and the parent directory is
/// fsynced afterwards (Unix only) so a power loss between the rename
/// returning and the kernel committing data + directory blocks can't leave
/// the credential file pointing at uninitialised content. A corrupt
/// credential file forces the user to re-enter their password (loud
/// recovery), which is acceptable; an empty-or-garbage credential file
/// silently failing decrypt is the failure mode this avoids.
fn atomic_write_sync(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;

    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)
            .with_context(|| format!("Failed to open temp file: {}", tmp.display()))?;
        f.write_all(data)
            .with_context(|| format!("Failed to write temp file: {}", tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("Failed to fsync temp file: {}", tmp.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("Failed to rename {} to {}", tmp.display(), path.display()))?;
    if let Err(e) = crate::fs_util::fsync_parent_dir(path) {
        tracing::warn!(
            path = %path.display(),
            error = %e,
            "fsync of parent directory failed after atomic_write_sync"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    /// Each test gets its own temp directory to avoid parallel test
    /// interference with the shared `.kei-state` key file.
    fn test_dir(_name: &str) -> (tempfile::TempDir, PathBuf) {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().to_path_buf();
        (td, path)
    }

    #[test]
    fn encrypted_namespace_does_not_read_copy_or_delete_colliding_legacy_credentials() {
        let directory = tempfile::tempdir().unwrap();
        let a = "first.last@example.invalid";
        let b = "firstlast@example.invalid";
        let legacy = directory.path().join("firstlastexampleinvalid.credential");
        std::fs::write(&legacy, b"synthetic legacy ciphertext").unwrap();
        let store = CredentialStore::new(a, directory.path(), "com");
        let other = CredentialStore::new(b, directory.path(), "com");
        let china = CredentialStore::new(a, directory.path(), "cn");
        assert!(store.file_retrieve().unwrap().is_none());
        assert_ne!(store.credential_file_path(), other.credential_file_path());
        assert_ne!(store.credential_file_path(), china.credential_file_path());
        store.file_store("synthetic new password").unwrap();
        assert!(other.file_retrieve().unwrap().is_none());
        assert!(china.file_retrieve().unwrap().is_none());
        store.file_delete_outcome();
        assert_eq!(
            std::fs::read(legacy).unwrap(),
            b"synthetic legacy ciphertext"
        );
    }

    #[test]
    fn encrypted_file_store_retrieve_cycle() {
        let (_td, dir) = test_dir("store_retrieve");
        let store = CredentialStore::new("user@example.com", &dir, "com");
        store.file_store("super_secret_pw").unwrap();
        let retrieved = store.file_retrieve().unwrap().unwrap();
        assert_eq!(retrieved.expose_secret(), "super_secret_pw");
    }

    #[test]
    fn encrypted_file_missing_returns_none() {
        let (_td, dir) = test_dir("missing");
        let store = CredentialStore::new("user@example.com", &dir, "com");
        let result = store.file_retrieve().unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn encrypted_file_delete() {
        let (_td, dir) = test_dir("delete");
        let store = CredentialStore::new("user@example.com", &dir, "com");
        store.file_store("to_be_deleted").unwrap();
        assert!(store.credential_file_path().exists());

        store.file_delete().unwrap();
        assert!(!store.credential_file_path().exists());
        // Key file intentionally preserved
        assert!(store.key_file_path().exists());
    }

    #[test]
    fn delete_outcome_file_only_success_reports_backend_state() {
        let cases = [
            (
                "only encrypted-file deleted",
                DeleteOutcome::NotFound,
                DeleteOutcome::Deleted,
            ),
            (
                "only keyring deleted",
                DeleteOutcome::Deleted,
                DeleteOutcome::NotFound,
            ),
        ];

        for (label, keyring, file) in cases {
            finish_delete("user@example.com", keyring, file)
                .unwrap_or_else(|err| panic!("{label} must be treated as success: {err}"));
        }

        let partial = finish_delete(
            "user@example.com",
            DeleteOutcome::Failed(anyhow::anyhow!("keyring locked")),
            DeleteOutcome::Deleted,
        )
        .unwrap_err()
        .to_string();
        assert!(
            partial.contains("deleted from encrypted-file")
                && partial.contains("could not delete it from keyring"),
            "partial delete must preserve which backend succeeded and which failed: {partial}"
        );

        let missing = finish_delete(
            "user@example.com",
            DeleteOutcome::NotFound,
            DeleteOutcome::NotFound,
        )
        .unwrap_err()
        .to_string();
        assert!(
            missing.contains("No stored credential was found"),
            "two missing backends must not be treated as file-only success: {missing}"
        );
    }

    #[test]
    fn delete_outcome_both_missing_reports_no_stored_credential() {
        let err = finish_delete(
            "user@example.com",
            DeleteOutcome::NotFound,
            DeleteOutcome::NotFound,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("No stored credential was found"),
            "expected no-credential error, got: {err}"
        );
    }

    #[test]
    fn delete_outcome_deleted_plus_failure_reports_partial_backend() {
        let err = finish_delete(
            "user@example.com",
            DeleteOutcome::Deleted,
            DeleteOutcome::Failed(anyhow::anyhow!("permission denied")),
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("could not delete it from encrypted-file"),
            "partial failure must name the failed backend: {msg}"
        );
    }

    #[test]
    fn delete_outcome_failure_without_delete_names_backend() {
        let err = finish_delete(
            "user@example.com",
            DeleteOutcome::Failed(anyhow::anyhow!("keyring locked")),
            DeleteOutcome::NotFound,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("keyring") && msg.contains("keyring locked"),
            "failed deletion must name backend and source error: {msg}"
        );
    }

    #[test]
    fn unavailable_secret_service_delete_is_not_found() {
        for message in [
            "DBus error: The name org.freedesktop.secrets was not provided by any .service files",
            "DBus error: The name is not activatable",
        ] {
            let outcome = keyring_delete_error_outcome(keyring::Error::PlatformFailure(Box::new(
                std::io::Error::other(message),
            )));
            assert!(
                matches!(outcome, DeleteOutcome::NotFound),
                "headless Secret Service delete should behave like an absent keyring"
            );
        }
    }

    #[test]
    fn inaccessible_keyring_delete_still_fails() {
        let outcome = keyring_delete_error_outcome(keyring::Error::NoStorageAccess(Box::new(
            std::io::Error::other("keyring locked"),
        )));
        assert!(
            matches!(outcome, DeleteOutcome::Failed(_)),
            "locked keyrings should not be treated as absent"
        );
    }

    #[test]
    fn encrypted_file_corrupt_data() {
        let (_td, dir) = test_dir("corrupt");
        let store = CredentialStore::new("user@example.com", &dir, "com");
        store.file_store("valid").unwrap();
        // Overwrite credential with garbage (too short)
        std::fs::write(store.credential_file_path(), b"short").unwrap();
        let err = store.file_retrieve().unwrap_err();
        assert!(err.to_string().contains("corrupt"), "{err}");
    }

    #[test]
    fn encrypted_file_wrong_key() {
        let (_td, dir) = test_dir("wrong_key");
        let store = CredentialStore::new("user@example.com", &dir, "com");
        store.file_store("secret").unwrap();
        // Overwrite key with different random key
        let bad_key: [u8; 32] = rand::random();
        std::fs::write(store.key_file_path(), bad_key).unwrap();
        let err = store.file_retrieve().unwrap_err();
        assert!(
            err.to_string().contains("decrypt"),
            "Expected decryption error, got: {err}"
        );
    }

    #[test]
    fn encrypted_file_rejects_valid_decrypt_invalid_utf8() {
        let (_td, dir) = test_dir("invalid_utf8");
        let store = CredentialStore::new("user@example.com", &dir, "com");
        std::fs::create_dir_all(&dir).unwrap();

        let key_bytes = store.load_or_create_key().unwrap();
        let cipher = Aes256Gcm::new_from_slice(&key_bytes).unwrap();
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ciphertext = cipher.encrypt(&nonce, [0xff, 0xfe].as_slice()).unwrap();
        let mut data = Vec::with_capacity(12 + ciphertext.len());
        data.extend_from_slice(&nonce);
        data.extend_from_slice(&ciphertext);
        atomic_write_sync(&store.credential_file_path(), &data).unwrap();

        let err = store.file_retrieve().unwrap_err();
        assert!(
            err.to_string().contains("valid UTF-8"),
            "invalid UTF-8 plaintext must be rejected loudly, got: {err}"
        );
    }

    #[test]
    fn encrypted_file_key_generation() {
        let (_td, dir) = test_dir("keygen");
        let store = CredentialStore::new("user@example.com", &dir, "com");
        assert!(!store.key_file_path().exists());

        store.file_store("pw").unwrap();
        assert!(store.key_file_path().exists());

        let key = std::fs::read(store.key_file_path()).unwrap();
        assert_eq!(key.len(), 32);
    }

    #[test]
    fn legacy_credential_key_is_not_auto_renamed() {
        let (_td, dir) = test_dir("legacy_key");
        let legacy_path = dir.join(".credential-key");
        std::fs::write(&legacy_path, [7u8; 32]).unwrap();

        let store = CredentialStore::new("user@example.com", &dir, "com");
        let key_path = store.key_file_path();
        assert_eq!(key_path, dir.join(".kei-state"));
        assert!(
            legacy_path.exists(),
            "legacy .credential-key must be left for manual user migration"
        );
        assert!(
            !key_path.exists(),
            ".kei-state must not be created or renamed by path lookup"
        );

        store.load_or_create_key().unwrap();
        assert!(
            legacy_path.exists(),
            "creating .kei-state must not consume legacy .credential-key"
        );
        assert!(key_path.exists());
        assert_ne!(
            std::fs::read(key_path).unwrap(),
            vec![7u8; 32],
            ".kei-state must not reuse the legacy key bytes automatically"
        );
    }

    #[test]
    fn has_credential_with_file() {
        let (_td, dir) = test_dir("has_cred");
        let store = CredentialStore::new("user@example.com", &dir, "com");
        assert!(!store.credential_file_path().exists());

        store.file_store("pw").unwrap();
        assert!(store.has_credential());
    }

    #[test]
    fn public_api_store_retrieve_round_trips() {
        let (_td, dir) = test_dir("pub_rt");
        let store = CredentialStore::new("pub-rt@kei-test.invalid", &dir, "com");
        store.store("public_api_password").unwrap();
        let retrieved = store.retrieve().unwrap().unwrap();
        assert_eq!(retrieved.expose_secret(), "public_api_password");
        let _ = store.delete();
    }

    #[test]
    fn public_api_delete_clears_credential() {
        let (_td, dir) = test_dir("pub_delete");
        let store = CredentialStore::new("pub-del@kei-test.invalid", &dir, "com");
        store.store("to_delete").unwrap();
        assert!(store.retrieve().unwrap().is_some());

        let delete_result = store.delete();
        if let Err(err) = delete_result {
            assert!(
                err.to_string().contains("partially failed") && err.to_string().contains("keyring"),
                "headless keyring failures should surface as partial deletion, got: {err}"
            );
            assert!(
                !store.credential_file_path().exists(),
                "partial delete should still remove the encrypted-file credential"
            );
        }
        assert!(store.retrieve().unwrap().is_none());
    }

    #[test]
    fn public_api_retrieve_empty_returns_none() {
        let (_td, dir) = test_dir("pub_empty");
        let store = CredentialStore::new("pub-empty@kei-test.invalid", &dir, "com");
        let result = store.retrieve().unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn atomic_write_sync_permissions() {
        let (_td, dir) = test_dir("atomic_perms");
        let path = dir.join("test_atomic.bin");
        atomic_write_sync(&path, b"test data").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let mode = std::fs::metadata(&path).unwrap().mode();
            assert_eq!(mode & 0o777, 0o600, "Expected 0o600, got {mode:o}");
        }
    }
}

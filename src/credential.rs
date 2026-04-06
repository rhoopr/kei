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

/// Credential store that tries the OS keyring first, falling back to an
/// AES-256-GCM encrypted file in the config directory.
pub struct CredentialStore {
    username: String,
    config_dir: PathBuf,
}

impl CredentialStore {
    pub fn new(username: &str, config_dir: &Path) -> Self {
        Self {
            username: username.to_string(),
            config_dir: config_dir.to_path_buf(),
        }
    }

    /// Store a password. Tries keyring first, falls back to encrypted file.
    pub fn store(&self, password: &str) -> Result<()> {
        match self.keyring_store(password) {
            Ok(()) => {
                tracing::debug!(backend = "keyring", "Credential stored");
                Ok(())
            }
            Err(e) => {
                tracing::debug!(error = %e, "Keyring unavailable, using encrypted file");
                self.file_store(password)?;
                tracing::debug!(backend = "encrypted-file", "Credential stored");
                Ok(())
            }
        }
    }

    /// Retrieve a stored password. Tries keyring first, falls back to encrypted file.
    pub fn retrieve(&self) -> Result<Option<SecretString>> {
        match self.keyring_retrieve() {
            Ok(Some(pw)) => {
                tracing::debug!(backend = "keyring", "Credential retrieved");
                return Ok(Some(pw));
            }
            Ok(None) => {}
            Err(e) => {
                tracing::debug!(error = %e, "Keyring unavailable, trying encrypted file");
            }
        }
        let result = self.file_retrieve()?;
        if result.is_some() {
            tracing::debug!(backend = "encrypted-file", "Credential retrieved");
        }
        Ok(result)
    }

    /// Delete stored credentials from all backends.
    pub fn delete(&self) -> Result<()> {
        let mut deleted = false;
        if let Err(e) = self.keyring_delete() {
            tracing::debug!(error = %e, "Keyring delete failed or not found");
        } else {
            deleted = true;
        }
        if let Err(e) = self.file_delete() {
            tracing::debug!(error = %e, "Encrypted file delete failed or not found");
        } else {
            deleted = true;
        }
        if deleted {
            Ok(())
        } else {
            anyhow::bail!("No stored credential found for {}", self.username)
        }
    }

    /// Check whether a credential exists in any backend (keyring or file).
    pub fn has_credential(&self) -> bool {
        if self.keyring_retrieve().ok().flatten().is_some() {
            return true;
        }
        self.credential_file_path().exists()
    }

    /// Return the name of the currently active backend.
    pub fn backend_name(&self) -> &'static str {
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
            .context("Failed to create keyring entry")
    }

    fn keyring_store(&self, password: &str) -> Result<()> {
        let entry = self.keyring_entry()?;
        entry
            .set_password(password)
            .context("Failed to store password in keyring")
    }

    fn keyring_retrieve(&self) -> Result<Option<SecretString>> {
        let entry = self.keyring_entry()?;
        match entry.get_password() {
            Ok(pw) => Ok(Some(SecretString::from(pw))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(anyhow::anyhow!(e).context("Failed to retrieve from keyring")),
        }
    }

    fn keyring_delete(&self) -> Result<()> {
        let entry = self.keyring_entry()?;
        entry
            .delete_credential()
            .context("Failed to delete keyring credential")
    }

    // ── Encrypted file backend ─────────────────────────────────────

    fn key_file_path(&self) -> PathBuf {
        self.config_dir.join(".credential-key")
    }

    fn credential_file_path(&self) -> PathBuf {
        let sanitized = crate::auth::session::sanitize_username(&self.username);
        self.config_dir.join(format!("{sanitized}.credential"))
    }

    /// Load or generate the AES-256 key.
    fn load_or_create_key(&self) -> Result<[u8; 32]> {
        let key_path = self.key_file_path();
        if key_path.exists() {
            let data = std::fs::read(&key_path)
                .with_context(|| format!("Failed to read key file: {}", key_path.display()))?;
            anyhow::ensure!(
                data.len() == 32,
                "Credential key file is corrupt (expected 32 bytes, got {})",
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
                "Failed to create config directory: {}",
                self.config_dir.display()
            )
        })?;

        let key_bytes = self.load_or_create_key()?;
        let cipher =
            Aes256Gcm::new_from_slice(&key_bytes).context("Failed to create AES cipher")?;
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ciphertext = cipher
            .encrypt(&nonce, password.as_bytes())
            .map_err(|e| anyhow::anyhow!("Encryption failed: {e}"))?;

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
            .with_context(|| format!("Failed to read credential file: {}", cred_path.display()))?;

        anyhow::ensure!(
            data.len() > 12,
            "Credential file is corrupt (too short): {}",
            cred_path.display()
        );

        let (nonce_bytes, ciphertext) = data.split_at(12);
        let nonce = aes_gcm::Nonce::from_slice(nonce_bytes);
        let cipher =
            Aes256Gcm::new_from_slice(&key_bytes).context("Failed to create AES cipher")?;
        let plaintext = cipher.decrypt(nonce, ciphertext).map_err(|_| {
            anyhow::anyhow!("Failed to decrypt credential (wrong key or corrupt file)")
        })?;

        let password =
            String::from_utf8(plaintext).context("Decrypted credential is not valid UTF-8")?;
        Ok(Some(SecretString::from(password)))
    }

    fn file_delete(&self) -> Result<()> {
        let cred_path = self.credential_file_path();
        if !cred_path.exists() {
            anyhow::bail!("No credential file found: {}", cred_path.display());
        }
        std::fs::remove_file(&cred_path).with_context(|| {
            format!("Failed to delete credential file: {}", cred_path.display())
        })?;
        // Leave the key file — it may be shared if the user re-stores later
        Ok(())
    }
}

/// Atomically write data to a file with 0o600 permissions (synchronous).
fn atomic_write_sync(path: &Path, data: &[u8]) -> Result<()> {
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    std::fs::write(&tmp, data)
        .with_context(|| format!("Failed to write temp file: {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("Failed to rename {} to {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    /// Each test gets its own subdirectory to avoid parallel test interference
    /// with the shared `.credential-key` file.
    fn test_dir(name: &str) -> PathBuf {
        let dir = PathBuf::from(format!("/tmp/claude/test_credential/{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn encrypted_file_store_retrieve_cycle() {
        let dir = test_dir("store_retrieve");
        let store = CredentialStore::new("user@example.com", &dir);
        store.file_store("super_secret_pw").unwrap();
        let retrieved = store.file_retrieve().unwrap().unwrap();
        assert_eq!(retrieved.expose_secret(), "super_secret_pw");
    }

    #[test]
    fn encrypted_file_missing_returns_none() {
        let dir = test_dir("missing");
        let store = CredentialStore::new("user@example.com", &dir);
        let result = store.file_retrieve().unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn encrypted_file_delete() {
        let dir = test_dir("delete");
        let store = CredentialStore::new("user@example.com", &dir);
        store.file_store("to_be_deleted").unwrap();
        assert!(store.credential_file_path().exists());

        store.file_delete().unwrap();
        assert!(!store.credential_file_path().exists());
        // Key file intentionally preserved
        assert!(store.key_file_path().exists());
    }

    #[test]
    fn encrypted_file_corrupt_data() {
        let dir = test_dir("corrupt");
        let store = CredentialStore::new("user@example.com", &dir);
        store.file_store("valid").unwrap();
        // Overwrite credential with garbage (too short)
        std::fs::write(store.credential_file_path(), b"short").unwrap();
        let err = store.file_retrieve().unwrap_err();
        assert!(err.to_string().contains("corrupt"), "{err}");
    }

    #[test]
    fn encrypted_file_wrong_key() {
        let dir = test_dir("wrong_key");
        let store = CredentialStore::new("user@example.com", &dir);
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
    fn encrypted_file_key_generation() {
        let dir = test_dir("keygen");
        let store = CredentialStore::new("user@example.com", &dir);
        assert!(!store.key_file_path().exists());

        store.file_store("pw").unwrap();
        assert!(store.key_file_path().exists());

        let key = std::fs::read(store.key_file_path()).unwrap();
        assert_eq!(key.len(), 32);
    }

    #[test]
    fn has_credential_with_file() {
        let dir = test_dir("has_cred");
        let store = CredentialStore::new("user@example.com", &dir);
        assert!(!store.credential_file_path().exists());

        store.file_store("pw").unwrap();
        assert!(store.has_credential());
    }

    #[test]
    fn atomic_write_sync_permissions() {
        let dir = test_dir("atomic_perms");
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

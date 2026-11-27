//! How commands find and open an account's sync state.
//!
//! The state belongs to one account: its configured namespace and, once a
//! sync has signed in, its provider identity. Every command checks both
//! before reading or changing anything. A previous-engine state file is
//! upgraded on first use, by `sync` or by an offline command.

use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::state::db::account::AccountOwner;
use crate::state::error::StateError;

use super::State;

const ACCOUNT_KEY: &str = "account_key";
const PROVIDER_KEY: &str = "account_provider";

/// Where an account's state lives.
#[derive(Debug, Clone)]
pub(crate) struct Location {
    /// The previous engine's state file.
    pub(crate) legacy: PathBuf,
    /// The sync core's state file.
    pub(crate) current: PathBuf,
}

impl Location {
    pub(crate) async fn resolve(
        cookie_dir: &Path,
        username: &str,
        realm: &str,
    ) -> anyhow::Result<Self> {
        let legacy = crate::state::db::account::state_path(cookie_dir, username, realm).await?;
        let current = State::path_for(&legacy);
        Ok(Self { legacy, current })
    }

    /// Whether any state exists for the account.
    pub(crate) fn exists(&self) -> bool {
        self.current.exists() || self.legacy.exists()
    }
}

/// Open the account's state, upgrading a previous-engine file first if
/// needed. Returns `None` when the account has no state. `directory` is the
/// download directory the upgrade resolves recorded paths against.
pub(crate) async fn open(
    location: &Location,
    owner: &AccountOwner,
    directory: Option<&Path>,
) -> anyhow::Result<Option<State>> {
    if !location.current.exists() {
        if !location.legacy.exists() {
            return Ok(None);
        }
        let directory = directory.context(
            "Set [download].directory in the config file so kei can upgrade the previous state.",
        )?;
        if let Some(import) =
            super::upgrade::prepare(&location.legacy, &location.current, owner, directory).await?
        {
            tracing::info!(
                recorded = import.recorded,
                carried_over = import.imported,
                changed_on_disk = import.changed_on_disk,
                outside_download_directory = import.outside,
                duplicates = import.duplicates,
                "Upgraded sync state; the previous state file is kept until the first full listing completes"
            );
        }
    }
    let state = State::open(&location.current).await?;
    check(&state, owner).await?;
    Ok(Some(state))
}

/// Create state for an account that has none. Refuses existing state.
pub(crate) async fn create(location: &Location, owner: &AccountOwner) -> anyhow::Result<State> {
    anyhow::ensure!(
        !location.exists(),
        "Sync state already exists for this account"
    );
    if let Some(parent) = location.current.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let state = State::open(&location.current).await?;
    bind(&state, owner.key(), owner.provider_key()).await?;
    Ok(state)
}

/// Record which account the state belongs to.
pub(crate) async fn bind(state: &State, key: &str, provider: Option<&str>) -> anyhow::Result<()> {
    state.set_kv(ACCOUNT_KEY, key).await?;
    if let Some(provider) = provider {
        state.set_kv(PROVIDER_KEY, provider).await?;
    }
    Ok(())
}

/// Refuse state bound to another account. An authenticated owner binds its
/// provider identity the first time it opens state that lacks one.
async fn check(state: &State, owner: &AccountOwner) -> anyhow::Result<()> {
    let key = state.get_kv(ACCOUNT_KEY).await?;
    if key.as_deref() != Some(owner.key()) {
        return Err(StateError::AccountOwnerMismatch.into());
    }
    if let Some(provider) = owner.provider_key() {
        match state.get_kv(PROVIDER_KEY).await? {
            Some(bound) if bound != provider => return Err(StateError::AccountOwnerMismatch.into()),
            Some(_) => {}
            None => state.set_kv(PROVIDER_KEY, provider).await?,
        }
    }
    Ok(())
}

/// Refuse unless every existing state file, previous and current, belongs
/// to the account. Reads only.
pub(crate) async fn check_existing(
    location: &Location,
    owner: &AccountOwner,
) -> anyhow::Result<()> {
    if location.current.exists() {
        let state = State::open(&location.current).await?;
        check(&state, owner).await?;
        state.close().await?;
    }
    if location.legacy.exists() {
        let legacy = location.legacy.clone();
        let owner = owner.clone();
        tokio::task::spawn_blocking(move || {
            crate::state::db::account::open_verified_read_only(&legacy, &owner).map(drop)
        })
        .await??;
    }
    Ok(())
}

/// Delete an account's state files, previous and current, after checking
/// ownership of each. Returns the files removed.
pub(crate) async fn delete(
    location: &Location,
    owner: &AccountOwner,
) -> anyhow::Result<Vec<PathBuf>> {
    check_existing(location, owner).await?;
    let mut removed = Vec::new();
    for base in [&location.current, &location.legacy] {
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let mut name = base.as_os_str().to_owned();
            name.push(suffix);
            let path = PathBuf::from(name);
            if tokio::fs::symlink_metadata(&path).await.is_ok() {
                tokio::fs::remove_file(&path).await?;
                removed.push(path);
            }
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signed_in(user: &str, dsid: &str) -> AccountOwner {
        AccountOwner::authenticated(
            user,
            "com",
            &serde_json::from_value(serde_json::json!({"dsInfo": {"dsid": dsid}})).unwrap(),
        )
        .unwrap()
    }

    async fn location(root: &Path, user: &str) -> Location {
        Location::resolve(root, user, "com").await.unwrap()
    }

    #[tokio::test]
    async fn state_opens_only_for_its_own_account() {
        let root = tempfile::tempdir().unwrap();
        let mine = location(root.path(), "me@example.invalid").await;
        assert!(
            open(&mine, &signed_in("me@example.invalid", "1"), None)
                .await
                .unwrap()
                .is_none()
        );
        create(&mine, &signed_in("me@example.invalid", "1"))
            .await
            .unwrap();

        // The configured account can read it offline.
        let offline = AccountOwner::configured("me@example.invalid", "com");
        assert!(open(&mine, &offline, None).await.unwrap().is_some());

        // A different provider identity for the same name is refused.
        let error = open(&mine, &signed_in("me@example.invalid", "2"), None)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("account ownership does not match"));

        // Another account's file placed at this account's path is refused.
        let theirs = location(root.path(), "them@example.invalid").await;
        std::fs::copy(&mine.current, &theirs.current).unwrap();
        let error = open(
            &theirs,
            &AccountOwner::configured("them@example.invalid", "com"),
            None,
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("account ownership does not match"));
        assert!(
            delete(
                &theirs,
                &AccountOwner::configured("them@example.invalid", "com")
            )
            .await
            .is_err()
        );
        assert!(theirs.current.exists());
    }

    #[tokio::test]
    async fn delete_removes_both_state_files_after_checking_them() {
        let root = tempfile::tempdir().unwrap();
        let owner = signed_in("me@example.invalid", "1");
        let mine = location(root.path(), "me@example.invalid").await;
        crate::test_helpers::legacy_state_db(&mine.legacy, &owner);
        let upgraded = open(&mine, &owner, Some(root.path()))
            .await
            .unwrap()
            .unwrap();
        upgraded.close().await.unwrap();
        assert!(mine.current.exists() && mine.legacy.exists());
        let removed = delete(
            &mine,
            &AccountOwner::configured("me@example.invalid", "com"),
        )
        .await
        .unwrap();
        assert!(removed.contains(&mine.current) && removed.contains(&mine.legacy));
        assert!(!mine.exists());
    }
}

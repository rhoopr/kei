//! Provider checkpoints, scoped database tokens, and enumeration progress.

use async_trait::async_trait;
use chrono::Utc;
use rusqlite::OptionalExtension;

use super::{CheckpointTransition, ScopedDbSyncToken, SqliteStateDb, SyncTokenStore};
use crate::state::error::StateError;

impl SqliteStateDb {
    pub(crate) async fn begin_enum_progress(&self, zone: &str) -> Result<(), StateError> {
        let key = format!("enum_in_progress:{zone}");
        let now = Utc::now().timestamp().to_string();
        self.with_conn("begin_enum_progress", move |conn| {
            // INSERT OR IGNORE so re-entry doesn't reset the age operators use to
            // judge stuck zones; `end_enum_progress` is the only path that clears.
            conn.execute(
                "INSERT OR IGNORE INTO metadata (key, value) VALUES (?1, ?2)",
                rusqlite::params![key, now],
            )
            .map_err(|e| StateError::query("begin_enum_progress", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn end_enum_progress(&self, zone: &str) -> Result<(), StateError> {
        let key = format!("enum_in_progress:{zone}");
        self.with_conn("end_enum_progress", move |conn| {
            conn.execute("DELETE FROM metadata WHERE key = ?1", [key])
                .map_err(|e| StateError::query("end_enum_progress", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn list_interrupted_enumerations(&self) -> Result<Vec<String>, StateError> {
        self.with_conn("list_interrupted_enumerations", move |conn| {
            let mut stmt = conn
                .prepare("SELECT key FROM metadata WHERE key LIKE 'enum_in_progress:%'")
                .map_err(|e| StateError::query("list_interrupted_enumerations", e))?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|e| StateError::query("list_interrupted_enumerations", e))?;
            let mut zones = Vec::new();
            for row in rows {
                let key = row.map_err(|e| StateError::query("list_interrupted_enumerations", e))?;
                if let Some(zone) = key.strip_prefix("enum_in_progress:") {
                    zones.push(zone.to_string());
                }
            }
            Ok(zones)
        })
        .await
    }

    pub(crate) async fn get_metadata(&self, key: &str) -> Result<Option<String>, StateError> {
        let key = key.to_owned();
        self.with_conn("get_metadata", move |conn| {
            let value = conn
                .query_row("SELECT value FROM metadata WHERE key = ?1", [&key], |row| {
                    row.get::<_, String>(0)
                })
                .optional()
                .map_err(|e| StateError::query("get_metadata", e))?;

            Ok(value)
        })
        .await
    }

    pub(crate) async fn set_metadata(&self, key: &str, value: &str) -> Result<(), StateError> {
        let key = key.to_owned();
        let value = value.to_owned();
        self.with_conn("set_metadata", move |conn| {
            conn.execute(
                "INSERT INTO metadata (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                rusqlite::params![key, value],
            )
            .map_err(|e| StateError::query("set_metadata", e))?;

            Ok(())
        })
        .await
    }

    pub(crate) async fn delete_metadata_by_prefix(&self, prefix: &str) -> Result<u64, StateError> {
        let prefix = prefix.to_owned();
        self.with_conn("delete_metadata_by_prefix", move |conn| {
            let mut stmt = conn
                .prepare_cached("DELETE FROM metadata WHERE key LIKE ?1")
                .map_err(|e| StateError::query("delete_metadata_by_prefix::prepare", e))?;
            let deleted = stmt
                .execute([format!("{prefix}%")])
                .map_err(|e| StateError::query("delete_metadata_by_prefix", e))?;

            Ok(deleted as u64)
        })
        .await
    }

    pub(crate) async fn commit_checkpoint_transition(
        &self,
        transition: CheckpointTransition,
    ) -> Result<(), StateError> {
        self.with_conn_mut("commit_checkpoint_transition", move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("commit_checkpoint_transition::begin", e))?;
            for (key, value) in transition.metadata_updates {
                tx.execute(
                    "INSERT INTO metadata (key, value) VALUES (?1, ?2) \
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    rusqlite::params![key, value],
                )
                .map_err(|e| StateError::query("commit_checkpoint_transition::update", e))?;
            }
            for key in transition.metadata_deletes {
                tx.execute("DELETE FROM metadata WHERE key = ?1", [key])
                    .map_err(|e| StateError::query("commit_checkpoint_transition::delete", e))?;
            }
            tx.commit()
                .map_err(|e| StateError::query("commit_checkpoint_transition::commit", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn get_scoped_db_sync_token(
        &self,
        provider: &str,
        account: &str,
        shape_version: i64,
        scope_hash: &str,
    ) -> Result<Option<ScopedDbSyncToken>, StateError> {
        let provider = provider.to_owned();
        let account = account.to_owned();
        let scope_hash = scope_hash.to_owned();
        self.with_conn("get_scoped_db_sync_token", move |conn| {
            conn.query_row(
                "SELECT selected_zones_json, scope_json, token \
                 FROM scoped_db_sync_tokens \
                 WHERE provider = ?1 AND account = ?2 AND shape_version = ?3 AND scope_hash = ?4",
                rusqlite::params![provider, account, shape_version, scope_hash],
                |row| {
                    Ok(ScopedDbSyncToken {
                        provider: provider.clone(),
                        account: account.clone(),
                        shape_version,
                        scope_hash: scope_hash.clone(),
                        selected_zones_json: row.get(0)?,
                        scope_json: row.get(1)?,
                        token: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(|e| StateError::query("get_scoped_db_sync_token", e))
        })
        .await
    }

    pub(crate) async fn upsert_scoped_db_sync_token(
        &self,
        token: ScopedDbSyncToken,
    ) -> Result<(), StateError> {
        self.with_conn("upsert_scoped_db_sync_token", move |conn| {
            let now = Utc::now().timestamp();
            conn.execute(
                "INSERT INTO scoped_db_sync_tokens \
                    (provider, account, shape_version, scope_hash, selected_zones_json, scope_json, token, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8) \
                 ON CONFLICT(provider, account, shape_version, scope_hash) DO UPDATE SET \
                    selected_zones_json = excluded.selected_zones_json, \
                    scope_json = excluded.scope_json, \
                    token = excluded.token, \
                    updated_at = excluded.updated_at",
                rusqlite::params![
                    token.provider,
                    token.account,
                    token.shape_version,
                    token.scope_hash,
                    token.selected_zones_json,
                    token.scope_json,
                    token.token,
                    now,
                ],
            )
            .map_err(|e| StateError::query("upsert_scoped_db_sync_token", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn delete_scoped_db_sync_tokens(&self) -> Result<u64, StateError> {
        self.with_conn("delete_scoped_db_sync_tokens", move |conn| {
            let deleted = conn
                .execute("DELETE FROM scoped_db_sync_tokens", [])
                .map_err(|e| StateError::query("delete_scoped_db_sync_tokens", e))?;
            Ok(deleted as u64)
        })
        .await
    }
}

#[async_trait]
impl SyncTokenStore for SqliteStateDb {
    async fn get_metadata(&self, key: &str) -> Result<Option<String>, StateError> {
        SqliteStateDb::get_metadata(self, key).await
    }

    async fn set_metadata(&self, key: &str, value: &str) -> Result<(), StateError> {
        SqliteStateDb::set_metadata(self, key, value).await
    }

    async fn delete_metadata_by_prefix(&self, prefix: &str) -> Result<u64, StateError> {
        SqliteStateDb::delete_metadata_by_prefix(self, prefix).await
    }

    async fn commit_checkpoint_transition(
        &self,
        transition: CheckpointTransition,
    ) -> Result<(), StateError> {
        SqliteStateDb::commit_checkpoint_transition(self, transition).await
    }

    async fn get_scoped_db_sync_token(
        &self,
        provider: &str,
        account: &str,
        shape_version: i64,
        scope_hash: &str,
    ) -> Result<Option<ScopedDbSyncToken>, StateError> {
        SqliteStateDb::get_scoped_db_sync_token(self, provider, account, shape_version, scope_hash)
            .await
    }

    async fn upsert_scoped_db_sync_token(
        &self,
        token: ScopedDbSyncToken,
    ) -> Result<(), StateError> {
        SqliteStateDb::upsert_scoped_db_sync_token(self, token).await
    }

    async fn delete_scoped_db_sync_tokens(&self) -> Result<u64, StateError> {
        SqliteStateDb::delete_scoped_db_sync_tokens(self).await
    }

    async fn begin_enum_progress(&self, zone: &str) -> Result<(), StateError> {
        SqliteStateDb::begin_enum_progress(self, zone).await
    }

    async fn end_enum_progress(&self, zone: &str) -> Result<(), StateError> {
        SqliteStateDb::end_enum_progress(self, zone).await
    }

    async fn list_interrupted_enumerations(&self) -> Result<Vec<String>, StateError> {
        SqliteStateDb::list_interrupted_enumerations(self).await
    }
}

#[cfg(test)]
mod tests;

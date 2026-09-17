//! Lossless temporary-path encoding and durable file ownership.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use chrono::Utc;

use super::{OwnedTempFile, SqliteStateDb, TempFileOwnershipStore};
use crate::state::error::StateError;

#[derive(Debug, Clone)]
struct OwnedTempPathKey(Vec<u8>);

impl OwnedTempPathKey {
    fn from_path(path: &Path) -> Result<Self, StateError> {
        let absolute =
            crate::fs_util::absolute_lexical(path).map_err(|source| StateError::TempPath {
                path: path.to_path_buf(),
                source,
            })?;

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            Ok(Self(absolute.as_os_str().as_bytes().to_vec()))
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            Ok(Self(
                absolute
                    .as_os_str()
                    .encode_wide()
                    .flat_map(u16::to_le_bytes)
                    .collect(),
            ))
        }
    }

    fn into_path(self) -> Result<PathBuf, StateError> {
        #[cfg(unix)]
        {
            use std::ffi::OsString;
            use std::os::unix::ffi::OsStringExt;
            Ok(PathBuf::from(OsString::from_vec(self.0)))
        }
        #[cfg(windows)]
        {
            use std::ffi::OsString;
            use std::os::windows::ffi::OsStringExt;

            let chunks = self.0.chunks_exact(2);
            if !chunks.remainder().is_empty() {
                return Err(StateError::Invariant {
                    operation: "decode_owned_temp_path",
                    detail: "stored Windows temporary path has an odd byte length".into(),
                });
            }
            let wide: Vec<u16> = chunks
                .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
                .collect();
            Ok(PathBuf::from(OsString::from_wide(&wide)))
        }
    }
}

impl SqliteStateDb {
    pub(crate) async fn claim_temp_file(&self, path: &Path) -> Result<(), StateError> {
        let path_key = OwnedTempPathKey::from_path(path)?.0;
        let claimed_at = Utc::now().timestamp();
        self.with_conn("claim_temp_file", move |conn| {
            conn.execute(
                "INSERT INTO owned_temp_files (path, claimed_at) VALUES (?1, ?2) \
                 ON CONFLICT(path) DO UPDATE SET claimed_at = excluded.claimed_at",
                rusqlite::params![path_key, claimed_at],
            )
            .map_err(|e| StateError::query("claim_temp_file", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn get_owned_temp_files_before(
        &self,
        claimed_before: i64,
    ) -> Result<Vec<OwnedTempFile>, StateError> {
        self.with_conn("get_owned_temp_files_before", move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT path, claimed_at FROM owned_temp_files \
                     WHERE claimed_at < ?1 ORDER BY path",
                )
                .map_err(|e| StateError::query("get_owned_temp_files_before", e))?;
            let rows = stmt
                .query_map([claimed_before], |row| {
                    Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?))
                })
                .map_err(|e| StateError::query("get_owned_temp_files_before", e))?;

            let mut owned = Vec::new();
            for row in rows {
                let (path_key, claimed_at) =
                    row.map_err(|e| StateError::query("get_owned_temp_files_before", e))?;
                owned.push(OwnedTempFile {
                    path: OwnedTempPathKey(path_key).into_path()?,
                    claimed_at,
                });
            }
            Ok(owned)
        })
        .await
    }

    pub(crate) async fn retire_temp_files(&self, paths: &[PathBuf]) -> Result<u64, StateError> {
        if paths.is_empty() {
            return Ok(0);
        }
        let path_keys = paths
            .iter()
            .map(|path| OwnedTempPathKey::from_path(path).map(|key| key.0))
            .collect::<Result<Vec<_>, _>>()?;
        self.with_conn_mut("retire_temp_files", move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("retire_temp_files", e))?;
            let mut retired = 0u64;
            for path_key in path_keys {
                retired = retired.saturating_add(
                    tx.execute("DELETE FROM owned_temp_files WHERE path = ?1", [path_key])
                        .map_err(|e| StateError::query("retire_temp_files", e))?
                        as u64,
                );
            }
            tx.commit()
                .map_err(|e| StateError::query("retire_temp_files", e))?;
            Ok(retired)
        })
        .await
    }
}

#[async_trait]
impl TempFileOwnershipStore for SqliteStateDb {
    async fn claim_temp_file(&self, path: &Path) -> Result<(), StateError> {
        SqliteStateDb::claim_temp_file(self, path).await
    }

    async fn get_owned_temp_files_before(
        &self,
        claimed_before: i64,
    ) -> Result<Vec<OwnedTempFile>, StateError> {
        SqliteStateDb::get_owned_temp_files_before(self, claimed_before).await
    }

    async fn retire_temp_files(&self, paths: &[PathBuf]) -> Result<u64, StateError> {
        SqliteStateDb::retire_temp_files(self, paths).await
    }
}

#[cfg(test)]
mod tests;

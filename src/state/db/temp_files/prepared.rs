//! Verified first-publication evidence retained by the existing temporary ledger.

use std::path::Path;

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::{OwnedTempPathKey, SqliteStateDb};
use crate::state::db::provider_selection::SelectionPath;
use crate::state::{VersionSizeKey, error::StateError};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreparedPublication {
    pub(crate) library: String,
    pub(crate) id: String,
    pub(crate) version: String,
    pub(crate) provider_checksum: String,
    pub(crate) provider_size: u64,
    pub(crate) temporary: SelectionPath,
    pub(crate) destination: SelectionPath,
    pub(crate) local_checksum: String,
    pub(crate) source_checksum: String,
}

fn invalid() -> StateError {
    StateError::Invariant {
        operation: "prepared_publication",
        detail: "prepared publication evidence is invalid or no longer matches current work".into(),
    }
}

impl PreparedPublication {
    fn validate(&self) -> Result<(), StateError> {
        let digest = |value: &str| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        };
        if self.library.is_empty()
            || self.id.is_empty()
            || self.provider_checksum.trim().is_empty()
            || VersionSizeKey::from_str(&self.version).is_none()
            || self.provider_size > i64::MAX as u64
            || !digest(&self.local_checksum)
            || !digest(&self.source_checksum)
        {
            return Err(invalid());
        }
        crate::fs_util::confined_path_key(&self.destination.to_path()).map_err(|_| invalid())?;
        crate::fs_util::confined_path_key(&self.temporary.to_path()).map_err(|_| invalid())?;
        Ok(())
    }
}

impl SqliteStateDb {
    pub(super) async fn prepare_publication(
        &self,
        part: &Path,
        proof: &PreparedPublication,
    ) -> Result<(), StateError> {
        proof.validate()?;
        let key = OwnedTempPathKey::from_path(part)?.0;
        if OwnedTempPathKey::from_path(&proof.temporary.to_path())?.0 != key {
            return Err(invalid());
        }
        let proof = proof.clone();
        let encoded = serde_json::to_string(&proof).map_err(|_| invalid())?;
        if encoded.len() > 64 * 1024 {
            return Err(invalid());
        }
        self.with_conn_mut("prepare_temp_publication", move |conn| {
            let tx = conn.transaction()?;
            let current: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM assets WHERE library=?1 AND id=?2 AND version_size=?3
                 AND checksum=?4 AND size_bytes=?5 AND is_deleted=0 AND status IN ('pending','failed','downloaded'))",
                params![proof.library,proof.id,proof.version,proof.provider_checksum,proof.provider_size as i64],
                |row| row.get(0),
            )?;
            if !current {
                return Err(invalid());
            }
            let saved: Option<Option<String>> = tx.query_row(
                "SELECT publication FROM owned_temp_files WHERE path=?1",
                [&key], |row| row.get(0),
            ).optional()?;
            match saved {
                Some(Some(prior)) if prior == encoded => return Ok(()),
                Some(None) => {},
                _ => return Err(invalid()),
            }
            tx.execute("UPDATE owned_temp_files SET publication=?2 WHERE path=?1 AND publication IS NULL",
                params![key,encoded])?;
            tx.commit()?;
            Ok(())
        }).await
    }

    pub(super) async fn prepared_publications(
        &self,
        library: &str,
        id: &str,
        version: VersionSizeKey,
    ) -> Result<Vec<PreparedPublication>, StateError> {
        let library = library.to_owned();
        let id = id.to_owned();
        self.with_conn("get_prepared_publications", move |conn| {
            let mut statement = conn.prepare_cached(
                "SELECT publication FROM owned_temp_files WHERE publication IS NOT NULL
                 AND json_extract(publication,'$.library')=?1 AND json_extract(publication,'$.id')=?2
                 AND json_extract(publication,'$.version')=?3",
            )?;
            let encoded = statement.query_map(params![library,id,version.as_str()], |row| row.get::<_,String>(0))?;
            let mut proofs = Vec::new();
            for value in encoded {
                let proof: PreparedPublication = serde_json::from_str(&value?).map_err(|_| invalid())?;
                proof.validate()?;
                proofs.push(proof);
            }
            Ok(proofs)
        }).await
    }
}

/// Runs in the same transaction as the final receipt. Any mismatch preserves
/// both the unfinished transfer evidence and the previous asset row.
pub(in crate::state::db) fn retire_prepared_receipt(
    conn: &rusqlite::Connection,
    library: &str,
    id: &str,
    version: &str,
    destination: &Path,
    local_checksum: &str,
    source_checksum: Option<&str>,
) -> Result<Vec<PreparedPublication>, StateError> {
    let destination_key = crate::fs_util::confined_path_key(destination).map_err(|_| invalid())?;
    let mut statement = conn.prepare_cached(
        "SELECT path,publication FROM owned_temp_files WHERE publication IS NOT NULL
         AND json_extract(publication,'$.library')=?1 AND json_extract(publication,'$.id')=?2
         AND json_extract(publication,'$.version')=?3",
    )?;
    let rows = statement.query_map(params![library, id, version], |row| {
        Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut retire = Vec::new();
    let mut proofs = Vec::new();
    for row in rows {
        let (key, encoded) = row?;
        let proof: PreparedPublication = serde_json::from_str(&encoded).map_err(|_| invalid())?;
        proof.validate()?;
        if OwnedTempPathKey::from_path(&proof.temporary.to_path())?.0 != key {
            return Err(invalid());
        }
        if crate::fs_util::confined_path_key(&proof.destination.to_path()).map_err(|_| invalid())?
            != destination_key
        {
            continue;
        }
        if proof.local_checksum != local_checksum
            || source_checksum != Some(proof.source_checksum.as_str())
        {
            return Err(invalid());
        }
        let current: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM assets WHERE library=?1 AND id=?2 AND version_size=?3
             AND checksum=?4 AND size_bytes=?5 AND is_deleted=0)",
            params![
                library,
                id,
                version,
                proof.provider_checksum,
                proof.provider_size as i64
            ],
            |row| row.get(0),
        )?;
        if !current {
            return Err(invalid());
        }
        proofs.push(proof);
        retire.push((key, encoded));
    }
    drop(statement);
    for (key, encoded) in retire {
        if conn.execute(
            "DELETE FROM owned_temp_files WHERE path=?1 AND publication=?2",
            params![key, encoded],
        )? != 1
        {
            return Err(invalid());
        }
    }
    Ok(proofs)
}

#[cfg(test)]
mod tests {
    use super::PreparedPublication;
    use crate::state::db::{TempFileOwnershipStore, provider_selection::SelectionPath};
    use crate::state::{SqliteStateDb, VersionSizeKey};

    #[tokio::test]
    async fn prepared_transfer_survives_reopen_stale_cleanup_and_claim_conflicts() {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("state.sqlite");
        let part = root.path().join("pending.part");
        let destination = root.path().join("photo.JPG");
        let db = SqliteStateDb::open(&database).await.unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("child")
            .checksum("opaque-generation")
            .size(4)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.claim_temp_file(&part).await.unwrap();
        let proof = PreparedPublication {
            library: record.library.to_string(),
            id: record.id.to_string(),
            version: record.version_size.as_str().to_owned(),
            provider_checksum: record.checksum.to_string(),
            provider_size: record.size_bytes,
            temporary: SelectionPath::from_path(&part),
            destination: SelectionPath::from_path(&destination),
            local_checksum: "a".repeat(64),
            source_checksum: "b".repeat(64),
        };
        db.prepare_temp_publication(&part, &proof).await.unwrap();
        drop(db);
        let db = SqliteStateDb::open(&database).await.unwrap();
        assert_eq!(
            db.get_prepared_publications(&record.library, &record.id, VersionSizeKey::Original)
                .await
                .unwrap(),
            vec![proof.clone()]
        );
        assert!(
            db.get_owned_temp_files_before(i64::MAX)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            db.retire_temp_files(std::slice::from_ref(&part))
                .await
                .unwrap(),
            0
        );
        assert!(db.claim_temp_file(&part).await.is_err());
        let mut changed = proof.clone();
        changed.source_checksum = "c".repeat(64);
        assert!(db.prepare_temp_publication(&part, &changed).await.is_err());
        assert_eq!(
            db.get_prepared_publications(&record.library, &record.id, VersionSizeKey::Original)
                .await
                .unwrap(),
            vec![proof]
        );
        assert!(!destination.exists());
        assert!(!part.exists());
    }

    #[tokio::test]
    async fn prepared_transfer_rejects_stale_provider_generation_without_retiring_owner() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let root = tempfile::tempdir().unwrap();
        let part = root.path().join("pending.part");
        let record = crate::test_helpers::TestAssetRecord::new("child")
            .checksum("current-generation")
            .size(4)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.claim_temp_file(&part).await.unwrap();
        let proof = PreparedPublication {
            library: record.library.to_string(),
            id: record.id.to_string(),
            version: record.version_size.as_str().to_owned(),
            provider_checksum: "stale-generation".to_owned(),
            provider_size: 4,
            temporary: SelectionPath::from_path(&part),
            destination: SelectionPath::from_path(&root.path().join("photo.JPG")),
            local_checksum: "a".repeat(64),
            source_checksum: "b".repeat(64),
        };
        assert!(db.prepare_temp_publication(&part, &proof).await.is_err());
        assert_eq!(
            db.get_owned_temp_files_before(i64::MAX)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            db.get_prepared_publications(&record.library, &record.id, VersionSizeKey::Original)
                .await
                .unwrap()
                .is_empty()
        );
    }
}

#[cfg(test)]
mod finalization_tests {
    use super::PreparedPublication;
    use crate::state::db::{
        DownloadStateStore, TempFileOwnershipStore, provider_selection::SelectionPath,
    };
    use crate::state::{SqliteStateDb, VersionSizeKey};

    #[tokio::test]
    async fn prepared_transfer_retirement_and_receipt_commit_are_atomic() {
        let root = tempfile::tempdir().unwrap();
        let part = root.path().join("pending.part");
        let destination = root.path().join("photo.JPG");
        let db = SqliteStateDb::open_in_memory().unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("child")
            .checksum("opaque-generation")
            .size(4)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.claim_temp_file(&part).await.unwrap();
        let proof = PreparedPublication {
            library: record.library.to_string(),
            id: record.id.to_string(),
            version: record.version_size.as_str().to_owned(),
            provider_checksum: record.checksum.to_string(),
            provider_size: 4,
            temporary: SelectionPath::from_path(&part),
            destination: SelectionPath::from_path(&destination),
            local_checksum: "a".repeat(64),
            source_checksum: "b".repeat(64),
        };
        db.prepare_temp_publication(&part, &proof).await.unwrap();
        db.acquire_lock("inject failed finalization")
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER fail_final BEFORE UPDATE OF status ON assets
             WHEN NEW.status='downloaded' BEGIN SELECT RAISE(ABORT,'injected'); END;",
            )
            .unwrap();
        assert!(
            db.mark_verified_download(
                &record.library,
                &record.id,
                "original",
                &destination,
                &proof.local_checksum,
                Some(&proof.source_checksum),
                false
            )
            .await
            .is_err()
        );
        assert_eq!(
            db.get_prepared_publications(&record.library, &record.id, VersionSizeKey::Original)
                .await
                .unwrap(),
            vec![proof.clone()]
        );
        assert_eq!(db.get_pending().await.unwrap().len(), 1);
        db.acquire_lock("clear failed finalization")
            .unwrap()
            .execute_batch("DROP TRIGGER fail_final;")
            .unwrap();
        assert!(
            db.mark_verified_download(
                &record.library,
                &record.id,
                "original",
                &destination,
                &"c".repeat(64),
                Some(&proof.source_checksum),
                false
            )
            .await
            .is_err()
        );
        assert_eq!(
            db.get_prepared_publications(&record.library, &record.id, VersionSizeKey::Original)
                .await
                .unwrap()
                .len(),
            1
        );
        db.mark_verified_download(
            &record.library,
            &record.id,
            "original",
            &destination,
            &proof.local_checksum,
            Some(&proof.source_checksum),
            false,
        )
        .await
        .unwrap();
        assert!(
            db.get_prepared_publications(&record.library, &record.id, VersionSizeKey::Original)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(db.get_pending().await.unwrap().is_empty());
        let source: String = db
            .acquire_lock("final receipt source")
            .unwrap()
            .query_row(
                "SELECT source_checksum FROM asset_metadata_paths WHERE id='child'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(source, proof.source_checksum);
    }
}

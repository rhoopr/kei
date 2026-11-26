//! Catalog and work queue for the sync core.
//!
//! One SQLite file holds what iCloud reported (assets, masters, albums,
//! memberships), where each zone resumes (zones), the local copies kei owes
//! (files) and a run log (runs). A zone's position is written in the same
//! transaction as the page that produced it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, params};

use crate::icloud::photos::feed::Observation;

const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS kv (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS zones (
    zone TEXT PRIMARY KEY,
    position TEXT,
    listed INTEGER NOT NULL DEFAULT 0
) STRICT;
CREATE TABLE IF NOT EXISTS assets (
    zone TEXT NOT NULL,
    name TEXT NOT NULL,
    master TEXT,
    body TEXT NOT NULL,
    deleted INTEGER NOT NULL,
    hidden INTEGER NOT NULL,
    purged INTEGER NOT NULL DEFAULT 0,
    sparse INTEGER NOT NULL DEFAULT 0,
    link_zone TEXT,
    link_name TEXT,
    dirty INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (zone, name)
) STRICT;
CREATE INDEX IF NOT EXISTS assets_by_master ON assets (zone, master);
CREATE INDEX IF NOT EXISTS assets_by_link ON assets (link_zone, link_name);
CREATE INDEX IF NOT EXISTS assets_dirty ON assets (zone) WHERE dirty = 1;
CREATE TABLE IF NOT EXISTS masters (
    zone TEXT NOT NULL,
    name TEXT NOT NULL,
    body TEXT NOT NULL,
    purged INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (zone, name)
) STRICT;
CREATE TABLE IF NOT EXISTS albums (
    zone TEXT NOT NULL,
    name TEXT NOT NULL,
    parent TEXT,
    title TEXT,
    is_folder INTEGER NOT NULL,
    deleted INTEGER NOT NULL,
    folder TEXT,
    PRIMARY KEY (zone, name)
) STRICT;
CREATE TABLE IF NOT EXISTS memberships (
    zone TEXT NOT NULL,
    relation TEXT NOT NULL,
    album TEXT NOT NULL,
    asset TEXT NOT NULL,
    present INTEGER NOT NULL,
    PRIMARY KEY (zone, relation)
) STRICT;
CREATE INDEX IF NOT EXISTS memberships_by_asset ON memberships (zone, asset);
CREATE INDEX IF NOT EXISTS memberships_by_album ON memberships (zone, album);
CREATE TABLE IF NOT EXISTS smart_members (
    zone TEXT NOT NULL,
    folder TEXT NOT NULL,
    asset TEXT NOT NULL,
    PRIMARY KEY (zone, folder, asset)
) STRICT;
CREATE TABLE IF NOT EXISTS files (
    id INTEGER PRIMARY KEY,
    zone TEXT NOT NULL,
    asset TEXT NOT NULL,
    rendition TEXT NOT NULL,
    album TEXT NOT NULL,
    path TEXT NOT NULL UNIQUE,
    size INTEGER NOT NULL,
    checksum TEXT NOT NULL,
    sha256 TEXT,
    state TEXT NOT NULL CHECK (state IN ('pending', 'adopt', 'done', 'failed', 'modified')),
    wanted INTEGER NOT NULL DEFAULT 1,
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    UNIQUE (zone, asset, rendition, album)
) STRICT;
CREATE INDEX IF NOT EXISTS files_by_state ON files (state) WHERE wanted = 1;
CREATE TABLE IF NOT EXISTS imported (
    path TEXT PRIMARY KEY,
    zone TEXT NOT NULL,
    master TEXT NOT NULL,
    rendition TEXT NOT NULL,
    size INTEGER NOT NULL,
    sha256 TEXT,
    checksum TEXT NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS imported_by_asset ON imported (zone, master, rendition);
CREATE TABLE IF NOT EXISTS runs (
    id INTEGER PRIMARY KEY,
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    outcome TEXT,
    downloaded INTEGER NOT NULL DEFAULT 0,
    failed INTEGER NOT NULL DEFAULT 0,
    pending INTEGER NOT NULL DEFAULT 0
) STRICT;
";

/// Where a file row stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileState {
    /// Download owed. With a recorded `sha256`, replace only if the local
    /// file still matches it.
    Pending,
    /// A file already at the path with the expected size; hash and adopt it.
    Adopt,
    Done,
    /// The last attempt failed; retried next cycle.
    Failed,
    /// The local file no longer matches what kei wrote. Left untouched.
    Modified,
}

impl FileState {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Adopt => "adopt",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Modified => "modified",
        }
    }

    fn parse(value: &str) -> anyhow::Result<Self> {
        Ok(match value {
            "pending" => Self::Pending,
            "adopt" => Self::Adopt,
            "done" => Self::Done,
            "failed" => Self::Failed,
            "modified" => Self::Modified,
            other => anyhow::bail!("Unknown file state {other}"),
        })
    }
}

/// One local copy kei owes or has written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileRow {
    pub(crate) id: i64,
    pub(crate) zone: String,
    pub(crate) asset: String,
    pub(crate) rendition: String,
    /// Album record name, or empty for the unfiled copy.
    pub(crate) album: String,
    /// Relative to the download directory, with `/` separators.
    pub(crate) path: String,
    pub(crate) size: u64,
    pub(crate) checksum: String,
    pub(crate) sha256: Option<String>,
    pub(crate) state: FileState,
    pub(crate) attempts: i64,
}

/// A copy the planner wants for one asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WantedFile {
    pub(crate) rendition: String,
    pub(crate) album: String,
    pub(crate) path: String,
    pub(crate) size: u64,
    pub(crate) checksum: String,
    /// The path already holds a file of this size that kei does not track.
    pub(crate) adopt: bool,
    /// A copy recorded by the previous engine that this row takes over.
    pub(crate) imported: Option<ImportedCopy>,
}

/// A verified local copy carried over from the previous engine's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImportedCopy {
    pub(crate) path: String,
    pub(crate) zone: String,
    pub(crate) master: String,
    pub(crate) rendition: String,
    pub(crate) size: u64,
    pub(crate) sha256: Option<String>,
    pub(crate) checksum: String,
}

/// An asset whose wanted copies must be recomputed.
#[derive(Debug, Clone)]
pub(crate) struct DirtyAsset {
    pub(crate) name: String,
    pub(crate) asset_body: String,
    pub(crate) master_body: Option<String>,
    pub(crate) deleted: bool,
    pub(crate) hidden: bool,
    pub(crate) purged: bool,
    /// A personal-library stand-in for a photo in the Shared Library.
    pub(crate) sparse: bool,
    /// Present album memberships as `(album zone, album)`, including those of
    /// personal-library stand-ins that link to this photo.
    pub(crate) albums: Vec<(String, String)>,
    /// Queried smart folders that contain this photo.
    pub(crate) smart: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AlbumRow {
    pub(crate) zone: String,
    pub(crate) name: String,
    pub(crate) parent: Option<String>,
    pub(crate) title: Option<String>,
    pub(crate) is_folder: bool,
    pub(crate) deleted: bool,
    /// Path components fixed when the album's first copy is planned.
    pub(crate) folder: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Completeness {
    pub(crate) done: u64,
    pub(crate) pending: u64,
    pub(crate) failed: u64,
    pub(crate) modified: u64,
}

#[derive(Clone)]
pub(crate) struct State {
    conn: Arc<Mutex<Connection>>,
}

impl State {
    /// The sync core's database file for an account's legacy state path.
    pub(crate) fn path_for(legacy_state: &Path) -> PathBuf {
        legacy_state.with_extension("sync.db")
    }

    pub(crate) async fn open(path: &Path) -> anyhow::Result<Self> {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&path)
                .with_context(|| format!("Could not open sync state {}", path.display()))?;
            Self::init(conn)
        })
        .await?
    }

    #[cfg(test)]
    pub(crate) fn open_in_memory() -> anyhow::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> anyhow::Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        anyhow::ensure!(
            version <= SCHEMA_VERSION,
            "Sync state was written by a newer kei (schema {version})"
        );
        conn.execute_batch(SCHEMA)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    async fn with<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> anyhow::Result<T> + Send + 'static,
    ) -> anyhow::Result<T> {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let mut conn = conn
                .lock()
                .map_err(|_poisoned| anyhow::anyhow!("Sync state lock was poisoned"))?;
            f(&mut conn)
        })
        .await?
    }

    pub(crate) async fn get_kv(&self, key: &str) -> anyhow::Result<Option<String>> {
        let key = key.to_owned();
        self.with(move |conn| {
            Ok(conn
                .query_row("SELECT value FROM kv WHERE key = ?1", [key], |row| {
                    row.get(0)
                })
                .optional()?)
        })
        .await
    }

    pub(crate) async fn set_kv(&self, key: &str, value: &str) -> anyhow::Result<()> {
        let (key, value) = (key.to_owned(), value.to_owned());
        self.with(move |conn| {
            conn.execute(
                "INSERT INTO kv (key, value) VALUES (?1, ?2)
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                [key, value],
            )?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn delete_kv(&self, key: &str) -> anyhow::Result<()> {
        let key = key.to_owned();
        self.with(move |conn| {
            conn.execute("DELETE FROM kv WHERE key = ?1", [key])?;
            Ok(())
        })
        .await
    }

    /// Check the database file's integrity.
    pub(crate) async fn validate(&self) -> anyhow::Result<()> {
        self.with(|conn| {
            let result: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
            anyhow::ensure!(
                result == "ok",
                "Sync state failed its integrity check: {result}"
            );
            Ok(())
        })
        .await
    }

    /// Close the database, folding the write-ahead log into the main file.
    pub(crate) async fn close(self) -> anyhow::Result<()> {
        let conn = Arc::try_unwrap(self.conn)
            .map_err(|_shared| anyhow::anyhow!("Sync state is still in use"))?
            .into_inner()
            .map_err(|_poisoned| anyhow::anyhow!("Sync state lock was poisoned"))?;
        tokio::task::spawn_blocking(move || {
            conn.pragma_update(None, "journal_mode", "DELETE")?;
            conn.close().map_err(|(_conn, error)| error)?;
            Ok(())
        })
        .await?
    }

    /// Saved position for a zone, or `None` when a full listing is owed.
    pub(crate) async fn position(&self, zone: &str) -> anyhow::Result<Option<String>> {
        let zone = zone.to_owned();
        self.with(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT position FROM zones WHERE zone = ?1",
                    [zone],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()?
                .flatten())
        })
        .await
    }

    /// Forget a zone's position so the next cycle runs a full listing.
    /// Catalog rows and files stay; the listing re-observes them.
    pub(crate) async fn reset_position(&self, zone: &str) -> anyhow::Result<()> {
        let zone = zone.to_owned();
        self.with(move |conn| {
            conn.execute(
                "INSERT INTO zones (zone, position, listed) VALUES (?1, NULL, 0)
                 ON CONFLICT (zone) DO UPDATE SET position = NULL, listed = 0",
                [zone],
            )?;
            Ok(())
        })
        .await
    }

    /// Store one page's observations and the position after it, atomically.
    pub(crate) async fn commit_page(
        &self,
        zone: &str,
        observations: Vec<Observation>,
        position: String,
        caught_up: bool,
    ) -> anyhow::Result<()> {
        let zone = zone.to_owned();
        self.with(move |conn| {
            let tx = conn.transaction()?;
            for observation in observations {
                apply_observation(&tx, &zone, observation)?;
            }
            tx.execute(
                "INSERT INTO zones (zone, position, listed) VALUES (?1, ?2, ?3)
                 ON CONFLICT (zone) DO UPDATE SET position = excluded.position,
                     listed = MAX(zones.listed, excluded.listed)",
                params![zone, position, i64::from(caught_up)],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Mark every asset for re-planning, for example after a config change.
    pub(crate) async fn mark_all_dirty(&self) -> anyhow::Result<()> {
        self.with(|conn| {
            conn.execute("UPDATE assets SET dirty = 1", [])?;
            Ok(())
        })
        .await
    }

    /// Every album the catalog knows, across zones.
    pub(crate) async fn albums(&self) -> anyhow::Result<Vec<AlbumRow>> {
        self.with(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT zone, name, parent, title, is_folder, deleted, folder FROM albums",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, i64>(4)? != 0,
                        row.get::<_, i64>(5)? != 0,
                        row.get::<_, Option<String>>(6)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows.into_iter()
                .map(|(zone, name, parent, title, is_folder, deleted, folder)| {
                    Ok(AlbumRow {
                        zone,
                        name,
                        parent,
                        title,
                        is_folder,
                        deleted,
                        folder: folder
                            .map(|folder| serde_json::from_str(&folder))
                            .transpose()
                            .context("Corrupt stored album folder")?,
                    })
                })
                .collect()
        })
        .await
    }

    /// Replace a queried smart folder's members in a zone, marking photos
    /// that joined or left it for planning.
    pub(crate) async fn replace_smart_members(
        &self,
        zone: &str,
        folder: &str,
        members: std::collections::HashSet<String>,
    ) -> anyhow::Result<()> {
        let (zone, folder) = (zone.to_owned(), folder.to_owned());
        self.with(move |conn| {
            let tx = conn.transaction()?;
            let previous: std::collections::HashSet<String> = tx
                .prepare("SELECT asset FROM smart_members WHERE zone = ?1 AND folder = ?2")?
                .query_map(params![zone, folder], |row| row.get(0))?
                .collect::<Result<_, _>>()?;
            for asset in previous.symmetric_difference(&members) {
                tx.execute(
                    "UPDATE assets SET dirty = 1 WHERE zone = ?1 AND name = ?2",
                    params![zone, asset],
                )?;
            }
            tx.execute(
                "DELETE FROM smart_members WHERE zone = ?1 AND folder = ?2",
                params![zone, folder],
            )?;
            for asset in &members {
                tx.execute(
                    "INSERT INTO smart_members (zone, folder, asset) VALUES (?1, ?2, ?3)",
                    params![zone, folder, asset],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Fix an album's folder the first time a copy is planned into it.
    pub(crate) async fn fix_album_folder(
        &self,
        zone: &str,
        album: &str,
        folder: &[String],
    ) -> anyhow::Result<()> {
        let (zone, album, folder) = (
            zone.to_owned(),
            album.to_owned(),
            serde_json::to_string(folder)?,
        );
        self.with(move |conn| {
            conn.execute(
                "UPDATE albums SET folder = ?3 WHERE zone = ?1 AND name = ?2 AND folder IS NULL",
                params![zone, album, folder],
            )?;
            Ok(())
        })
        .await
    }

    /// Assets awaiting planning after `after`, in name order.
    pub(crate) async fn dirty_assets(
        &self,
        zone: &str,
        after: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<DirtyAsset>> {
        let (zone, after) = (zone.to_owned(), after.to_owned());
        self.with(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT a.name, a.body, m.body, a.deleted, a.hidden,
                        a.purged OR COALESCE(m.purged, 0), a.sparse
                 FROM assets a LEFT JOIN masters m ON m.zone = a.zone AND m.name = a.master
                 WHERE a.zone = ?1 AND a.dirty = 1 AND a.name > ?2 ORDER BY a.name LIMIT ?3",
            )?;
            let mut albums = conn.prepare(
                "SELECT ms.zone, ms.album FROM memberships ms
                 WHERE ms.present = 1 AND (
                     (ms.zone = ?1 AND ms.asset = ?2)
                     OR (ms.zone, ms.asset) IN
                         (SELECT zone, name FROM assets WHERE link_zone = ?1 AND link_name = ?2))
                 ORDER BY ms.zone, ms.album",
            )?;
            let mut smart = conn.prepare(
                "SELECT folder FROM smart_members WHERE zone = ?1 AND asset = ?2 ORDER BY folder",
            )?;
            let rows = stmt
                .query_map(params![zone, after, i64::try_from(limit)?], |row| {
                    Ok(DirtyAsset {
                        name: row.get(0)?,
                        asset_body: row.get(1)?,
                        master_body: row.get(2)?,
                        deleted: row.get::<_, i64>(3)? != 0,
                        hidden: row.get::<_, i64>(4)? != 0,
                        purged: row.get::<_, i64>(5)? != 0,
                        sparse: row.get::<_, i64>(6)? != 0,
                        albums: Vec::new(),
                        smart: Vec::new(),
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows.into_iter()
                .map(|mut asset| {
                    asset.albums = albums
                        .query_map(params![zone, asset.name], |row| {
                            Ok((row.get(0)?, row.get(1)?))
                        })?
                        .collect::<Result<_, _>>()?;
                    asset.smart = smart
                        .query_map(params![zone, asset.name], |row| row.get(0))?
                        .collect::<Result<_, _>>()?;
                    Ok(asset)
                })
                .collect()
        })
        .await
    }

    /// Whether a zone has finished a full listing since its last reset.
    pub(crate) async fn listed(&self, zone: &str) -> anyhow::Result<bool> {
        let zone = zone.to_owned();
        self.with(move |conn| {
            Ok(conn
                .query_row("SELECT listed FROM zones WHERE zone = ?1", [zone], |row| {
                    row.get::<_, i64>(0)
                })
                .optional()?
                .is_some_and(|listed| listed != 0))
        })
        .await
    }

    /// Master record names for assets, as the catalog last saw them.
    pub(crate) async fn masters_of(
        &self,
        zone: &str,
        assets: Vec<String>,
    ) -> anyhow::Result<std::collections::HashMap<String, String>> {
        let zone = zone.to_owned();
        self.with(move |conn| {
            let mut stmt =
                conn.prepare("SELECT master FROM assets WHERE zone = ?1 AND name = ?2")?;
            let mut out = std::collections::HashMap::new();
            for asset in assets {
                if let Some(Some(master)) = stmt
                    .query_row(params![zone, asset], |row| row.get::<_, Option<String>>(0))
                    .optional()?
                {
                    out.insert(asset, master);
                }
            }
            Ok(out)
        })
        .await
    }

    /// Paths already claimed by any file row.
    pub(crate) async fn path_owner(&self, path: &str) -> anyhow::Result<Option<i64>> {
        let path = path.to_owned();
        self.with(move |conn| {
            Ok(conn
                .query_row("SELECT id FROM files WHERE path = ?1", [path], |row| {
                    row.get(0)
                })
                .optional()?)
        })
        .await
    }

    pub(crate) async fn files_for_asset(
        &self,
        zone: &str,
        asset: &str,
    ) -> anyhow::Result<Vec<FileRow>> {
        let (zone, asset) = (zone.to_owned(), asset.to_owned());
        self.with(move |conn| {
            let mut stmt =
                conn.prepare(&format!("{FILE_COLUMNS} WHERE zone = ?1 AND asset = ?2"))?;
            let rows = stmt
                .query_map(params![zone, asset], file_row)?
                .collect::<Result<Vec<_>, _>>()?;
            rows.into_iter().collect()
        })
        .await
    }

    /// Apply the planner's wanted copies for one asset and clear its dirty
    /// mark. Existing paths never change. A new version of a done copy
    /// becomes pending with its recorded hash, so it is replaced only if
    /// the local file is untouched. Copies no longer wanted stay on disk.
    pub(crate) async fn apply_plan(
        &self,
        zone: &str,
        asset: &str,
        wanted: Vec<WantedFile>,
    ) -> anyhow::Result<()> {
        let (zone, asset) = (zone.to_owned(), asset.to_owned());
        self.with(move |conn| {
            let tx = conn.transaction()?;
            tx.execute(
                "UPDATE files SET wanted = 0 WHERE zone = ?1 AND asset = ?2",
                params![zone, asset],
            )?;
            for file in wanted {
                let existing: Option<(i64, String, String)> = tx
                    .query_row(
                        "SELECT id, checksum, state FROM files
                         WHERE zone = ?1 AND asset = ?2 AND rendition = ?3 AND album = ?4",
                        params![zone, asset, file.rendition, file.album],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .optional()?;
                match existing {
                    Some((id, checksum, state)) => {
                        let state = FileState::parse(&state)?;
                        // A new version makes a saved or adoptable copy owed again; a
                        // locally modified copy stays as it is.
                        let next = if checksum == file.checksum || state == FileState::Modified {
                            state
                        } else if matches!(state, FileState::Adopt | FileState::Done) {
                            FileState::Pending
                        } else {
                            state
                        };
                        tx.execute(
                            "UPDATE files SET wanted = 1, size = ?2, checksum = ?3, state = ?4
                             WHERE id = ?1",
                            params![id, i64::try_from(file.size)?, file.checksum, next.as_str()],
                        )?;
                    }
                    None => {
                        // A carried-over copy keeps its recorded hash: it is
                        // done if iCloud still serves the same version, and
                        // replaced only while untouched if it does not.
                        let (state, sha256) = match &file.imported {
                            Some(copy) => {
                                tx.execute("DELETE FROM imported WHERE path = ?1", [&copy.path])?;
                                match &copy.sha256 {
                                    None => (FileState::Adopt, None),
                                    Some(_) if copy.checksum == file.checksum => {
                                        (FileState::Done, copy.sha256.clone())
                                    }
                                    Some(_) => (FileState::Pending, copy.sha256.clone()),
                                }
                            }
                            None if file.adopt => (FileState::Adopt, None),
                            None => (FileState::Pending, None),
                        };
                        tx.execute(
                            "INSERT INTO files (zone, asset, rendition, album, path, size, checksum, state, sha256)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                            params![
                                zone,
                                asset,
                                file.rendition,
                                file.album,
                                file.path,
                                i64::try_from(file.size)?,
                                file.checksum,
                                state.as_str(),
                                sha256
                            ],
                        )?;
                    }
                }
            }
            tx.execute(
                "UPDATE assets SET dirty = 0 WHERE zone = ?1 AND name = ?2",
                params![zone, asset],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Wanted copies that still need work, in id order after `after`.
    pub(crate) async fn work(
        &self,
        zone: &str,
        after: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<FileRow>> {
        let zone = zone.to_owned();
        self.with(move |conn| {
            let mut stmt = conn.prepare(&format!(
                "{FILE_COLUMNS} WHERE zone = ?1 AND wanted = 1 AND id > ?2
                 AND state IN ('pending', 'adopt', 'failed') ORDER BY id LIMIT ?3"
            ))?;
            let rows = stmt
                .query_map(params![zone, after, i64::try_from(limit)?], file_row)?
                .collect::<Result<Vec<_>, _>>()?;
            rows.into_iter().collect()
        })
        .await
    }

    /// Update a row whose expected version changed since it was planned.
    pub(crate) async fn refresh_version(
        &self,
        id: i64,
        size: u64,
        checksum: &str,
    ) -> anyhow::Result<()> {
        let checksum = checksum.to_owned();
        self.with(move |conn| {
            conn.execute(
                "UPDATE files SET size = ?2, checksum = ?3 WHERE id = ?1",
                params![id, i64::try_from(size)?, checksum],
            )?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn mark_done(&self, id: i64, sha256: &str) -> anyhow::Result<()> {
        let sha256 = sha256.to_owned();
        self.with(move |conn| {
            conn.execute(
                "UPDATE files SET state = 'done', sha256 = ?2, last_error = NULL WHERE id = ?1",
                params![id, sha256],
            )?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn mark_modified(&self, id: i64) -> anyhow::Result<()> {
        self.with(move |conn| {
            conn.execute(
                "UPDATE files SET state = 'modified', last_error = NULL WHERE id = ?1",
                [id],
            )?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn mark_failed(&self, id: i64, error: &str) -> anyhow::Result<()> {
        let error = error.to_owned();
        self.with(move |conn| {
            conn.execute(
                "UPDATE files SET state = 'failed', attempts = attempts + 1, last_error = ?2
                 WHERE id = ?1",
                params![id, error],
            )?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn mark_pending(&self, id: i64) -> anyhow::Result<()> {
        self.with(move |conn| {
            conn.execute("UPDATE files SET state = 'pending' WHERE id = ?1", [id])?;
            Ok(())
        })
        .await
    }

    /// Record why an owed copy could not start, without counting a failure.
    pub(crate) async fn note_waiting(&self, id: i64, reason: &str) -> anyhow::Result<()> {
        let reason = reason.to_owned();
        self.with(move |conn| {
            conn.execute(
                "UPDATE files SET last_error = ?2 WHERE id = ?1",
                params![id, reason],
            )?;
            Ok(())
        })
        .await
    }

    /// Store carried-over copies. Existing rows with the same path are kept.
    pub(crate) async fn insert_imported(&self, copies: Vec<ImportedCopy>) -> anyhow::Result<u64> {
        self.with(move |conn| {
            let tx = conn.transaction()?;
            let mut inserted = 0u64;
            for copy in copies {
                inserted += u64::try_from(tx.execute(
                    "INSERT OR IGNORE INTO imported (path, zone, master, rendition, size, sha256, checksum)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        copy.path,
                        copy.zone,
                        copy.master,
                        copy.rendition,
                        i64::try_from(copy.size)?,
                        copy.sha256,
                        copy.checksum
                    ],
                )?)?;
            }
            tx.commit()?;
            Ok(inserted)
        })
        .await
    }

    /// A carried-over copy for a new row: the one at `path`, or else one of
    /// the same photo and rendition in the same folder. Paths already owned
    /// by a file row are never offered.
    pub(crate) async fn imported_copy(
        &self,
        zone: &str,
        master: &str,
        rendition: &str,
        path: &str,
    ) -> anyhow::Result<Option<ImportedCopy>> {
        let (zone, master, rendition, path) = (
            zone.to_owned(),
            master.to_owned(),
            rendition.to_owned(),
            path.to_owned(),
        );
        self.with(move |conn| {
            let parent = path
                .rsplit_once('/')
                .map_or("", |(parent, _)| parent)
                .to_owned();
            let mut stmt = conn.prepare(
                "SELECT path, zone, master, rendition, size, sha256, checksum FROM imported i
                 WHERE NOT EXISTS (SELECT 1 FROM files f WHERE f.path = i.path)
                   AND (i.path = ?1 OR (i.zone = ?2 AND i.master = ?3 AND i.rendition = ?4))
                 ORDER BY i.path = ?1 DESC, i.path",
            )?;
            let rows = stmt
                .query_map(params![path, zone, master, rendition], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, String>(6)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            for (candidate, zone, master, rendition, size, sha256, checksum) in rows {
                let same_folder =
                    candidate.rsplit_once('/').map_or("", |(parent, _)| parent) == parent;
                if candidate == path || same_folder {
                    return Ok(Some(ImportedCopy {
                        path: candidate,
                        zone,
                        master,
                        rendition,
                        size: u64::try_from(size)?,
                        sha256,
                        checksum,
                    }));
                }
            }
            Ok(None)
        })
        .await
    }

    /// Drop carried-over copies no row took over. Their files stay on disk.
    pub(crate) async fn clear_imported(&self) -> anyhow::Result<u64> {
        self.with(|conn| Ok(u64::try_from(conn.execute("DELETE FROM imported", [])?)?))
            .await
    }

    /// Personal-library stand-ins whose Shared Library photo no selected
    /// zone covers. Their photos are not saved.
    pub(crate) async fn uncovered_stand_ins(&self, zones: Vec<String>) -> anyhow::Result<u64> {
        self.with(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT link_zone, COUNT(*) FROM assets
                 WHERE sparse = 1 AND deleted = 0 AND hidden = 0 AND purged = 0
                 GROUP BY link_zone",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, Option<String>>(0)?, row.get::<_, i64>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let mut uncovered = 0u64;
            for (zone, count) in rows {
                if zone.is_none_or(|zone| !zones.contains(&zone)) {
                    uncovered += u64::try_from(count)?;
                }
            }
            Ok(uncovered)
        })
        .await
    }

    pub(crate) async fn completeness(&self) -> anyhow::Result<Completeness> {
        self.with(|conn| {
            let mut out = Completeness::default();
            let mut stmt =
                conn.prepare("SELECT state, COUNT(*) FROM files WHERE wanted = 1 GROUP BY state")?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;
            for row in rows {
                let (state, count) = row?;
                let count = u64::try_from(count)?;
                match FileState::parse(&state)? {
                    FileState::Done => out.done += count,
                    FileState::Pending | FileState::Adopt => out.pending += count,
                    FileState::Failed => out.failed += count,
                    FileState::Modified => out.modified += count,
                }
            }
            Ok(out)
        })
        .await
    }

    pub(crate) async fn start_run(&self, now: i64) -> anyhow::Result<i64> {
        self.with(move |conn| {
            conn.execute(
                "UPDATE runs SET outcome = 'interrupted', finished_at = ?1 WHERE finished_at IS NULL",
                [now],
            )?;
            conn.execute("INSERT INTO runs (started_at) VALUES (?1)", [now])?;
            Ok(conn.last_insert_rowid())
        })
        .await
    }

    pub(crate) async fn finish_run(
        &self,
        id: i64,
        now: i64,
        outcome: &str,
        downloaded: u64,
        failed: u64,
        pending: u64,
    ) -> anyhow::Result<()> {
        let outcome = outcome.to_owned();
        self.with(move |conn| {
            conn.execute(
                "UPDATE runs SET finished_at = ?2, outcome = ?3, downloaded = ?4, failed = ?5, pending = ?6
                 WHERE id = ?1",
                params![
                    id,
                    now,
                    outcome,
                    i64::try_from(downloaded)?,
                    i64::try_from(failed)?,
                    i64::try_from(pending)?
                ],
            )?;
            Ok(())
        })
        .await
    }
}

/// One row of a command listing.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct ListedFile {
    pub(crate) path: String,
    pub(crate) zone: String,
    pub(crate) asset: String,
    pub(crate) rendition: String,
    /// Album record name, `smart:<name>` for a smart folder, or empty for unfiled.
    pub(crate) album: String,
    pub(crate) size: u64,
    pub(crate) sha256: Option<String>,
    pub(crate) state: &'static str,
    /// Still selected; copies that stopped being wanted keep their files.
    pub(crate) wanted: bool,
    pub(crate) last_error: Option<String>,
}

/// The most recent cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunRow {
    pub(crate) started_at: i64,
    pub(crate) finished_at: Option<i64>,
    pub(crate) outcome: Option<String>,
    pub(crate) downloaded: u64,
    pub(crate) failed: u64,
}

/// A zone's resume point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ZoneRow {
    pub(crate) zone: String,
    pub(crate) has_position: bool,
    pub(crate) listed: bool,
}

impl State {
    /// File rows, optionally only those in `states`, ordered by path.
    pub(crate) async fn list_files(&self, states: &[FileState]) -> anyhow::Result<Vec<ListedFile>> {
        let states: Vec<&'static str> = states.iter().map(|state| state.as_str()).collect();
        self.with(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT path, zone, asset, rendition, album, size, sha256, state, wanted, last_error
                 FROM files ORDER BY path",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, i64>(8)? != 0,
                        row.get::<_, Option<String>>(9)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let mut out = Vec::new();
            for (path, zone, asset, rendition, album, size, sha256, state, wanted, last_error) in
                rows
            {
                let state = FileState::parse(&state)?.as_str();
                if !states.is_empty() && !states.contains(&state) {
                    continue;
                }
                out.push(ListedFile {
                    path,
                    zone,
                    asset,
                    rendition,
                    album,
                    size: u64::try_from(size)?,
                    sha256,
                    state,
                    wanted,
                    last_error,
                });
            }
            Ok(out)
        })
        .await
    }

    /// Copies carried over from the previous engine that no row has taken
    /// over yet.
    pub(crate) async fn list_imported(&self) -> anyhow::Result<Vec<ImportedCopy>> {
        self.with(|conn| {
            let mut stmt = conn.prepare(
                "SELECT path, zone, master, rendition, size, sha256, checksum FROM imported ORDER BY path",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, String>(6)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows.into_iter()
                .map(|(path, zone, master, rendition, size, sha256, checksum)| {
                    Ok(ImportedCopy {
                        path,
                        zone,
                        master,
                        rendition,
                        size: u64::try_from(size)?,
                        sha256,
                        checksum,
                    })
                })
                .collect()
        })
        .await
    }

    pub(crate) async fn zones(&self) -> anyhow::Result<Vec<ZoneRow>> {
        self.with(|conn| {
            let mut stmt =
                conn.prepare("SELECT zone, position IS NOT NULL, listed FROM zones ORDER BY zone")?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(ZoneRow {
                        zone: row.get(0)?,
                        has_position: row.get::<_, i64>(1)? != 0,
                        listed: row.get::<_, i64>(2)? != 0,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    pub(crate) async fn last_run(&self) -> anyhow::Result<Option<RunRow>> {
        self.with(|conn| {
            let row = conn
                .query_row(
                    "SELECT started_at, finished_at, outcome, downloaded, failed FROM runs
                     ORDER BY id DESC LIMIT 1",
                    [],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, Option<i64>>(1)?,
                            row.get::<_, Option<String>>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, i64>(4)?,
                        ))
                    },
                )
                .optional()?;
            row.map(|(started_at, finished_at, outcome, downloaded, failed)| {
                Ok(RunRow {
                    started_at,
                    finished_at,
                    outcome,
                    downloaded: u64::try_from(downloaded)?,
                    failed: u64::try_from(failed)?,
                })
            })
            .transpose()
        })
        .await
    }

    /// Photos the catalog knows, across zones.
    pub(crate) async fn asset_count(&self) -> anyhow::Result<u64> {
        self.with(|conn| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM assets WHERE purged = 0 AND sparse = 0",
                [],
                |row| row.get(0),
            )?;
            Ok(u64::try_from(count)?)
        })
        .await
    }

    /// Forget every zone's position so the next sync lists each again.
    pub(crate) async fn forget_positions(&self) -> anyhow::Result<u64> {
        self.with(|conn| {
            Ok(u64::try_from(conn.execute(
                "UPDATE zones SET position = NULL, listed = 0 WHERE position IS NOT NULL OR listed = 1",
                [],
            )?)?)
        })
        .await
    }

    /// The file row at a relative path.
    pub(crate) async fn file_at(&self, path: &str) -> anyhow::Result<Option<FileRow>> {
        let path = path.to_owned();
        self.with(move |conn| {
            let mut stmt = conn.prepare(&format!("{FILE_COLUMNS} WHERE path = ?1"))?;
            let row = stmt.query_row([path], file_row).optional()?;
            row.transpose()
        })
        .await
    }

    /// Owe a done copy again, replacing it only while it still has `sha256`.
    pub(crate) async fn owe_again(&self, id: i64, sha256: Option<&str>) -> anyhow::Result<()> {
        let sha256 = sha256.map(ToOwned::to_owned);
        self.with(move |conn| {
            conn.execute(
                "UPDATE files SET state = 'pending', sha256 = ?2, last_error = NULL WHERE id = ?1",
                params![id, sha256],
            )?;
            Ok(())
        })
        .await
    }
}

const FILE_COLUMNS: &str = "SELECT id, zone, asset, rendition, album, path, size, checksum, sha256, state, attempts FROM files";

fn file_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<anyhow::Result<FileRow>> {
    let state: String = row.get(9)?;
    let size: i64 = row.get(6)?;
    Ok((|| {
        Ok(FileRow {
            id: row.get(0)?,
            zone: row.get(1)?,
            asset: row.get(2)?,
            rendition: row.get(3)?,
            album: row.get(4)?,
            path: row.get(5)?,
            size: u64::try_from(size)?,
            checksum: row.get(7)?,
            sha256: row.get(8)?,
            state: FileState::parse(&state)?,
            attempts: row.get(10)?,
        })
    })())
}

/// Mark the photo a stand-in links to for planning.
fn mark_linked_dirty(tx: &rusqlite::Transaction<'_>, zone: &str, name: &str) -> anyhow::Result<()> {
    tx.execute(
        "UPDATE assets SET dirty = 1 WHERE (zone, name) IN
             (SELECT link_zone, link_name FROM assets WHERE zone = ?1 AND name = ?2)",
        params![zone, name],
    )?;
    Ok(())
}

fn apply_observation(
    tx: &rusqlite::Transaction<'_>,
    zone: &str,
    observation: Observation,
) -> anyhow::Result<()> {
    match observation {
        Observation::Asset {
            name,
            master,
            body,
            deleted,
            hidden,
            sparse,
        } => {
            // A stand-in's albums belong to its linked photo: re-plan the
            // photo it linked to before and the one it links to now.
            mark_linked_dirty(tx, zone, &name)?;
            let target = sparse.as_ref().and_then(|link| link.target.clone());
            tx.execute(
                "INSERT INTO assets (zone, name, master, body, deleted, hidden, purged, sparse,
                     link_zone, link_name, dirty)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7, ?8, ?9, 1)
                 ON CONFLICT (zone, name) DO UPDATE SET master = excluded.master,
                     body = excluded.body, deleted = excluded.deleted,
                     hidden = excluded.hidden, purged = 0, sparse = excluded.sparse,
                     link_zone = excluded.link_zone, link_name = excluded.link_name, dirty = 1",
                params![
                    zone,
                    name,
                    master,
                    body,
                    i64::from(deleted),
                    i64::from(hidden),
                    i64::from(sparse.is_some()),
                    target.as_ref().map(|(zone, _)| zone.as_str()),
                    target.as_ref().map(|(_, name)| name.as_str()),
                ],
            )?;
            mark_linked_dirty(tx, zone, &name)?;
        }
        Observation::Master { name, body } => {
            tx.execute(
                "INSERT INTO masters (zone, name, body, purged) VALUES (?1, ?2, ?3, 0)
                 ON CONFLICT (zone, name) DO UPDATE SET body = excluded.body, purged = 0",
                params![zone, name, body],
            )?;
            tx.execute(
                "UPDATE assets SET dirty = 1 WHERE zone = ?1 AND master = ?2",
                params![zone, name],
            )?;
        }
        Observation::Album {
            name,
            parent,
            title,
            is_folder,
            deleted,
        } => {
            tx.execute(
                "INSERT INTO albums (zone, name, parent, title, is_folder, deleted)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (zone, name) DO UPDATE SET
                     parent = COALESCE(excluded.parent, albums.parent),
                     title = COALESCE(excluded.title, albums.title),
                     is_folder = CASE WHEN excluded.deleted = 1 THEN albums.is_folder ELSE excluded.is_folder END,
                     deleted = excluded.deleted",
                params![zone, name, parent, title, i64::from(is_folder), i64::from(deleted)],
            )?;
            tx.execute(
                "UPDATE assets SET dirty = 1 WHERE zone = ?1 AND name IN
                     (SELECT asset FROM memberships WHERE zone = ?1 AND album = ?2)",
                params![zone, name],
            )?;
            tx.execute(
                "UPDATE assets SET dirty = 1 WHERE (zone, name) IN
                     (SELECT a.link_zone, a.link_name FROM assets a
                      JOIN memberships ms ON ms.zone = a.zone AND ms.asset = a.name
                      WHERE ms.zone = ?1 AND ms.album = ?2)",
                params![zone, name],
            )?;
        }
        Observation::Membership {
            relation,
            album,
            asset,
            present,
        } => {
            tx.execute(
                "INSERT INTO memberships (zone, relation, album, asset, present)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (zone, relation) DO UPDATE SET present = excluded.present",
                params![zone, relation, album, asset, i64::from(present)],
            )?;
            tx.execute(
                "UPDATE assets SET dirty = 1 WHERE zone = ?1 AND name = ?2",
                params![zone, asset],
            )?;
            mark_linked_dirty(tx, zone, &asset)?;
        }
        Observation::Purged { name } => {
            tx.execute(
                "UPDATE assets SET purged = 1, dirty = 1 WHERE zone = ?1 AND name = ?2",
                params![zone, name],
            )?;
            tx.execute(
                "UPDATE masters SET purged = 1 WHERE zone = ?1 AND name = ?2",
                params![zone, name],
            )?;
            tx.execute(
                "UPDATE assets SET dirty = 1 WHERE zone = ?1 AND master = ?2",
                params![zone, name],
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(name: &str, master: &str) -> Observation {
        Observation::Asset {
            name: name.into(),
            master: Some(master.into()),
            body: "{}".into(),
            deleted: false,
            hidden: false,
            sparse: None,
        }
    }

    #[tokio::test]
    async fn page_and_position_commit_together() {
        let state = State::open_in_memory().unwrap();
        assert_eq!(state.position("Z").await.unwrap(), None);
        state
            .commit_page("Z", vec![asset("A1", "M1")], "t1".into(), false)
            .await
            .unwrap();
        assert_eq!(state.position("Z").await.unwrap().as_deref(), Some("t1"));
        let dirty = state.dirty_assets("Z", "", 10).await.unwrap();
        assert_eq!(dirty.len(), 1);
        assert_eq!(dirty[0].master_body, None);
        state.reset_position("Z").await.unwrap();
        assert_eq!(state.position("Z").await.unwrap(), None);
        assert_eq!(state.dirty_assets("Z", "", 10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn plans_keep_paths_and_replace_only_through_pending() {
        let state = State::open_in_memory().unwrap();
        state
            .commit_page("Z", vec![asset("A1", "M1")], "t1".into(), true)
            .await
            .unwrap();
        let wanted = |checksum: &str, path: &str| WantedFile {
            rendition: "original".into(),
            album: String::new(),
            path: path.into(),
            size: 3,
            checksum: checksum.into(),
            adopt: false,
            imported: None,
        };
        state
            .apply_plan("Z", "A1", vec![wanted("c1", "a.jpg")])
            .await
            .unwrap();
        let row = state.work("Z", 0, 10).await.unwrap().remove(0);
        state.mark_done(row.id, "h1").await.unwrap();
        assert!(state.work("Z", 0, 10).await.unwrap().is_empty());

        // A new version keeps the original path and the recorded hash.
        state
            .apply_plan("Z", "A1", vec![wanted("c2", "renamed.jpg")])
            .await
            .unwrap();
        let row = state.work("Z", 0, 10).await.unwrap().remove(0);
        assert_eq!(row.path, "a.jpg");
        assert_eq!(row.checksum, "c2");
        assert_eq!(row.sha256.as_deref(), Some("h1"));
        assert_eq!(row.state, FileState::Pending);

        // A copy no longer wanted leaves the queue but keeps its row.
        state.apply_plan("Z", "A1", Vec::new()).await.unwrap();
        assert!(state.work("Z", 0, 10).await.unwrap().is_empty());
        assert_eq!(state.files_for_asset("Z", "A1").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn memberships_and_masters_mark_assets_for_planning() {
        let state = State::open_in_memory().unwrap();
        state
            .commit_page("Z", vec![asset("A1", "M1")], "t1".into(), true)
            .await
            .unwrap();
        state.apply_plan("Z", "A1", Vec::new()).await.unwrap();
        assert!(state.dirty_assets("Z", "", 10).await.unwrap().is_empty());
        state
            .commit_page(
                "Z",
                vec![
                    Observation::Album {
                        name: "AL".into(),
                        parent: None,
                        title: Some("Trip".into()),
                        is_folder: false,
                        deleted: false,
                    },
                    Observation::Membership {
                        relation: "A1-IN-AL".into(),
                        album: "AL".into(),
                        asset: "A1".into(),
                        present: true,
                    },
                ],
                "t2".into(),
                true,
            )
            .await
            .unwrap();
        let dirty = state.dirty_assets("Z", "", 10).await.unwrap();
        assert_eq!(dirty[0].albums, vec![("Z".to_owned(), "AL".to_owned())]);
        state.apply_plan("Z", "A1", Vec::new()).await.unwrap();
        state
            .commit_page(
                "Z",
                vec![Observation::Master {
                    name: "M1".into(),
                    body: "{}".into(),
                }],
                "t3".into(),
                true,
            )
            .await
            .unwrap();
        let dirty = state.dirty_assets("Z", "", 10).await.unwrap();
        assert_eq!(dirty[0].master_body.as_deref(), Some("{}"));
    }
}

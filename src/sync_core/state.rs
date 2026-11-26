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
    dirty INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (zone, name)
) STRICT;
CREATE INDEX IF NOT EXISTS assets_by_master ON assets (zone, master);
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
    /// Present memberships in albums that still exist.
    pub(crate) albums: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AlbumRow {
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
                .map_err(|_| anyhow::anyhow!("Sync state lock was poisoned"))?;
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

    pub(crate) async fn albums(&self, zone: &str) -> anyhow::Result<Vec<AlbumRow>> {
        let zone = zone.to_owned();
        self.with(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT name, parent, title, is_folder, deleted, folder FROM albums WHERE zone = ?1",
            )?;
            let rows = stmt
                .query_map([zone], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, i64>(3)? != 0,
                        row.get::<_, i64>(4)? != 0,
                        row.get::<_, Option<String>>(5)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows.into_iter()
                .map(|(name, parent, title, is_folder, deleted, folder)| {
                    Ok(AlbumRow {
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
                "SELECT a.name, a.body, m.body, a.deleted, a.hidden, a.purged OR COALESCE(m.purged, 0)
                 FROM assets a LEFT JOIN masters m ON m.zone = a.zone AND m.name = a.master
                 WHERE a.zone = ?1 AND a.dirty = 1 AND a.name > ?2 ORDER BY a.name LIMIT ?3",
            )?;
            let mut albums = conn.prepare(
                "SELECT album FROM memberships WHERE zone = ?1 AND asset = ?2 AND present = 1
                 ORDER BY album",
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
                        albums: Vec::new(),
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows.into_iter()
                .map(|mut asset| {
                    asset.albums = albums
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
                        let next = if checksum == file.checksum || state == FileState::Modified {
                            state
                        } else if state == FileState::Adopt {
                            FileState::Pending
                        } else if state == FileState::Done {
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
                        let state = if file.adopt { FileState::Adopt } else { FileState::Pending };
                        tx.execute(
                            "INSERT INTO files (zone, asset, rendition, album, path, size, checksum, state)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                            params![
                                zone,
                                asset,
                                file.rendition,
                                file.album,
                                file.path,
                                i64::try_from(file.size)?,
                                file.checksum,
                                state.as_str()
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
        } => {
            tx.execute(
                "INSERT INTO assets (zone, name, master, body, deleted, hidden, purged, dirty)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 1)
                 ON CONFLICT (zone, name) DO UPDATE SET master = excluded.master,
                     body = excluded.body, deleted = excluded.deleted,
                     hidden = excluded.hidden, purged = 0, dirty = 1",
                params![
                    zone,
                    name,
                    master,
                    body,
                    i64::from(deleted),
                    i64::from(hidden)
                ],
            )?;
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
        assert_eq!(dirty[0].albums, vec!["AL".to_owned()]);
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

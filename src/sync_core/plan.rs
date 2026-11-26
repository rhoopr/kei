//! Turn catalog facts into the local copies kei owes.
//!
//! A copy's path is chosen once, when its row is created, and never changes.
//! An album's folder is fixed the first time a copy is planned into it, so
//! renaming the album in iCloud moves nothing. Copies that stop being wanted
//! keep their files.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::download::core_io::expected_files;
use crate::download::filter::PathDerivationConfig;
use crate::download::paths;
use crate::icloud::photos::feed::stored_asset;
use crate::selection::AlbumSelector;

use super::CoreConfig;
use super::state::{AlbumRow, DirtyAsset, State, WantedFile};

const BATCH: usize = 256;
/// Upper bound on collision suffixes tried for one path.
const MAX_ORDINAL: u64 = 1000;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlanStats {
    pub(crate) planned: u64,
    /// Assets that cannot be planned yet, such as one whose master or album
    /// iCloud has not reported. They stay marked and are retried.
    pub(crate) waiting: u64,
}

/// Plan every marked asset in a zone.
pub(crate) async fn plan_zone(
    state: &State,
    zone: &str,
    config: &CoreConfig,
) -> anyhow::Result<PlanStats> {
    let albums = Albums::new(state.albums(zone).await?, &config.albums);
    let mut stats = PlanStats::default();
    let mut after = String::new();
    loop {
        let batch = state.dirty_assets(zone, &after, BATCH).await?;
        let Some(last) = batch.last() else {
            break;
        };
        after.clone_from(&last.name);
        for asset in batch {
            match plan_asset(state, zone, config, &albums, &asset).await? {
                Some(wanted) => {
                    state.apply_plan(zone, &asset.name, wanted).await?;
                    stats.planned += 1;
                }
                None => stats.waiting += 1,
            }
        }
    }
    Ok(stats)
}

/// Where one asset's copies go. `None` means it must wait for more facts.
async fn plan_asset(
    state: &State,
    zone: &str,
    config: &CoreConfig,
    albums: &Albums,
    asset: &DirtyAsset,
) -> anyhow::Result<Option<Vec<WantedFile>>> {
    if asset.purged || asset.deleted || asset.hidden {
        return Ok(Some(Vec::new()));
    }
    let Some(master) = &asset.master_body else {
        return Ok(None);
    };
    let photo = match stored_asset(master, &asset.asset_body) {
        Ok(photo) => photo,
        Err(error) => {
            tracing::warn!(error = %error, "Could not read a stored photo record");
            return Ok(None);
        }
    };

    let mut member_of_any = false;
    let mut placements = Vec::new();
    for album in &asset.albums {
        let Some(row) = albums.rows.get(album) else {
            return Ok(None);
        };
        if row.deleted || row.is_folder {
            continue;
        }
        member_of_any = true;
        if !albums.selected(album) {
            continue;
        }
        let Some(folder) = albums.folder(album) else {
            return Ok(None);
        };
        placements.push((album.clone(), album_config(config, &folder)));
    }
    if config.unfiled && !member_of_any {
        placements.push((String::new(), unfiled_config(config, zone)));
    }

    let existing: HashSet<(String, String)> = state
        .files_for_asset(zone, &asset.name)
        .await?
        .into_iter()
        .map(|row| (row.rendition, row.album))
        .collect();
    let mut taken = HashSet::new();
    let mut wanted = Vec::new();
    for (album, placement) in placements {
        let files = expected_files(&photo, &placement);
        if !album.is_empty()
            && !files.is_empty()
            && let Some(folder) = albums.folder(&album)
        {
            state.fix_album_folder(zone, &album, &folder).await?;
        }
        for file in files {
            let relative = relative_path(&config.directory, &file.path)?;
            let (path, adopt) = if existing.contains(&(file.rendition.to_owned(), album.clone())) {
                (relative, false)
            } else {
                choose_path(
                    state,
                    &config.directory,
                    &relative,
                    file.size,
                    photo.id(),
                    &taken,
                )
                .await?
            };
            taken.insert(path.clone());
            wanted.push(WantedFile {
                rendition: file.rendition.to_owned(),
                album: album.clone(),
                path,
                size: file.size,
                checksum: file.checksum,
                adopt,
            });
        }
    }
    Ok(Some(wanted))
}

/// Pick a free path for a new copy: the derived path, then the asset's
/// identity suffix, then numbered suffixes. A file already at a candidate
/// with the expected size is adopted; anything else there is left alone.
async fn choose_path(
    state: &State,
    directory: &Path,
    relative: &str,
    size: u64,
    asset_id: &str,
    taken: &HashSet<String>,
) -> anyhow::Result<(String, bool)> {
    let candidates =
        [
            relative.to_owned(),
            paths::insert_asset_identity_suffix(relative, asset_id),
        ]
        .into_iter()
        .chain((2..=MAX_ORDINAL).map(|ordinal| {
            paths::insert_asset_identity_ordinal_suffix(relative, asset_id, ordinal)
        }));
    for candidate in candidates {
        if taken.contains(&candidate) || state.path_owner(&candidate).await?.is_some() {
            continue;
        }
        match tokio::fs::symlink_metadata(directory.join(&candidate)).await {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok((candidate, false));
            }
            Err(error) => return Err(error.into()),
            Ok(meta) if meta.is_file() && meta.len() == size => return Ok((candidate, true)),
            Ok(_) => {}
        }
    }
    anyhow::bail!("No free path for {relative}")
}

fn relative_path(directory: &Path, path: &Path) -> anyhow::Result<String> {
    let relative = path.strip_prefix(directory).map_err(|_outside| {
        anyhow::anyhow!(
            "Planned path {} is outside the download directory",
            path.display()
        )
    })?;
    let parts = relative
        .components()
        .map(|part| match part {
            std::path::Component::Normal(part) => part
                .to_str()
                .map(ToOwned::to_owned)
                .ok_or_else(|| anyhow::anyhow!("Planned path is not valid UTF-8")),
            _ => Err(anyhow::anyhow!(
                "Planned path {} is not a plain relative path",
                path.display()
            )),
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    anyhow::ensure!(!parts.is_empty(), "Planned path is empty");
    Ok(parts.join("/"))
}

fn unfiled_config(config: &CoreConfig, zone: &str) -> PathDerivationConfig {
    let mut placement = config.paths.clone();
    let template = paths::expand_named_token(&placement.folder_structure, paths::TOKEN_ALBUM, None);
    placement.folder_structure = paths::expand_named_token(
        &template,
        paths::TOKEN_LIBRARY,
        Some(paths::truncate_library_zone(zone)),
    );
    placement.album_name = None;
    placement
}

fn album_config(config: &CoreConfig, folder: &[String]) -> PathDerivationConfig {
    let mut placement = config.paths.clone();
    let template = paths::expand_album_components(&placement.folder_structure_albums, folder);
    placement.folder_structure = paths::expand_named_token(
        &template,
        paths::TOKEN_LIBRARY,
        Some(paths::truncate_library_zone(&placement.library)),
    );
    placement.album_name = Some(folder.join("/").into());
    placement
}

/// Album rows with their resolved selection.
struct Albums {
    rows: HashMap<String, AlbumRow>,
    selected: HashSet<String>,
}

impl Albums {
    fn new(rows: Vec<AlbumRow>, selector: &AlbumSelector) -> Self {
        let rows: HashMap<_, _> = rows
            .into_iter()
            .map(|row| (row.name.clone(), row))
            .collect();
        let selected = rows
            .values()
            .filter(|row| !row.deleted && !row.is_folder)
            .filter(|row| {
                components(&rows, &row.name)
                    .is_some_and(|components| selects(selector, &components))
            })
            .map(|row| row.name.clone())
            .collect();
        Self { rows, selected }
    }

    fn selected(&self, album: &str) -> bool {
        self.selected.contains(album)
    }

    /// The album's fixed folder, or its current components if none is fixed yet.
    fn folder(&self, album: &str) -> Option<Vec<String>> {
        let row = self.rows.get(album)?;
        row.folder.clone().or_else(|| components(&self.rows, album))
    }
}

/// Current names from the top folder down to the album.
fn components(rows: &HashMap<String, AlbumRow>, album: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut current = rows.get(album)?;
    loop {
        out.push(current.title.clone()?);
        match &current.parent {
            None => break,
            Some(parent) => current = rows.get(parent)?,
        }
        if out.len() > 32 {
            return None;
        }
    }
    out.reverse();
    Some(out)
}

fn selects(selector: &AlbumSelector, components: &[String]) -> bool {
    let matches = |name: &String| {
        if name.contains('/') {
            components.join("/") == *name
        } else {
            components.last() == Some(name)
        }
    };
    match selector {
        AlbumSelector::None => false,
        AlbumSelector::All { excluded } => !excluded.iter().any(matches),
        AlbumSelector::Named { included, excluded } => {
            included.iter().any(matches) && !excluded.iter().any(matches)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, title: &str, parent: Option<&str>) -> AlbumRow {
        AlbumRow {
            name: name.into(),
            parent: parent.map(Into::into),
            title: Some(title.into()),
            is_folder: false,
            deleted: false,
            folder: None,
        }
    }

    #[test]
    fn selection_matches_leaf_names_and_full_paths() {
        let mut folder = row("F", "Travel", None);
        folder.is_folder = true;
        let rows = vec![folder, row("A", "Paris", Some("F")), row("B", "Home", None)];
        let all = Albums::new(rows.clone(), &AlbumSelector::default());
        assert!(all.selected("A") && all.selected("B") && !all.selected("F"));
        assert_eq!(
            all.folder("A").unwrap(),
            vec!["Travel".to_owned(), "Paris".to_owned()]
        );

        let named = AlbumSelector::Named {
            included: ["Travel/Paris".to_owned()].into(),
            excluded: Default::default(),
        };
        let named = Albums::new(rows.clone(), &named);
        assert!(named.selected("A") && !named.selected("B"));

        let excluded = AlbumSelector::All {
            excluded: ["Home".to_owned()].into(),
        };
        assert!(!Albums::new(rows, &excluded).selected("B"));
    }

    #[test]
    fn fixed_folders_survive_renames() {
        let mut album = row("A", "Renamed", None);
        album.folder = Some(vec!["Original".into()]);
        let albums = Albums::new(vec![album], &AlbumSelector::default());
        assert_eq!(albums.folder("A").unwrap(), vec!["Original".to_owned()]);
    }

    #[test]
    fn relative_paths_reject_escapes() {
        let dir = Path::new("/photos");
        assert_eq!(
            relative_path(dir, Path::new("/photos/2024/a.jpg")).unwrap(),
            "2024/a.jpg"
        );
        assert!(relative_path(dir, Path::new("/elsewhere/a.jpg")).is_err());
    }
}

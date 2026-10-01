//! Load the current library-scoped groupings for a queued rewrite page.

use crate::download::AssetGroupings;
use crate::state::MembershipStore;
use crate::state::db::PendingMetadataRewrite;
use std::collections::{BTreeMap, HashMap, HashSet};

pub(super) async fn load_pending_groupings<D>(
    db: &D,
    pending: &[PendingMetadataRewrite],
) -> (HashMap<String, AssetGroupings>, HashSet<String>)
where
    D: MembershipStore + ?Sized,
{
    let mut ids_by_library: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for pending in pending {
        let record = &pending.asset;
        ids_by_library
            .entry(record.library.as_ref())
            .or_default()
            .push(record.id.as_ref());
    }

    let mut groupings_by_library = HashMap::with_capacity(ids_by_library.len());
    let mut failed_libraries = HashSet::new();
    for (library, asset_ids) in ids_by_library {
        match db.get_asset_groupings(library, &asset_ids).await {
            Ok(rows) => {
                let mut groupings = AssetGroupings::default();
                for (asset_id, album) in rows.albums {
                    groupings.albums.entry(asset_id).or_default().push(album);
                }
                for (asset_id, person) in rows.people {
                    groupings.people.entry(asset_id).or_default().push(person);
                }
                groupings_by_library.insert(library.to_owned(), groupings);
            }
            Err(e) => {
                tracing::warn!(
                    target: "kei::download::metadata_rewrite",
                    error = %e,
                    library,
                    "Failed to load asset groupings for metadata rewrites; leaving markers for retry"
                );
                failed_libraries.insert(library.to_owned());
            }
        }
    }
    (groupings_by_library, failed_libraries)
}

#[cfg(test)]
mod tests;

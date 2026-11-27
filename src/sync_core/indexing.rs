//! iCloud's indexing state, read once per zone per cycle.
//!
//! `changes/zone` is read whatever the state is. While iCloud says it is
//! still indexing (RUNNING), smart folders that come from its search index
//! (Screenshots, Selfies and the like) wait, and since Apple does not say
//! whether a listing taken then is complete, the zone is listed once more
//! from the start when indexing stops running. Some accounts report `FAILED`
//! or no state for good while the library reads normally; that is treated as
//! unavailable rather than as indexing, so it neither holds the cycle
//! incomplete nor waits for a FINISHED that may never come.

use crate::icloud::error::ICloudError;
use crate::icloud::photos::PhotoLibrary;

use super::State;

/// Kv key prefix; the zone name follows.
const KEY_PREFIX: &str = "indexing_relist:";
/// The zone was observed while iCloud was indexing; a full listing is owed.
const OWED: &str = "owed";
/// That full listing has started. A restart continues it from its position.
const STARTED: &str = "started";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Indexing {
    Finished,
    /// iCloud said it is still indexing (RUNNING).
    NotFinished,
    /// iCloud reported a state kei does not recognize, such as `FAILED`, or
    /// none. Listing carries on as if indexing were finished.
    Unavailable(&'static str),
    /// The check itself failed. Nothing about the zone's listing changes.
    Unknown,
}

impl Indexing {
    pub(super) async fn check(library: &PhotoLibrary) -> Self {
        match library.check_indexing_once().await {
            Ok(true) => Self::Finished,
            Ok(false) => Self::NotFinished,
            Err(ICloudError::IndexingUnavailable(reason)) => Self::Unavailable(reason),
            Err(error) => {
                tracing::debug!(
                    zone = library.zone_name(),
                    error = %error,
                    "Could not read iCloud's indexing state"
                );
                Self::Unknown
            }
        }
    }
}

fn key(zone: &str) -> String {
    format!("{KEY_PREFIX}{zone}")
}

/// Record what this cycle's indexing state means for the zone's listing,
/// before the zone is observed. Starts the owed full listing when indexing
/// has finished.
pub(super) async fn before_observe(
    state: &State,
    zone: &str,
    indexing: Indexing,
) -> anyhow::Result<()> {
    let key = key(zone);
    match indexing {
        Indexing::NotFinished => {
            tracing::warn!(
                zone,
                "iCloud is still indexing this library; listing it anyway. Smart folders such as \
                 Screenshots wait, and the library is listed again once indexing finishes"
            );
            state.set_kv(&key, OWED).await?;
        }
        Indexing::Finished | Indexing::Unavailable(_) => {
            if let Indexing::Unavailable(reason) = indexing {
                tracing::warn!(
                    zone,
                    reason,
                    "iCloud did not report its photo index as ready; listing and downloading continue"
                );
            }
            if state.get_kv(&key).await?.as_deref() == Some(OWED) {
                tracing::info!(
                    zone,
                    "iCloud is no longer indexing this library; listing it again from the start"
                );
                state.relist(zone).await?;
                state.set_kv(&key, STARTED).await?;
            }
        }
        Indexing::Unknown => {}
    }
    Ok(())
}

/// Clear the obligation once the full listing after indexing reached the end.
pub(super) async fn after_observe(
    state: &State,
    zone: &str,
    indexing: Indexing,
    caught_up: bool,
) -> anyhow::Result<()> {
    let key = key(zone);
    if matches!(indexing, Indexing::Finished | Indexing::Unavailable(_))
        && caught_up
        && state.get_kv(&key).await?.as_deref() == Some(STARTED)
    {
        state.delete_kv(&key).await?;
    }
    Ok(())
}

/// A full listing is still owed for this zone because of indexing.
pub(super) async fn relist_owed(state: &State, zone: &str) -> anyhow::Result<bool> {
    Ok(state.get_kv(&key(zone)).await?.is_some())
}

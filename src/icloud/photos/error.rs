//! Provider errors whose meaning must survive orchestration boundaries.

/// A refused observation cannot be recovered by an uncaptured rank inventory.
/// Preserve the redacted underlying diagnostic while preventing static fallback.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub(crate) struct ShadowPageError(#[from] anyhow::Error);

/// Preserve typed token fallback while refusing a new uncaptured observation.
pub(crate) fn classify_shadow_page_error(
    error: anyhow::Error,
    capture_enabled: bool,
) -> anyhow::Error {
    if capture_enabled && error.downcast_ref::<super::SyncTokenError>().is_none() {
        ShadowPageError::from(error).into()
    } else {
        error
    }
}

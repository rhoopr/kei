//! Provider errors whose meaning must survive orchestration boundaries.

/// A refused observation cannot be recovered by an uncaptured rank inventory.
/// Preserve the redacted underlying diagnostic while preventing static fallback.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub(crate) struct ShadowPageError(#[from] anyhow::Error);

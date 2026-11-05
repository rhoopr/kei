//! Provider errors whose meaning must survive orchestration boundaries.

/// Current work without a scoped durable owner must retain its source cursor.
/// A rank inventory cannot substitute for accounting for this observation.
#[derive(Debug, thiserror::Error)]
#[error("Current provider record has no scoped retry owner; retaining the source cursor")]
pub(crate) struct UnaccountedChangeError;

/// A scoped changes page cannot advance without its successor cursor.
#[derive(Debug, thiserror::Error)]
#[error("Missing changes/zone successor")]
pub(crate) struct MissingChangesSuccessor;

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
    if capture_enabled
        && error.downcast_ref::<super::SyncTokenError>().is_none()
        && error.downcast_ref::<MissingChangesSuccessor>().is_none()
        && error.downcast_ref::<UnaccountedChangeError>().is_none()
    {
        ShadowPageError::from(error).into()
    } else {
        error
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn unaccounted_change_stays_typed_with_shadow_capture() {
        let error = super::classify_shadow_page_error(super::UnaccountedChangeError.into(), true);
        assert!(
            error
                .downcast_ref::<super::UnaccountedChangeError>()
                .is_some()
        );
    }
}

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

/// Observation-only classification: reports whether a catalog projection failed
/// because the provider catalog hit its typed capacity limit. It never changes
/// the error, the caller's fallback, or the recorded diagnostic.
pub(crate) fn classify_catalog_projection_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<crate::state::error::StateError>(),
            Some(crate::state::error::StateError::ProviderCatalogFull)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::classify_catalog_projection_error;
    use crate::state::error::StateError;

    #[test]
    fn unaccounted_change_stays_typed_with_shadow_capture() {
        let error = super::classify_shadow_page_error(super::UnaccountedChangeError.into(), true);
        assert!(
            error
                .downcast_ref::<super::UnaccountedChangeError>()
                .is_some()
        );
    }

    #[test]
    fn catalog_projection_classifier_matches_typed_capacity() {
        assert!(classify_catalog_projection_error(&anyhow::Error::new(
            StateError::ProviderCatalogFull
        )));
        let wrapped =
            anyhow::Error::new(StateError::ProviderCatalogFull).context("projecting catalog page");
        assert!(classify_catalog_projection_error(&wrapped));
    }

    #[test]
    fn catalog_projection_classifier_ignores_other_and_textual_errors() {
        assert!(!classify_catalog_projection_error(&anyhow::Error::new(
            StateError::ProviderCatalogInvalid
        )));
        let textual = anyhow::anyhow!("{}", StateError::ProviderCatalogFull);
        assert!(!classify_catalog_projection_error(&textual));
    }
}

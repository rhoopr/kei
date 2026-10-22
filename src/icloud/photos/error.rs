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

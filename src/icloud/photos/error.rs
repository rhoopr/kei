//! Provider errors whose meaning must survive orchestration boundaries.

/// A refused observation cannot be recovered by an uncaptured rank inventory.
/// Preserve the redacted underlying diagnostic while preventing static fallback.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub(crate) struct ShadowPageError(#[from] anyhow::Error);

#[derive(Debug, thiserror::Error)]
#[error("Missing changes/zone successor")]
pub(crate) struct MissingZoneSuccessor;

/// Classify only typed evidence. A captured page can wrap the provider error.
pub(crate) fn bridge_error_reason(error: &anyhow::Error) -> &'static str {
    let cause = error
        .downcast_ref::<ShadowPageError>()
        .map_or(error, |e| &e.0);
    if cause.is::<MissingZoneSuccessor>() {
        "missing_successor"
    } else {
        "replay_error"
    }
}

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
    fn bridge_error_reason_does_not_infer_cause_from_text() {
        let error = anyhow::anyhow!("{}", super::MissingZoneSuccessor);
        assert_eq!(super::bridge_error_reason(&error), "replay_error");
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

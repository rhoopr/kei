//! Provider errors whose meaning must survive orchestration boundaries.

use super::SyncTokenError;
use crate::state::db::provider_inbox::CapturedPageId;

/// A refused observation cannot be recovered by an uncaptured rank inventory.
/// Preserve the redacted underlying diagnostic while preventing static fallback.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub(crate) struct ShadowPageError(#[from] anyhow::Error);

/// A raw body rejection that only live replay may turn into a library hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(super) enum ChangesBodyRejection {
    #[error("Unsuccessful provider response")]
    UnsuccessfulStatus,
    #[error("Provider changes page exceeded the capture byte limit")]
    TooLarge,
    #[error("Invalid or ambiguous provider changes JSON")]
    Malformed,
    #[error("Invalid or ambiguous provider changes JSON")]
    DuplicateKeys,
}

impl ChangesBodyRejection {
    pub(super) fn into_shadow_error(self) -> anyhow::Error {
        ShadowPageError::from(anyhow::Error::new(self)).into()
    }

    pub(super) fn from_shadow_error(error: &anyhow::Error) -> Option<Self> {
        error
            .downcast_ref::<ShadowPageError>()?
            .0
            .downcast_ref::<Self>()
            .copied()
    }
}

const GENERIC_BRIDGE_REASON: &str = "replay_error";

// The enum and exact reporting allowlists use the same fixed vocabulary.
macro_rules! replay_refusals {
    ($($variant:ident: $message:literal, $checkpoint:literal, $bridge:literal, $explanation:literal;)+) => {
        /// Refusals from a scoped changes stream that can hold just that zone.
        #[derive(Clone, Copy, Debug, thiserror::Error)]
        pub(crate) enum ReplayRefusal {
            $(#[error($message)] $variant,)+
        }

        impl ReplayRefusal {
            pub(crate) fn checkpoint_reason(self) -> &'static str {
                match self {
                    $(Self::$variant => concat!("icloud_changes_", $checkpoint),)+
                }
            }

            pub(crate) fn checkpoint_explanation(reason: &str) -> Option<&'static str> {
                match reason {
                    $(concat!("icloud_changes_", $checkpoint) => Some($explanation),)+
                    _ => None,
                }
            }

            fn bridge_reason(self) -> &'static str {
                match self {
                    $(Self::$variant => $bridge,)+
                }
            }

            pub(crate) fn is_bridge_reason(reason: &str) -> bool {
                matches!(reason, GENERIC_BRIDGE_REASON $(| $bridge)+)
            }
        }
    };
}

// Columns: error text, checkpoint suffix, bridge label, checkpoint explanation.
replay_refusals! {
    UnsuccessfulResponse: "iCloud Photos replay did not return a successful response",
        "unsuccessful_response", "unsuccessful_response",
        "iCloud did not return a successful changes page; kei will retry from the saved checkpoint";
    OversizedBody: "iCloud Photos replay response exceeded the byte limit",
        "oversized_body", "oversized_body",
        "iCloud returned a response over kei's replay byte limit; kei will retry from the saved checkpoint";
    MalformedJson: "iCloud Photos replay returned malformed JSON",
        "malformed_json", "malformed_json",
        "iCloud returned malformed JSON; kei will retry from the saved checkpoint";
    DuplicateJsonKeys: "iCloud Photos replay returned JSON with duplicate keys",
        "duplicate_json_keys", "duplicate_json_keys",
        "iCloud returned JSON with duplicate keys; kei will retry from the saved checkpoint";
    MissingSuccessor: "Missing changes/zone successor",
        "missing_successor", "missing_successor",
        "iCloud did not return a usable next page cursor; kei will retry from the saved checkpoint";
    RejectedContinuation: "Rejected changes/zone continuation",
        "continuation_rejected", "continuation_rejected",
        "iCloud rejected a continuation cursor; kei will retry from the saved checkpoint";
    RepeatedContinuation: "Non-progressing changes/zone continuation",
        "repeated_continuation", "repeated_continuation",
        "iCloud repeated a continuation cursor; kei will retry from the saved checkpoint";
    InvalidContinuationFlag: "Missing or invalid changes/zone continuation flag",
        "invalid_continuation_flag", "invalid_continuation_flag",
        "iCloud did not say whether more changes were coming; kei will retry from the saved checkpoint";
    InvalidReason: "Invalid changes/zone reason field",
        "invalid_reason", "invalid_reason",
        "iCloud returned a malformed reason field; kei will retry from the saved checkpoint";
    InvalidRecords: "Missing or invalid changes/zone records",
        "invalid_records", "invalid_records",
        "iCloud returned a page without usable records; kei will retry from the saved checkpoint";
    RecordError: "Changes/zone record failed",
        "record_error", "record_error",
        "iCloud returned a record error in this zone; kei will retry from the saved checkpoint";
    MissingZone: "iCloud Photos zone unavailable during replay",
        "zone_not_found", "zone_not_found",
        "iCloud did not report the zone during replay; kei will keep the saved checkpoint";
    InvalidZonesField: "iCloud Photos replay did not return a zones array",
        "invalid_zones_field", "invalid_zones_field",
        "iCloud did not return a zones array; kei will retry from the saved checkpoint";
    EmptyZoneResults: "iCloud Photos replay returned no zone result",
        "empty_zone_results", "empty_zone_results",
        "iCloud returned no zone result; kei will retry from the saved checkpoint";
    MultipleZoneResults: "iCloud Photos replay returned multiple zone results",
        "multiple_zone_results", "multiple_zone_results",
        "iCloud returned more than one zone result for this request; kei will retry from the saved checkpoint";
    InvalidZoneIdentity: "iCloud Photos replay did not return a usable zone identity",
        "invalid_zone_identity", "invalid_zone_identity",
        "iCloud returned a page without a usable zone identity; kei will retry from the saved checkpoint";
    ZoneNameMismatch: "iCloud Photos replay did not match the requested zone name",
        "zone_name_mismatch", "zone_name_mismatch",
        "iCloud did not return the requested zone name; kei will retry from the saved checkpoint";
    ZoneOwnerMismatch: "iCloud Photos replay did not match the requested zone owner",
        "zone_owner_mismatch", "zone_owner_mismatch",
        "iCloud did not return the requested zone owner; kei will retry from the saved checkpoint";
    RecordScopeMismatch: "iCloud Photos replay contained a record outside the page's zone scope",
        "record_scope_mismatch", "record_scope_mismatch",
        "iCloud returned a record outside the page's zone scope; kei will retry from the saved checkpoint";
    RetryLater: "iCloud Photos zone asked to retry replay later",
        "retry_later", "retry_later",
        "iCloud asked this zone to retry later; kei will retry from the saved checkpoint";
    ServerInternalError: "iCloud Photos zone returned an internal error during replay",
        "server_internal_error", "server_error",
        "iCloud returned a server error for this zone; kei will retry from the saved checkpoint";
    UnrecognizedZoneError: "iCloud Photos zone returned an unrecognized error during replay",
        "unrecognized_zone_error", "unrecognized_zone_error",
        "iCloud returned an unrecognized error for this zone; kei will retry from the saved checkpoint";
    Throttled: "iCloud Photos zone throttled replay",
        "zone_throttled", "throttled",
        "iCloud throttled this zone; kei will retry from the saved checkpoint";
}

impl ReplayRefusal {
    /// Classify errors from a validated zone response. The caller must
    /// leave an explicitly rejected initial saved cursor eligible for fallback.
    pub(super) fn from_zone_error(error: &SyncTokenError) -> Self {
        match error {
            SyncTokenError::InvalidToken { .. } => Self::RejectedContinuation,
            SyncTokenError::ZoneNotFound { .. } => Self::MissingZone,
            SyncTokenError::UnexpectedZoneError { error_code, .. } => match error_code.as_ref() {
                "RETRY_LATER" => Self::RetryLater,
                "SERVER_INTERNAL_ERROR" => Self::ServerInternalError,
                "THROTTLED" => Self::Throttled,
                _ => Self::UnrecognizedZoneError,
            },
        }
    }

    pub(super) fn into_shadow_error(self) -> anyhow::Error {
        ShadowPageError::from(anyhow::Error::new(self)).into()
    }

    /// Only use the refusal at the replay boundary, not in a raw page parser.
    pub(crate) fn from_shadow_error(error: &anyhow::Error) -> Option<Self> {
        let cause = &error.downcast_ref::<ShadowPageError>()?.0;
        cause.downcast_ref::<Self>().copied().or_else(|| {
            cause
                .downcast_ref::<AcceptedReplay>()
                .map(|prefix| prefix.reason)
        })
    }
}

/// A scoped refusal can carry durable media evidence without carrying a cursor.
#[derive(Debug, thiserror::Error)]
#[error("{reason}")]
pub(crate) struct AcceptedReplay {
    reason: ReplayRefusal,
    pub(crate) pages: Vec<CapturedPageId>,
}

impl AcceptedReplay {
    pub(super) fn attach(error: anyhow::Error, pages: Vec<CapturedPageId>) -> anyhow::Error {
        match ReplayRefusal::from_shadow_error(&error) {
            Some(reason) if !pages.is_empty() => {
                ShadowPageError::from(anyhow::Error::new(Self { reason, pages })).into()
            }
            _ => error,
        }
    }

    pub(crate) fn from_shadow_error(error: &anyhow::Error) -> Option<&Self> {
        error.downcast_ref::<ShadowPageError>()?.0.downcast_ref()
    }
}

/// The initial saved cursor was rejected. The changes adapter wraps a
/// rejected continuation as a refused replay.
pub(crate) fn saved_cursor_rejected(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<SyncTokenError>(),
        Some(SyncTokenError::InvalidToken { .. })
    )
}

/// Classify only typed evidence. A captured page can wrap the provider error.
pub(crate) fn bridge_error_reason(error: &anyhow::Error) -> &'static str {
    ReplayRefusal::from_shadow_error(error)
        .or_else(|| error.downcast_ref::<ReplayRefusal>().copied())
        .map_or(GENERIC_BRIDGE_REASON, ReplayRefusal::bridge_reason)
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
        let error = anyhow::anyhow!("{}", super::ReplayRefusal::MissingSuccessor);
        assert_eq!(super::bridge_error_reason(&error), "replay_error");
        assert!(super::ReplayRefusal::from_shadow_error(&error).is_none());
    }

    #[test]
    fn isolates_only_captured_missing_successors() {
        let missing = super::ReplayRefusal::MissingSuccessor;
        let error = super::ShadowPageError(missing.into()).into();
        assert_eq!(super::bridge_error_reason(&error), "missing_successor");
        assert!(matches!(
            super::ReplayRefusal::from_shadow_error(&error),
            Some(super::ReplayRefusal::MissingSuccessor)
        ));
        assert!(super::ReplayRefusal::from_shadow_error(&missing.into()).is_none());
    }

    #[test]
    fn legacy_bridge_labels_remain_distinct_from_checkpoint_labels() {
        use super::ReplayRefusal;

        for (refusal, bridge, checkpoint) in [
            (
                ReplayRefusal::ServerInternalError,
                "server_error",
                "icloud_changes_server_internal_error",
            ),
            (
                ReplayRefusal::Throttled,
                "throttled",
                "icloud_changes_zone_throttled",
            ),
        ] {
            let error = super::AcceptedReplay::attach(
                refusal.into_shadow_error(),
                vec![super::CapturedPageId(1)],
            );
            assert_eq!(super::bridge_error_reason(&error), bridge);
            assert_eq!(
                crate::download::replay_refusal_reason(&error),
                Some(checkpoint)
            );
            assert_eq!(
                crate::download::sync_token_blocked_source(checkpoint),
                "icloud"
            );
        }
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

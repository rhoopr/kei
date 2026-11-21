//! Shared selection decisions and lossless destinations for active generations.

use serde::{Deserialize, Serialize};

use crate::state::error::StateError;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SelectionDestination {
    pub(crate) version_size: String,
    pub(crate) path: SelectionPath,
    pub(crate) checksum: String,
    pub(crate) size: u64,
    pub(crate) metadata_hash: String,
}

/// Preserve native path identity without imposing a new UTF-8 root policy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "encoding", content = "path", rename_all = "snake_case")]
pub(crate) enum SelectionPath {
    Utf8(String),
    #[cfg(unix)]
    UnixBytes(Vec<u8>),
    #[cfg(windows)]
    WindowsUnits(Vec<u16>),
}

impl SelectionPath {
    pub(crate) fn from_path(path: &std::path::Path) -> Self {
        if let Some(path) = path.to_str() {
            return Self::Utf8(path.to_owned());
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            Self::UnixBytes(path.as_os_str().as_bytes().to_vec())
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            Self::WindowsUnits(path.as_os_str().encode_wide().collect())
        }
    }

    pub(crate) fn to_path(&self) -> std::path::PathBuf {
        match self {
            Self::Utf8(path) => std::path::PathBuf::from(path),
            #[cfg(unix)]
            Self::UnixBytes(bytes) => {
                use std::os::unix::ffi::OsStringExt;
                std::ffi::OsString::from_vec(bytes.clone()).into()
            }
            #[cfg(windows)]
            Self::WindowsUnits(units) => {
                use std::os::windows::ffi::OsStringExt;
                std::ffi::OsString::from_wide(units).into()
            }
        }
    }

    pub(crate) fn key(&self) -> Result<String, StateError> {
        serde_json::to_string(self).map_err(|_invalid| StateError::ProviderSelectionInvalid)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SelectionOutcome {
    Selected,
    Excluded,
    Deferred,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SelectionMediaSource {
    pub(crate) scope: String,
    pub(crate) zone: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SelectionDecision {
    pub(crate) pass_key: String,
    pub(crate) child: String,
    pub(crate) master: Option<String>,
    /// Original response bytes, including unknowns and numeric lexemes.
    pub(crate) confirmation: Option<Vec<u8>>,
    /// Absent in retained single-zone decisions. Omission must preserve their
    /// original serialized bytes and hashes during replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) media_source: Option<SelectionMediaSource>,
    pub(crate) outcome: SelectionOutcome,
    /// Internal diagnostic, never provider error text.
    pub(crate) reason: String,
    pub(crate) destinations: Vec<SelectionDestination>,
}

#[cfg(test)]
mod tests {
    use super::{SelectionDecision, SelectionMediaSource};

    #[test]
    fn retained_decision_encoding_and_explicit_media_source_round_trip() {
        let retained = br#"{"pass_key":"album","child":"child","master":null,"confirmation":null,"outcome":"deferred","reason":"current_lookup_unresolved","destinations":[]}"#;
        let mut decision: SelectionDecision = serde_json::from_slice(retained).unwrap();
        assert!(decision.media_source.is_none());
        assert_eq!(serde_json::to_vec(&decision).unwrap(), retained);
        decision.media_source = Some(SelectionMediaSource {
            scope: "authenticated-media-scope".into(),
            zone: serde_json::json!({"zoneName":"media","ownerRecordName":"_defaultOwner"}),
        });
        let encoded = serde_json::to_vec(&decision).unwrap();
        let reopened: SelectionDecision = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(reopened, decision);
        assert_eq!(serde_json::to_vec(&reopened).unwrap(), encoded);
    }
}

use std::sync::Arc;

use rustc_hash::FxHashSet;

use crate::commands::{AlbumPass, PassKind};

use super::super::test_support::mock_album;
use super::incremental_requires_full_enumeration;

#[test]
fn incremental_full_enumeration_gate_ignores_unfiled_only_pass() {
    let session = crate::test_helpers::MockPhotosSession::new();
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: mock_album("", session),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    assert!(
        !incremental_requires_full_enumeration(&passes),
        "unfiled-only sync can use zone-level incremental changes"
    );
}

#[test]
fn incremental_full_enumeration_gate_fires_on_album_pass() {
    let session = crate::test_helpers::MockPhotosSession::new();
    let passes = vec![AlbumPass {
        kind: PassKind::Album,
        album: mock_album("Vacation", session),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    assert!(
        incremental_requires_full_enumeration(&passes),
        "album-scoped sync needs full enumeration to preserve membership"
    );
}

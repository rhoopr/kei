//! Pending-publication integrity remains exercised independently of the removed
//! automatic archive-layout engine. Recorded journal recovery is covered by
//! primary_layout/tests/normal_boundary_tests.rs.
use super::super::dispatch::download_photos_with_sync;
use super::super::models::{DownloadControls, DownloadOutcome, SyncResult};
use super::super::test_support::{
    album_with_session, incremental_photo_records_with_url, test_config,
};

#[path = "issue_770_recovery.rs"]
mod issue_770_recovery;

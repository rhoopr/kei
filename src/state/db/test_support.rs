//! Shared database-test directory fixture.

pub(super) fn test_dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

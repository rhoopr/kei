use super::{is_operational_storage_error, is_operational_storage_io};

#[test]
fn operational_storage_classifier_selects_only_destination_failure_kinds() {
    for kind in [
        std::io::ErrorKind::StorageFull,
        std::io::ErrorKind::ReadOnlyFilesystem,
        std::io::ErrorKind::PermissionDenied,
    ] {
        assert!(is_operational_storage_io(&std::io::Error::from(kind)));
        let wrapped = anyhow::Error::new(std::io::Error::from(kind))
            .context("Could not create temporary embedded metadata file");
        assert!(is_operational_storage_error(&wrapped), "{kind:?}");
    }
}

#[test]
fn operational_storage_classifier_rejects_ordinary_metadata_failures() {
    for kind in [
        std::io::ErrorKind::NotFound,
        std::io::ErrorKind::AlreadyExists,
        std::io::ErrorKind::InvalidData,
        std::io::ErrorKind::Unsupported,
    ] {
        assert!(!is_operational_storage_io(&std::io::Error::from(kind)));
        assert!(!is_operational_storage_error(&anyhow::Error::new(
            std::io::Error::from(kind)
        )));
    }
    assert!(!is_operational_storage_error(&anyhow::anyhow!(
        "metadata rewrite refused drifted file"
    )));
}

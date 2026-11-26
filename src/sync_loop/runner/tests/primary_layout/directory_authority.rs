use super::{
    EditedNaming, ORIGINAL, album_directory_options, assert_bytes, cycle_options, db_count,
    records, server,
};

#[cfg(feature = "xmp")]
#[tokio::test]
async fn ordinary_metadata_ownership_survives_managed_adoption_and_clear() {
    use crate::download::metadata::{MetadataWrite, apply_metadata};
    use little_exif::{exif_tag::ExifTag, metadata::Metadata};
    use xmp_toolkit::{OpenFileOptions, XmpFile, xmp_ns};

    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source.jpg");
    std::fs::write(
        &source,
        include_bytes!("../../../../../tests/data/media/pattern.jpg"),
    )
    .unwrap();
    apply_metadata(
        &source,
        &MetadataWrite {
            description: Some("A".into()),
            ..Default::default()
        },
        ".fixture",
    )
    .unwrap();
    let mut native = Metadata::new_from_path(&source).unwrap();
    native.set_tag(ExifTag::ImageDescription("B".into()));
    native.set_tag(ExifTag::Artist("camera artist".into()));
    native.write_to_file(&source).unwrap();
    let bytes = std::fs::read(&source).unwrap();
    let server = super::server_with_media(&bytes, super::VALID_EDIT, super::VALID_EDIT).await;
    let root = tempfile::tempdir().unwrap();
    let options = || super::CycleOptions {
        embed_xmp: true,
        exif_description: true,
        ..Default::default()
    };
    let mut provider = super::records_with_original(&server, &bytes, None);
    provider[1]["fields"]["extendedDescEnc"] = serde_json::json!({"type":"STRING","value":"B"});
    let (first, _) = cycle_options(
        root.path(),
        provider,
        false,
        EditedNaming::Suffix,
        options(),
    )
    .await;
    assert_eq!(first.failed_count, 0, "{:?}", first.stats);
    assert!(
        db_count(
            root.path(),
            "SELECT count(*) FROM asset_metadata_paths WHERE json_extract(embedded_metadata_receipt,'$.written.description')='B'"
        ) > 0,
        "ordinary publication must record its changed XMP description"
    );
    let mut provider =
        super::records_with_original(&server, &bytes, Some(("edit-one", super::VALID_EDIT)));
    provider[1]["fields"]["extendedDescEnc"] = serde_json::json!({"type":"STRING","value":"B"});
    let (adopted, _) = cycle_options(
        root.path(),
        provider.clone(),
        false,
        EditedNaming::Primary,
        options(),
    )
    .await;
    assert_eq!(adopted.failed_count, 0, "{:?}", adopted.stats);
    let original = root.path().join("media/IMG_0501_original.JPG");
    assert!(
        original.exists(),
        "adopted original must retain its archive path"
    );
    assert!(
        db_count(
            root.path(),
            "SELECT count(*) FROM asset_metadata_paths WHERE local_path LIKE '%IMG_0501_original.JPG' AND json_extract(embedded_metadata_receipt,'$.written.description')='B'"
        ) > 0,
        "managed adoption must transfer original field evidence"
    );
    provider[1]["fields"]["extendedDescEnc"] = serde_json::json!({"type":"STRING","value":""});
    provider[1]["fields"]["captionEnc"] = serde_json::json!({"type":"STRING","value":""});
    let requests = server.received_requests().await.unwrap().len();
    for quiet in [false, true, true] {
        let (result, _) = cycle_options(
            root.path(),
            provider.clone(),
            quiet,
            EditedNaming::Primary,
            options(),
        )
        .await;
        assert_eq!(result.failed_count, 0, "{:?}", result.stats);
        assert!(result.db_sync_token_advance_safe);
        let native = Metadata::new_from_path(&original).unwrap();
        assert!(
            matches!(native.get_tag(&ExifTag::ImageDescription(String::new())).next(), Some(ExifTag::ImageDescription(value)) if value == "B")
        );
        assert!(
            matches!(native.get_tag(&ExifTag::Artist(String::new())).next(), Some(ExifTag::Artist(value)) if value == "camera artist")
        );
        let mut packet = XmpFile::new().unwrap();
        packet
            .open_file(&original, OpenFileOptions::default().use_packet_scanning())
            .unwrap();
        assert!(
            !packet
                .xmp()
                .unwrap()
                .contains_property(xmp_ns::DC, "description")
        );
    }
    assert_eq!(server.received_requests().await.unwrap().len(), requests);
}

#[cfg(unix)]
#[tokio::test]
async fn album_directory_rename_retains_embedded_ownership_on_native_paths() {
    use little_exif::{exif_tag::ExifTag, metadata::Metadata};
    use std::os::unix::ffi::OsStringExt;
    // The toolkit backend cannot open non-UTF-8 paths; the native build exercises them.
    let native_roots: &[bool] = if cfg!(feature = "xmp") {
        &[false]
    } else {
        &[false, true]
    };
    for &native in native_roots {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(if native {
            std::ffi::OsString::from_vec(b"native-\xff".to_vec())
        } else {
            "utf8".into()
        });
        std::fs::create_dir(&root).unwrap();
        let server =
            super::server_with_media(super::VALID_ORIGINAL, super::VALID_EDIT, super::VALID_EDIT)
                .await;
        let mut provider = super::records_with_original(&server, super::VALID_ORIGINAL, None);
        provider[1]["fields"]["extendedDescEnc"] =
            serde_json::json!({"type":"STRING","value":"managed caption"});
        let options = |name| {
            let mut options = album_directory_options(name);
            options.exif_description = true;
            options
        };
        for name in ["Old", "New"] {
            let (result, _) = cycle_options(
                &root,
                provider.clone(),
                false,
                EditedNaming::Suffix,
                options(name),
            )
            .await;
            assert_eq!(
                result.failed_count, 0,
                "{native}/{name}: {:?}",
                result.stats
            );
            assert!(result.db_sync_token_advance_safe);
        }
        let media = root.join("media/Old/IMG_0501.JPG");
        let metadata = Metadata::new_from_path(&media).unwrap();
        assert!(
            matches!(metadata.get_tag(&ExifTag::ImageDescription(String::new())).next(), Some(ExifTag::ImageDescription(value)) if value == "managed caption")
        );
        assert_eq!(
            db_count(
                &root,
                "SELECT count(*) FROM provider_active_destinations WHERE current_publication=1 AND (embedded_metadata_receipt IS NULL OR json_extract(embedded_metadata_receipt,'$.output_checksum')<>local_checksum)"
            ),
            0
        );
        provider[1]["fields"]["extendedDescEnc"] = serde_json::json!({"type":"STRING","value":""});
        provider[1]["fields"]["captionEnc"] = serde_json::json!({"type":"STRING","value":""});
        let requests = server.received_requests().await.unwrap().len();
        for quiet in [false, true, true] {
            let (result, _) = cycle_options(
                &root,
                provider.clone(),
                quiet,
                EditedNaming::Suffix,
                options("New"),
            )
            .await;
            assert_eq!(
                result.failed_count, 0,
                "{native}/{quiet}: {:?}",
                result.stats
            );
            assert!(result.db_sync_token_advance_safe);
            let metadata = Metadata::new_from_path(&media).unwrap();
            assert!(
                metadata
                    .get_tag(&ExifTag::ImageDescription(String::new()))
                    .next()
                    .is_none(),
                "managed field must clear after cloud rename"
            );
        }
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
#[tokio::test]
async fn album_directory_native_root_preserves_native_publication_authority() {
    use std::os::unix::ffi::OsStringExt;
    let server = server().await;
    let temp = tempfile::tempdir().unwrap();
    let root = temp
        .path()
        .join(std::ffi::OsString::from_vec(b"native-\xff".to_vec()));
    std::fs::create_dir(&root).unwrap();
    let provider = records(&server, None);
    for (name, quiet) in [("Old", false), ("New", false), ("New", true), ("New", true)] {
        let (result, _) = cycle_options(
            &root,
            provider.clone(),
            quiet,
            EditedNaming::Suffix,
            album_directory_options(name),
        )
        .await;
        assert!(result.db_sync_token_advance_safe, "{:?}", result.stats);
        let path = root.join("media").join("Old").join("IMG_0501.JPG");
        assert_bytes(&path, ORIGINAL);
        let conn = rusqlite::Connection::open(root.join("state.db")).unwrap();
        let expected = crate::state::db::provider_selection::SelectionPath::from_path(&path)
            .key()
            .unwrap();
        assert!(conn.query_row::<bool,_,_>("SELECT EXISTS(SELECT 1 FROM provider_active_destinations WHERE path=?1 AND current_publication=1)", [expected], |row| row.get(0)).unwrap());
    }
    assert!(!root.join("media/New").exists());
}

#[tokio::test]
async fn album_directory_mixed_projection_retains_independent_smart_copy() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let provider = records(&server, None);
    for (name, quiet) in [("Old", false), ("New", false), ("New", true), ("New", true)] {
        let mut options = album_directory_options(name);
        options.folder_structure = "%Y/{album}";
        options.shared_folder_structure = Some("%Y/Old");
        let (result, _) = cycle_options(
            root.path(),
            provider.clone(),
            quiet,
            EditedNaming::Suffix,
            options,
        )
        .await;
        assert!(result.db_sync_token_advance_safe, "{:?}", result.stats);
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        let paths: Vec<std::path::PathBuf> = conn.prepare("SELECT DISTINCT compat_path FROM provider_active_destinations WHERE current_publication=1 ORDER BY compat_path").unwrap().query_map([], |row| row.get::<_, String>(0).map(std::path::PathBuf::from)).unwrap().collect::<Result<_,_>>().unwrap();
        let has_parent = |parent: &str| {
            let directory = root.path().join("media/2023").join(parent);
            paths.iter().any(|path| path.parent() == Some(&directory))
        };
        assert!(has_parent("Old"), "{paths:?}");
        if name == "New" {
            assert!(!has_parent("New"), "{paths:?}");
        }
        for path in paths {
            assert_bytes(&path, ORIGINAL);
        }
    }
}

use super::{CycleOptions, EditedNaming, cycle_options, db_count, live_records, resource};
use serde_json::json;
use std::collections::BTreeMap;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

#[tokio::test]
async fn same_name_live_photos_verify_each_child_and_reopen_after_root_move() {
    verify_same_name_live_photos(
        EditedNaming::Primary,
        22,
        &[("media", false), ("moved", false), ("moved", true)],
    )
    .await;
}

#[tokio::test]
async fn same_name_live_photos_suffix_admission_preserves_each_child() {
    verify_same_name_live_photos(
        EditedNaming::Suffix,
        22,
        &[("media", false), ("media", true)],
    )
    .await;
}

#[tokio::test]
async fn same_name_live_photos_after_manual_catalog_prefix_change() {
    verify_same_name_live_photos(
        EditedNaming::Primary,
        5,
        &[("media", false), ("rewritten", false), ("rewritten", true)],
    )
    .await;
}

async fn verify_same_name_live_photos(
    naming: EditedNaming,
    children: usize,
    cycles: &[(&'static str, bool)],
) {
    let server = MockServer::start().await;
    let root = tempfile::tempdir().unwrap();
    let mut records = Vec::new();
    let mut expected = BTreeMap::new();
    for index in 0..children {
        let master = format!("master-{index}");
        let child = format!("asset-{index}");
        let mut pair = live_records(&server.uri(), b"", b"", None);
        pair[0]["recordName"] = json!(master);
        pair[0]["fields"]["filenameEnc"]["value"] = json!("FullSizeRender.HEIC");
        pair[1]["recordName"] = json!(child);
        pair[1]["fields"]["masterRef"]["value"]["recordName"] = json!(master);
        for (field, version) in [
            ("resOriginalRes", "original"),
            ("resOriginalVidComplRes", "live_original"),
        ] {
            let endpoint = format!("{child}-{version}");
            let brand = if version == "original" {
                "heic"
            } else {
                "qt  "
            };
            let bytes = format!("\0\0\0\x18ftyp{brand}synthetic-{index:02}").into_bytes();
            pair[0]["fields"][field] = resource(&server.uri(), &endpoint, &bytes);
            Mock::given(method("GET"))
                .and(path(format!("/{endpoint}")))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.clone()))
                .mount(&server)
                .await;
            expected.insert((child.clone(), version), bytes);
        }
        records.extend(pair);
    }
    for &(directory, quiet) in cycles {
        if directory != "media" && !quiet {
            let old = root.path().join("media");
            let new = root.path().join(directory);
            std::fs::rename(&old, &new).unwrap();
            if directory == "rewritten" {
                // Model a partial manual migration: catalog paths reflect the move,
                // while durable layout bindings and claims still name the old root.
                let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
                for table in ["assets", "asset_metadata_paths"] {
                    conn.execute(
                        &format!("UPDATE {table} SET local_path=replace(local_path,?1,?2)"),
                        [old.to_str().unwrap(), new.to_str().unwrap()],
                    )
                    .unwrap();
                }
            }
        }
        let (result, _) = cycle_options(
            root.path(),
            records.clone(),
            quiet,
            naming,
            CycleOptions {
                media_directory: directory,
                ..CycleOptions::default()
            },
        )
        .await;
        assert_eq!(result.failed_count, 0, "{directory}: {:?}", result.stats);
        assert!(
            result.db_sync_token_advance_safe,
            "{directory}: {:?}",
            result.stats
        );
        if quiet {
            assert_eq!(result.stats.downloaded, 0);
        }
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        let mut rows = conn
            .prepare("SELECT id,version_size,local_path FROM assets WHERE status='downloaded'")
            .unwrap();
        let rows = rows
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .unwrap();
        let mut paths = std::collections::BTreeSet::new();
        for row in rows {
            let (child, version, path) = row.unwrap();
            let path = std::path::PathBuf::from(path);
            assert!(path.starts_with(root.path().join(directory)), "{path:?}");
            assert_eq!(
                std::fs::read(&path).unwrap(),
                expected[&(child, version.as_str())]
            );
            assert!(paths.insert(path));
        }
        assert_eq!(paths.len(), expected.len());
        assert_eq!(
            db_count(
                root.path(),
                "SELECT count(*) FROM metadata WHERE key='pending_download_config_hash'"
            ),
            0
        );
        assert_eq!(
            db_count(
                root.path(),
                "SELECT count(*) FROM primary_layout_operations WHERE phase NOT IN ('committed','cancelled')"
            ),
            0
        );
        assert_eq!(
            db_count(
                root.path(),
                "SELECT count(*) FROM provider_active_destinations WHERE admitted=1 AND verified_media=0"
            ),
            0
        );
    }
}

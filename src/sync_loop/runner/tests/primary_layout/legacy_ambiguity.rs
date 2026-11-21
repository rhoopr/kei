use super::{EditedNaming, ORIGINAL, assert_bytes, cycle, db_count, second_date_records};
use base64::Engine as _;
use sha2::{Digest, Sha256};

const DISPUTED: &[u8] = b"\xff\xd8\xff\xe0legacy plain-name owner bytes";

fn provider_hash(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes))
}

fn local_hash(bytes: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(bytes))
}

fn healthy_peer_records(server: &wiremock::MockServer) -> Vec<serde_json::Value> {
    let mut peer = second_date_records(server);
    peer[0]["recordName"] = serde_json::json!("503");
    peer[0]["fields"]["filenameEnc"]["value"] = serde_json::json!("IMG_0503.JPG");
    peer[1]["recordName"] = serde_json::json!("asset-503");
    peer[1]["fields"]["masterRef"]["value"]["recordName"] = serde_json::json!("503");
    peer
}

/// Issue #1054: inherited `assets` / `asset_metadata_paths` drift and a
/// second owner make a real legacy path ambiguous. Suffix sync is the
/// supported current path for that family, so it must bypass managed
/// planning for the unbound asset without touching the disputed bytes, while
/// an unrelated healthy peer completes and the checkpoint settles.
#[tokio::test]
async fn supported_suffix_sync_preserves_disputed_legacy_path_and_progresses() {
    let server = super::server().await;
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join("media");

    // Establish an unrelated managed family so `has_primary_layouts` keeps
    // managed planning enabled. The disputed family must still bypass it.
    let established = second_date_records(&server);
    let (managed, _) = cycle(root.path(), established, false, EditedNaming::Primary).await;
    assert_eq!(managed.failed_count, 0, "{:?}", managed.stats);
    assert!(managed.db_sync_token_advance_safe);
    assert_bytes(&media.join("IMG_0502.JPG"), ORIGINAL);
    let established_bindings = db_count(
        root.path(),
        "SELECT COUNT(*) FROM primary_layout_bindings WHERE child='asset-502'",
    );
    assert_eq!(established_bindings, 1);
    let operations = db_count(
        root.path(),
        "SELECT COUNT(*) FROM primary_layout_operations",
    );

    let disputed = media.join("IMG_0501.JPG");
    std::fs::write(&disputed, DISPUTED).unwrap();
    let qualified = media.join("IMG_0501 (1).JPG");
    let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
    // Subject: the catalogue currently claims its qualified destination, but
    // its stale metadata receipt still points at the old plain path.
    conn.execute(
        "INSERT INTO assets(library,id,version_size,checksum,filename,created_at,added_at,size_bytes,media_type,status,local_path,last_seen_at) \
         VALUES ('PrimarySync','asset-501','original',?1,'IMG_0501.JPG',1731628800,1731628800,?2,'photo','pending',?3,1731628800)",
        rusqlite::params![provider_hash(ORIGINAL), ORIGINAL.len() as i64, qualified.to_str().unwrap()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO asset_metadata_paths(library,id,version_size,local_path,provider_checksum,local_checksum,download_checksum,source_checksum) \
         VALUES ('PrimarySync','asset-501','original',?1,?2,?3,?3,?2)",
        rusqlite::params![
            disputed.to_str().unwrap(),
            provider_hash(ORIGINAL),
            local_hash(DISPUTED)
        ],
    )
    .unwrap();
    // The disputed real path is owned by a second legacy row, so the catalogue
    // sees two different owners for it: the subject's metadata receipt and
    // this asset. No managed binding, claim or operation exists for either.
    conn.execute(
        "INSERT INTO assets(library,id,version_size,checksum,filename,created_at,added_at,size_bytes,media_type,status,local_path,local_checksum,download_checksum,downloaded_at,last_seen_at) \
         VALUES ('PrimarySync','legacy-plain-owner','original',?1,'IMG_0501.JPG',1731628800,1731628800,?2,'photo','downloaded',?3,?4,?4,1731628800,1731628800)",
        rusqlite::params![
            provider_hash(DISPUTED),
            DISPUTED.len() as i64,
            disputed.to_str().unwrap(),
            local_hash(DISPUTED)
        ],
    )
    .unwrap();
    assert_eq!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_bindings WHERE child='asset-501' OR child='legacy-plain-owner'"
        ),
        0
    );
    assert_eq!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_claims WHERE child='asset-501' OR child='legacy-plain-owner'"
        ),
        0
    );
    let owners: i64 = conn
        .query_row(
            "SELECT COUNT(DISTINCT id) FROM (\
             SELECT id FROM assets WHERE local_path=?1 \
             UNION SELECT id FROM asset_metadata_paths WHERE local_path=?1)",
            [disputed.to_str().unwrap()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        owners, 2,
        "the disputed path must carry two catalogue owners"
    );
    drop(conn);

    let mut provider = super::records(&server, None);
    provider.extend(healthy_peer_records(&server));
    let (first, _) = cycle(root.path(), provider.clone(), false, EditedNaming::Suffix).await;
    assert_eq!(first.failed_count, 0, "{:?}", first.stats);
    assert!(first.db_sync_token_advance_safe, "{:?}", first.stats);
    assert_bytes(&disputed, DISPUTED);
    assert_bytes(&media.join("IMG_0503.JPG"), ORIGINAL);
    assert_eq!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_operations"
        ),
        operations,
        "suffix sync must not create managed work for the ambiguous family"
    );
    assert_eq!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_bindings WHERE child='asset-501' OR child='legacy-plain-owner'"
        ),
        0
    );
    assert_eq!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_claims WHERE child='asset-501' OR child='legacy-plain-owner'"
        ),
        0
    );

    let requests = server.received_requests().await.unwrap().len();
    for _ in 0..2 {
        let (quiet, _) = cycle(root.path(), provider.clone(), true, EditedNaming::Suffix).await;
        assert_eq!(quiet.failed_count, 0, "{:?}", quiet.stats);
        assert!(quiet.db_sync_token_advance_safe, "{:?}", quiet.stats);
        assert_eq!(quiet.stats.downloaded, 0, "{:?}", quiet.stats);
        assert_bytes(&disputed, DISPUTED);
    }
    assert_eq!(server.received_requests().await.unwrap().len(), requests);
}

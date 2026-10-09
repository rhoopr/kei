#![allow(
    clippy::print_stdout,
    reason = "CLI subcommand whose primary purpose is to print an export to stdout"
)]

use std::path::Path;

use serde::Serialize;

use crate::cli;
use crate::config;
use crate::state::{self, db::ManifestAssetRow};

#[derive(Debug, Serialize)]
struct ManifestRow {
    library: String,
    asset_id: String,
    version: String,
    filename: String,
    local_path: Option<String>,
    checksum: String,
    local_checksum: Option<String>,
    download_checksum: Option<String>,
    size_bytes: u64,
    created_at: chrono::DateTime<chrono::Utc>,
    added_at: Option<chrono::DateTime<chrono::Utc>>,
    downloaded_at: Option<chrono::DateTime<chrono::Utc>>,
    last_seen_at: chrono::DateTime<chrono::Utc>,
    media_type: String,
    status: String,
    albums: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    preserved_files: Vec<crate::state::db::primary_layout::PreservedManifestFile>,
}

impl From<ManifestAssetRow> for ManifestRow {
    fn from(row: ManifestAssetRow) -> Self {
        Self {
            library: row.library,
            asset_id: row.asset_id,
            version: row.version,
            filename: row.filename,
            local_path: row.local_path.map(|p| p.display().to_string()),
            checksum: row.checksum,
            local_checksum: row.local_checksum,
            download_checksum: row.download_checksum,
            size_bytes: row.size_bytes,
            created_at: row.created_at,
            added_at: row.added_at,
            downloaded_at: row.downloaded_at,
            last_seen_at: row.last_seen_at,
            media_type: row.media_type,
            status: row.status,
            albums: row.albums,
            preserved_files: row.preserved_files,
        }
    }
}

pub(crate) async fn run_manifest(
    args: cli::ManifestArgs,
    globals: &config::GlobalArgs,
    toml: Option<&config::TomlConfig>,
) -> anyhow::Result<()> {
    let db_path = super::super::get_db_path(globals, toml).await?;
    if !db_path.exists() {
        anyhow::bail!(
            "No state database found at {}. Run a sync first to create the local catalog.",
            db_path.display()
        );
    }

    let rows =
        load_owned_manifest_rows(&db_path, &super::super::get_account_owner(globals, toml)).await?;
    match args.format {
        cli::ManifestFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&rows)?);
        }
        cli::ManifestFormat::Csv => {
            print!("{}", render_csv(&rows)?);
        }
    }

    Ok(())
}

async fn load_owned_manifest_rows(
    path: &Path,
    owner: &state::db::account::AccountOwner,
) -> anyhow::Result<Vec<ManifestRow>> {
    let db = state::SqliteStateDb::open_owned_read_only(path, owner).await?;
    Ok(db
        .get_manifest_assets()
        .await?
        .into_iter()
        .map(ManifestRow::from)
        .collect())
}

#[cfg(test)]
async fn load_manifest_rows(path: &Path) -> anyhow::Result<Vec<ManifestRow>> {
    let db = state::SqliteStateDb::open_read_only(path).await?;
    let rows = db
        .get_manifest_assets()
        .await?
        .into_iter()
        .map(ManifestRow::from)
        .collect();
    Ok(rows)
}

fn render_csv(rows: &[ManifestRow]) -> anyhow::Result<String> {
    const HEADER: &[&str] = &[
        "library",
        "asset_id",
        "version",
        "filename",
        "local_path",
        "checksum",
        "local_checksum",
        "download_checksum",
        "size_bytes",
        "created_at",
        "added_at",
        "downloaded_at",
        "last_seen_at",
        "media_type",
        "status",
        "albums",
    ];

    let mut out = String::new();
    let has_preserved_files = rows.iter().any(|row| !row.preserved_files.is_empty());
    let mut header = HEADER.to_vec();
    if has_preserved_files {
        header.push("preserved_files");
    }
    push_csv_record(&mut out, &header);
    for row in rows {
        let albums = serde_json::to_string(&row.albums)?;
        let size_bytes = row.size_bytes.to_string();
        let created_at = row.created_at.to_rfc3339();
        let added_at = row.added_at.map(|dt| dt.to_rfc3339()).unwrap_or_default();
        let downloaded_at = row
            .downloaded_at
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_default();
        let last_seen_at = row.last_seen_at.to_rfc3339();
        let mut fields = vec![
            row.library.as_str(),
            row.asset_id.as_str(),
            row.version.as_str(),
            row.filename.as_str(),
            row.local_path.as_deref().unwrap_or(""),
            row.checksum.as_str(),
            row.local_checksum.as_deref().unwrap_or(""),
            row.download_checksum.as_deref().unwrap_or(""),
            size_bytes.as_str(),
            created_at.as_str(),
            added_at.as_str(),
            downloaded_at.as_str(),
            last_seen_at.as_str(),
            row.media_type.as_str(),
            row.status.as_str(),
            albums.as_str(),
        ];
        let preserved_files = serde_json::to_string(&row.preserved_files)?;
        if has_preserved_files {
            fields.push(&preserved_files);
        }
        push_csv_record(&mut out, &fields);
    }
    Ok(out)
}

fn push_csv_record(out: &mut String, fields: &[&str]) {
    for (idx, field) in fields.iter().enumerate() {
        if idx > 0 {
            out.push(',');
        }
        push_csv_field(out, field);
    }
    out.push('\n');
}

fn push_csv_field(out: &mut String, field: &str) {
    let must_quote = field
        .bytes()
        .any(|b| matches!(b, b',' | b'"' | b'\n' | b'\r'));
    if !must_quote {
        out.push_str(field);
        return;
    }

    out.push('"');
    for ch in field.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::state::SqliteStateDb;
    use crate::test_helpers::TestAssetRecord;

    #[tokio::test]
    async fn manifest_reads_local_sqlite_state_without_auth() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("manifest.db");
        let local_path = dir.path().join("photos").join("img001.jpg");
        let db = SqliteStateDb::open(&db_path).await.expect("open state");
        let record = TestAssetRecord::new("ASSET_1")
            .filename("img001.jpg")
            .checksum("remote-sha")
            .size(42)
            .created_at(Utc.with_ymd_and_hms(2024, 1, 2, 3, 4, 5).unwrap())
            .build();
        db.upsert_seen(&record).await.expect("insert asset");
        db.mark_downloaded(
            "PrimarySync",
            "ASSET_1",
            "original",
            &local_path,
            "local-sha",
            Some("download-sha"),
        )
        .await
        .expect("mark downloaded");
        db.add_asset_album("PrimarySync", "ASSET_1", "Family", "icloud")
            .await
            .expect("album");
        db.add_asset_album("PrimarySync", "ASSET_1", "Vacation", "icloud")
            .await
            .expect("album");
        drop(db);

        let rows = load_manifest_rows(&db_path).await.expect("manifest rows");

        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.library, "PrimarySync");
        assert_eq!(row.asset_id, "ASSET_1");
        assert_eq!(row.version, "original");
        assert_eq!(row.filename, "img001.jpg");
        let expected_local_path = local_path.to_string_lossy().to_string();
        assert_eq!(
            row.local_path.as_deref(),
            Some(expected_local_path.as_str())
        );
        assert_eq!(row.checksum, "remote-sha");
        assert_eq!(row.local_checksum.as_deref(), Some("local-sha"));
        assert_eq!(row.download_checksum.as_deref(), Some("download-sha"));
        assert_eq!(row.size_bytes, 42);
        assert_eq!(
            row.created_at,
            Utc.with_ymd_and_hms(2024, 1, 2, 3, 4, 5).unwrap()
        );
        assert_eq!(row.media_type, "photo");
        assert_eq!(row.status, "downloaded");
        assert_eq!(row.albums, ["Family", "Vacation"]);
    }

    #[tokio::test]
    async fn tactical_manifest_keeps_library_id_version_and_album_identity() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("manifest.db");
        let db = SqliteStateDb::open(&db_path).await.unwrap();
        let mut expected = Vec::new();
        for library in ["LibraryA", "LibraryB"] {
            for id in ["SAME_ID", "SECOND_ID"] {
                let albums = if library == "LibraryA" {
                    vec!["A-only", "Shared-name"]
                } else {
                    vec!["B-only", "Shared-name"]
                };
                for version in [
                    crate::state::VersionSizeKey::Original,
                    crate::state::VersionSizeKey::Medium,
                ] {
                    let key = format!("{library}-{id}-{}", version.as_str());
                    let provider = format!("provider-{key}");
                    let local = format!("local-{key}");
                    let download = format!("download-{key}");
                    let path = dir.path().join(&key).join("same.jpg");
                    let record = TestAssetRecord::new(id)
                        .library(library)
                        .version_size(version)
                        .filename("same.jpg")
                        .checksum(&provider)
                        .build();
                    db.upsert_seen(&record).await.unwrap();
                    db.mark_downloaded(
                        library,
                        id,
                        version.as_str(),
                        &path,
                        &local,
                        Some(&download),
                    )
                    .await
                    .unwrap();
                    expected.push((
                        library.to_string(),
                        id.to_string(),
                        version.as_str().to_string(),
                        "same.jpg".to_string(),
                        Some(path.display().to_string()),
                        provider,
                        Some(local),
                        Some(download),
                        albums.iter().map(|v| (*v).to_string()).collect::<Vec<_>>(),
                    ));
                }
                for album in albums {
                    db.add_asset_album(library, id, album, "icloud")
                        .await
                        .unwrap();
                }
            }
        }
        drop(db);
        expected.sort();
        for _ in 0..2 {
            let rows = load_manifest_rows(&db_path).await.unwrap();
            assert_eq!(rows.len(), 8);
            let actual: Vec<_> = rows
                .into_iter()
                .map(|r| {
                    (
                        r.library,
                        r.asset_id,
                        r.version,
                        r.filename,
                        r.local_path,
                        r.checksum,
                        r.local_checksum,
                        r.download_checksum,
                        r.albums,
                    )
                })
                .collect();
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn csv_escapes_special_fields_and_keeps_albums_as_json() {
        let mut rows = vec![ManifestRow {
            library: "PrimarySync".to_string(),
            asset_id: "ASSET,1".to_string(),
            version: "original".to_string(),
            filename: "quote\"photo.jpg".to_string(),
            local_path: None,
            checksum: "remote-sha".to_string(),
            local_checksum: None,
            download_checksum: None,
            size_bytes: 7,
            created_at: Utc.with_ymd_and_hms(2024, 1, 2, 3, 4, 5).unwrap(),
            added_at: None,
            downloaded_at: None,
            last_seen_at: Utc.with_ymd_and_hms(2024, 1, 3, 3, 4, 5).unwrap(),
            media_type: "photo".to_string(),
            status: "pending".to_string(),
            albums: vec!["Family".to_string(), "Vacation, 2024".to_string()],
            preserved_files: Vec::new(),
        }];

        let csv = render_csv(&rows).expect("csv");

        assert!(csv.contains("\"ASSET,1\""));
        assert!(csv.contains("\"quote\"\"photo.jpg\""));
        assert!(csv.contains("\"[\"\"Family\"\",\"\"Vacation, 2024\"\"]\""));
        assert_eq!(csv.lines().next().unwrap().split(',').count(), 16);
        assert!(
            serde_json::to_value(&rows).unwrap()[0]
                .get("preserved_files")
                .is_none()
        );
        rows[0]
            .preserved_files
            .push(crate::state::db::primary_layout::PreservedManifestFile {
                operation: "operation-501".into(),
                family: "family-501".into(),
                previous_generation: 2,
                phase: "committed".into(),
                native_path: crate::state::db::provider_selection::SelectionPath::Utf8(
                    "/photos/.kei-history/old.jpg".into(),
                ),
                provider_checksum: "earlier-provider-checksum".into(),
                local_checksum: "exact-local-checksum".into(),
                source_checksum: Some("source-checksum".into()),
                sidecar_checksum: Some("sidecar-checksum".into()),
                size_bytes: 42,
            });
        let json = serde_json::to_value(&rows).unwrap();
        assert_eq!(
            json[0]["preserved_files"][0]["native_path"],
            serde_json::json!({"encoding":"utf8","path":"/photos/.kei-history/old.jpg"})
        );
        assert_eq!(
            json[0]["preserved_files"][0]["provider_checksum"],
            "earlier-provider-checksum"
        );
        let csv = render_csv(&rows).unwrap();
        assert!(csv.lines().next().unwrap().ends_with(",preserved_files"));
        assert_eq!(csv.lines().next().unwrap().split(',').count(), 17);
        assert!(csv.contains("earlier-provider-checksum"));
    }
}
